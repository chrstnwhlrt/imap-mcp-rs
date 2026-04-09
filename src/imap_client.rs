use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_imap::Session;
use async_imap::types::Fetch;
use futures_util::TryStreamExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::config::{AccountConfig, AuthMethod};
use crate::email::{self, EmailFull, EmailSummary};

use std::time::Duration;

type ImapSession = Session<TlsStream<TcpStream>>;

pub struct ImapClient {
    session: Option<ImapSession>,
    config: AccountConfig,
    allowed_folders: Option<HashSet<String>>,
    selected_folder: Option<String>,
    selected_exists: u32,
    cached_folder_names: Option<Vec<String>>,
}

impl ImapClient {
    pub fn new(config: AccountConfig) -> Self {
        let allowed_folders = config
            .allowed_folders
            .as_ref()
            .map(|f| f.iter().cloned().collect());

        Self {
            session: None,
            config,
            allowed_folders,
            selected_folder: None,
            selected_exists: 0,
            cached_folder_names: None,
        }
    }

    pub async fn connect(&mut self) -> Result<()> {
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
                let access_token = crate::oauth2::refresh_access_token(oauth2_config).await?;
                let auth_string = format!(
                    "user={}\x01auth=Bearer {}\x01\x01",
                    self.config.username, access_token
                );
                tracing::debug!(auth_len = auth_string.len(), "Attempting XOAUTH2");
                client
                    .authenticate("XOAUTH2", XOAuth2Authenticator(auth_string))
                    .await
                    .map_err(|(e, _)| e)
                    .context("IMAP OAuth2 authentication failed")?
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
        if let Some(mut session) = self.session.take()
            && let Err(e) = session.logout().await
        {
            tracing::warn!("Error during IMAP logout: {e}");
        }
        self.selected_folder = None;
    }

    fn session(&mut self) -> Result<&mut ImapSession> {
        self.session
            .as_mut()
            .context("Not connected to IMAP server")
    }

    /// Mark the session as dead. The next `ensure_connected` call
    /// will trigger a reconnect attempt.
    pub fn mark_dead(&mut self) {
        self.session = None;
        self.selected_folder = None;
        self.selected_exists = 0;
    }

    /// Ensure we have a live IMAP session, reconnecting if necessary.
    async fn ensure_connected(&mut self) -> Result<()> {
        if self.session.is_some() {
            return Ok(());
        }
        tracing::info!("IMAP session lost, attempting reconnect...");
        self.connect().await
    }

    /// Select a folder, skipping the IMAP command if already selected.
    /// Returns the message count (exists) from the SELECT response, or the
    /// cached value if we skipped the command.
    async fn ensure_selected(&mut self, folder: &str) -> Result<u32> {
        self.ensure_connected().await?;
        if self.selected_folder.as_deref() == Some(folder) {
            return Ok(self.selected_exists);
        }
        let session = self.session()?;
        let mailbox = match session.select(folder).await {
            Ok(m) => m,
            Err(e) => {
                let err: anyhow::Error = e.into();
                return Err(self.check_error(err));
            }
        };
        self.selected_folder = Some(folder.to_string());
        self.selected_exists = mailbox.exists;
        Ok(mailbox.exists)
    }

    /// Check if an error is a connection error. If so, mark dead for reconnect.
    pub fn check_error(&mut self, e: anyhow::Error) -> anyhow::Error {
        if is_connection_error(&e.to_string()) {
            tracing::warn!("IMAP connection error, will reconnect on next call: {e}");
            self.mark_dead();
        }
        e
    }

    pub fn is_folder_allowed(&self, folder: &str) -> bool {
        match &self.allowed_folders {
            Some(allowed) => allowed.contains(folder),
            None => true,
        }
    }

    // ========== Folder operations ==========

    pub async fn list_folders(&mut self) -> Result<Vec<FolderInfo>> {
        let names = self.get_folder_names().await?;

        let mut result = Vec::new();
        for name in names {
            self.ensure_connected().await?;
            let session = self.session()?;
            let (total, unread) = match session.status(&name, "(MESSAGES UNSEEN)").await {
                Ok(mailbox) => (mailbox.exists, mailbox.unseen.unwrap_or(0)),
                Err(_) => (0, 0),
            };
            // STATUS doesn't change the selected folder
            result.push(FolderInfo {
                name,
                total,
                unread,
            });
        }

        Ok(result)
    }

    /// Get folder names with caching (IMAP LIST is called once per session).
    pub async fn get_folder_names(&mut self) -> Result<Vec<String>> {
        if let Some(cached) = &self.cached_folder_names {
            return Ok(cached.clone());
        }

        self.ensure_connected().await?;
        let session = self.session()?;
        let folders_stream = session.list(Some(""), Some("*")).await?;
        let names: Vec<String> = folders_stream
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .map(|f| f.name().to_string())
            .filter(|name| self.is_folder_allowed(name))
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
        let total = self.ensure_selected(folder).await?;
        let session = self.session()?;

        let criteria = if unread_only { "UNSEEN" } else { "ALL" };
        let uids_stream = session.uid_search(criteria).await?;
        let mut uids: Vec<u32> = uids_stream.into_iter().collect();
        #[allow(clippy::cast_possible_truncation)]
        let matched = uids.len() as u32;
        uids.sort_unstable_by(|a, b| b.cmp(a)); // newest first

        let paged_uids: Vec<u32> = uids
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();

        if paged_uids.is_empty() {
            return Ok((vec![], total, matched));
        }

        let uid_set = uid_set_string(&paged_uids);
        let query = "(BODY.PEEK[] FLAGS)";
        let stream = session.uid_fetch(&uid_set, &query).await?;
        let fetches: Vec<Fetch> = stream.try_collect().await?;

        let mut summaries = Vec::new();
        for fetch in &fetches {
            if let Some(body) = fetch.body() {
                let flags = parse_flags(fetch);
                let uid = fetch.uid.unwrap_or(0);
                let full = email::parse_email(uid, folder, body, flags);
                summaries.push(email::summarize(&full, 200));
            }
        }

        Ok((summaries, total, matched))
    }

    /// Fetch raw email bytes for a single message (for attachment extraction).
    pub async fn fetch_raw(&mut self, folder: &str, uid: u32) -> Result<Option<Vec<u8>>> {
        self.ensure_selected(folder).await?;
        let session = self.session()?;
        let stream = session
            .uid_fetch(uid.to_string(), "(BODY.PEEK[] FLAGS)")
            .await?;
        let fetches: Vec<Fetch> = stream.try_collect().await?;

        let Some(fetch) = fetches.first() else {
            return Ok(None);
        };

        Ok(fetch.body().map(<[u8]>::to_vec))
    }

    pub async fn get_email(&mut self, folder: &str, uid: u32) -> Result<Option<EmailFull>> {
        self.ensure_selected(folder).await?;
        let session = self.session()?;
        let stream = session
            .uid_fetch(uid.to_string(), "(BODY.PEEK[] FLAGS)")
            .await?;
        let fetches: Vec<Fetch> = stream.try_collect().await?;

        let Some(fetch) = fetches.first() else {
            return Ok(None);
        };

        let body = fetch.body().context("Email has no body")?;
        let flags = parse_flags(fetch);
        Ok(Some(email::parse_email(uid, folder, body, flags)))
    }

    pub async fn search_emails(
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
        let query = "(BODY.PEEK[] FLAGS)";
        let stream = session.uid_fetch(&uid_set, &query).await?;
        let fetches: Vec<Fetch> = stream.try_collect().await?;

        let mut summaries = Vec::new();
        for fetch in &fetches {
            if let Some(body) = fetch.body() {
                let flags = parse_flags(fetch);
                let uid = fetch.uid.unwrap_or(0);
                let full = email::parse_email(uid, folder, body, flags);
                summaries.push(email::summarize(&full, 200));
            }
        }

        Ok(summaries)
    }

    pub async fn get_thread(&mut self, folder: &str, uid: u32) -> Result<Vec<EmailFull>> {
        // 1. Fetch the initial email (1 roundtrip)
        let initial = self
            .get_email(folder, uid)
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

        for ref_id in &initial.references {
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
            if let Ok(uids) = session.uid_search(&combined).await {
                thread_uids.extend(uids.into_iter());
            }
        }

        // 3. Subject-based fallback for small threads (0-1 roundtrips, conditional)
        if thread_uids.len() <= 2 {
            let clean_subject = strip_email_prefixes(&initial.subject);
            if !clean_subject.is_empty() {
                self.ensure_selected(folder).await?;
                let session = self.session()?;
                let criteria = format!("SUBJECT \"{}\"", escape_imap_string(clean_subject));
                if let Ok(uids) = session.uid_search(&criteria).await
                    && uids.len() < 20
                {
                    thread_uids.extend(uids.into_iter());
                }
            }
        }

        // 4. Fetch all thread emails from the primary folder (1 roundtrip)
        let mut emails = Vec::new();
        if !thread_uids.is_empty() {
            let uid_set = uid_set_string(&thread_uids.iter().copied().collect::<Vec<_>>());
            self.ensure_selected(folder).await?;
            let session = self.session()?;
            let stream = session.uid_fetch(&uid_set, "(BODY.PEEK[] FLAGS)").await?;
            let fetches: Vec<Fetch> = stream.try_collect().await?;
            for fetch in &fetches {
                if let Some(body) = fetch.body() {
                    let flags = parse_flags(fetch);
                    let uid = fetch.uid.unwrap_or(0);
                    emails.push(email::parse_email(uid, folder, body, flags));
                }
            }
        }

        // 5. Search Sent folder with a single combined OR search (1-2 roundtrips)
        if let Some(sent) = self.detect_folder_by_role(SENT_FOLDER_NAMES).await
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
                if let Ok(sent_uids) = session.uid_search(&combined).await
                    && !sent_uids.is_empty()
                {
                    let uid_set = uid_set_string(&sent_uids.into_iter().collect::<Vec<_>>());
                    let session = self.session()?;
                    let stream = session.uid_fetch(&uid_set, "(BODY.PEEK[] FLAGS)").await?;
                    if let Ok(fetches) = stream.try_collect::<Vec<Fetch>>().await {
                        for fetch in &fetches {
                            if let Some(body) = fetch.body() {
                                let flags = parse_flags(fetch);
                                let uid = fetch.uid.unwrap_or(0);
                                emails.push(email::parse_email(uid, &sent, body, flags));
                            }
                        }
                    }
                }
            }
        }

        emails.sort_by(|a, b| a.date.cmp(&b.date));
        Ok(emails)
    }

    // ========== Write operations ==========

    pub async fn mark_flags(
        &mut self,
        folder: &str,
        uids: &[u32],
        flag: &str,
        add: bool,
    ) -> Result<Vec<u32>> {
        self.ensure_selected(folder).await?;
        let session = self.session()?;

        let uid_set = uid_set_string(uids);
        if add {
            session
                .uid_store(&uid_set, format!("+FLAGS ({flag})"))
                .await?
                .try_collect::<Vec<_>>()
                .await?;
        } else {
            session
                .uid_store(&uid_set, format!("-FLAGS ({flag})"))
                .await?
                .try_collect::<Vec<_>>()
                .await?;
        }

        Ok(uids.to_vec())
    }

    pub async fn move_emails(
        &mut self,
        folder: &str,
        uids: &[u32],
        target: &str,
    ) -> Result<Vec<u32>> {
        self.ensure_selected(folder).await?;
        let session = self.session()?;

        let uid_set = uid_set_string(uids);
        session.uid_copy(&uid_set, target).await?;
        session
            .uid_store(&uid_set, "+FLAGS (\\Deleted)")
            .await?
            .try_collect::<Vec<_>>()
            .await?;
        session.expunge().await?.try_collect::<Vec<_>>().await?;

        Ok(uids.to_vec())
    }

    pub async fn delete_emails(
        &mut self,
        folder: &str,
        uids: &[u32],
        permanent: bool,
    ) -> Result<Vec<u32>> {
        if permanent {
            self.ensure_selected(folder).await?;
            let session = self.session()?;
            let uid_set = uid_set_string(uids);
            session
                .uid_store(&uid_set, "+FLAGS (\\Deleted)")
                .await?
                .try_collect::<Vec<_>>()
                .await?;
            session.expunge().await?.try_collect::<Vec<_>>().await?;
        } else {
            let trash = self
                .detect_folder_by_role(TRASH_FOLDER_NAMES)
                .await
                .unwrap_or_else(|| "Trash".to_string());
            self.move_emails(folder, uids, &trash).await?;
        }
        Ok(uids.to_vec())
    }

    pub async fn save_draft(&mut self, message_bytes: &[u8]) -> Result<()> {
        let drafts = self
            .detect_folder_by_role(DRAFTS_FOLDER_NAMES)
            .await
            .unwrap_or_else(|| "Drafts".to_string());

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

    /// Detect a special folder by matching against known names. Uses cached folder list.
    async fn detect_folder_by_role(&mut self, candidates: &[&str]) -> Option<String> {
        let folders = self.get_folder_names().await.ok()?;
        for folder in &folders {
            let lower = folder.to_lowercase();
            for name in candidates {
                if lower == name.to_lowercase() {
                    return Some(folder.clone());
                }
            }
        }
        None
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

fn uid_set_string(uids: &[u32]) -> String {
    uids.iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Escape a string for use in IMAP search quoted strings.
/// Strips control characters and escapes backslash + double quote.
pub(crate) fn escape_imap_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => result.push_str("\\\\"),
            '"' => result.push_str("\\\""),
            c if c.is_control() => {} // strip NUL, CR, LF, etc.
            c => result.push(c),
        }
    }
    result
}

/// Convert ISO 8601 date (YYYY-MM-DD) to IMAP date format (DD-Mon-YYYY).
pub(crate) fn iso_to_imap_date(iso: &str) -> Result<String> {
    let parts: Vec<&str> = iso.split('-').collect();
    if parts.len() != 3 {
        anyhow::bail!("Invalid date format: {iso}. Expected YYYY-MM-DD");
    }
    let year = parts[0];
    let month_num: u32 = parts[1].parse().context("Invalid month")?;
    let day: u32 = parts[2].parse().context("Invalid day")?;

    let month_name = match month_num {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => anyhow::bail!("Invalid month: {month_num}"),
    };

    Ok(format!("{day}-{month_name}-{year}"))
}

/// Build an IMAP OR chain from multiple search criteria.
/// IMAP OR is prefix notation: `OR crit1 OR crit2 crit3` = crit1 OR (crit2 OR crit3).
/// Returns None if the input is empty.
fn build_or_criteria(criteria: &[String]) -> Option<String> {
    match criteria.len() {
        0 => None,
        1 => Some(criteria[0].clone()),
        _ => {
            let mut result = criteria.last().unwrap().clone();
            for c in criteria[..criteria.len() - 1].iter().rev() {
                result = format!("OR {c} {result}");
            }
            Some(result)
        }
    }
}

/// Heuristic to detect connection-level errors vs. IMAP protocol errors.
fn is_connection_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("broken pipe")
        || lower.contains("connection reset")
        || lower.contains("connection refused")
        || lower.contains("connection closed")
        || lower.contains("unexpected eof")
        || lower.contains("timed out")
        || lower.contains("network is unreachable")
        || lower.contains("no route to host")
}

/// Clean and escape a Message-ID for safe use in IMAP HEADER search.
/// Strips angle brackets, then escapes quotes/backslashes/control chars
/// to prevent IMAP injection via crafted Message-IDs in received emails.
fn clean_message_id(id: &str) -> String {
    escape_imap_string(id.trim_matches(|c| c == '<' || c == '>'))
}

/// Strip Re:/Fwd:/etc. prefixes from a subject line.
fn strip_email_prefixes(subject: &str) -> &str {
    let mut s = subject;
    loop {
        let trimmed = s.trim_start();
        if let Some(rest) = trimmed
            .strip_prefix("Re:")
            .or_else(|| trimmed.strip_prefix("RE:"))
            .or_else(|| trimmed.strip_prefix("re:"))
            .or_else(|| trimmed.strip_prefix("Fwd:"))
            .or_else(|| trimmed.strip_prefix("FWD:"))
            .or_else(|| trimmed.strip_prefix("fwd:"))
            .or_else(|| trimmed.strip_prefix("Fw:"))
            .or_else(|| trimmed.strip_prefix("FW:"))
        {
            s = rest;
        } else {
            return trimmed;
        }
    }
}
