//! Async IMAP client wrapped around `async-imap`, plus pure helpers in
//! [`util`].
//!
//! Per-account state lives in one [`ImapClient`] guarded by a Mutex (created
//! by `main`). The client transparently reconnects on transport-level errors
//! via the `retry_read!` macro, caches the currently-selected folder to
//! skip redundant SELECTs, and enforces `allowed_folders` inside
//! `ensure_selected` so an LLM can't bypass the whitelist by passing an
//! unfiltered folder name.
//!
//! Pure helpers (search-criteria escaping, ISO-date conversion, host detection)
//! live in [`util`] and are unit-tested in isolation.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_imap::Session;
use async_imap::types::Fetch;
use futures_util::TryStreamExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::config::{AccountConfig, AuthMethod};
use crate::email::{self, EmailFull, EmailSummary};

mod util;
pub use util::{
    FORWARD_PREFIXES, REPLY_PREFIXES, build_or_criteria, clean_imap_error,
    host_supports_unicode_search, imap_astring, is_retryable_error, iso_to_imap_date,
    sanitize_log_str, starts_with_ignore_ascii_case,
};
use util::{clean_message_id, is_connection_error, strip_email_prefixes};

/// Retry a read-only IMAP operation once on connection errors. After a connection
/// error, the client is marked dead; the second call triggers `ensure_connected`
/// → fresh TLS + login. Only for idempotent operations (SEARCH / FETCH / LIST /
/// STATUS). Never use for APPEND / COPY / non-idempotent STOREs.
macro_rules! retry_read {
    ($self:ident . $op:ident ( $($arg:expr),* $(,)? )) => {{
        match $self.$op($($arg),*).await {
            Ok(r) => Ok(r),
            Err(e) if is_connection_error(&e.to_string()) => {
                tracing::info!(
                    "Connection error on {}, retrying after reconnect: {e}",
                    stringify!($op)
                );
                $self.mark_dead();
                $self.$op($($arg),*).await
            }
            Err(e) => Err(e),
        }
    }};
}

type ImapSession = Session<TlsStream<TcpStream>>;

/// Hard cap on a single email body (raw RFC 822 bytes, already decoded by
/// IMAP's BODY[] fetch). Defends against a compromised or malicious server
/// claiming a multi-GB body to OOM the process. 100 MiB is several times
/// larger than realistic emails with max-size attachments.
const MAX_EMAIL_BYTES: usize = 100 * 1024 * 1024;

/// Hard cap on the number of folders we'll process in a single `LIST` response.
/// A malicious server could return millions of folders to drive the subsequent
/// per-folder STATUS loop into a `DoS`.
const MAX_FOLDER_COUNT: usize = 10_000;

// TRUNCATED-BODY FETCH (deferred): an earlier iteration added
// `BODY.PEEK[]<0.16384>` here to save 5–10× bandwidth on HTML-heavy
// inboxes. It works against RFC-compliant servers, but `imap-proto`
// 0.16.6 (our parser) requires a literal SP between the `<origin>`
// marker and the `{size}` literal per RFC 3501 §7.4.2, and at least
// `GreenMail` 2.1.2 emits them adjacent (`BODY[]<0>{416}`). That tripped
// the FETCH-response parser end-to-end. Fixing it needs a fork of
// imap-proto (and by extension async-imap) to accept the missing-SP
// form leniently. Kept as the existing full-body fetch below until
// upstream relaxes the parse or we swap to a different IMAP crate.
// INTERNALDATE feeds the sub-day `since`/`before` cut: IMAP's own
// SINCE/BEFORE are day-granular, so the exact bound is applied client-side
// against the arrival timestamp (not the sender-controlled Date header).
const SUMMARY_FETCH_ITEMS: &str = "(BODY.PEEK[] FLAGS UID INTERNALDATE)";

/// Client-side full-text criteria for servers whose SEARCH cannot take them
/// (Outlook 365 silently returns zero matches for `CHARSET UTF-8`). Applied
/// in [`summarize_fetches`] against the parsed message — subject, the
/// addressing headers and the full `body_text`, approximating RFC 3501's
/// `TEXT` (which matches header OR body) — BEFORE the message is cut down
/// to a 200-character snippet. Matching only the snippet (the original
/// design) silently dropped every mail whose term appeared later in the
/// body; matching only the body dropped mails carrying the term in their
/// subject, which the server-side `TEXT` finds.
///
/// **Invariant: all stored needles are already lowercased** — the caller
/// owns the `.to_lowercase()` so the per-mail work folds only the message.
/// `all` terms are AND-combined; each `any` group must have at least one
/// member present (groups themselves AND-combine).
#[derive(Debug, Default)]
pub struct BodyTextFilter {
    pub all: Vec<String>,
    pub any: Vec<Vec<String>>,
}

impl BodyTextFilter {
    /// The no-op filter the plain list paths pass.
    pub const EMPTY: Self = Self {
        all: Vec::new(),
        any: Vec::new(),
    };

    pub const fn is_empty(&self) -> bool {
        self.all.is_empty() && self.any.is_empty()
    }

    fn matches(&self, email: &EmailFull) -> bool {
        let hay = Self::haystack(email);
        self.all.iter().all(|t| hay.contains(t.as_str()))
            && self
                .any
                .iter()
                .all(|group| group.iter().any(|t| hay.contains(t.as_str())))
    }

    /// One lowercased searchable text per message: subject and addressing
    /// headers first, then the full body. Fields are newline-separated so a
    /// term can never match by straddling two fields. The full-body fold is
    /// a deliberate cost: it runs only on the fallback path, and any partial
    /// view would reintroduce the silent false negatives this filter fixes.
    fn haystack(email: &EmailFull) -> String {
        let mut hay = String::with_capacity(email.body_text.len() + 256);
        hay.push_str(&email.subject.to_lowercase());
        hay.push('\n');
        for addr in email
            .from
            .iter()
            .chain(email.to.iter())
            .chain(email.cc.iter())
        {
            if let Some(name) = &addr.name {
                hay.push_str(&name.to_lowercase());
                hay.push(' ');
            }
            hay.push_str(&addr.address.to_lowercase());
            hay.push('\n');
        }
        hay.push_str(&email.body_text.to_lowercase());
        hay
    }
}

/// Everything applied to a fetched message AFTER the server's SEARCH:
/// full-text criteria the server could not take, and the sub-day part of
/// `since`/`before` (IMAP's own operators are day-granular; the widened
/// server window is cut exactly here, against INTERNALDATE — the arrival
/// time, deliberately not the sender-controlled `Date` header).
#[derive(Debug, Default)]
pub struct PostFetchFilter {
    pub body: BodyTextFilter,
    /// Keep messages whose INTERNALDATE is at or after this Unix second.
    pub internal_since_unix: Option<i64>,
    /// Keep messages whose INTERNALDATE is strictly before this Unix second.
    pub internal_before_unix: Option<i64>,
}

impl PostFetchFilter {
    /// The no-op filter the plain list paths pass.
    pub const EMPTY: Self = Self {
        body: BodyTextFilter::EMPTY,
        internal_since_unix: None,
        internal_before_unix: None,
    };

    pub const fn is_empty(&self) -> bool {
        self.body.is_empty()
            && self.internal_since_unix.is_none()
            && self.internal_before_unix.is_none()
    }

    /// Whether any sub-day time bound is present — the part the server's
    /// day-granular SINCE/BEFORE cannot express, resolved on a lightweight
    /// `(UID INTERNALDATE)` round before counting and paging.
    const fn has_time_bounds(&self) -> bool {
        self.internal_since_unix.is_some() || self.internal_before_unix.is_some()
    }

    /// The time bounds alone. A message whose INTERNALDATE the server did
    /// not report is KEPT — dropping it would silently lose mail over a
    /// missing metadata field, and the day-granular server window already
    /// bounds how wrong that can be.
    fn time_matches(&self, internal_unix: Option<i64>) -> bool {
        let Some(ts) = internal_unix else {
            return true;
        };
        self.internal_since_unix.is_none_or(|since| ts >= since)
            && self.internal_before_unix.is_none_or(|before| ts < before)
    }
}

/// Turn a list of summary-shaped fetch responses into `EmailSummary`
/// rows. Centralizes the UID-skip + bounded body + parse + post-filter +
/// summarize pipeline shared by `list_emails`, `list_unread_emails`, and
/// `search_emails`.
fn summarize_fetches(
    fetches: &[Fetch],
    folder: &str,
    post_filter: &PostFetchFilter,
) -> Vec<EmailSummary> {
    // Decoded once per call, not per row — display sugar for folder names
    // that travel as modified UTF-7 (`Gel&APY-schte Elemente`).
    let folder_display = safe_display_name(folder);
    let mut out = Vec::with_capacity(fetches.len());
    for fetch in fetches {
        // Skip responses without a UID rather than defaulting to 0 — two
        // such responses would otherwise collide at uid=0 and yield
        // unaddressable entries (callers can't later FETCH/STORE uid=0).
        let Some(uid) = fetch.uid else { continue };
        let Some(body) = bounded_body(fetch, uid) else {
            continue;
        };
        // NOT redundant with the pre-paging INTERNALDATE cut in
        // `search_emails_once`: an unsolicited FETCH row without
        // INTERNALDATE (parallel flag update) rides through that cut on
        // the KEEP policy — this second check catches it once the full
        // fetch reports the real timestamp. It is also the only cut for
        // callers that skip the prefetch.
        if !post_filter.time_matches(fetch.internal_date().map(|d| d.timestamp())) {
            continue;
        }
        let flags = parse_flags(fetch);
        let full = email::parse_email_no_html(uid, folder, body, flags);
        // Full-text criteria run here, on the complete message, while it
        // still exists — `summarize` drops the body for the snippet right
        // after.
        if !post_filter.body.is_empty() && !post_filter.body.matches(&full) {
            continue;
        }
        let mut summary = email::summarize(full, 200);
        summary.folder_display.clone_from(&folder_display);
        // Echo the arrival time exactly when a sub-day bound cut on it:
        // `date` is the sender's header and may legitimately sit outside
        // the requested window — without this field that looks like a
        // filter bug and is unverifiable from the result.
        if post_filter.has_time_bounds() {
            summary.internal_date = fetch
                .internal_date()
                .and_then(|d| email::unix_to_utc_iso(d.timestamp()));
        }
        out.push(summary);
    }
    out
}

/// Return the fetch body only if within [`MAX_EMAIL_BYTES`]. Oversize bodies
/// are treated as if absent and logged so ops notice repeated skips. Defends
/// against a compromised server returning a multi-GB body that would OOM
/// either `parse_email`'s internal allocations or the subsequent serialization.
fn bounded_body(fetch: &Fetch, uid: u32) -> Option<&[u8]> {
    let body = fetch.body()?;
    if body.len() > MAX_EMAIL_BYTES {
        tracing::warn!(
            uid = uid,
            size = body.len(),
            cap = MAX_EMAIL_BYTES,
            "Skipping oversized email body"
        );
        return None;
    }
    Some(body)
}

pub struct ImapClient {
    session: Option<ImapSession>,
    config: AccountConfig,
    // `Vec` not `HashSet` — the reader does case-insensitive linear match
    // (`eq_ignore_ascii_case`), which can't use hash-based lookup. HashSet
    // was misleading about the lookup cost. For typical whitelists (< 50
    // entries) linear is negligible.
    allowed_folders: Option<Vec<String>>,
    selected_folder: Option<String>,
    selected_exists: u32,
    /// UIDVALIDITY captured on the last successful SELECT for
    /// `selected_folder`. If a re-SELECT returns a different value for the
    /// same folder, any UIDs the LLM obtained from a prior call are stale
    /// (different epoch). We surface a warning rather than erroring because
    /// the MCP protocol has no structured "cache invalidated" signal — the
    /// caller's chosen mitigation is typically a fresh list/search.
    last_uid_validity: Option<(String, u32)>,
    cached_folder_names: Option<Vec<String>>,
    /// Cached `OAuth2` access token, reused across reconnects until expiry.
    /// Gmail/Outlook 365 tokens last ~1h; every needless refresh is a
    /// 100-500ms HTTPS roundtrip that delays reconnect.
    cached_oauth_token: Option<crate::oauth2::AccessToken>,
    /// The `OAuth2` refresh token currently in play: taken from the token
    /// state file when one exists (`adopt_stored_token`), otherwise
    /// bootstrapped from the config, and updated in place whenever the
    /// provider rotates it. The config value itself is never mutated.
    active_refresh_token: Option<String>,
    /// Sanitized description of the last error that caused `mark_dead`
    /// (or the last failed `connect()`). Surfaced via
    /// [`ConnectionState::last_error`] for the `account_health` tool so
    /// operators can answer "why is this account offline?" without tailing
    /// stderr.
    last_error: Option<String>,
}

/// Read-only snapshot of an `ImapClient`'s health for the `account_health`
/// MCP tool. Tries to stay free of transient I/O — just reports whatever
/// the client already knows locally.
#[derive(Debug, serde::Serialize)]
pub struct ConnectionState {
    pub connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// `Some(true)` means an `OAuth2` token is cached and within its TTL.
    /// `Some(false)` means `OAuth2` is configured but no valid cached
    /// token right now. `None` for password-auth accounts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_token_valid: Option<bool>,
    /// Seconds until the cached `OAuth2` access token expires, when
    /// available. Useful to predict the next reconnect cost.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_expires_in_secs: Option<u64>,
}

impl ImapClient {
    pub fn new(config: AccountConfig) -> Self {
        let allowed_folders = config.allowed_folders.clone();

        if config.accept_invalid_certs {
            tracing::warn!(
                account = %config.name,
                host = %config.host,
                "TLS certificate verification is DISABLED — traffic on this account can be intercepted. Only use on trusted networks."
            );
        }

        // A blank config value counts as "unset" rather than becoming an
        // active token that could only ever be rejected.
        let active_refresh_token = config
            .oauth2
            .as_ref()
            .and_then(|o| o.refresh_token.clone())
            .filter(|s| !s.is_empty());
        Self {
            session: None,
            config,
            allowed_folders,
            selected_folder: None,
            selected_exists: 0,
            last_uid_validity: None,
            cached_folder_names: None,
            cached_oauth_token: None,
            active_refresh_token,
            last_error: None,
        }
    }

    /// Adopt the refresh token persisted by an earlier run, a parallel
    /// process or `imap-mcp-rs reauth`. Call once after `new`. A stored token
    /// always wins: it is server-managed state, whereas the config value is
    /// only the bootstrap value for a mailbox that has never been authorized
    /// through this server (see [`crate::token_state`]).
    pub fn adopt_stored_token(&mut self) {
        if !matches!(self.config.auth_method, AuthMethod::OAuth2) {
            return;
        }
        let key = crate::token_state::account_key(&self.config);
        if let Some(stored) = crate::token_state::load(&key) {
            tracing::debug!(account = %self.config.name, "Using persisted refresh token");
            self.active_refresh_token = Some(stored.refresh_token);
        }
    }

    /// Refresh the access token using the active refresh token, adopting and
    /// persisting provider-side rotation (see [`crate::token_state`]).
    /// Returns the raw access token for the XOAUTH2 exchange; a dead token
    /// goes through [`Self::refresh_fallback`] first.
    async fn refresh_access_token_with_rotation(&mut self) -> Result<String> {
        let oauth2_config = self
            .config
            .oauth2
            .as_ref()
            .context("OAuth2 config required")?
            .clone();
        let active = self.active_refresh_token.clone().with_context(|| {
            format!(
                "no OAuth2 refresh token for this account — run `imap-mcp-rs reauth {}`",
                self.config.name
            )
        })?;

        let outcome = match crate::oauth2::refresh_access_token(&oauth2_config, &active).await {
            Ok(o) => o,
            Err(e) if Self::is_invalid_grant(&e) => {
                self.refresh_fallback(&oauth2_config, &active, e).await?
            }
            // Anything else the provider can explain (expired client secret,
            // unknown client): pass its remedy up so `last_error` tells the
            // operator what to fix instead of just what broke.
            Err(e) => return Err(self.with_remedy(e)),
        };

        let token = outcome.access.token.clone();
        self.cached_oauth_token = Some(outcome.access);
        if let Some(new_rt) = outcome.rotated_refresh_token {
            self.active_refresh_token = Some(new_rt.clone());
            let key = crate::token_state::account_key(&self.config);
            if let Err(e) = crate::token_state::store(&key, &new_rt) {
                // A persist failure must not take mail down — but the
                // rotation then only lives in this process; warn loudly.
                tracing::warn!(
                    account = %self.config.name,
                    error = %e,
                    "Failed to persist rotated refresh token — rotation is lost at next restart"
                );
            }
        }
        Ok(token)
    }

    /// The active refresh token was rejected as `invalid_grant`. One retry
    /// is worthwhile: a parallel server process may have rotated the token
    /// in the meantime, so re-read the state file and try what it holds now.
    /// If that fails too, the grant is genuinely gone — return the original
    /// error with reauth guidance so `last_error` says what to do.
    async fn refresh_fallback(
        &mut self,
        oauth2_config: &crate::config::OAuth2Config,
        tried: &str,
        original_err: anyhow::Error,
    ) -> Result<crate::oauth2::RefreshOutcome> {
        let key = crate::token_state::account_key(&self.config);
        let candidate = crate::token_state::load(&key)
            .map(|s| s.refresh_token)
            .filter(|c| !c.is_empty() && c != tried);

        if let Some(candidate) = candidate {
            tracing::info!(
                account = %self.config.name,
                "Active refresh token rejected (invalid_grant) — retrying with the one \
                 another process has since stored"
            );
            match crate::oauth2::refresh_access_token(oauth2_config, &candidate).await {
                Ok(outcome) => {
                    self.active_refresh_token = Some(candidate);
                    return Ok(outcome);
                }
                Err(e) => {
                    tracing::warn!(
                        account = %self.config.name,
                        error = %e,
                        "Stored refresh token also rejected"
                    );
                }
            }
        }

        Err(original_err.context(format!(
            "refresh token rejected (invalid_grant) — run `imap-mcp-rs reauth {}` to re-authorize",
            self.config.name
        )))
    }

    /// `true` when the error chain bottoms out in an `OAuth2` `invalid_grant`
    /// — the refresh token itself is dead and retrying it is pointless.
    fn is_invalid_grant(e: &anyhow::Error) -> bool {
        e.downcast_ref::<crate::oauth2::OAuth2Error>()
            .is_some_and(crate::oauth2::OAuth2Error::is_invalid_grant)
    }

    /// Attach the provider's remedy to an `OAuth2` failure, if it has one.
    /// Errors without a known fix are returned untouched rather than padded
    /// with guesswork.
    fn with_remedy(&self, e: anyhow::Error) -> anyhow::Error {
        match e
            .downcast_ref::<crate::oauth2::OAuth2Error>()
            .and_then(|o| o.remedy(&self.config.name))
        {
            Some(remedy) => e.context(remedy),
            None => e,
        }
    }

    /// Snapshot of this client's current health for the `account_health`
    /// tool. Pure read — doesn't touch the network.
    pub fn connection_state(&self) -> ConnectionState {
        let (oauth_token_valid, oauth_expires_in_secs) =
            if matches!(self.config.auth_method, AuthMethod::OAuth2) {
                let now = std::time::Instant::now();
                let (valid, secs) = self.cached_oauth_token.as_ref().map_or((false, None), |t| {
                    if t.expires_at > now {
                        (t.is_valid(), Some((t.expires_at - now).as_secs()))
                    } else {
                        (false, Some(0))
                    }
                });
                (Some(valid), secs)
            } else {
                (None, None)
            };
        ConnectionState {
            connected: self.session.is_some(),
            last_error: self.last_error.clone(),
            oauth_token_valid,
            oauth_expires_in_secs,
        }
    }

    /// Establish a session, recording the outcome in `last_error` so
    /// `account_health` can explain a dead account. Wrapping here (rather
    /// than only in `ensure_connected`) means the *startup* connect is
    /// covered too — that is the path an expired OAuth token fails on, and
    /// leaving it unrecorded was why "why is Office offline?" previously
    /// required reading stderr.
    pub async fn connect(&mut self) -> Result<()> {
        let result = self.connect_inner().await;
        match &result {
            Ok(()) => self.last_error = None,
            // `{e:#}` (whole chain): the actionable part is often the outer
            // context ("run `imap-mcp-rs reauth …`") while the reason sits
            // underneath (`AADSTS700082`). Safe since token-endpoint errors
            // now carry only the sanitized RFC 6749 fields, never raw bodies.
            Err(e) => self.last_error = Some(sanitize_log_str(&format!("{e:#}"))),
        }
        result
    }

    async fn connect_inner(&mut self) -> Result<()> {
        // Callers must `disconnect` or `mark_dead` before reconnecting — a
        // session overwrite would otherwise drop the old TLS stream without
        // a clean LOGOUT, leaving the server with a hanging half-session
        // until TCP timeout.
        debug_assert!(
            self.session.is_none(),
            "connect() called while session is Some — caller forgot mark_dead/disconnect"
        );
        let tls_stream = self.establish_tls().await?;
        let mut client = async_imap::Client::new(tls_stream);

        // Read the server greeting before any commands.
        // async-imap's Client::new() doesn't consume the greeting, which causes
        // authenticate() to misinterpret it as a response to the AUTHENTICATE command.
        let _greeting = client
            .read_response()
            .await
            .context("Failed to read server greeting")?;

        let session = match self.config.auth_method {
            AuthMethod::Password => {
                let password = self
                    .config
                    .password
                    .as_deref()
                    .context("Password required but not configured")?;
                client
                    .login(&self.config.username, password)
                    .await
                    .map_err(|(e, _)| e)
                    .context("IMAP login failed")?
            }
            AuthMethod::OAuth2 => {
                // Reuse a cached token when it's still within its TTL. Only
                // hit the OAuth endpoint on first connect or after expiry.
                let access_token = match &self.cached_oauth_token {
                    Some(t) if t.is_valid() => {
                        tracing::debug!("Using cached OAuth2 access token");
                        t.token.clone()
                    }
                    _ => self.refresh_access_token_with_rotation().await?,
                };
                // Strip any stray `\x01` from both values before format —
                // the char is the XOAUTH2 field separator, and injection via
                // config.username or a malicious OAuth-token-endpoint response
                // could otherwise confuse the server-side parser.
                let clean_user: String = self.config.username.replace('\x01', "");
                let clean_token: String = access_token.replace('\x01', "");
                let auth_string = format!("user={clean_user}\x01auth=Bearer {clean_token}\x01\x01");
                tracing::debug!(auth_len = auth_string.len(), "Attempting XOAUTH2");
                match client
                    .authenticate("XOAUTH2", XOAuth2Authenticator(auth_string))
                    .await
                {
                    Ok(session) => session,
                    Err((e, _)) => {
                        // Server rejected the token — invalidate cache so the
                        // next connect attempt refreshes rather than replaying
                        // a revoked token.
                        self.cached_oauth_token = None;
                        return Err(
                            anyhow::Error::from(e).context("IMAP OAuth2 authentication failed")
                        );
                    }
                }
            }
        };

        tracing::info!(
            host = %self.config.host,
            user = %self.config.username,
            "Connected to IMAP server"
        );

        self.session = Some(session);
        self.selected_folder = None;
        self.selected_exists = 0;
        self.cached_folder_names = None;
        Ok(())
    }

    pub async fn disconnect(&mut self) {
        if let Some(mut session) = self.session.take() {
            // Cap LOGOUT at 5s — otherwise a half-dead TCP connection (server
            // vanished, keepalive hasn't fired yet) would hang the entire
            // process shutdown until the OS times out the TCP close.
            let logout = tokio::time::timeout(Duration::from_secs(5), session.logout()).await;
            match logout {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("Error during IMAP logout: {e}"),
                Err(_) => tracing::warn!("IMAP logout timed out, abandoning session"),
            }
        }
        self.selected_folder = None;
        self.selected_exists = 0;
        self.cached_folder_names = None;
    }

    fn session(&mut self) -> Result<&mut ImapSession> {
        self.session
            .as_mut()
            .context("Not connected to IMAP server")
    }

    /// Mark the session as dead. The next `ensure_connected` call
    /// will trigger a reconnect attempt. Also clears `cached_folder_names`
    /// so a caller hitting `get_folder_names_once` between `mark_dead` and
    /// the next `ensure_connected` does not return stale cache without
    /// going through a reconnect.
    pub fn mark_dead(&mut self) {
        self.session = None;
        self.selected_folder = None;
        self.selected_exists = 0;
        self.cached_folder_names = None;
    }

    /// Ensure we have a live IMAP session, reconnecting if necessary. The
    /// reconnect path is wrapped in a 15s timeout matching the initial
    /// `main.rs` connect — without this, a hostile server stuck in a
    /// never-ending XOAUTH2 continuation-challenge loop could hold this
    /// account's mutex indefinitely (TCP keepalive only cuts in ~60s, and
    /// the inner `XOAuth2Authenticator::process` re-sends the same token
    /// on every challenge with no internal bound).
    ///
    /// Reconnect failures are stashed in `last_error` so `account_health`
    /// can surface them to the operator without tailing stderr — `connect`
    /// records its own outcome; the timeout case is recorded here, since it
    /// aborts `connect` before it can do so itself.
    async fn ensure_connected(&mut self) -> Result<()> {
        if self.session.is_some() {
            return Ok(());
        }
        tracing::info!(
            account = %self.config.name,
            "IMAP session lost, attempting reconnect..."
        );
        let Ok(result) = tokio::time::timeout(Duration::from_secs(15), self.connect()).await else {
            const TIMED_OUT: &str = "IMAP reconnect timed out";
            self.last_error = Some(TIMED_OUT.to_string());
            return Err(anyhow::anyhow!(TIMED_OUT));
        };
        result
    }

    /// Select a folder, skipping the IMAP command if already selected.
    /// Returns the message count (exists) from the SELECT response, or the
    /// cached value if we skipped the command. Enforces `allowed_folders`
    /// — an explicit folder name bypassing this check was the main security
    /// gap of the previous implementation.
    async fn ensure_selected(&mut self, folder: &str) -> Result<u32> {
        // Fast path: if this folder is already selected, it was vetted against
        // `allowed_folders` on the prior SELECT — skip the re-check entirely.
        if self.selected_folder.as_deref() == Some(folder) {
            return Ok(self.selected_exists);
        }
        if !self.is_folder_allowed(folder) {
            anyhow::bail!("Folder \"{folder}\" is not in allowed_folders for this account");
        }
        self.ensure_connected().await?;
        let session = self.session()?;
        let mailbox = match session.select(folder).await {
            Ok(m) => m,
            Err(e) => {
                // Per RFC 3501 §6.3.1: a failed SELECT deselects the previously
                // selected mailbox. Our cache must match that reality, otherwise
                // the next ensure_selected hit would skip a necessary re-SELECT
                // and subsequent FETCH/SEARCH fail with "no mailbox selected".
                self.selected_folder = None;
                self.selected_exists = 0;
                let err: anyhow::Error = e.into();
                return Err(self.check_error(err));
            }
        };
        // Compare UIDVALIDITY against the previous SELECT of the same folder.
        // A change means the server rotated UIDs — per RFC 3501 any UID the
        // LLM obtained from a prior call now addresses a different (or no)
        // message. Rare under normal servers, possible after mailbox
        // rebuilds, and exploitable by a hostile server to redirect an LLM
        // mark_as_read/move/delete onto freshly-injected content.
        if let Some(new_uv) = mailbox.uid_validity
            && let Some((prev_folder, prev_uv)) = &self.last_uid_validity
            && prev_folder.eq_ignore_ascii_case(folder)
            && *prev_uv != new_uv
        {
            tracing::warn!(
                account = %self.config.name,
                folder = %sanitize_log_str(folder),
                prev_uid_validity = prev_uv,
                new_uid_validity = new_uv,
                "UIDVALIDITY changed — UIDs from prior calls may reference different messages"
            );
        }
        self.selected_folder = Some(folder.to_string());
        self.selected_exists = mailbox.exists;
        if let Some(uv) = mailbox.uid_validity {
            self.last_uid_validity = Some((folder.to_string(), uv));
        }
        Ok(mailbox.exists)
    }

    /// Check if an error is a connection error. If so, mark dead for reconnect.
    pub fn check_error(&mut self, e: anyhow::Error) -> anyhow::Error {
        if is_connection_error(&e.to_string()) {
            tracing::warn!("IMAP connection error, will reconnect on next call: {e}");
            // Sanitize before stashing — the error string may embed server
            // output which could contain bidi/control chars the
            // `account_health` surface eventually echoes to the LLM.
            self.last_error = Some(sanitize_log_str(&e.to_string()));
            self.mark_dead();
        }
        e
    }

    pub fn is_folder_allowed(&self, folder: &str) -> bool {
        // Scan with `eq_ignore_ascii_case` instead of allocating a lowercased
        // copy of `folder` per call. For short folder names this is strictly
        // faster than `to_lowercase() + HashSet::contains`: no allocation.
        self.allowed_folders
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|a| a.eq_ignore_ascii_case(folder)))
    }

    // ========== Folder operations ==========

    pub async fn list_folders(&mut self) -> Result<Vec<FolderInfo>> {
        retry_read!(self.list_folders_once())
    }

    async fn list_folders_once(&mut self) -> Result<Vec<FolderInfo>> {
        let names = self.get_folder_names_once().await?;

        // Cap — a malicious or misconfigured server could return millions
        // of folders and drive the per-folder STATUS loop below into a DoS.
        let names: Vec<String> = names.into_iter().take(MAX_FOLDER_COUNT).collect();

        let mut result = Vec::new();
        for name in names {
            self.ensure_connected().await?;
            let session = self.session()?;
            // Per-folder 10s timeout: a single stuck folder (shared-mailbox
            // ACL loop, broken server-side index) must not hang the whole
            // `list_folders` tool call forever. Missing STATUS falls through
            // to (0, 0) like the non-connection error branch below.
            let status_fut = tokio::time::timeout(
                Duration::from_secs(10),
                session.status(&name, "(MESSAGES UNSEEN)"),
            );
            let (total, unread) = match status_fut.await {
                Ok(Ok(mailbox)) => (mailbox.exists, mailbox.unseen.unwrap_or(0)),
                Ok(Err(e)) => {
                    // Propagate connection errors so the outer retry wrapper
                    // can reconnect. Other errors (permission, no-such-folder)
                    // fall through to (0, 0) so one bad folder doesn't kill
                    // the whole list.
                    let err_str = e.to_string();
                    if is_connection_error(&err_str) {
                        return Err(anyhow::Error::new(e));
                    }
                    tracing::warn!(folder = %sanitize_log_str(&name), error = %sanitize_log_str(&err_str), "STATUS failed, using 0/0");
                    (0, 0)
                }
                Err(_elapsed) => {
                    tracing::warn!(folder = %sanitize_log_str(&name), "STATUS timed out after 10s, using 0/0");
                    (0, 0)
                }
            };
            // STATUS doesn't change the selected folder
            let role = detect_folder_role(&name);
            let display_name = safe_display_name(&name);
            result.push(FolderInfo {
                name,
                total,
                unread,
                role,
                display_name,
            });
        }

        Ok(result)
    }

    /// Get folder names with caching (IMAP LIST is called once per session).
    pub async fn get_folder_names(&mut self) -> Result<Vec<String>> {
        retry_read!(self.get_folder_names_once())
    }

    async fn get_folder_names_once(&mut self) -> Result<Vec<String>> {
        if let Some(cached) = &self.cached_folder_names {
            return Ok(cached.clone());
        }

        self.ensure_connected().await?;
        let session = self.session()?;
        let folders_stream = session.list(Some(""), Some("*")).await?;
        // Skip folder names containing control / bidi / zero-width chars.
        // Those can't occur in legitimate IMAP folder names but a
        // compromised server or shared-mailbox setup could return them to
        // disguise a malicious folder to the LLM (e.g. a bidi-override
        // flips `INBOX/innocent` into something that renders as `Trash`).
        // Filtering keeps "what the LLM sees == what we can SELECT"; we'd
        // otherwise need a parallel sanitized↔real name map.
        let names: Vec<String> = folders_stream
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .map(|f| f.name().to_string())
            .filter(|name| {
                if crate::email::sanitize_external_str(name) != *name {
                    tracing::warn!(
                        folder = %sanitize_log_str(name),
                        "dropping folder with control/bidi/zero-width chars in name"
                    );
                    return false;
                }
                self.is_folder_allowed(name)
            })
            .collect();

        self.cached_folder_names = Some(names.clone());
        Ok(names)
    }

    // ========== Email read operations ==========

    pub async fn list_emails(
        &mut self,
        folder: &str,
        limit: u32,
        offset: u32,
        unread_only: bool,
    ) -> Result<(Vec<EmailSummary>, u32, u32)> {
        retry_read!(self.list_emails_once(folder, limit, offset, unread_only))
    }

    async fn list_emails_once(
        &mut self,
        folder: &str,
        limit: u32,
        offset: u32,
        unread_only: bool,
    ) -> Result<(Vec<EmailSummary>, u32, u32)> {
        let total = self.ensure_selected(folder).await?;
        if total == 0 {
            return Ok((vec![], 0, 0));
        }

        if unread_only {
            return self.list_unread_emails(folder, limit, offset, total).await;
        }

        // Unfiltered path: use sequence numbers. Avoids `UID SEARCH ALL`
        // which transfers EVERY UID in the folder (~900 KB for a 130K INBOX)
        // just to sort + discard 99%. Sequence numbers 1..=total are implicit
        // from `EXISTS` in the SELECT response; newest = highest seq.
        //
        // Page window (newest-first): seq range `(total-offset-limit+1)..=(total-offset)`.
        let end = total.saturating_sub(offset);
        if end == 0 {
            return Ok((vec![], total, total));
        }
        let start = end.saturating_sub(limit.saturating_sub(1)).max(1);
        let seq_set = if start == end {
            start.to_string()
        } else {
            format!("{start}:{end}")
        };
        let session = self.session()?;
        let stream = session.fetch(&seq_set, SUMMARY_FETCH_ITEMS).await?;
        let mut fetches: Vec<Fetch> = stream.try_collect().await?;
        // Sort descending by sequence number — IMAP's FETCH response order
        // isn't formally guaranteed, and we want newest-first in the output.
        fetches.sort_by_key(|f| std::cmp::Reverse(f.message));

        let summaries = summarize_fetches(&fetches, folder, &PostFetchFilter::EMPTY);
        Ok((summaries, total, total))
    }

    /// Unread-only list path: needs a UID SEARCH because the server is the
    /// only thing that knows which sequence numbers are `\Unseen`.
    async fn list_unread_emails(
        &mut self,
        folder: &str,
        limit: u32,
        offset: u32,
        total: u32,
    ) -> Result<(Vec<EmailSummary>, u32, u32)> {
        let session = self.session()?;
        let uids_stream = session.uid_search("UNSEEN").await?;
        let mut uids: Vec<u32> = uids_stream.into_iter().collect();
        // Saturate rather than wrap: a folder with >4 billion matches is not
        // reachable here, and silently reporting a tiny number would be worse
        // than reporting the ceiling.
        let matched = u32::try_from(uids.len()).unwrap_or(u32::MAX);
        uids.sort_unstable_by(|a, b| b.cmp(a)); // newest first by UID

        let paged_uids: Vec<u32> = uids
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
        if paged_uids.is_empty() {
            return Ok((vec![], total, matched));
        }

        let uid_set = uid_set_string(&paged_uids);
        let stream = session.uid_fetch(&uid_set, SUMMARY_FETCH_ITEMS).await?;
        let fetches: Vec<Fetch> = stream.try_collect().await?;
        let summaries = summarize_fetches(&fetches, folder, &PostFetchFilter::EMPTY);
        Ok((summaries, total, matched))
    }

    /// Fetch raw email bytes for a single message (for attachment extraction).
    pub async fn fetch_raw(&mut self, folder: &str, uid: u32) -> Result<Option<Vec<u8>>> {
        retry_read!(self.fetch_raw_once(folder, uid))
    }

    async fn fetch_raw_once(&mut self, folder: &str, uid: u32) -> Result<Option<Vec<u8>>> {
        self.ensure_selected(folder).await?;
        let session = self.session()?;
        let stream = session
            .uid_fetch(uid.to_string(), "(BODY.PEEK[] FLAGS)")
            .await?;
        let fetches: Vec<Fetch> = stream.try_collect().await?;

        let Some(fetch) = fetches.first() else {
            return Ok(None);
        };

        Ok(bounded_body(fetch, uid).map(<[u8]>::to_vec))
    }

    pub async fn get_email(&mut self, folder: &str, uid: u32) -> Result<Option<EmailFull>> {
        retry_read!(self.get_email_once(folder, uid))
    }

    async fn get_email_once(&mut self, folder: &str, uid: u32) -> Result<Option<EmailFull>> {
        self.ensure_selected(folder).await?;
        let session = self.session()?;
        let stream = session
            .uid_fetch(uid.to_string(), "(BODY.PEEK[] FLAGS)")
            .await?;
        let fetches: Vec<Fetch> = stream.try_collect().await?;

        let Some(fetch) = fetches.first() else {
            return Ok(None);
        };

        let body = bounded_body(fetch, uid).context("Email has no body (or exceeds size cap)")?;
        let flags = parse_flags(fetch);
        Ok(Some(email::parse_email(uid, folder, body, flags)))
    }

    /// Search one folder. Returns the summaries (newest first, capped at
    /// `limit`) together with the number of messages that actually matched.
    ///
    /// The count is taken before the cap: a caller that only sees `limit`
    /// results cannot otherwise tell "exactly this many" from "the first of
    /// many", and would silently report a partial answer as complete.
    ///
    /// `post_filter` carries what the server could not take. Its sub-day
    /// time bounds are resolved on a lightweight `(UID INTERNALDATE)` round
    /// BEFORE counting and paging, so `matched` and `offset` operate on
    /// rows a caller can actually get. Its full-text criteria (Outlook
    /// 365's broken `CHARSET UTF-8` SEARCH) need the fetched body and are
    /// applied per fetched message, so for those `matched` stays an upper
    /// bound, exactly as documented.
    pub async fn search_emails(
        &mut self,
        folder: &str,
        criteria: &str,
        limit: u32,
        offset: u32,
        post_filter: &PostFetchFilter,
    ) -> Result<(Vec<EmailSummary>, u32)> {
        retry_read!(self.search_emails_once(folder, criteria, limit, offset, post_filter))
    }

    async fn search_emails_once(
        &mut self,
        folder: &str,
        criteria: &str,
        limit: u32,
        offset: u32,
        post_filter: &PostFetchFilter,
    ) -> Result<(Vec<EmailSummary>, u32)> {
        self.ensure_selected(folder).await?;
        let session = self.session()?;

        let uids_stream = session.uid_search(criteria).await?;
        let mut uids: Vec<u32> = uids_stream.into_iter().collect();
        uids.sort_unstable_by(|a, b| b.cmp(a));

        // Resolve the sub-day time cut BEFORE counting and paging, on a
        // lightweight `(UID INTERNALDATE)` round — tens of bytes per row.
        // Cutting after the full fetch (the first design) transferred
        // complete messages just to discard them: with `before`, the newest
        // rows of the day-widened window all fall past the cut, so whole
        // pages arrived, died, and `returned: 0` with `has_more: true` sent
        // the caller into the next empty page. Cutting here also means
        // `matched` and `offset` operate on rows a caller can actually get.
        if post_filter.has_time_bounds() && !uids.is_empty() {
            // Unlike every other uid_set_string call site this one covers
            // the FULL match list, and a fragmented list (unread + time
            // bound over a big mailbox) can compress badly — IMAP servers
            // cap the command line (Dovecot defaults to 64 KiB). Past a
            // conservative threshold, fall back to the covering range: the
            // command shrinks to a constant, the response gains non-match
            // rows (~tens of bytes each), and the `retain` below
            // intersects against the match list either way.
            let mut uid_set = uid_set_string(&uids);
            if uid_set.len() > 4096
                && let (Some(&newest), Some(&oldest)) = (uids.first(), uids.last())
            {
                uid_set = format!("{oldest}:{newest}");
            }
            let stream = session.uid_fetch(&uid_set, "(UID INTERNALDATE)").await?;
            let fetches: Vec<Fetch> = stream.try_collect().await?;
            // A missing INTERNALDATE keeps the row — same policy as
            // `time_matches`. Rows absent from the response entirely
            // (expunged since the SEARCH) drop out with the retain.
            let keep: HashSet<u32> = fetches
                .iter()
                .filter(|f| post_filter.time_matches(f.internal_date().map(|d| d.timestamp())))
                .filter_map(|f| f.uid)
                .collect();
            uids.retain(|uid| keep.contains(uid));
        }

        let matched = u32::try_from(uids.len()).unwrap_or(u32::MAX);
        // Page on the UID list, before fetching: SEARCH already returned every
        // match, so skipping costs nothing, while FETCH is the expensive part
        // and now only covers the requested window.
        let uids: Vec<u32> = uids
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();

        if uids.is_empty() {
            return Ok((vec![], matched));
        }

        let uid_set = uid_set_string(&uids);
        let stream = session.uid_fetch(&uid_set, SUMMARY_FETCH_ITEMS).await?;
        let fetches: Vec<Fetch> = stream.try_collect().await?;
        Ok((summarize_fetches(&fetches, folder, post_filter), matched))
    }

    /// `strict=true` (recommended): match only via `Message-ID` /
    /// `In-Reply-To` / `References` — same algorithm as
    /// `list_emails(group_by_thread=true)`, so the counts line up.
    /// `strict=false`: additionally fall back to subject-kernel matching
    /// for small threads; useful on Lotus-Notes-style mailers that omit
    /// References headers, but can merge unrelated mails that happen to
    /// share a subject keyword.
    pub async fn get_thread(
        &mut self,
        folder: &str,
        uid: u32,
        strict: bool,
    ) -> Result<Vec<EmailFull>> {
        retry_read!(self.get_thread_once(folder, uid, strict))
    }

    // Linear 5-phase workflow (fetch initial, primary search, subject fallback,
    // fetch thread emails, sent-folder search) — splitting would trade
    // readability for line count.
    #[allow(clippy::too_many_lines)]
    async fn get_thread_once(
        &mut self,
        folder: &str,
        uid: u32,
        strict: bool,
    ) -> Result<Vec<EmailFull>> {
        // Caps against attacker-controlled fan-out. A malicious email can ship
        // thousands of entries in its `References:` header; without caps we'd
        // build a giant OR-criteria SEARCH and then FETCH every returned UID
        // (up to 100 MiB each after the bounded_body cap). 50 references is
        // more than any legitimate thread ever grows to; 200 UIDs bounds the
        // worst-case `uid_fetch`.
        const MAX_REFERENCES: usize = 50;
        const MAX_THREAD_UIDS: usize = 200;

        // 1. Fetch the initial email (1 roundtrip) — use _once to avoid nested retry.
        let initial = self
            .get_email_once(folder, uid)
            .await?
            .context("Email not found")?;

        let mut thread_uids: HashSet<u32> = HashSet::new();
        thread_uids.insert(uid);

        // 2. Build a single combined OR search for the primary folder (1 roundtrip)
        //    Instead of N sequential searches, we combine:
        //    - "who references our Message-ID" (in References or In-Reply-To)
        //    - "messages we reference" (by their Message-ID)
        //    - "message we reply to" (In-Reply-To target)
        let mut criteria_parts: Vec<String> = Vec::new();

        if let Some(msg_id) = &initial.message_id {
            let clean = clean_message_id(msg_id);
            criteria_parts.push(format!("HEADER References \"{clean}\""));
            criteria_parts.push(format!("HEADER In-Reply-To \"{clean}\""));
        }

        for ref_id in initial.references.iter().take(MAX_REFERENCES) {
            let clean = clean_message_id(ref_id);
            criteria_parts.push(format!("HEADER Message-ID \"{clean}\""));
        }

        if let Some(reply_to) = &initial.in_reply_to {
            let clean = clean_message_id(reply_to);
            criteria_parts.push(format!("HEADER Message-ID \"{clean}\""));
        }

        if let Some(combined) = build_or_criteria(&criteria_parts) {
            self.ensure_selected(folder).await?;
            let session = self.session()?;
            match session.uid_search(&combined).await {
                Ok(uids) => thread_uids.extend(uids),
                Err(e) => propagate_conn_or_warn(e, "thread primary search failed")?,
            }
        }

        // 3. Subject-based fallback for small threads (0-1 roundtrips, conditional).
        //    Off by default — it merges mails that only share a subject word
        //    (e.g. every recurring "Monthly report" notice ends up in one
        //    fake thread) and makes `get_thread` inconsistent with
        //    `list_emails(group_by_thread)`. Opt-in via `strict=false`.
        if !strict && thread_uids.len() <= 2 {
            let clean_subject = strip_email_prefixes(&initial.subject);
            if !clean_subject.is_empty() {
                self.ensure_selected(folder).await?;
                let session = self.session()?;
                let arg = imap_astring(clean_subject);
                let criteria = if clean_subject.is_ascii() {
                    format!("SUBJECT {arg}")
                } else {
                    format!("CHARSET UTF-8 SUBJECT {arg}")
                };
                match session.uid_search(&criteria).await {
                    Ok(uids) if uids.len() < 20 => thread_uids.extend(uids),
                    Ok(_) => {} // too broad, skip
                    Err(e) => propagate_conn_or_warn(e, "thread subject fallback failed")?,
                }
            }
        }

        // 4. Fetch all thread emails from the primary folder (1 roundtrip).
        //    Cap thread_uids to bound the fetch set — sort ascending (newest
        //    UIDs last) and take from the TOP so we keep the newest messages
        //    when the cap is hit. This favours the user's recent context over
        //    old quoted ancestors.
        let mut emails = Vec::new();
        if !thread_uids.is_empty() {
            let mut uid_vec: Vec<u32> = thread_uids.iter().copied().collect();
            uid_vec.sort_unstable();
            let start = uid_vec.len().saturating_sub(MAX_THREAD_UIDS);
            let uid_set = uid_set_string(&uid_vec[start..]);
            self.ensure_selected(folder).await?;
            let session = self.session()?;
            let stream = session.uid_fetch(&uid_set, "(BODY.PEEK[] FLAGS)").await?;
            let fetches: Vec<Fetch> = stream.try_collect().await?;
            for fetch in &fetches {
                let Some(uid) = fetch.uid else { continue };
                if let Some(body) = bounded_body(fetch, uid) {
                    let flags = parse_flags(fetch);
                    emails.push(email::parse_email(uid, folder, body, flags));
                }
            }
        }

        // 5. Search Sent folder. Use `_once` variant of folder detection so that
        //    connection errors propagate (instead of silently becoming None).
        if let Some(sent) = self.find_folder_by_role_once(SENT_FOLDER_NAMES).await?
            && sent != folder
        {
            // Collect all known Message-IDs to search for in Sent
            let mut sent_criteria: Vec<String> = Vec::new();
            if let Some(msg_id) = &initial.message_id {
                let clean = clean_message_id(msg_id);
                sent_criteria.push(format!("HEADER References \"{clean}\""));
                sent_criteria.push(format!("HEADER In-Reply-To \"{clean}\""));
            }
            for email in &emails {
                if let Some(msg_id) = &email.message_id {
                    let clean = clean_message_id(msg_id);
                    sent_criteria.push(format!("HEADER References \"{clean}\""));
                    sent_criteria.push(format!("HEADER In-Reply-To \"{clean}\""));
                }
            }

            if let Some(combined) = build_or_criteria(&sent_criteria) {
                self.ensure_selected(&sent).await?;
                let session = self.session()?;
                let sent_uids_result = session.uid_search(&combined).await;
                let mut sent_uids: Vec<u32> = match sent_uids_result {
                    Ok(uids) => uids.into_iter().collect(),
                    Err(e) => {
                        propagate_conn_or_warn(e, "sent folder search failed")?;
                        Vec::new()
                    }
                };
                // Cap sent-folder matches too — same attacker-controlled
                // fan-out vector as the primary search.
                sent_uids.sort_unstable();
                let start = sent_uids.len().saturating_sub(MAX_THREAD_UIDS);
                let sent_uids = &sent_uids[start..];
                if !sent_uids.is_empty() {
                    let uid_set = uid_set_string(sent_uids);
                    let session = self.session()?;
                    // Use silent-fail semantics here too: a FETCH failure for
                    // sent-folder messages shouldn't lose the primary-folder
                    // thread emails we already collected.
                    let fetch_result = session.uid_fetch(&uid_set, "(BODY.PEEK[] FLAGS)").await;
                    let fetches: Vec<Fetch> = match fetch_result {
                        Ok(stream) => match stream.try_collect().await {
                            Ok(v) => v,
                            Err(e) => {
                                propagate_conn_or_warn(e, "sent folder fetch failed")?;
                                Vec::new()
                            }
                        },
                        Err(e) => {
                            propagate_conn_or_warn(e, "sent folder fetch failed")?;
                            Vec::new()
                        }
                    };
                    for fetch in &fetches {
                        let Some(uid) = fetch.uid else { continue };
                        if let Some(body) = bounded_body(fetch, uid) {
                            let flags = parse_flags(fetch);
                            emails.push(email::parse_email(uid, &sent, body, flags));
                        }
                    }
                }
            }
        }

        // Dedup by Message-ID: the same thread message can surface in both
        // the primary folder and Sent (user BCC'd themselves, or a non-
        // Gmail-style server without all-mail). Keep the first occurrence;
        // we sorted by date afterwards so ordering is stable. Fall back to
        // `(folder, uid)` for messages without a Message-ID.
        let mut seen_ids: HashSet<String> = HashSet::new();
        let mut seen_fallback: HashSet<(String, u32)> = HashSet::new();
        emails.retain(|e| {
            e.message_id.as_ref().map_or_else(
                || seen_fallback.insert((e.folder.clone(), e.uid)),
                |mid| seen_ids.insert(mid.clone()),
            )
        });
        emails.sort_by(|a, b| a.date.cmp(&b.date));
        Ok(emails)
    }

    /// `_once` variant of folder-role lookup — uses `get_folder_names_once`
    /// so callers inside `retry_read!`-wrapped methods don't nest retries.
    /// Errors propagate so the outer wrapper sees connection failures.
    async fn find_folder_by_role_once(&mut self, candidates: &[&str]) -> Result<Option<String>> {
        let folders = self.get_folder_names_once().await?;
        Ok(folders
            .into_iter()
            .find(|f| candidates.iter().any(|c| f.eq_ignore_ascii_case(c))))
    }

    // ========== Write operations ==========

    pub async fn mark_flags(
        &mut self,
        folder: &str,
        uids: &[u32],
        flag: &str,
        add: bool,
    ) -> Result<Vec<u32>> {
        if uids.is_empty() {
            return Ok(vec![]);
        }
        self.ensure_selected(folder).await?;
        let session = self.session()?;

        let uid_set = uid_set_string(uids);
        let op = if add { "+FLAGS" } else { "-FLAGS" };
        let fetches: Vec<Fetch> = session
            .uid_store(&uid_set, format!("{op} ({flag})"))
            .await?
            .try_collect()
            .await?;
        Ok(acknowledged_uids(&fetches, uids))
    }

    pub async fn move_emails(
        &mut self,
        folder: &str,
        uids: &[u32],
        target: &str,
    ) -> Result<Vec<u32>> {
        if uids.is_empty() {
            return Ok(vec![]);
        }
        // ensure_selected validates source via allowed_folders; validate the
        // target too so a moved email can't end up in a restricted folder.
        if !self.is_folder_allowed(target) {
            anyhow::bail!("Target folder \"{target}\" is not in allowed_folders for this account");
        }
        self.ensure_selected(folder).await?;
        // Confirmation source for the response — see `existing_uids` for why
        // this is a SEARCH up front rather than the STORE acknowledgements.
        let existing = self.existing_uids(uids).await?;
        if existing.is_empty() {
            return Ok(existing);
        }
        let uid_set = uid_set_string(&existing);
        // COPY first. If it fails the source is unchanged — the caller may
        // safely retry. If it SUCCEEDS, any subsequent failure leaves the
        // messages in BOTH folders; we contextualize those errors so the
        // caller (and the LLM) doesn't blindly retry into a third copy.
        {
            let session = self.session()?;
            session.uid_copy(&uid_set, target).await?;
        }
        {
            let session = self.session()?;
            let store_stream = session
                .uid_store(&uid_set, "+FLAGS (\\Deleted)")
                .await
                .map_err(|e| {
                    anyhow::Error::new(e).context(
                        "COPY to target succeeded but \\Deleted-flag STORE on source failed — \
                         messages now exist in both folders; do NOT retry this move without \
                         re-listing the source folder",
                    )
                })?;
            store_stream.try_collect::<Vec<_>>().await.map_err(|e| {
                anyhow::Error::new(e).context(
                    "COPY + STORE submitted but response stream errored — source likely \
                         flagged \\Deleted; re-list source before retrying",
                )
            })?;
        }
        if let Err(e) = self.scoped_expunge(&uid_set).await {
            return Err(e.context(
                "COPY + STORE succeeded but EXPUNGE failed — source messages are flagged \
                 \\Deleted; retry would duplicate in target. Investigate server state",
            ));
        }
        // EXPUNGE changed the folder's message count — invalidate the cache
        // so the next `list_emails` doesn't return a stale `total`.
        self.selected_folder = None;
        self.selected_exists = 0;
        Ok(existing)
    }

    /// The subset of `uids` that exists in the currently selected folder
    /// right now, verified with a `UID SEARCH`.
    ///
    /// This is the confirmation source for destructive operations, replacing
    /// the STORE acknowledgements: RFC 3501 only SHOULDs the untagged FETCH
    /// response, and RFC 7162 explicitly allows omitting it when the STORE
    /// changed nothing — e.g. `+FLAGS \Deleted` on a message another client
    /// already flagged. An under-report on a move or delete invites the
    /// caller to retry, and a retried move duplicates the message — the
    /// exact outcome the surrounding error contexts warn against.
    /// Existence-before-action cannot under-report a processed message.
    /// (`mark_flags` keeps its STORE-based reply: "actually updated" is its
    /// documented meaning, and retrying a flag write is harmless.)
    ///
    /// The result is intersected with the input — paranoia against a server
    /// echoing UIDs that were never asked about — and normalized.
    async fn existing_uids(&mut self, uids: &[u32]) -> Result<Vec<u32>> {
        let query = format!("UID {}", uid_set_string(uids));
        let session = self.session()?;
        let found = session.uid_search(&query).await?;
        let input: HashSet<u32> = uids.iter().copied().collect();
        let mut existing: Vec<u32> = found.into_iter().filter(|u| input.contains(u)).collect();
        existing.sort_unstable();
        existing.dedup();
        Ok(existing)
    }

    /// Flag the given UIDs `\Deleted` in the currently selected folder and
    /// expunge exactly them, reporting the UIDs that existed when the
    /// operation ran (see [`Self::existing_uids`]). Shared by
    /// `delete_emails`' permanent branch and `delete_draft`, so the two can
    /// never drift in how they report the same stale-UID case.
    async fn expunge_uids_in_selected(&mut self, uids: &[u32]) -> Result<Vec<u32>> {
        let existing = self.existing_uids(uids).await?;
        if existing.is_empty() {
            return Ok(existing);
        }
        let uid_set = uid_set_string(&existing);
        {
            let session = self.session()?;
            session
                .uid_store(&uid_set, "+FLAGS (\\Deleted)")
                .await?
                .try_collect::<Vec<_>>()
                .await?;
        }
        self.scoped_expunge(&uid_set).await?;
        self.selected_folder = None;
        self.selected_exists = 0;
        Ok(existing)
    }

    /// Remove `\Deleted`-flagged messages matching the given UID set, scoped
    /// via UID EXPUNGE (RFC 4315 UIDPLUS) so other `\Deleted` messages in the
    /// folder from parallel clients are untouched.
    ///
    /// Distinguishes error types:
    /// - **Connection error**: propagated up so the caller's `retry_read!`
    ///   or equivalent sees it and reconnects. NOT the right moment to
    ///   fall back to plain EXPUNGE — the session is dead.
    /// - **Other `uid_expunge` error** (e.g. `BAD`: UIDPLUS not supported):
    ///   fall back to plain `EXPUNGE` only if `allow_unsafe_expunge = true`
    ///   in config. Otherwise refuse: plain EXPUNGE would sweep away
    ///   `\Deleted` messages that concurrent clients (phone, webmail) have
    ///   flagged-but-not-yet-expunged. Silent data loss is worse than a
    ///   loud refusal.
    async fn scoped_expunge(&mut self, uid_set: &str) -> Result<()> {
        // Try UID EXPUNGE first; collect the outcome into a simple enum so the
        // session borrow is released before we attempt the fallback path.
        enum Outcome {
            Ok,
            ConnErr(async_imap::error::Error),
            Fallback(String),
        }
        let outcome = {
            let session = self.session()?;
            match session.uid_expunge(uid_set).await {
                Ok(stream) => match stream.try_collect::<Vec<_>>().await {
                    Ok(_) => Outcome::Ok,
                    Err(e) => {
                        if is_connection_error(&e.to_string()) {
                            Outcome::ConnErr(e)
                        } else {
                            Outcome::Fallback(e.to_string())
                        }
                    }
                },
                Err(e) => {
                    if is_connection_error(&e.to_string()) {
                        Outcome::ConnErr(e)
                    } else {
                        Outcome::Fallback(e.to_string())
                    }
                }
            }
        };
        match outcome {
            Outcome::Ok => Ok(()),
            // Session is unusable — don't mask as UIDPLUS-missing.
            Outcome::ConnErr(e) => Err(anyhow::Error::new(e)),
            Outcome::Fallback(msg) => {
                // Command-level rejection (most likely UIDPLUS not advertised).
                // Refuse by default — a plain EXPUNGE sweeps EVERY `\Deleted`
                // message in the folder, including ones a parallel client
                // (phone, webmail) flagged-but-not-yet-expunged. Users who
                // know their server semantics can opt in per account.
                if !self.config.allow_unsafe_expunge {
                    // Sanitize the server-provided `msg` — JSON escaping
                    // already neutralizes CR/LF for the LLM view, but a
                    // hostile server could otherwise smuggle bidi/zero-width
                    // chars into the error surface to mislead prompt
                    // rendering. Consistent with the rest of the codebase's
                    // "server strings going to LLM pass the sanitizer" rule.
                    let safe_msg = crate::email::sanitize_external_str(&msg);
                    anyhow::bail!(
                        "UID EXPUNGE rejected by server ({safe_msg}) and \
                         allow_unsafe_expunge is false — refusing plain EXPUNGE to avoid \
                         collateral removal of concurrent clients' \\Deleted messages. \
                         Set `allow_unsafe_expunge = true` for this account if you trust \
                         the single-client assumption"
                    );
                }
                tracing::warn!(
                    err = %sanitize_log_str(&msg),
                    "uid_expunge unsupported, falling back to plain EXPUNGE (allow_unsafe_expunge)"
                );
                let session = self.session()?;
                session.expunge().await?.try_collect::<Vec<_>>().await?;
                Ok(())
            }
        }
    }

    pub async fn delete_emails(
        &mut self,
        folder: &str,
        uids: &[u32],
        permanent: bool,
    ) -> Result<Vec<u32>> {
        if uids.is_empty() {
            return Ok(vec![]);
        }
        if permanent {
            self.ensure_selected(folder).await?;
            self.expunge_uids_in_selected(uids).await
        } else {
            let trash = self
                .find_folder_by_role(TRASH_FOLDER_NAMES)
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| "Trash".to_string());
            self.move_emails(folder, uids, &trash).await
        }
    }

    /// Expunge one or more drafts from the Drafts folder. Bypasses the
    /// generic `allow_delete` gate because the Drafts folder is the user's
    /// own workspace — allowing draft cleanup is expected draft lifecycle
    /// even when mailbox-wide delete is disabled. Still honours
    /// `allowed_folders`.
    ///
    /// Uses UID EXPUNGE (RFC 4315 UIDPLUS) when the server advertises it,
    /// so only the requested UIDs are removed — any other `\Deleted`-flagged
    /// messages in the folder are left alone. Falls back to plain EXPUNGE on
    /// servers without UIDPLUS only when `allow_unsafe_expunge` is enabled
    /// (rare: Gmail, Outlook 365, Dovecot, Cyrus all have UIDPLUS).
    pub async fn delete_draft(&mut self, uids: &[u32]) -> Result<Vec<u32>> {
        if uids.is_empty() {
            return Ok(vec![]);
        }
        let drafts = self
            .find_folder_by_role(DRAFTS_FOLDER_NAMES)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "Drafts".to_string());

        if !self.is_folder_allowed(&drafts) {
            anyhow::bail!("Drafts folder \"{drafts}\" is not in allowed_folders for this account");
        }

        self.ensure_selected(&drafts).await?;
        // Reports the UIDs that existed — an already-gone draft must not be
        // reported as deleted; `note_replacement` turns exactly that case
        // into its `replace_warning` instead of a false `replaced_uid`.
        self.expunge_uids_in_selected(uids).await
    }

    /// Append a message to the Drafts folder and report the UID it landed on.
    ///
    /// `Ok(None)` means the draft was stored but its UID could not be
    /// determined — the caller must treat that as success, not as failure.
    /// The UID is a convenience (it lets a caller revise the draft without
    /// listing the folder first); the draft existing is the contract.
    pub async fn save_draft(&mut self, message_bytes: &[u8]) -> Result<Option<u32>> {
        let drafts = self
            .find_folder_by_role(DRAFTS_FOLDER_NAMES)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "Drafts".to_string());

        // Enforce `allowed_folders` also on the auto-detected Drafts folder,
        // so an account restricted to (say) INBOX cannot be used to APPEND
        // drafts anywhere else.
        if !self.is_folder_allowed(&drafts) {
            anyhow::bail!("Drafts folder \"{drafts}\" is not in allowed_folders for this account");
        }

        self.ensure_connected().await?;
        let session = self.session()?;
        // `\Seen` alongside `\Draft`: clients save their own drafts as read;
        // an unseen draft shows up bold/unread in the Drafts folder and marks
        // it as externally injected.
        session
            .append(&drafts, Some("(\\Draft \\Seen)"), None, message_bytes)
            .await?;
        // APPEND doesn't change selection, but be safe
        self.selected_folder = None;
        self.selected_exists = 0;

        Ok(self.locate_appended_draft(&drafts, message_bytes).await)
    }

    /// Find the UID of the draft just appended to `drafts`.
    ///
    /// `async-imap` drops the `APPENDUID` response code, so the UID has to be
    /// searched for by `Message-ID` — unique per message and generated by us
    /// while building the MIME, so no other message can match.
    ///
    /// Every failure yields `None`: at this point the draft is already stored,
    /// and turning a lookup problem into an error would report a successful
    /// save as failed — the one outcome that would make a caller retry and
    /// duplicate the draft.
    async fn locate_appended_draft(&mut self, drafts: &str, message_bytes: &[u8]) -> Option<u32> {
        let message_id = util::extract_message_id(message_bytes)?;
        let criteria = format!(
            "HEADER Message-ID \"{}\"",
            util::clean_message_id(&message_id)
        );

        self.ensure_selected(drafts).await.ok()?;
        let session = self.session().ok()?;
        // A duplicate Message-ID would make "the draft we just wrote"
        // ambiguous; the highest UID is the append we are reporting on.
        session.uid_search(&criteria).await.ok()?.into_iter().max()
    }

    // ========== Helpers ==========

    /// Find the first folder matching any of `candidates` in the session's
    /// folder list (case-insensitive). Returns Ok(None) on clean "not
    /// found"; errors propagate from the underlying LIST call so the
    /// caller can choose to swallow via `.ok().flatten()` or handle
    /// connection issues.
    async fn find_folder_by_role(&mut self, candidates: &[&str]) -> Result<Option<String>> {
        let folders = self.get_folder_names().await?;
        Ok(folders
            .into_iter()
            .find(|f| candidates.iter().any(|c| f.eq_ignore_ascii_case(c))))
    }

    /// Public accessor for the Drafts folder — thin wrapper over
    /// `find_folder_by_role(DRAFTS_FOLDER_NAMES)` kept for call-site clarity.
    pub async fn detect_drafts_folder(&mut self) -> Result<Option<String>> {
        self.find_folder_by_role(DRAFTS_FOLDER_NAMES).await
    }

    async fn establish_tls(&self) -> Result<TlsStream<TcpStream>> {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        let tcp_stream = TcpStream::connect(&addr)
            .await
            .with_context(|| format!("Failed to connect to {addr}"))?;

        // TCP keepalive: detect dead connections within ~30s instead of ~2h default
        let sock_ref = socket2::SockRef::from(&tcp_stream);
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(30))
            .with_interval(Duration::from_secs(10));
        sock_ref.set_tcp_keepalive(&keepalive)?;

        let tls_config = if self.config.accept_invalid_certs {
            rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerifier))
                .with_no_client_auth()
        } else {
            let mut root_store = rustls::RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth()
        };

        let connector = TlsConnector::from(Arc::new(tls_config));
        let domain = rustls::pki_types::ServerName::try_from(self.config.host.clone())
            .context("Invalid server hostname")?;

        let tls_stream = connector
            .connect(domain, tcp_stream)
            .await
            .context("TLS handshake failed")?;

        Ok(tls_stream)
    }
}

// ========== Well-known folder names ==========

const SENT_FOLDER_NAMES: &[&str] = &[
    "Sent",
    "Sent Items",
    "Sent Mail",
    "[Gmail]/Sent Mail",
    "[Google Mail]/Sent Mail",
    "[Google Mail]/Gesendet",
    "INBOX.Sent",
    "Gesendete Elemente",
    "Gesendete Objekte",
];

const TRASH_FOLDER_NAMES: &[&str] = &[
    "Trash",
    "[Gmail]/Trash",
    "[Google Mail]/Trash",
    "[Google Mail]/Papierkorb",
    "Deleted Items",
    "INBOX.Trash",
    "Papierkorb",
    "Gelöschte Elemente",
    "Gel&APY-schte Elemente",
];

const DRAFTS_FOLDER_NAMES: &[&str] = &[
    "Drafts",
    "[Gmail]/Drafts",
    "[Google Mail]/Drafts",
    "[Google Mail]/Entwürfe",
    "[Google Mail]/Entw&APw-rfe",
    "Draft",
    "INBOX.Drafts",
    "Entwürfe",
    "Entw&APw-rfe",
];

// ========== Types ==========

#[derive(Debug, Clone, serde::Serialize)]
pub struct FolderInfo {
    pub name: String,
    pub total: u32,
    pub unread: u32,
    /// Well-known role of this folder — `"drafts"`, `"sent"`, or `"trash"`
    /// when the name matches one of the known conventions (Gmail, Outlook,
    /// Dovecot, German localizations). None for regular folders.
    /// Exposed so an LLM can pick the Trash folder directly instead of
    /// heuristically matching folder names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    /// The name as a human reads it, when `name` is modified-UTF-7 encoded
    /// (`Entw&APw-rfe` → `Entwürfe`). Absent for plain-ASCII names.
    ///
    /// Display only — every other tool takes `name`, which is what the server
    /// accepts. Set only when the decoded form passes the same control/bidi
    /// check as the raw name: the encoding hides non-ASCII from that filter,
    /// so a crafted folder could otherwise smuggle a right-to-left override
    /// into the listing and render as a different folder entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// The readable form of a folder name, when it is safe to show.
///
/// Modified UTF-7 encodes non-ASCII into plain ASCII, which means the
/// control/bidi filter applied to raw names cannot see what the name actually
/// says. A folder called `INBOX/&IC4-evil` passes that filter as ASCII yet
/// decodes to a right-to-left override — enough to make a listing render one
/// folder as another. Re-running the same check on the decoded form and
/// dropping only the display field keeps the folder usable while refusing to
/// render anything deceptive.
///
/// `None` when the name is already readable or must not be shown decoded.
fn safe_display_name(name: &str) -> Option<String> {
    util::decode_modified_utf7(name)
        .filter(|decoded| crate::email::sanitize_external_str(decoded) == *decoded)
}

/// Classify a folder name against the well-known role lists. Returns a
/// stable role tag the LLM can match against ("drafts" | "sent" | "trash").
fn detect_folder_role(name: &str) -> Option<&'static str> {
    if DRAFTS_FOLDER_NAMES
        .iter()
        .any(|n| name.eq_ignore_ascii_case(n))
    {
        return Some("drafts");
    }
    if SENT_FOLDER_NAMES
        .iter()
        .any(|n| name.eq_ignore_ascii_case(n))
    {
        return Some("sent");
    }
    if TRASH_FOLDER_NAMES
        .iter()
        .any(|n| name.eq_ignore_ascii_case(n))
    {
        return Some("trash");
    }
    None
}

impl std::fmt::Debug for ImapClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapClient")
            .field("name", &self.config.name)
            .field("host", &self.config.host)
            .field("connected", &self.session.is_some())
            .field("selected_folder", &self.selected_folder)
            .finish_non_exhaustive()
    }
}

/// Certificate verifier that accepts all certificates (for testing / internal CAs).
#[derive(Debug)]
struct NoVerifier;

impl rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

struct XOAuth2Authenticator(String);

impl async_imap::Authenticator for XOAuth2Authenticator {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        self.0.clone()
    }
}

// ========== Utility functions ==========

/// Map IMAP flags to strings, dropping `\Recent`: it is session-scoped
/// server bookkeeping (whether THIS connection is the first to see the
/// message since the last session), obsolete enough that RFC 9051 removed
/// it — and it reads like "new for me", which it is not. Surfacing it
/// invited exactly that misread.
fn parse_flags(fetch: &Fetch) -> Vec<String> {
    fetch
        .flags()
        .filter(|f| !matches!(f, async_imap::types::Flag::Recent))
        .map(|f| match f {
            async_imap::types::Flag::Seen => "\\Seen".to_string(),
            async_imap::types::Flag::Answered => "\\Answered".to_string(),
            async_imap::types::Flag::Flagged => "\\Flagged".to_string(),
            async_imap::types::Flag::Deleted => "\\Deleted".to_string(),
            async_imap::types::Flag::Draft => "\\Draft".to_string(),
            async_imap::types::Flag::Recent => "\\Recent".to_string(),
            async_imap::types::Flag::MayCreate => "\\MayCreate".to_string(),
            async_imap::types::Flag::Custom(c) => c.to_string(),
        })
        .collect()
}

/// Build an IMAP UID set string like `"1,3,5:10,42"`. Sorts a local copy
/// ascending and coalesces contiguous runs into `lo:hi` ranges — saves
/// substantial network bytes on paged FETCH requests (e.g. a 100-UID
/// contiguous page collapses from ~900B of comma-separated IDs to ~20B).
fn uid_set_string(uids: &[u32]) -> String {
    use std::fmt::Write;
    if uids.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<u32> = uids.to_vec();
    sorted.sort_unstable();
    // Estimate: worst case one token per uid (no runs), 11 chars each.
    let mut out = String::with_capacity(sorted.len() * 11);
    let mut run_start = sorted[0];
    let mut run_end = sorted[0];
    let mut first = true;
    let flush = |out: &mut String, start: u32, end: u32, first: &mut bool| {
        if !*first {
            out.push(',');
        }
        *first = false;
        if start == end {
            write!(out, "{start}").unwrap();
        } else {
            write!(out, "{start}:{end}").unwrap();
        }
    };
    for &uid in &sorted[1..] {
        // `checked_add` guards against overflow when `run_end == u32::MAX`
        // (debug builds would panic; release wraps). Real IMAP UIDs stay well
        // below u32::MAX but defense-in-depth costs nothing here.
        let continues = run_end.checked_add(1).is_some_and(|n| uid == n);
        if !continues {
            flush(&mut out, run_start, run_end, &mut first);
            run_start = uid;
        }
        run_end = uid;
    }
    flush(&mut out, run_start, run_end, &mut first);
    out
}

/// The UIDs a `UID STORE` response actually acknowledged, intersected with
/// the caller's input set and normalized (sorted, deduped).
///
/// The server emits one FETCH response per UID whose flags it updated; UIDs
/// passed in but absent from the folder (stale after UIDVALIDITY rotation /
/// external expunge / typo) produce no response, and echoing them as
/// "succeeded" would silently mislead the LLM. The intersection also stops
/// a hostile or buggy server from inflating the result with UIDs we never
/// asked about.
///
/// Used by `mark_flags` only: "actually updated" is that tool's documented
/// meaning, and a retried flag write is harmless. The destructive
/// operations confirm via [`ImapClient::existing_uids`] instead — see there
/// for why STORE acknowledgements can under-report a processed message.
fn acknowledged_uids(fetches: &[Fetch], input: &[u32]) -> Vec<u32> {
    let input: HashSet<u32> = input.iter().copied().collect();
    let mut updated: Vec<u32> = fetches
        .iter()
        .filter_map(|f| f.uid)
        .filter(|u| input.contains(u))
        .collect();
    updated.sort_unstable();
    updated.dedup();
    updated
}

/// For silent-fail paths (e.g. optional Sent-folder search in `get_thread`):
/// propagate connection errors so the outer `retry_read!` wrapper can
/// reconnect, but swallow other errors (bad syntax, permission) so one
/// optional lookup failing doesn't kill the whole aggregate operation.
fn propagate_conn_or_warn<E>(e: E, what: &str) -> Result<()>
where
    E: std::error::Error + Send + Sync + 'static,
{
    let err_str = e.to_string();
    if is_connection_error(&err_str) {
        return Err(anyhow::Error::new(e));
    }
    tracing::warn!(error = %sanitize_log_str(&err_str), what = %what, "continuing with partial data");
    Ok(())
}

#[cfg(test)]
mod tests {

    /// A folder whose raw name is pure ASCII sails past the control/bidi
    /// filter, while its decoded form carries a right-to-left override — the
    /// one case where decoding for display could deceive rather than help.
    #[test]
    fn safe_display_name_refuses_a_decoded_bidi_override() {
        let raw = "INBOX/&IC4-evil";
        assert!(
            raw.is_ascii(),
            "the raw name is what the folder filter sees"
        );
        assert_eq!(
            safe_display_name(raw),
            None,
            "must not offer a display name that renders deceptively"
        );
    }

    #[test]
    fn safe_display_name_decodes_ordinary_localized_folders() {
        assert_eq!(
            safe_display_name("Entw&APw-rfe").as_deref(),
            Some("Entwürfe")
        );
        assert_eq!(
            safe_display_name("Gel&APY-schte Elemente").as_deref(),
            Some("Gelöschte Elemente")
        );
    }

    /// Plain ASCII needs no second name — the field stays absent rather than
    /// duplicating what `name` already says.
    #[test]
    fn safe_display_name_is_absent_for_plain_names() {
        assert_eq!(safe_display_name("INBOX"), None);
        assert_eq!(safe_display_name("Clients/Acme"), None);
    }
    use super::*;

    #[test]
    fn uid_set_string_empty_returns_empty() {
        assert_eq!(uid_set_string(&[]), "");
    }

    #[test]
    fn uid_set_string_single_uid() {
        assert_eq!(uid_set_string(&[42]), "42");
    }

    #[test]
    fn uid_set_string_non_contiguous_comma_joined() {
        assert_eq!(uid_set_string(&[1, 3, 5]), "1,3,5");
    }

    #[test]
    fn uid_set_string_contiguous_collapsed_to_range() {
        assert_eq!(uid_set_string(&[1, 2, 3, 4, 5]), "1:5");
    }

    #[test]
    fn uid_set_string_mixed_runs_and_singles() {
        assert_eq!(uid_set_string(&[1, 2, 3, 7, 10, 11, 12]), "1:3,7,10:12");
    }

    #[test]
    fn uid_set_string_sorts_input() {
        // Input ordering must not matter; result is always sorted.
        assert_eq!(uid_set_string(&[5, 1, 3, 2, 4]), "1:5");
        assert_eq!(uid_set_string(&[100, 2, 1, 101]), "1:2,100:101");
    }

    #[test]
    fn uid_set_string_handles_u32_max_boundary() {
        // `checked_add` on u32::MAX must not panic; the run simply
        // terminates there without overflow.
        assert_eq!(uid_set_string(&[u32::MAX]), u32::MAX.to_string());
        assert_eq!(
            uid_set_string(&[u32::MAX - 1, u32::MAX]),
            format!("{}:{}", u32::MAX - 1, u32::MAX)
        );
    }

    #[test]
    fn uid_set_string_duplicates_emitted_verbatim() {
        // Not deduped; the run-coalescer only merges contiguous UIDs
        // (`uid == run_end + 1`), so `[3, 3, 3]` produces three separate
        // entries. IMAP servers tolerate duplicates in a UID set.
        assert_eq!(uid_set_string(&[3, 3, 3]), "3,3,3");
    }

    #[test]
    fn detect_folder_role_matches_known_names() {
        assert_eq!(detect_folder_role("Drafts"), Some("drafts"));
        assert_eq!(detect_folder_role("[Gmail]/Drafts"), Some("drafts"));
        assert_eq!(detect_folder_role("Entwürfe"), Some("drafts"));
        assert_eq!(detect_folder_role("Sent"), Some("sent"));
        assert_eq!(detect_folder_role("[Gmail]/Sent Mail"), Some("sent"));
        assert_eq!(detect_folder_role("Trash"), Some("trash"));
        assert_eq!(detect_folder_role("Papierkorb"), Some("trash"));
    }

    #[test]
    fn detect_folder_role_case_insensitive() {
        assert_eq!(detect_folder_role("DRAFTS"), Some("drafts"));
        assert_eq!(detect_folder_role("sent"), Some("sent"));
        assert_eq!(detect_folder_role("TRASH"), Some("trash"));
    }

    #[test]
    fn detect_folder_role_returns_none_for_unknown() {
        assert_eq!(detect_folder_role("INBOX"), None);
        assert_eq!(detect_folder_role(""), None);
        assert_eq!(detect_folder_role("MyCustomFolder"), None);
    }

    #[test]
    fn parse_flags_known_standard_flags() {
        // Build a test Fetch isn't worth the effort; parse_flags is
        // exercised transitively by all read ops in integration. Skip
        // direct unit test — the function is a pure 1-to-1 mapping.
    }

    /// Minimal message for the filter tests.
    fn mail(subject: &str, from: &str, body: &str) -> EmailFull {
        EmailFull {
            uid: 1,
            folder: "INBOX".to_string(),
            from: Some(crate::email::EmailAddress {
                name: None,
                address: from.to_string(),
            }),
            to: vec![],
            cc: vec![],
            subject: subject.to_string(),
            date: None,
            date_original: None,
            message_id: None,
            in_reply_to: None,
            references: vec![],
            flags: vec![],
            body_text: body.to_string(),
            body_html: None,
            attachments: vec![],
            body_parts_diverge: false,
        }
    }

    /// The reason this filter runs in the client layer at all: it must see
    /// the FULL body. The previous snippet-level fallback silently dropped
    /// every mail whose term sat past the first 200 characters.
    #[test]
    fn body_text_filter_matches_beyond_the_snippet_window() {
        let mut body = "x".repeat(5_000);
        body.push_str(" Bestätigung Ihrer Bestellung");
        let filter = BodyTextFilter {
            all: vec!["bestätigung".to_string()],
            any: vec![],
        };
        assert!(
            filter.matches(&mail("s", "a@b", &body)),
            "term past 200 chars must match"
        );
        assert!(!filter.matches(&mail("s", "a@b", &"x".repeat(5_000))));
    }

    /// RFC 3501's `TEXT` matches header OR body; the fallback must not be
    /// narrower, or a term sitting only in the subject or an address is a
    /// silent false negative exactly on the servers that need the fallback.
    #[test]
    fn body_text_filter_matches_subject_and_addresses_like_imap_text() {
        let filter = BodyTextFilter {
            all: vec!["zahlungsbestätigung".to_string()],
            any: vec![],
        };
        assert!(filter.matches(&mail(
            "Zahlungsbestätigung Q3",
            "a@b",
            "no term in the body"
        )));
        assert!(!filter.matches(&mail("Unrelated", "a@b", "no term here either")));

        let by_sender = BodyTextFilter {
            all: vec!["müller@example.de".to_string()],
            any: vec![],
        };
        assert!(by_sender.matches(&mail("s", "müller@example.de", "body")));
        // Fields are joined with newlines: a term must not match by
        // straddling the subject/body boundary.
        let straddle = BodyTextFilter {
            all: vec!["subjectbody".to_string()],
            any: vec![],
        };
        assert!(!straddle.matches(&mail("subject", "a@b", "body")));
    }

    #[test]
    fn body_text_filter_combines_all_and_any() {
        let filter = BodyTextFilter {
            all: vec!["praktikum".to_string()],
            any: vec![vec!["akku".to_string(), "battery".to_string()]],
        };
        let m = |body: &str| mail("s", "a@b", body);
        assert!(filter.matches(&m("Praktikum mit Battery-Testing")));
        assert!(!filter.matches(&m("Praktikum ohne den zweiten Begriff")));
        assert!(!filter.matches(&m("Nur Akku, kein erster Begriff")));
        assert!(BodyTextFilter::EMPTY.matches(&m("anything")));
        assert!(BodyTextFilter::EMPTY.is_empty());
    }
}
