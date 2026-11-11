//! Async IMAP client wrapped around `async-imap`, plus pure helpers in
//! `util`.
//!
//! Per-account state lives in one [`ImapClient`] guarded by a Mutex (created
//! by `main`). The client transparently reconnects on transport-level errors
//! via the `retry_read!` macro, caches the currently-selected folder to
//! skip redundant SELECTs, and enforces `allowed_folders` on every SELECT
//! (`select`, which `ensure_selected` goes through) so an LLM can't bypass
//! the whitelist by passing an unfiltered folder name.
//!
//! Pure helpers (search-criteria escaping, ISO-date conversion, host detection)
//! live in `util` and are unit-tested in isolation.

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
    host_supports_unicode_search, imap_astring, is_retryable_error, iso_to_imap_date, same_folder,
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

type ImapSession = Session<GuardedStream<TlsStream<TcpStream>>>;

/// How long the session waits without a single byte from the server, or
/// without the server taking a single byte, before the connection counts as
/// dead (see [`GuardedStream`]). Generous for a live server: an answer that
/// keeps coming restarts it with every byte, and Dovecot reports progress
/// during a long SEARCH.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// The TLS stream as the session reads and writes it, with two kinds of
/// silence turned into errors.
///
/// A clean end of stream. A server that ends a session itself (Gmail and
/// Outlook after a spell without traffic, Dovecot on `doveadm kick`) sends
/// `BYE` and closes the connection. async-imap reads a command's responses
/// up to its tagged completion and takes a clean end of stream for that
/// completion: the command "succeeded" with what it had read until then,
/// usually nothing. A LIST then reported no folders, and the empty list was
/// cached, so the account had no Drafts or Trash until a restart; a STORE
/// reported nothing changed and a SELECT an empty folder.
///
/// A wait without progress. A server that takes a command and then says
/// nothing, or a path that died without FIN or RST (a DSL reconnect at
/// night), kept the read waiting for TCP's retransmission timeout, a quarter
/// of an hour, with the account's lock held: every later call on the account
/// ran into its client's timeout until the server restarted. After `stall`
/// without a byte either way, the wait ends with the error a dead connection
/// gives anyway (see [`stalled`]).
///
/// Both take the path of every other dead connection: reads reconnect and
/// repeat, the caller of a write gets `retryable: true`, and the next call
/// finds the account free.
#[derive(Debug)]
struct GuardedStream<S> {
    inner: S,
    stall: Duration,
    read_wait: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    write_wait: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

impl<S> GuardedStream<S> {
    const fn new(inner: S, stall: Duration) -> Self {
        Self {
            inner,
            stall,
            read_wait: None,
            write_wait: None,
        }
    }
}

/// The error of a stalled connection: the operating system's own for a
/// connection that timed out, the one TCP keepalive raises for a dead peer.
/// A caller (and a model) sees nothing it could not see before, and it
/// counts as a dead connection, retryable.
fn stalled() -> std::io::Error {
    #[cfg(unix)]
    {
        std::io::Error::from_raw_os_error(libc::ETIMEDOUT)
    }
    #[cfg(not(unix))]
    {
        std::io::Error::from(std::io::ErrorKind::TimedOut)
    }
}

/// What a pending read or write becomes: still pending, or the error of a
/// stalled connection once `stall` has passed since it began to wait. The
/// timer is armed on the first `Pending` and dropped on every `Ready`, so
/// each byte starts the wait anew.
fn pending_or_stalled<T>(
    wait: &mut Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    stall: Duration,
    cx: &mut std::task::Context<'_>,
) -> std::task::Poll<std::io::Result<T>> {
    let timer = wait.get_or_insert_with(|| Box::pin(tokio::time::sleep(stall)));
    if timer.as_mut().poll(cx).is_ready() {
        *wait = None;
        return std::task::Poll::Ready(Err(stalled()));
    }
    std::task::Poll::Pending
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for GuardedStream<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // Zero bytes into a buffer with room is how a reader reports the end
        // of the stream; into a full buffer they mean nothing.
        let room = buf.remaining() > 0;
        let before = buf.filled().len();
        let this = &mut *self;
        match std::pin::Pin::new(&mut this.inner).poll_read(cx, buf) {
            std::task::Poll::Pending => pending_or_stalled(&mut this.read_wait, this.stall, cx),
            std::task::Poll::Ready(result) => {
                this.read_wait = None;
                if room && result.is_ok() && buf.filled().len() == before {
                    return std::task::Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "connection closed by the server",
                    )));
                }
                std::task::Poll::Ready(result)
            }
        }
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for GuardedStream<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = &mut *self;
        match std::pin::Pin::new(&mut this.inner).poll_write(cx, buf) {
            std::task::Poll::Pending => pending_or_stalled(&mut this.write_wait, this.stall, cx),
            ready @ std::task::Poll::Ready(_) => {
                this.write_wait = None;
                ready
            }
        }
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = &mut *self;
        match std::pin::Pin::new(&mut this.inner).poll_write_vectored(cx, bufs) {
            std::task::Poll::Pending => pending_or_stalled(&mut this.write_wait, this.stall, cx),
            ready @ std::task::Poll::Ready(_) => {
                this.write_wait = None;
                ready
            }
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = &mut *self;
        match std::pin::Pin::new(&mut this.inner).poll_flush(cx) {
            std::task::Poll::Pending => pending_or_stalled(&mut this.write_wait, this.stall, cx),
            ready @ std::task::Poll::Ready(_) => {
                this.write_wait = None;
                ready
            }
        }
    }

    // Not watched: LOGOUT has its own five seconds, and a session dropped
    // while shutting down is closed by the operating system anyway.
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Hard cap on a single email body (raw RFC 822 bytes, already decoded by
/// IMAP's BODY[] fetch). Defends against a compromised or malicious server
/// claiming a multi-GB body to OOM the process. 100 MiB is several times
/// larger than realistic emails with max-size attachments.
const MAX_EMAIL_BYTES: usize = 100 * 1024 * 1024;

/// Upper bound for establishing a session (TCP, TLS, login), at startup and
/// on every reconnect — see [`ImapClient::connect_bounded`].
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

// The token refresh inside a connect gives up first, with its own error.
const _: () = assert!(crate::oauth2::OAUTH2_TIMEOUT.as_secs() < CONNECT_TIMEOUT.as_secs());

/// Hard cap on the number of folders we'll process in a single `LIST` response.
/// A malicious server could return millions of folders to drive the subsequent
/// per-folder STATUS loop into a `DoS`.
const MAX_FOLDER_COUNT: usize = 10_000;

// TRUNCATED-BODY FETCH (deferred): an earlier iteration added
// `BODY.PEEK[]<0.16384>` here to save 5–10× bandwidth on HTML-heavy
// inboxes. It works against RFC-compliant servers, but `imap-proto`
// (our parser, still as of 0.17) requires the literal SP between the
// `<origin>` marker and the `{size}` literal that RFC 3501 §7.4.2
// prescribes, and `GreenMail` (still as of 2.1.15) emits them adjacent
// (`BODY[]<0>{416}`). That tripped the FETCH-response parser end-to-end.
// Fixing it needs a fork of imap-proto (and by extension async-imap) to
// accept the missing-SP form leniently. Kept as the existing full-body
// fetch below until one side changes or we swap to a different IMAP crate.
// INTERNALDATE feeds the sub-day `since`/`before` cut: IMAP's own
// SINCE/BEFORE are day-granular, so the exact bound is applied client-side
// against the arrival timestamp (not the sender-controlled Date header).
const SUMMARY_FETCH_ITEMS: &str = "(BODY.PEEK[] FLAGS UID INTERNALDATE)";
/// One message in full: INTERNALDATE stands in for a missing Date header
/// (see [`dated`]).
const MESSAGE_FETCH_ITEMS: &str = "(BODY.PEEK[] FLAGS INTERNALDATE)";

/// A parsed message whose `date` falls back to its arrival time
/// (INTERNALDATE) when it has no usable Date header. Without one a message
/// had `date: null`: nothing to tell the user, and nothing to sort it by
/// among others, which rows merged from several accounts need.
fn dated(mut email: EmailFull, fetch: &Fetch) -> EmailFull {
    if email.date.is_none() {
        email.date = fetch
            .internal_date()
            .and_then(|d| email::unix_to_utc_iso(d.timestamp()));
    }
    email
}

/// Client-side full-text criteria for servers whose SEARCH cannot take them
/// (Outlook 365 silently returns zero matches for `CHARSET UTF-8`). Applied
/// in `summarize_fetches` against the parsed message — subject, the
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
        let full = dated(email::parse_email_no_html(uid, folder, body, flags), fetch);
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
    /// (or the last connect that failed or timed out). Surfaced via
    /// [`ConnectionState::last_error`] for the `account_health` tool so
    /// operators can answer "why is this account offline?" without tailing
    /// stderr.
    last_error: Option<String>,
    /// How long a command may wait without progress, see [`GuardedStream`].
    stall: Duration,
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
            last_uid_validity: None,
            cached_folder_names: None,
            cached_oauth_token: None,
            active_refresh_token,
            last_error: None,
            stall: STALL_TIMEOUT,
        }
    }

    /// The same client with another stall time, so a test of a silent server
    /// need not wait a minute.
    #[cfg(test)]
    const fn stall_after(mut self, stall: Duration) -> Self {
        self.stall = stall;
        self
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
    ///
    /// Unbounded; the server goes through [`Self::connect_bounded`].
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
        let mut client = async_imap::Client::new(GuardedStream::new(tls_stream, self.stall));

        // Read the server greeting before any commands.
        // async-imap's Client::new() doesn't consume the greeting, which causes
        // authenticate() to misinterpret it as a response to the AUTHENTICATE command.
        let _greeting = client
            .read_response()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read server greeting: {e:#}"))?;

        let session = match self.config.auth_method {
            AuthMethod::Password => {
                let password = self
                    .config
                    .password
                    .as_deref()
                    .context("Password required but not configured")?;
                // The server's reason in the message: "IMAP login failed"
                // alone left "why does my mail not work?" without an answer.
                client
                    .login(&self.config.username, password)
                    .await
                    .map_err(|(e, _)| {
                        anyhow::anyhow!("IMAP login failed: {}", clean_imap_error(&e.to_string()))
                    })?
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
                    .authenticate("XOAUTH2", XOAuth2Authenticator::new(auth_string))
                    .await
                {
                    Ok(session) => session,
                    Err((e, _)) => {
                        // Server rejected the token — invalidate cache so the
                        // next connect attempt refreshes rather than replaying
                        // a revoked token.
                        self.cached_oauth_token = None;
                        return Err(anyhow::anyhow!(
                            "IMAP OAuth2 authentication failed: {}",
                            clean_imap_error(&e.to_string())
                        ));
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
        self.cached_folder_names = None;
    }

    /// [`Self::connect`] within `CONNECT_TIMEOUT` (15 s) — the startup
    /// connect and every reconnect. Without the bound, a hostile server that keeps
    /// sending XOAUTH2 continuation challenges could hold this account's
    /// mutex indefinitely (`XOAuth2Authenticator` answers each one, and TCP
    /// keepalive never fires on a connection that keeps talking).
    ///
    /// `connect` records its own outcome in `last_error`; a timeout aborts it
    /// before it can, so that case is recorded here. At startup it used to
    /// be logged only, leaving `account_health` with an offline account and
    /// no reason.
    pub async fn connect_bounded(&mut self) -> Result<()> {
        let Ok(result) = tokio::time::timeout(CONNECT_TIMEOUT, self.connect()).await else {
            let msg = format!(
                "IMAP connect timed out after {}s",
                CONNECT_TIMEOUT.as_secs()
            );
            self.last_error = Some(msg.clone());
            return Err(anyhow::Error::msg(msg));
        };
        result
    }

    /// Ensure we have a live IMAP session, reconnecting if necessary (within
    /// [`CONNECT_TIMEOUT`], see [`Self::connect_bounded`]).
    async fn ensure_connected(&mut self) -> Result<()> {
        if self.session.is_some() {
            return Ok(());
        }
        tracing::info!(
            account = %self.config.name,
            "IMAP session lost, attempting reconnect..."
        );
        self.connect_bounded().await
    }

    /// Before a write, which is never retried: replace a session the server
    /// has closed since the last command, so the write does not fail on it.
    /// Servers end quiet sessions after a while (Gmail and Outlook do), and
    /// only the next command finds out; a read then reconnects and repeats,
    /// a write came back as "Broken pipe". NOOP changes nothing, so it may
    /// go through `retry_read!`.
    async fn ensure_live(&mut self) -> Result<()> {
        retry_read!(self.noop_once())
    }

    async fn noop_once(&mut self) -> Result<()> {
        self.ensure_connected().await?;
        self.session()?.noop().await?;
        Ok(())
    }

    /// Select a folder, skipping the IMAP command if it is already selected.
    /// Enough for everything that addresses messages by UID or searches —
    /// those see the folder's current state either way. Counting by sequence
    /// number needs [`Self::select`] instead.
    async fn ensure_selected(&mut self, folder: &str) -> Result<()> {
        // Fast path: if this folder is already selected, it was vetted against
        // `allowed_folders` on the prior SELECT — skip the re-check entirely.
        if self.selected_folder.as_deref() == Some(folder) {
            return Ok(());
        }
        self.select(folder).await.map(|_exists| ())
    }

    /// Issue a SELECT — also when `folder` is already selected — and return
    /// its current message count (EXISTS). Enforces `allowed_folders`: an
    /// explicit folder name bypassing this check was the main security gap
    /// of the previous implementation.
    ///
    /// The count must come from a SELECT of its own. A server reports mail
    /// arriving in the selected folder only as an untagged EXISTS alongside
    /// a later command, and async-imap queues those unsolicited responses
    /// where nothing reads them; a count cached from an earlier SELECT
    /// therefore stays at that value for as long as the folder remains
    /// selected — and paging by sequence number with it hid every message
    /// that arrived in the meantime.
    async fn select(&mut self, folder: &str) -> Result<u32> {
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
                let err = self.check_error(e.into());
                if says_nonexistent(&err.to_string())
                    && let Some(names) = self.folder_list_without(folder).await
                {
                    return Err(unknown_folder_error(folder, &names, self.may_delete()));
                }
                return Err(err);
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
            && same_folder(prev_folder, folder)
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
        if let Some(uv) = mailbox.uid_validity {
            self.last_uid_validity = Some((folder.to_string(), uv));
        }
        Ok(mailbox.exists)
    }

    /// The wire name of the folder a caller means, see
    /// [`resolve_folder_name`]: the name itself, its readable form or a role
    /// word. A miss in the cached folder list is checked against a fresh
    /// LIST. When nothing fits, an ASCII name is passed on unchanged (the
    /// server's refusal then turns into the error with the folder list), a
    /// name with other characters is refused here: no wire name has them,
    /// IMAP encodes those as modified UTF-7.
    pub async fn resolve_folder(&mut self, requested: &str) -> Result<String> {
        if let Some(name) = self.find_folder(requested).await? {
            return Ok(name);
        }
        if !requested.is_ascii() {
            let names = self.get_folder_names().await?;
            return Err(unknown_folder_error(requested, &names, self.may_delete()));
        }
        Ok(requested.to_string())
    }

    /// The wire name of the folder a caller means, as [`resolve_folder`]
    /// finds it, or `None` when this account has no such folder (outside
    /// `allowed_folders` counts as none). For a read over several accounts,
    /// where a folder one account lacks is an answer, not an error.
    ///
    /// [`resolve_folder`]: Self::resolve_folder
    pub async fn find_folder(&mut self, requested: &str) -> Result<Option<String>> {
        if requested.eq_ignore_ascii_case("INBOX") {
            return Ok(self.is_folder_allowed("INBOX").then(|| "INBOX".to_string()));
        }
        let names = self.get_folder_names().await?;
        if let Some(name) = resolve_folder_name(requested, &names) {
            return Ok(Some(name));
        }
        self.cached_folder_names = None;
        let names = self.get_folder_names().await?;
        Ok(resolve_folder_name(requested, &names))
    }

    /// Whether this account may delete emails: the folder error points at
    /// `delete_email` only then.
    const fn may_delete(&self) -> bool {
        !self.config.read_only && self.config.allow_delete
    }

    /// The folder list when it does not contain `folder`; `None` when it does
    /// or cannot be read. A miss in the cached list is checked against a
    /// fresh LIST first: a folder created in another client since must not be
    /// reported missing. Only `allowed_folders` appear, like everywhere else.
    async fn folder_list_without(&mut self, folder: &str) -> Option<Vec<String>> {
        let names = self.folder_names_or_mark().await?;
        if names.iter().any(|n| same_folder(n, folder)) {
            return None;
        }
        self.cached_folder_names = None;
        let names = self.folder_names_or_mark().await?;
        (!names.iter().any(|n| same_folder(n, folder))).then_some(names)
    }

    /// The folder names, or `None` with a dead or stalled session marked
    /// dead: the caller only wants them for a better error, and swallowing
    /// the failure alone kept a session whose LIST answer was still due.
    async fn folder_names_or_mark(&mut self) -> Option<Vec<String>> {
        match self.get_folder_names_once().await {
            Ok(names) => Some(names),
            Err(e) => {
                let _ = self.check_error(e);
                None
            }
        }
    }

    /// Check if an error is a connection error. If so, mark dead for reconnect.
    ///
    /// Judged on the whole chain: a write step wraps the cause in what it
    /// means for the caller ("COPY to target succeeded but ..."), and judged
    /// on that alone a stalled or broken session stayed in use. A session
    /// kept after a command was given up hands that command's late answers
    /// to the next one.
    pub fn check_error(&mut self, e: anyhow::Error) -> anyhow::Error {
        if is_connection_error(&format!("{e:#}")) {
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
        // Listed afresh, not from the cache: the call that shows the folders
        // missed one created in another client since and kept one deleted
        // there, until the next reconnect. Next to a STATUS per folder the
        // LIST costs nothing.
        self.cached_folder_names = None;
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
                    // The STATUS went out and its answer is still on the
                    // way: kept, the session would read that answer as the
                    // one to the next command, the next folder's counts
                    // among them. Dropped, the next folder reconnects first.
                    self.mark_dead();
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

    /// The folder names for an answer that only shows them (a read over
    /// several accounts, one of which lacks the folder asked for): empty
    /// when they cannot be read, and a dead or stalled session is marked
    /// dead rather than kept with its LIST answer still due.
    pub async fn folder_names_or_none(&mut self) -> Vec<String> {
        match self.get_folder_names().await {
            Ok(names) => names,
            Err(e) => {
                let _ = self.check_error(e);
                Vec::new()
            }
        }
    }

    /// The folder names as the server lists them now, for a call that goes
    /// through every folder (a search across all of them): from the cache it
    /// skipped a folder created in another client since.
    pub async fn current_folder_names(&mut self) -> Result<Vec<String>> {
        self.cached_folder_names = None;
        self.get_folder_names().await
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
        // Always a fresh SELECT: the page window below is computed from this
        // count — see `select` for why a cached one hid new mail.
        let total = self.select(folder).await?;
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
        let mut fetches: Vec<Fetch> = stream.try_collect().await?;
        // The server answers in its own order, ascending on Dovecot and
        // GreenMail: without this the unread listing came oldest first,
        // although the UIDs were picked newest first.
        fetches.sort_by_key(|f| std::cmp::Reverse(f.uid));
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

        let Some(fetch) = requested_fetch(fetches, uid) else {
            return Ok(None);
        };

        Ok(bounded_body(&fetch, uid).map(<[u8]>::to_vec))
    }

    pub async fn get_email(&mut self, folder: &str, uid: u32) -> Result<Option<EmailFull>> {
        retry_read!(self.get_email_once(folder, uid))
    }

    async fn get_email_once(&mut self, folder: &str, uid: u32) -> Result<Option<EmailFull>> {
        let Some(fetch) = self.fetch_message_once(folder, uid).await? else {
            return Ok(None);
        };
        let body = bounded_body(&fetch, uid).context("Email has no body (or exceeds size cap)")?;
        Ok(Some(dated(
            email::parse_email(uid, folder, body, parse_flags(&fetch)),
            &fetch,
        )))
    }

    /// [`Self::get_email`] together with the message as the server stores
    /// it, from one fetch: a forward carries the original's attachments and
    /// a reply the images its quote shows, which only the raw message holds.
    pub async fn get_email_with_raw(
        &mut self,
        folder: &str,
        uid: u32,
    ) -> Result<Option<(EmailFull, Vec<u8>)>> {
        retry_read!(self.get_email_with_raw_once(folder, uid))
    }

    async fn get_email_with_raw_once(
        &mut self,
        folder: &str,
        uid: u32,
    ) -> Result<Option<(EmailFull, Vec<u8>)>> {
        let Some(fetch) = self.fetch_message_once(folder, uid).await? else {
            return Ok(None);
        };
        let body = bounded_body(&fetch, uid).context("Email has no body (or exceeds size cap)")?;
        let email = dated(
            email::parse_email(uid, folder, body, parse_flags(&fetch)),
            &fetch,
        );
        Ok(Some((email, body.to_vec())))
    }

    /// The FETCH response for message `uid` of `folder` with its body, flags
    /// and arrival time; `None` when the folder has no such message.
    async fn fetch_message_once(&mut self, folder: &str, uid: u32) -> Result<Option<Fetch>> {
        self.ensure_selected(folder).await?;
        let session = self.session()?;
        let stream = session
            .uid_fetch(uid.to_string(), MESSAGE_FETCH_ITEMS)
            .await?;
        let fetches: Vec<Fetch> = stream.try_collect().await?;
        Ok(requested_fetch(fetches, uid))
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
        //    - "who descends from the thread root" (sibling branches)
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

        // Everything that descends from the thread's root. The links above
        // reach only the initial message's own ancestors and its replies; a
        // sibling branch (another reply to an ancestor) references neither,
        // so "the full thread" silently lacked it. The root is the first
        // References entry — mailers keep it when they trim a long chain —
        // else the In-Reply-To target.
        if let Some(root) = initial.references.first().or(initial.in_reply_to.as_ref()) {
            let clean = clean_message_id(root);
            criteria_parts.push(format!("HEADER References \"{clean}\""));
            criteria_parts.push(format!("HEADER In-Reply-To \"{clean}\""));
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
            let stream = session.uid_fetch(&uid_set, MESSAGE_FETCH_ITEMS).await?;
            let fetches: Vec<Fetch> = stream.try_collect().await?;
            for fetch in &fetches {
                let Some(uid) = fetch.uid else { continue };
                if let Some(body) = bounded_body(fetch, uid) {
                    let flags = parse_flags(fetch);
                    emails.push(dated(email::parse_email(uid, folder, body, flags), fetch));
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
                    let fetch_result = session.uid_fetch(&uid_set, MESSAGE_FETCH_ITEMS).await;
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
                            emails.push(dated(email::parse_email(uid, &sent, body, flags), fetch));
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
        self.ensure_live().await?;
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
        // A folder moved into itself is COPY-then-expunge of its own
        // messages: nothing moves, yet every UID the caller holds goes stale
        // and the reply would report the messages as moved.
        if same_folder(folder, target) {
            anyhow::bail!(
                "Source and target folder are the same (\"{folder}\") — there is nothing to move"
            );
        }
        self.ensure_live().await?;
        self.ensure_selected(folder).await?;
        // Checked before anything moves; the server's own refusal of the
        // COPY would only say "doesn't exist", without the names to pick from.
        if let Some(names) = self.folder_list_without(target).await {
            return Err(unknown_folder_error(target, &names, self.may_delete()));
        }
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
        // EXPUNGE renumbered the folder — forget the selection so the next
        // operation starts from a fresh SELECT.
        self.selected_folder = None;
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
            self.ensure_live().await?;
            self.ensure_selected(folder).await?;
            self.expunge_uids_in_selected(uids).await
        } else {
            // No guessed name: a fallback "Trash" that does not exist only
            // ended in the server's "doesn't exist", which told the caller
            // neither why nor what to do instead.
            let Some(trash) = self.find_folder_by_role(TRASH_FOLDER_NAMES).await? else {
                // Ask, not `permanent: true` outright: a small model reads a
                // suggestion in an error as the next call, and a deletion for
                // good is not one to take on its own.
                anyhow::bail!(
                    "This account has no trash folder under a known name (Trash, Deleted Items, \
                     Papierkorb, Gelöschte Elemente, …), so nothing was deleted. Ask the user \
                     whether to delete for good (`permanent: true`) or to move the emails with \
                     move_email to a folder from list_folders"
                );
            };
            // Deleting from the Trash itself: a move would leave the messages
            // where they are (under new UIDs) while reporting them deleted.
            if same_folder(folder, &trash) {
                anyhow::bail!(
                    "\"{folder}\" is the Trash folder — moving its messages to Trash changes \
                     nothing. Pass `permanent: true` to delete them for good"
                );
            }
            self.move_emails(folder, uids, &trash).await
        }
    }

    /// Expunge one or more drafts from the Drafts folder. Bypasses the
    /// generic `allow_delete` gate because the Drafts folder is the user's
    /// own workspace — allowing draft cleanup is expected draft lifecycle
    /// even when mailbox-wide delete is disabled. Still honours
    /// `allowed_folders`.
    ///
    /// Uses UID EXPUNGE (RFC 4315 UIDPLUS), so only the requested UIDs are
    /// removed — any other `\Deleted`-flagged messages in the folder are left
    /// alone. A server that rejects it gets plain EXPUNGE only when
    /// `allow_unsafe_expunge` is enabled (see `scoped_expunge`; rare: Gmail,
    /// Outlook 365, Dovecot, Cyrus all have UIDPLUS).
    pub async fn delete_draft(&mut self, uids: &[u32]) -> Result<Vec<u32>> {
        if uids.is_empty() {
            return Ok(vec![]);
        }
        let drafts = self
            .drafts_folder_or_default()
            .await
            .unwrap_or_else(|| "Drafts".to_string());

        if !self.is_folder_allowed(&drafts) {
            anyhow::bail!("Drafts folder \"{drafts}\" is not in allowed_folders for this account");
        }

        self.ensure_live().await?;
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
            .drafts_folder_or_default()
            .await
            .unwrap_or_else(|| "Drafts".to_string());

        // Enforce `allowed_folders` also on the auto-detected Drafts folder,
        // so an account restricted to (say) INBOX cannot be used to APPEND
        // drafts anywhere else.
        if !self.is_folder_allowed(&drafts) {
            anyhow::bail!("Drafts folder \"{drafts}\" is not in allowed_folders for this account");
        }

        self.ensure_live().await?;
        let session = self.session()?;
        // `\Seen` alongside `\Draft`: clients save their own drafts as read;
        // an unseen draft shows up bold/unread in the Drafts folder and marks
        // it as externally injected.
        session
            .append(&drafts, Some("(\\Draft \\Seen)"), None, message_bytes)
            .await?;
        // APPEND doesn't change selection, but be safe
        self.selected_folder = None;

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

        // A duplicate Message-ID would make "the draft we just wrote"
        // ambiguous; the highest UID is the append we are reporting on.
        match self.search_uids(drafts, &criteria).await {
            Ok(uids) => uids.into_iter().max(),
            Err(e) => {
                // Swallowed for the caller, but a dead or stalled session
                // must not carry this search's late answer into the next
                // command.
                let _ = self.check_error(e);
                None
            }
        }
    }

    /// `UID SEARCH` in `folder`, once.
    async fn search_uids(&mut self, folder: &str, criteria: &str) -> Result<HashSet<u32>> {
        self.ensure_selected(folder).await?;
        Ok(self.session()?.uid_search(criteria).await?)
    }

    /// The Drafts folder by its name, `None` when there is none or the list
    /// cannot be read. A list that failed on a dead or stalled session marks
    /// it dead, so the write that follows does not run into the late answer.
    async fn drafts_folder_or_default(&mut self) -> Option<String> {
        match self.find_folder_by_role(DRAFTS_FOLDER_NAMES).await {
            Ok(found) => found,
            Err(e) => {
                let _ = self.check_error(e);
                None
            }
        }
    }

    // ========== Helpers ==========

    /// Find the first folder matching any of `candidates` in the session's
    /// folder list (case-insensitive). Returns Ok(None) on clean "not
    /// found"; errors propagate from the underlying LIST call. A caller
    /// that can do without the folder passes the error through
    /// [`Self::check_error`] before dropping it (see
    /// [`Self::drafts_folder_or_default`]): dropped alone, a dead or stalled
    /// session stays in use with the LIST answer still due.
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
        // The cause in the message ("Connection refused", "timed out"): it is
        // what tells a caller the failure is transient (`retryable`), and
        // what `account_health` reports.
        let tcp_stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to connect to {addr}: {e}"))?;

        // TCP keepalive: first probe after 30 s idle, then every 10 s, and
        // three unanswered probes drop the connection — a dead peer is
        // noticed within ~60 s instead of the OS default of two hours before
        // probing even starts. The probe count matters as much as the
        // timing: Linux defaults to nine, which stretched detection to two
        // minutes.
        let sock_ref = socket2::SockRef::from(&tcp_stream);
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(30))
            .with_interval(Duration::from_secs(10));
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let keepalive = keepalive.with_retries(3);
        sock_ref.set_tcp_keepalive(&keepalive)?;

        // The crypto provider is pinned here rather than left to the process
        // default: `ClientConfig::builder()` resolves that from the enabled
        // crate features and panics at runtime the day a second provider
        // (`ring`) enters the dependency tree.
        let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .context("TLS setup failed")?;
        let tls_config = if self.config.accept_invalid_certs {
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerifier))
                .with_no_client_auth()
        } else {
            let mut root_store = rustls::RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            builder
                .with_root_certificates(root_store)
                .with_no_client_auth()
        };

        let connector = TlsConnector::from(Arc::new(tls_config));
        let domain = rustls::pki_types::ServerName::try_from(self.config.host.clone())
            .context("Invalid server hostname")?;

        let tls_stream = connector
            .connect(domain, tcp_stream)
            .await
            .map_err(|e| anyhow::anyhow!("TLS handshake failed: {e}"))?;

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
    /// For showing — `name` is what the server accepts; the tools take this
    /// form as well (see [`resolve_folder_name`], which only counts it when
    /// it is set). Set only when the decoded form passes the same control/bidi
    /// check as the raw name: the encoding hides non-ASCII from that filter,
    /// so a crafted folder could otherwise smuggle a right-to-left override
    /// into the listing and render as a different folder entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// Words that name a folder by its role, as people and models say them, and
/// the role they stand for (the tags `detect_folder_role` gives).
const ROLE_WORDS: &[(&str, &str)] = &[
    ("inbox", "inbox"),
    ("posteingang", "inbox"),
    ("eingang", "inbox"),
    ("drafts", "drafts"),
    ("draft", "drafts"),
    ("entwurf", "drafts"),
    ("entwürfe", "drafts"),
    ("entwuerfe", "drafts"),
    ("sent", "sent"),
    ("gesendet", "sent"),
    ("gesendete", "sent"),
    ("sent items", "sent"),
    ("sent mail", "sent"),
    ("gesendete elemente", "sent"),
    ("gesendete objekte", "sent"),
    ("trash", "trash"),
    ("papierkorb", "trash"),
    ("gelöscht", "trash"),
    ("gelöschte", "trash"),
    ("gelöschte elemente", "trash"),
    ("gelöschte objekte", "trash"),
    ("geloescht", "trash"),
    ("geloeschte", "trash"),
    ("geloeschte elemente", "trash"),
    ("deleted", "trash"),
    ("deleted items", "trash"),
    ("bin", "trash"),
    ("müll", "trash"),
    ("mülleimer", "trash"),
];

/// The folder of `names` that `requested` means, or `None`. Tried in order,
/// the first fit wins:
/// 1. the wire name itself (`INBOX` in any case);
/// 2. a folder's readable name (`Entwürfe` for `Entw&APw-rfe`);
/// 3. the wire or readable name in any case, when exactly one folder fits
///    (`rechnungen` for `Rechnungen`); with several it stops there, unresolved;
/// 4. a role word in any case (`Papierkorb`, `trash`, `Entwurf`,
///    `Posteingang` and the like, see `ROLE_WORDS`), resolved to the folder
///    with that role.
///
/// Models pass what they show the user or what the user said: the readable
/// name, which the server cannot select (Dovecot refuses it as "not valid
/// mUTF-7"), or the role, which is not a name at all.
pub fn resolve_folder_name(requested: &str, names: &[String]) -> Option<String> {
    if let Some(name) = names.iter().find(|n| same_folder(n, requested)) {
        return Some(name.clone());
    }
    // Only a readable name that list_folders shows counts (see
    // `safe_display_name`): one with control or bidi characters names no
    // folder a model could have been shown.
    if let Some(name) = names
        .iter()
        .find(|n| safe_display_name(n).as_deref() == Some(requested))
    {
        return Some(name.clone());
    }
    let wanted = requested.trim().to_lowercase();
    let mut any_case = names.iter().filter(|n| {
        n.to_lowercase() == wanted
            || safe_display_name(n).is_some_and(|shown| shown.to_lowercase() == wanted)
    });
    match (any_case.next(), any_case.next()) {
        (Some(name), None) => return Some(name.clone()),
        (Some(_), Some(_)) => return None,
        _ => {}
    }
    let role = ROLE_WORDS
        .iter()
        .find(|(word, _)| *word == wanted)
        .map(|(_, role)| *role)?;
    if role == "inbox" {
        return names
            .iter()
            .find(|n| n.eq_ignore_ascii_case("INBOX"))
            .cloned();
    }
    names
        .iter()
        .find(|n| detect_folder_role(n) == Some(role))
        .cloned()
}

/// Whether a server's refusal says the mailbox is not there, in the wordings
/// of the servers around: RFC 5530's `[NONEXISTENT]`, Dovecot and Outlook
/// ("doesn't exist", "does not exist"), `GreenMail` ("No such mailbox"), Gmail
/// ("Unknown Mailbox"). Anything else, a busy mailbox included, keeps the
/// server's own words.
fn says_nonexistent(message: &str) -> bool {
    let lower = message.to_lowercase();
    [
        "[nonexistent]",
        "[trycreate]",
        "doesn't exist",
        "does not exist",
        "no such mailbox",
        "unknown mailbox",
        "folder not found",
        "mailbox not found",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
}

/// The error for a folder the account does not have, naming the ones it has
/// so the next call can take one instead of guessing again (see
/// [`folder_names_shown`]). `trash_hint`: the account may delete, so the
/// error can point at `delete_email` for a model looking for the trash.
fn unknown_folder_error(folder: &str, names: &[String], trash_hint: bool) -> anyhow::Error {
    // The last sentence names the next call: "use delete_email" alone made a
    // model tell the user which folder to use instead of making the call.
    let trash = if trash_hint {
        " To move emails to the trash, call delete_email with the same folder and uids; it puts \
         them there."
    } else {
        ""
    };
    anyhow::anyhow!(
        "Folder \"{folder}\" does not exist. Folders: {}.{trash}",
        folder_names_shown(names)
    )
}

/// The folder names an error offers: INBOX first, then alphabetically, the
/// readable name in parentheses where the wire name is encoded, at most 20
/// of them.
pub fn folder_names_shown(names: &[String]) -> String {
    const SHOWN: usize = 20;
    let mut ordered: Vec<&String> = names.iter().collect();
    ordered.sort_by_key(|name| (!name.eq_ignore_ascii_case("INBOX"), name.to_lowercase()));
    let shown: Vec<String> = ordered
        .iter()
        .take(SHOWN)
        .map(|name| {
            safe_display_name(name)
                .map_or_else(|| (*name).clone(), |shown| format!("{name} ({shown})"))
        })
        .collect();
    let more = ordered.len().saturating_sub(SHOWN);
    let rest = if more > 0 {
        format!(" … and {more} more, see list_folders")
    } else {
        String::new()
    };
    format!("{}{rest}", shown.join(", "))
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
/// Pure name matching — usable where no IMAP round trip may happen, such as
/// a `dry_run` preview.
pub fn detect_folder_role(name: &str) -> Option<&'static str> {
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

/// SASL `XOAUTH2`. The first challenge (empty) is answered with the
/// credentials. A later one is the server's error report — base64 JSON
/// naming the failure — which the mechanism answers with an empty response,
/// so the server can conclude with the tagged `NO` that carries its reason.
/// Replaying the credentials there (the previous behaviour) is a protocol
/// violation, leaving the outcome of a failed login to however the server
/// treats one.
struct XOAuth2Authenticator {
    auth: String,
    sent: bool,
}

impl XOAuth2Authenticator {
    const fn new(auth: String) -> Self {
        Self { auth, sent: false }
    }
}

impl async_imap::Authenticator for XOAuth2Authenticator {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        if std::mem::replace(&mut self.sent, true) {
            String::new()
        } else {
            self.auth.clone()
        }
    }
}

// ========== Utility functions ==========

/// The response for `uid` among a single-message UID FETCH's responses.
///
/// The stream also carries unsolicited FETCH responses, such as a flag
/// update for another message a parallel client just changed. Taking the
/// first one made `get_email` fail with "no body" whenever such an update
/// came in ahead of the answer. RFC 3501 has a UID FETCH response carry the
/// UID, so the right one is unambiguous.
fn requested_fetch(fetches: Vec<Fetch>, uid: u32) -> Option<Fetch> {
    fetches.into_iter().find(|f| f.uid == Some(uid))
}

/// Map IMAP flags to strings, dropping `\Recent`: it is session-scoped
/// server bookkeeping (whether THIS connection is the first to see the
/// message since the last session), obsolete enough that RFC 9051 removed
/// it — and it reads like "new for me", which it is not. Surfacing it
/// invited exactly that misread.
fn parse_flags(fetch: &Fetch) -> Vec<String> {
    fetch.flags().filter_map(|flag| flag_name(&flag)).collect()
}

/// The wire spelling of one flag, `None` for `\Recent` (see [`parse_flags`]).
fn flag_name(flag: &async_imap::types::Flag<'_>) -> Option<String> {
    use async_imap::types::Flag;
    Some(match flag {
        Flag::Seen => "\\Seen".to_string(),
        Flag::Answered => "\\Answered".to_string(),
        Flag::Flagged => "\\Flagged".to_string(),
        Flag::Deleted => "\\Deleted".to_string(),
        Flag::Draft => "\\Draft".to_string(),
        Flag::Recent => return None,
        Flag::MayCreate => "\\MayCreate".to_string(),
        Flag::Custom(c) => c.to_string(),
    })
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
    use super::*;

    /// A server that ends the session closes the connection cleanly. Read as
    /// the end of a command's responses, that made the command "succeed"
    /// with nothing; it has to surface as the connection error it is.
    #[tokio::test]
    async fn a_closed_connection_reads_as_an_error_not_as_the_end() {
        use tokio::io::AsyncReadExt as _;
        let mut stream = GuardedStream::new(&b"* BYE Disconnected\r\n"[..], STALL_TIMEOUT);
        let mut buf = [0u8; 64];
        assert_eq!(stream.read(&mut buf).await.unwrap(), 20);
        let err = stream.read(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(is_connection_error(&err.to_string()), "{err}");
        assert!(is_retryable_error(&err.to_string()), "{err}");
        // A read with no room in the buffer is not an end of stream.
        assert_eq!(stream.read(&mut []).await.unwrap(), 0);
    }

    /// Silence ends a read, and a write the peer does not take, with the
    /// error of a dead connection, which reads retry on. Without the guard
    /// both waited for TCP's retransmission timeout, a quarter of an hour.
    #[tokio::test]
    async fn silence_ends_a_read_and_a_write_as_a_dead_connection() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let (near, _far) = tokio::io::duplex(8);
        let mut stream = GuardedStream::new(near, Duration::from_millis(100));
        let mut buf = [0u8; 4];
        let err = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("the read must end, not hang")
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(is_connection_error(&err.to_string()), "{err}");
        assert!(is_retryable_error(&err.to_string()), "{err}");
        // The peer takes nothing: the write fills the pipe and stalls too.
        let err = tokio::time::timeout(Duration::from_secs(5), stream.write_all(&[0u8; 64]))
            .await
            .expect("the write must end, not hang")
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    /// Every byte starts the wait anew: an answer that keeps coming, however
    /// slowly, is no stall.
    #[tokio::test]
    async fn an_answer_that_keeps_coming_is_no_stall() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let (near, mut far) = tokio::io::duplex(64);
        let mut stream = GuardedStream::new(near, Duration::from_millis(200));
        tokio::spawn(async move {
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_millis(80)).await;
                let _ = far.write_all(b"x").await;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let mut got = Vec::new();
        let mut buf = [0u8; 8];
        while got.len() < 5 {
            let n = stream
                .read(&mut buf)
                .await
                .expect("no stall while bytes come");
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, b"xxxxx");
    }

    /// A throwaway self-signed certificate (P-256, CN=localhost) and its key,
    /// for the fake server below and valid for nothing else.
    const FAKE_CERT: &str = "MIIBfzCCASWgAwIBAgIUVmRbvFRXWEIwwOfIRd9NC+eRM7AwCgYIKoZIzj0EAwIwFDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MTExMTAwNDkyMVoYDzIxMjYxMDE4MDA0OTIxWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAR0bdX1KvMmduT6WBBs9EUfmWhWFB7x3+cdYOb4AIOFiWGeu38F2+qHXQ0JnWKQJ4APKj/PQinauWnr3qy4q0qTo1MwUTAdBgNVHQ4EFgQUqdcJPd34gLZBGRrRKfZxaSCJlhIwHwYDVR0jBBgwFoAUqdcJPd34gLZBGRrRKfZxaSCJlhIwDwYDVR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNIADBFAiAZvZpAy7LBEpHmnX23B06gj5bthW9SYjpbCoU4QsCsQAIhAMPqisnLF8/8yh8zzOo9iZaRd/Zv5BXUPLtwDN9scEWU";
    const FAKE_KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg9q5WF+ut6HmCU0k4cmUrA6YESHF4jgKbPYiHya5b392hRANCAAR0bdX1KvMmduT6WBBs9EUfmWhWFB7x3+cdYOb4AIOFiWGeu38F2+qHXQ0JnWKQJ4APKj/PQinauWnr3qy4q0qT";

    /// What the fake server answers to a command, given its tag, its word
    /// (upper case) and how often that word came before on any connection:
    /// the reply, or `None` to read the command and say nothing, as a hung
    /// server or a dead path does.
    type FakeAnswer = Arc<dyn Fn(&str, &str, usize) -> Option<String> + Send + Sync>;

    /// An IMAP server on loopback over TLS that greets, takes any LOGIN and
    /// LOGOUT, and answers everything else as `answer` says.
    async fn fake_server(answer: FakeAnswer) -> std::net::SocketAddr {
        use base64::Engine as _;
        use base64::engine::general_purpose::STANDARD;
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

        let cert = rustls::pki_types::CertificateDer::from(STANDARD.decode(FAKE_CERT).unwrap());
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(STANDARD.decode(FAKE_KEY).unwrap()),
        );
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
            String,
            usize,
        >::new()));
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (acceptor, answer, seen) =
                    (acceptor.clone(), Arc::clone(&answer), Arc::clone(&seen));
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let (read, mut write) = tokio::io::split(tls);
                    let mut lines = tokio::io::BufReader::new(read).lines();
                    if write.write_all(b"* OK fake IMAP ready\r\n").await.is_err() {
                        return;
                    }
                    while let Ok(Some(line)) = lines.next_line().await {
                        let mut words = line.split_whitespace();
                        let tag = words.next().unwrap_or("*").to_string();
                        let word = words.next().unwrap_or("").to_ascii_uppercase();
                        let reply = match word.as_str() {
                            "LOGIN" => Some(format!("{tag} OK LOGIN completed\r\n")),
                            "LOGOUT" => Some(format!("* BYE bye\r\n{tag} OK LOGOUT completed\r\n")),
                            _ => {
                                let before = count(&seen, &word);
                                answer(&tag, &word, before)
                            }
                        };
                        let Some(reply) = reply else { continue };
                        if write.write_all(reply.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        addr
    }

    /// How often `word` came before, counting this one in.
    fn count(
        seen: &std::sync::Mutex<std::collections::HashMap<String, usize>>,
        word: &str,
    ) -> usize {
        let mut seen = seen.lock().unwrap();
        let count = seen.entry(word.to_string()).or_insert(0);
        let before = *count;
        *count += 1;
        drop(seen);
        before
    }

    /// A client of the fake server at `addr` that gives up after 300 ms of
    /// silence instead of a minute.
    async fn fake_client(addr: std::net::SocketAddr) -> ImapClient {
        let account: AccountConfig = toml::from_str(&format!(
            "name = \"Fake\"\nhost = \"127.0.0.1\"\nport = {}\nusername = \"u@h\"\n\
             password = \"p\"\naccept_invalid_certs = true\n",
            addr.port()
        ))
        .unwrap();
        let mut client = ImapClient::new(account).stall_after(Duration::from_millis(300));
        client.connect_bounded().await.expect("greeting and login");
        client
    }

    /// The SELECT of an empty folder, answered.
    fn selected(tag: &str) -> String {
        format!(
            "* 0 EXISTS\r\n* OK [UIDVALIDITY 1] ok\r\n{tag} OK [READ-WRITE] SELECT completed\r\n"
        )
    }

    /// The case Otomo ran into: the server takes the command and says
    /// nothing. The read gives up after the stall time, reconnects once and
    /// meets the same silence, and the call fails as retryable instead of
    /// holding the account until the server restarts. The next call
    /// reconnects and gets its answer.
    #[tokio::test]
    async fn a_server_that_goes_silent_frees_the_account() {
        let addr = fake_server(Arc::new(|tag, word, before| match word {
            "SELECT" if before < 2 => None,
            "SELECT" => Some(selected(tag)),
            _ => Some(format!("{tag} OK done\r\n")),
        }))
        .await;
        let mut client = fake_client(addr).await;

        let started = std::time::Instant::now();
        let err = tokio::time::timeout(
            Duration::from_secs(10),
            client.list_emails("INBOX", 1, 0, false),
        )
        .await
        .expect("the silence must end the call, not hang it")
        .expect_err("both SELECTs went unanswered");
        assert!(is_retryable_error(&err.to_string()), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            !client.connection_state().connected,
            "the silent session is gone"
        );

        let (rows, total, _) = tokio::time::timeout(
            Duration::from_secs(10),
            client.list_emails("INBOX", 1, 0, false),
        )
        .await
        .expect("no hang")
        .expect("the third SELECT is answered");
        assert!(rows.is_empty() && total == 0);
    }

    /// A failure swallowed for a better error message (here the folder list
    /// behind "does not exist") must still drop a session that stalled: kept,
    /// it would hand the late LIST answer to the next command.
    #[tokio::test]
    async fn a_stall_behind_a_swallowed_error_drops_the_session() {
        let addr = fake_server(Arc::new(|tag, word, _| match word {
            "SELECT" => Some(format!("{tag} NO [NONEXISTENT] Unknown Mailbox\r\n")),
            "LIST" => None,
            _ => Some(format!("{tag} OK done\r\n")),
        }))
        .await;
        let mut client = fake_client(addr).await;
        let err = tokio::time::timeout(
            Duration::from_secs(10),
            client.list_emails("Nope", 1, 0, false),
        )
        .await
        .expect("no hang")
        .expect_err("the folder does not exist");
        assert!(err.to_string().contains("Unknown Mailbox"), "{err}");
        assert!(
            !client.connection_state().connected,
            "the session whose LIST stalled must be gone"
        );
    }

    /// A write step wraps the cause in what it means for the caller. A dead
    /// or stalled connection underneath must still drop the session.
    #[test]
    fn a_connection_error_under_a_context_still_drops_the_session() {
        let account: AccountConfig = toml::from_str(
            "name = \"T\"\nhost = \"127.0.0.1\"\nport = 9\nusername = \"u@h\"\npassword = \"p\"\n",
        )
        .unwrap();
        let mut client = ImapClient::new(account);
        let wrapped = anyhow::Error::new(stalled())
            .context("COPY to target succeeded but \\Deleted-flag STORE on source failed");
        let _ = client.check_error(wrapped);
        assert!(
            client.connection_state().last_error.is_some(),
            "the session counts as dead"
        );
    }

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

    /// The refusal for a missing folder lists the folders there are, so the
    /// caller can pick one: INBOX first, encoded names readable in
    /// parentheses, long lists cut with a pointer to `list_folders`.
    #[test]
    fn unknown_folder_error_names_the_folders_there_are() {
        let names: Vec<String> = ["Rechnungen", "Entw&APw-rfe", "INBOX", "Gesendete Elemente"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let err = unknown_folder_error("Trash", &names, true).to_string();
        assert_eq!(
            err,
            "Folder \"Trash\" does not exist. Folders: INBOX, Entw&APw-rfe (Entwürfe), \
             Gesendete Elemente, Rechnungen. To move emails to the trash, call delete_email with \
             the same folder and uids; it puts them there."
        );
        // An account that may not delete is not sent to delete_email.
        let err = unknown_folder_error("Trash", &names, false).to_string();
        assert!(err.ends_with("Gesendete Elemente, Rechnungen."), "{err}");
        let many: Vec<String> = (0..25).map(|i| format!("Ordner {i:02}")).collect();
        let err = unknown_folder_error("X", &many, true).to_string();
        assert!(
            err.contains("Ordner 19 … and 5 more, see list_folders."),
            "{err}"
        );
        assert!(!err.contains("Ordner 20"), "{err}");
    }

    /// Models pass the readable name or the role instead of the wire name;
    /// each resolves to the folder meant, the wire name first.
    #[test]
    fn resolve_folder_name_takes_wire_readable_and_role_names() {
        let names: Vec<String> = [
            "INBOX",
            "Entw&APw-rfe",
            "Gesendete Elemente",
            "Gel&APY-schte Elemente",
            "Rechnungen",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let resolve = |requested: &str| resolve_folder_name(requested, &names);
        assert_eq!(resolve("Rechnungen").as_deref(), Some("Rechnungen"));
        assert_eq!(resolve("rechnungen").as_deref(), Some("Rechnungen"));
        assert_eq!(resolve("inbox").as_deref(), Some("INBOX"));
        assert_eq!(resolve("Posteingang").as_deref(), Some("INBOX"));
        assert_eq!(resolve("Entwürfe").as_deref(), Some("Entw&APw-rfe"));
        assert_eq!(resolve("DRAFT").as_deref(), Some("Entw&APw-rfe"));
        assert_eq!(
            resolve("gelöschte elemente").as_deref(),
            Some("Gel&APY-schte Elemente")
        );
        assert_eq!(
            resolve("Papierkorb").as_deref(),
            Some("Gel&APY-schte Elemente")
        );
        assert_eq!(resolve("Trash").as_deref(), Some("Gel&APY-schte Elemente"));
        assert_eq!(resolve("Gesendet").as_deref(), Some("Gesendete Elemente"));
        assert_eq!(resolve("Archiv"), None);
        // A real folder of that name wins over the role word.
        let with_trash: Vec<String> = ["INBOX", "Trash", "Gel&APY-schte Elemente"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(
            resolve_folder_name("Trash", &with_trash).as_deref(),
            Some("Trash")
        );
        // Two folders differing only in case: no guess, not even through
        // the role word the name also is.
        let twins: Vec<String> = ["Projekt", "PROJEKT", "Trash", "TRASH"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(resolve_folder_name("projekt", &twins), None);
        assert_eq!(resolve_folder_name("trash", &twins), None);
        assert_eq!(
            resolve_folder_name("PROJEKT", &twins).as_deref(),
            Some("PROJEKT")
        );
        // The readable name in its own case beats a case-insensitive fit.
        let readable_twins: Vec<String> = ["Gel&APY-scht", "gel&APY-scht"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(
            resolve_folder_name("gelöscht", &readable_twins).as_deref(),
            Some("gel&APY-scht")
        );
        assert_eq!(resolve_folder_name("GELÖSCHT", &readable_twins), None);
        // What people type: blanks around the name, umlauts spelled out,
        // the full localized name of a folder that has another name here.
        let plain: Vec<String> = ["INBOX", "Drafts", "Sent", "Trash"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let resolve = |requested: &str| resolve_folder_name(requested, &plain);
        assert_eq!(resolve(" Sent ").as_deref(), Some("Sent"));
        assert_eq!(resolve("Entwuerfe").as_deref(), Some("Drafts"));
        assert_eq!(resolve("geloescht").as_deref(), Some("Trash"));
        assert_eq!(resolve("Geloeschte Elemente").as_deref(), Some("Trash"));
        assert_eq!(resolve("Gelöschte Objekte").as_deref(), Some("Trash"));
        assert_eq!(resolve("Gelöschte Elemente").as_deref(), Some("Trash"));
        assert_eq!(resolve("Gesendete Objekte").as_deref(), Some("Sent"));
        assert_eq!(resolve("Müll").as_deref(), Some("Trash"));
        assert_eq!(resolve("Eingang").as_deref(), Some("INBOX"));
    }

    #[test]
    fn says_nonexistent_knows_the_server_wordings() {
        for message in [
            "no response: code: None, info: Some(\"[NONEXISTENT] Unknown Mailbox: X (Failure)\")",
            "Mailbox doesn't exist: Papierkorb",
            "SELECT failed. No such mailbox",
            "Mailbox does not exist, or must be subscribed to.",
        ] {
            assert!(says_nonexistent(message), "{message}");
        }
        for message in [
            "[INUSE] Mailbox is in use",
            "connection lost",
            "Permission denied",
        ] {
            assert!(!says_nonexistent(message), "{message}");
        }
    }

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

    /// `\Recent` is session-scoped bookkeeping that reads like "new for
    /// me"; it must never reach a caller. Everything else keeps its wire
    /// spelling, keywords included.
    #[test]
    fn flag_names_drop_recent_and_keep_keywords() {
        use async_imap::types::Flag;
        assert_eq!(flag_name(&Flag::Seen).as_deref(), Some("\\Seen"));
        assert_eq!(flag_name(&Flag::Flagged).as_deref(), Some("\\Flagged"));
        assert_eq!(flag_name(&Flag::Draft).as_deref(), Some("\\Draft"));
        assert_eq!(flag_name(&Flag::Recent), None);
        assert_eq!(
            flag_name(&Flag::Custom("$Forwarded".into())).as_deref(),
            Some("$Forwarded")
        );
    }

    #[test]
    fn xoauth2_answers_the_error_challenge_with_an_empty_response() {
        use async_imap::Authenticator as _;
        let mut auth = XOAuth2Authenticator::new("user=a\x01auth=Bearer t\x01\x01".into());
        // The initial (empty) challenge gets the credentials …
        assert_eq!(auth.process(b""), "user=a\x01auth=Bearer t\x01\x01");
        // … every later one is an error report and gets an empty response.
        assert_eq!(auth.process(br#"{"status":"401","schemes":"bearer"}"#), "");
        assert_eq!(auth.process(b"again"), "");
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
            reply_to: vec![],
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
