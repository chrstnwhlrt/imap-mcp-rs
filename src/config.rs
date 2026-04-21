//! TOML config loading and validation.
//!
//! Supports multiple `[[accounts]]` blocks, each with either password or
//! `OAuth2` (Gmail / Outlook 365) auth. Per-account `read_only` / `allow_move` /
//! `allow_delete` switches act as a hard safety gate independent of the
//! configured `allowed_folders` whitelist.

use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub accounts: Vec<AccountConfig>,
    /// Filesystem directories from which attachment files may be read.
    /// Defaults to `["/tmp/imap-mcp-rs"]` (where `download_attachment` saves
    /// files). Paths outside these directories are rejected — this prevents
    /// a prompt-injected LLM from attaching arbitrary local files like SSH
    /// keys or /etc/passwd. Symlinks are resolved via `canonicalize`.
    #[serde(default = "default_attachment_dirs")]
    pub allowed_attachment_dirs: Vec<String>,
}

fn default_attachment_dirs() -> Vec<String> {
    vec![default_attachment_dir()]
}

/// Default attachment directory. Prefer `$XDG_RUNTIME_DIR` (guaranteed 0700
/// and user-private on systemd systems) so a multi-user host cannot race us
/// into a symlink. Falls back to a cache-dir location, finally `/tmp` —
/// still per-user by virtue of the username suffix on systems where
/// `dirs::cache_dir()` also fails.
pub fn default_attachment_dir() -> String {
    if let Some(runtime) = dirs::runtime_dir() {
        return runtime.join("imap-mcp-rs").to_string_lossy().into_owned();
    }
    if let Some(cache) = dirs::cache_dir() {
        return cache.join("imap-mcp-rs").to_string_lossy().into_owned();
    }
    let user = env::var("USER").unwrap_or_else(|_| "default".to_string());
    format!("/tmp/imap-mcp-rs-{user}")
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Deserialize, Clone)]
pub struct AccountConfig {
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    /// Email address used as From in drafts. Defaults to username if not set.
    pub email: Option<String>,
    /// Display name for From header (e.g. "John Doe").
    pub display_name: Option<String>,
    /// HTML signature appended to all drafts from this account.
    pub signature_html: Option<String>,
    /// Locale for draft formatting: "en" or "de". Controls labels (From/Von),
    /// date format, font, and reply/forward prefixes. Defaults to "en".
    pub locale: Option<String>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default = "default_true")]
    pub allow_delete: bool,
    #[serde(default = "default_true")]
    pub allow_move: bool,
    #[serde(default)]
    pub accept_invalid_certs: bool,
    /// Opt-in: allow plain IMAP `EXPUNGE` when the server doesn't advertise
    /// UIDPLUS (no `UID EXPUNGE`). Plain EXPUNGE removes every `\Deleted`
    /// message in the folder — including ones flagged by a concurrent
    /// client (phone, webmail) that hasn't expunged yet. On modern servers
    /// (Gmail, Outlook 365, Dovecot, Cyrus) UIDPLUS is universal and this
    /// never matters; on older/custom servers a permanent delete or move
    /// will refuse unless this is explicitly enabled.
    #[serde(default)]
    pub allow_unsafe_expunge: bool,
    pub allowed_folders: Option<Vec<String>>,
    #[serde(default = "default_auth_method")]
    pub auth_method: AuthMethod,
    pub password: Option<String>,
    pub oauth2: Option<OAuth2Config>,
}

impl AccountConfig {
    /// The email address to use as From in outgoing drafts.
    pub fn sender_address(&self) -> &str {
        self.email.as_deref().unwrap_or(&self.username)
    }
}

impl std::fmt::Debug for AccountConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountConfig")
            .field("name", &self.name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("read_only", &self.read_only)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("oauth2", &self.oauth2)
            .finish_non_exhaustive()
    }
}

const fn default_port() -> u16 {
    993
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    Password,
    OAuth2,
}

const fn default_auth_method() -> AuthMethod {
    AuthMethod::Password
}

#[derive(Deserialize, Clone)]
pub struct OAuth2Config {
    #[serde(default = "default_provider")]
    pub provider: OAuth2Provider,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub refresh_token: Option<String>,
    pub tenant: Option<String>,
    pub token_url: Option<String>,
}

impl std::fmt::Debug for OAuth2Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2Config")
            .field("provider", &self.provider)
            .field("client_id", &self.client_id.as_ref().map(|_| "[REDACTED]"))
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("tenant", &self.tenant)
            .field("token_url", &self.token_url)
            .finish()
    }
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OAuth2Provider {
    Gmail,
    Outlook365,
    Custom,
}

const fn default_provider() -> OAuth2Provider {
    OAuth2Provider::Gmail
}

impl OAuth2Config {
    pub fn token_url(&self) -> Result<String> {
        if let Some(url) = &self.token_url {
            return Ok(url.clone());
        }
        match self.provider {
            OAuth2Provider::Gmail => Ok("https://oauth2.googleapis.com/token".to_string()),
            OAuth2Provider::Outlook365 => {
                let tenant = self.tenant.as_deref().unwrap_or("common");
                Ok(format!(
                    "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token"
                ))
            }
            OAuth2Provider::Custom => {
                bail!("token_url required for custom OAuth2 provider")
            }
        }
    }
}

pub fn load_config(path: Option<&str>) -> Result<ServerConfig> {
    let _ = dotenvy::dotenv();

    let config_path = if let Some(p) = path {
        PathBuf::from(p)
    } else if let Ok(p) = env::var("IMAP_MCP_CONFIG") {
        PathBuf::from(p)
    } else {
        find_config_file()
            .context("No config file found. Create config.toml or set IMAP_MCP_CONFIG")?
    };

    let content = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;

    let config: ServerConfig = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config file: {}", config_path.display()))?;

    // Validate
    if config.accounts.is_empty() {
        bail!("Config must contain at least one [[accounts]] entry");
    }

    // Check for duplicate names (case-insensitive)
    let mut seen = std::collections::HashSet::new();
    for account in &config.accounts {
        let lower = account.name.to_lowercase();
        if !seen.insert(lower) {
            bail!("Duplicate account name: \"{}\"", account.name);
        }
    }

    // `sender_address()` falls back to `username` when `email` is unset. If
    // the username is not an email (e.g. "alice" for a login-only IMAP setup),
    // outgoing drafts would have a malformed From header that many MTAs
    // reject. Fail loudly at config load instead of at first draft_email.
    for account in &config.accounts {
        if account.email.is_none() && !account.username.contains('@') {
            bail!(
                "Account \"{}\" needs an `email` field — username \"{}\" is not an email address \
                 and drafts require a valid From header",
                account.name,
                account.username
            );
        }
    }

    // `allowed_folders = []` would make every folder operation return
    // "not in allowed_folders", which is almost certainly a misconfiguration
    // — users who want "deny all" don't configure the account at all, and
    // users who want "allow all" omit the field entirely (default `None`).
    // Fail loudly at load so the operator discovers the broken config at
    // startup, not at first tool call.
    for account in &config.accounts {
        if account.allowed_folders.as_ref().is_some_and(Vec::is_empty) {
            bail!(
                "Account \"{}\" has an empty `allowed_folders` list — omit the field to allow \
                 all folders, or list the folders you want to expose",
                account.name
            );
        }
    }

    // Same rationale for `allowed_attachment_dirs = []`: if empty,
    // `download_attachment` would silently fall back to the default dir
    // (outside the user's explicit opt-out), and `draft_*` would reject
    // every attachment anyway. Omit the field to get the default; don't
    // provide an empty list.
    if config.allowed_attachment_dirs.is_empty() {
        bail!(
            "`allowed_attachment_dirs = []` is invalid — omit the field for the default \
             (XDG_RUNTIME_DIR), or list the dirs from which attachments may be read"
        );
    }

    tracing::info!("Loaded config from {}", config_path.display());
    for account in &config.accounts {
        tracing::info!(
            name = %account.name,
            host = %account.host,
            user = %account.username,
            read_only = account.read_only,
            "Account configured"
        );
    }

    Ok(config)
}

fn find_config_file() -> Option<PathBuf> {
    // Deliberately do NOT search CWD: if the server is launched from a
    // user-chosen directory, an attacker with write access to that directory
    // could drop a `config.toml` with their own OAuth refresh tokens and
    // have the MCP server read the attacker's mailbox. To opt in to a CWD
    // config, the user must pass `--config ./config.toml` or set
    // `IMAP_MCP_CONFIG=./config.toml` explicitly.
    let candidates = [
        dirs::config_dir().map(|d| d.join("imap-mcp-rs").join("config.toml")),
        Some(PathBuf::from("/etc/imap-mcp-rs/config.toml")),
    ];

    candidates.into_iter().flatten().find(|p| p.exists())
}
