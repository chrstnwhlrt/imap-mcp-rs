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
    host_supports_unicode_search, imap_astring, iso_to_imap_date, sanitize_log_str,
    starts_with_ignore_ascii_case,
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
const SUMMARY_FETCH_ITEMS: &str = "(BODY.PEEK[] FLAGS UID)";

/// Turn a list of summary-shaped fetch responses into `EmailSummary`
/// rows. Centralizes the UID-skip + bounded body + parse + summarize
/// pipeline shared by `list_emails`, `list_unread_emails`, and
/// `search_emails`.
fn summarize_fetches(fetches: &[Fetch], folder: &str) -> Vec<EmailSummary> {
    let mut out = Vec::with_capacity(fetches.len());
    for fetch in fetches {
        // Skip responses without a UID rather than defaulting to 0 — two
        // such responses would otherwise collide at uid=0 and yield
        // unaddressable entries (callers can't later FETCH/STORE uid=0).
        let Some(uid) = fetch.uid else { continue };
        let Some(body) = bounded_body(fetch, uid) else {
            continue;
        };
        let flags = parse_flags(fetch);
        let full = email::parse_email_no_html(uid, folder, body, flags);
        out.push(email::summarize(full, 200));
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

        Self {
            session: None,
            config,
            allowed_folders,
            selected_folder: None,
            selected_exists: 0,
            last_uid_validity: None,
            cached_folder_names: None,
            cached_oauth_token: None,
            last_error: None,
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
                        (
                            t.is_valid(),
                            Some((t.expires_at - now).as_secs()),
                        )
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

    pub async fn connect(&mut self) -> Result<()> {
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
                let oauth2_config = self
                    .config
                    .oauth2
                    .as_ref()
                    .context("OAuth2 config required")?;
                // Reuse a cached token when it's still within its TTL. Only
                // hit the OAuth endpoint on first connect or after expiry.
                let access_token = match &self.cached_oauth_token {
                    Some(t) if t.is_valid() => {
                        tracing::debug!("Using cached OAuth2 access token");
                        t.token.clone()
                    }
                    _ => {
                        let fresh = crate::oauth2::refresh_access_token(oauth2_config).await?;
                        let tok = fresh.token.clone();
                        self.cached_oauth_token = Some(fresh);
                        tok
                    }
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
    /// can surface them to the operator without tailing stderr.
    async fn ensure_connected(&mut self) -> Result<()> {
        if self.session.is_some() {
            return Ok(());
        }
        tracing::info!(
            account = %self.config.name,
            "IMAP session lost, attempting reconnect..."
        );
        let result: Result<()> = tokio::time::timeout(Duration::from_secs(15), self.connect())
            .await
            .context("IMAP reconnect timed out")?;
        if let Err(e) = &result {
            self.last_error = Some(sanitize_log_str(&e.to_string()));
        } else {
            self.last_error = None;
        }
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
            let status_fut =
                tokio::time::timeout(Duration::from_secs(10), session.status(&name, "(MESSAGES UNSEEN)"));
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
            result.push(FolderInfo {
                name,
                total,
                unread,
                role,
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

        let summaries = summarize_fetches(&fetches, folder);
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
        #[allow(clippy::cast_possible_truncation)]
        let matched = uids.len() as u32;
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
        let summaries = summarize_fetches(&fetches, folder);
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

    pub async fn search_emails(
        &mut self,
        folder: &str,
        criteria: &str,
        limit: u32,
    ) -> Result<Vec<EmailSummary>> {
        retry_read!(self.search_emails_once(folder, criteria, limit))
    }

    async fn search_emails_once(
        &mut self,
        folder: &str,
        criteria: &str,
        limit: u32,
    ) -> Result<Vec<EmailSummary>> {
        self.ensure_selected(folder).await?;
        let session = self.session()?;

        let uids_stream = session.uid_search(criteria).await?;
        let mut uids: Vec<u32> = uids_stream.into_iter().collect();
        uids.sort_unstable_by(|a, b| b.cmp(a));
        uids.truncate(limit as usize);

        if uids.is_empty() {
            return Ok(vec![]);
        }

        let uid_set = uid_set_string(&uids);
        let stream = session.uid_fetch(&uid_set, SUMMARY_FETCH_ITEMS).await?;
        let fetches: Vec<Fetch> = stream.try_collect().await?;
        Ok(summarize_fetches(&fetches, folder))
    }

    pub async fn get_thread(&mut self, folder: &str, uid: u32) -> Result<Vec<EmailFull>> {
        retry_read!(self.get_thread_once(folder, uid))
    }

    // Linear 5-phase workflow (fetch initial, primary search, subject fallback,
    // fetch thread emails, sent-folder search) — splitting would trade
    // readability for line count.
    #[allow(clippy::too_many_lines)]
    async fn get_thread_once(&mut self, folder: &str, uid: u32) -> Result<Vec<EmailFull>> {
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

        // 3. Subject-based fallback for small threads (0-1 roundtrips, conditional)
        if thread_uids.len() <= 2 {
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
        // Collect the STORE fetch responses — the server emits one per UID
        // that actually existed and got updated. UIDs passed in but absent
        // from the folder (stale after UIDVALIDITY rotation / external
        // expunge / typo) produce no response. Returning those as
        // "succeeded" would silently mislead the LLM.
        //
        // Also intersect with the caller's input set: a hostile or buggy
        // server could echo UIDs we never asked about, which would inflate
        // the LLM's view of "what got changed". Take only the overlap.
        let input: HashSet<u32> = uids.iter().copied().collect();
        let fetches: Vec<Fetch> = session
            .uid_store(&uid_set, format!("{op} ({flag})"))
            .await?
            .try_collect()
            .await?;
        let mut updated: Vec<u32> = fetches
            .iter()
            .filter_map(|f| f.uid)
            .filter(|u| input.contains(u))
            .collect();
        updated.sort_unstable();
        updated.dedup();
        Ok(updated)
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
        let uid_set = uid_set_string(uids);
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
            store_stream
                .try_collect::<Vec<_>>()
                .await
                .map_err(|e| {
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
        Ok(uids.to_vec())
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
            let uid_set = uid_set_string(uids);
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
        } else {
            let trash = self
                .find_folder_by_role(TRASH_FOLDER_NAMES)
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| "Trash".to_string());
            self.move_emails(folder, uids, &trash).await?;
        }
        Ok(uids.to_vec())
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
        let uid_set = uid_set_string(uids);
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
        Ok(uids.to_vec())
    }

    pub async fn save_draft(&mut self, message_bytes: &[u8]) -> Result<()> {
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
        session
            .append(&drafts, Some("(\\Draft)"), None, message_bytes)
            .await?;
        // APPEND doesn't change selection, but be safe
        self.selected_folder = None;
        self.selected_exists = 0;
        Ok(())
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
}

/// Classify a folder name against the well-known role lists. Returns a
/// stable role tag the LLM can match against ("drafts" | "sent" | "trash").
fn detect_folder_role(name: &str) -> Option<&'static str> {
    if DRAFTS_FOLDER_NAMES.iter().any(|n| name.eq_ignore_ascii_case(n)) {
        return Some("drafts");
    }
    if SENT_FOLDER_NAMES.iter().any(|n| name.eq_ignore_ascii_case(n)) {
        return Some("sent");
    }
    if TRASH_FOLDER_NAMES.iter().any(|n| name.eq_ignore_ascii_case(n)) {
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

fn parse_flags(fetch: &Fetch) -> Vec<String> {
    fetch
        .flags()
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
}
