//! TOML config loading and validation.
//!
//! Supports multiple `[[accounts]]` blocks, each with either password or
//! `OAuth2` (Gmail / Outlook 365) auth. Per-account `read_only` / `allow_move` /
//! `allow_delete` / `allow_flag_change` switches act as a hard safety gate
//! independent of the configured `allowed_folders` whitelist.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub accounts: Vec<AccountConfig>,
    /// Filesystem directories from which attachment files may be read.
    /// Defaults to the directory `download_attachment` saves into (see
    /// [`default_attachment_dir`]: `$XDG_RUNTIME_DIR`, then the user cache
    /// dir, then `/tmp/imap-mcp-rs-$USER`). Paths outside these directories
    /// are rejected — this prevents a prompt-injected LLM from attaching
    /// arbitrary local files like SSH keys or /etc/passwd. Symlinks are
    /// resolved via `canonicalize`.
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

/// Create the attachment directory if needed and make sure it is private: a
/// real directory (no symlink) of this process's own user, at mode 0700.
///
/// `download_attachment` writes into it, `draft_*` reads from it and the
/// startup prune deletes in it, so whoever controls it could read every
/// download or swap a file between the download and the draft that attaches
/// it. The checks therefore run twice: before creating (a planted symlink
/// must not be created and chmod-ed *through*) and after the chmod. Then a
/// mode that stayed open means others can reach the directory, and an owner
/// other than this process means someone else controls it even when it is
/// private to them: they can open it up again at any time. The mode check
/// alone let such a directory through. On a `/tmp` fallback another user can
/// pre-create exactly that.
pub async fn prepare_attachment_dir(dir: &Path) -> Result<()> {
    let refuse_symlink = |meta: &fs::Metadata| -> Result<()> {
        if meta.file_type().is_symlink() {
            bail!(
                "attachment dir \"{}\" is a symlink — refusing to use it; remove it or change \
                 `allowed_attachment_dirs` in the config",
                dir.display()
            );
        }
        Ok(())
    };
    // Only "does not exist yet" passes without a verdict: swallowing other
    // stat errors (a transiently unreadable parent) would skip the check.
    match tokio::fs::symlink_metadata(dir).await {
        Ok(meta) => refuse_symlink(&meta)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => bail!(
            "cannot stat attachment dir \"{}\": {e} — refusing to proceed without symlink \
             verification",
            dir.display()
        ),
    }
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("cannot create attachment dir \"{}\"", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Not checked here: on a directory someone else owns this fails,
        // and the checks below refuse it.
        let _ = tokio::fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).await;
    }

    let meta = tokio::fs::symlink_metadata(dir)
        .await
        .with_context(|| format!("cannot stat attachment dir \"{}\"", dir.display()))?;
    refuse_symlink(&meta)?;
    if !meta.is_dir() {
        bail!("attachment dir \"{}\" is not a directory", dir.display());
    }
    #[cfg(unix)]
    {
        // SAFETY: `geteuid` takes no arguments, cannot fail and touches no
        // memory of ours.
        let euid = unsafe { libc::geteuid() };
        refuse_foreign(dir, &meta, euid)?;
    }
    Ok(())
}

/// The checks after the chmod: the directory must be closed to others and
/// belong to `euid`, the user this process runs as.
#[cfg(unix)]
fn refuse_foreign(dir: &Path, meta: &fs::Metadata, euid: u32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "attachment dir \"{}\" is accessible to other users (mode {mode:04o}) and could \
             not be restricted to 0700 — it is probably owned by someone else. Point \
             `allowed_attachment_dirs` at a directory of your own",
            dir.display()
        );
    }
    if meta.uid() != euid {
        bail!(
            "attachment dir \"{}\" belongs to another user (uid {}), who could read and replace \
             every download in it. Point `allowed_attachment_dirs` at a directory of your own",
            dir.display(),
            meta.uid()
        );
    }
    Ok(())
}

/// One `[[accounts]]` block. A key it does not know is an error: ignored, a
/// misspelled `read_only` or `allow_delete` left the account writable or
/// deletable, the opposite of what the line says. Same reasoning as the
/// strict command line (`main.rs`).
#[allow(clippy::struct_excessive_bools)]
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
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
    /// Plain-text signature for the text/plain MIME part of drafts. When
    /// unset, a text rendering is derived from `signature_html` so both MIME
    /// parts carry the signature — desktop clients include it in both, and a
    /// draft whose text part lacks it is distinguishable from a hand-written
    /// one.
    pub signature_text: Option<String>,
    /// Domain used in generated `Message-ID` headers (`<random@domain>`).
    /// Defaults to the domain of the sender address. Without an explicit
    /// Message-ID the MIME builder would invent one on `localhost` — marking
    /// the draft as machine-generated — and with its default `gethostname`
    /// feature (disabled in Cargo.toml) on the machine's own name, leaking
    /// it into every draft.
    pub message_id_domain: Option<String>,
    /// Locale for draft formatting: "en" or "de". Controls labels (From/Von),
    /// date format, font, and reply/forward prefixes. Defaults to "en".
    pub locale: Option<String>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default = "default_true")]
    pub allow_delete: bool,
    #[serde(default = "default_true")]
    pub allow_move: bool,
    /// `false` blocks `mark_as_read` / `mark_as_unread` / `flag_email` /
    /// `unflag_email`. The unread state is often a human's work queue: an
    /// accidental bulk `mark_as_read` erases it with no trash to recover from
    /// and no record of which messages it hit — for such setups this is the
    /// most destructive operation that stays enabled, so it gets the same
    /// per-account gate the others have.
    #[serde(default = "default_true")]
    pub allow_flag_change: bool,
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

    /// Domain for generated `Message-ID` headers: the configured
    /// `message_id_domain`, otherwise the domain of the sender address,
    /// otherwise the RFC 2606 reserved `.invalid` TLD (never a real host).
    /// An empty config value counts as unset — honouring it would mint
    /// malformed ids like `<uuid@>` in Message-ID and Content-ID headers.
    pub fn message_id_domain(&self) -> &str {
        if let Some(domain) = self.message_id_domain.as_deref()
            && !domain.is_empty()
        {
            return domain;
        }
        match self.sender_address().rsplit_once('@') {
            Some((_, domain)) if !domain.is_empty() => domain,
            _ => "message-id.invalid",
        }
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

/// `[accounts.oauth2]`, strict for the same reason as the account block.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
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
    // No `.env` loading: a dotenv file in the working directory (or any parent)
    // could set `IMAP_MCP_CONFIG` — or the XDG variables that place the token
    // state and the attachment directory — and so inject exactly the
    // attacker-chosen config `find_config_file` refuses to pick up from there.
    let config_path = config_path(path, env::var_os("IMAP_MCP_CONFIG"))?;

    let content = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;

    warn_if_not_private(&config_path);

    let config: ServerConfig = toml::from_str(&content)
        .map_err(|e| anyhow::anyhow!(toml_error(&e, &content)))
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
    // Nothing expands `~`: `"~/Downloads"` made the server create a literal
    // `~` directory in whatever directory the MCP client started it in, and
    // save downloads there. Refused. Other relative paths resolve against
    // that same directory, which is fragile but can be deliberate (a service
    // with a fixed working directory), so they only draw a warning.
    for dir in &config.allowed_attachment_dirs {
        if dir.starts_with('~') {
            bail!(
                "`allowed_attachment_dirs` entry \"{dir}\" starts with `~`, which is not \
                 expanded — write the full path (e.g. /home/you/Downloads)"
            );
        }
        if !Path::new(dir).is_absolute() {
            tracing::warn!(
                dir = %dir,
                "allowed_attachment_dirs entry is relative — it resolves against the directory \
                 the MCP client starts the server in"
            );
        }
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

/// A TOML error without the line it quotes. toml's own rendering prints the
/// offending line, and in the config that may hold a password or a client
/// secret, in the token state a refresh token: printed at startup or logged,
/// it reached whatever keeps the server's stderr. Position and message are
/// enough to find the mistake.
pub(crate) fn toml_error(e: &toml::de::Error, input: &str) -> String {
    let Some(before) = e.span().and_then(|span| input.get(..span.start)) else {
        return e.message().to_string();
    };
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    format!("line {line}, column {column}: {}", e.message())
}

/// The config file to read: `--config`, else `IMAP_MCP_CONFIG`, else the
/// standard locations. An empty variable counts as unset, as with most
/// variables; it used to end in "Failed to read config file: " with no path.
/// Read as an `OsString`: a path that is not UTF-8 was skipped silently, and
/// the server started on the default config instead of the one named.
fn config_path(path: Option<&str>, from_env: Option<std::ffi::OsString>) -> Result<PathBuf> {
    if let Some(p) = path {
        return Ok(PathBuf::from(p));
    }
    if let Some(p) = from_env.filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    find_config_file().context(missing_config_help())
}

/// First-run guidance. A bare "no config found" leaves a new user guessing
/// where the file goes and what belongs in it — and inside an MCP client the
/// only visible symptom is a server that refuses to start. So name the exact
/// paths that were searched, show the smallest config that works, and point
/// at the one command OAuth accounts need.
fn missing_config_help() -> String {
    let user_path = dirs::config_dir().map_or_else(
        || "~/.config/imap-mcp-rs/config.toml".to_string(),
        |d| {
            d.join("imap-mcp-rs")
                .join("config.toml")
                .display()
                .to_string()
        },
    );
    format!(
        "No config file found.\n\n\
         Searched (in order):\n  \
         1. --config <path>\n  \
         2. $IMAP_MCP_CONFIG\n  \
         3. {user_path}\n  \
         4. /etc/imap-mcp-rs/config.toml\n\n\
         Create {user_path} — minimal example:\n\n\
         [[accounts]]\n\
         name = \"Personal\"\n\
         host = \"imap.example.com\"\n\
         port = 993\n\
         username = \"you@example.com\"\n\
         auth_method = \"password\"\n\
         password = \"your-app-specific-password\"\n\n\
         Then `chmod 600` it — it holds credentials.\n\
         For OAuth2 (Outlook 365 / Gmail) set auth_method = \"oauth2\" with an\n\
         [accounts.oauth2] section and run `imap-mcp-rs reauth <account>` once;\n\
         no token belongs in the config. See the README for the full reference."
    )
}

/// Warn when the config is readable beyond its owner. It holds passwords and
/// `OAuth2` client secrets, so the common `0644` default (e.g. a file created
/// with a plain editor) hands them to every local account. Advisory only —
/// refusing to start would be worse than running with a warning.
#[cfg(unix)]
fn warn_if_not_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = fs::metadata(path) else { return };
    let mode = meta.permissions().mode() & 0o077;
    if mode != 0 {
        tracing::warn!(
            path = %path.display(),
            mode = format!("{:04o}", meta.permissions().mode() & 0o7777),
            "Config is readable by other users but contains credentials — run `chmod 600` on it"
        );
    }
}

#[cfg(not(unix))]
fn warn_if_not_private(_path: &std::path::Path) {}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_help_names_paths_and_next_steps() {
        let help = missing_config_help();
        // The three things a first-run user needs: where it looks, what a
        // working file contains, and how OAuth accounts get their token.
        assert!(help.contains("IMAP_MCP_CONFIG"));
        assert!(help.contains("/etc/imap-mcp-rs/config.toml"));
        assert!(help.contains("[[accounts]]"));
        assert!(help.contains("auth_method"));
        assert!(help.contains("chmod 600"));
        assert!(help.contains("reauth"));
        // …and no token in the config.
        assert!(help.contains("no token belongs in the config"));
    }

    /// A broken line must not reach the log with its content: in the config
    /// it may be the password line.
    #[test]
    fn config_errors_name_the_position_not_the_line() {
        let scratch = Scratch::new();
        let path = scratch.0.join("config.toml");
        fs::write(
            &path,
            "[[accounts]]\nname = \"A\"\nhost = \"h\"\nusername = \"u@h\"\n\
             password = \"geheim-123\n",
        )
        .unwrap();
        let err = format!("{:#}", load_config(path.to_str()).unwrap_err());
        assert!(!err.contains("geheim-123"), "{err}");
        assert!(err.contains("line 5"), "{err}");
        assert!(err.contains("Failed to parse config file"), "{err}");
    }

    /// `--config` wins, then `IMAP_MCP_CONFIG` when it names a path; an empty
    /// value counts as unset, and a path that is not UTF-8 is still a path.
    #[test]
    fn the_config_path_comes_from_the_argument_then_the_variable() {
        use std::ffi::OsString;
        assert_eq!(
            config_path(Some("/a.toml"), Some(OsString::from("/b.toml"))).unwrap(),
            PathBuf::from("/a.toml")
        );
        assert_eq!(
            config_path(None, Some(OsString::from("/b.toml"))).unwrap(),
            PathBuf::from("/b.toml")
        );
        // Empty: the standard locations, which may or may not hold a file
        // on this machine, but never the empty path.
        let empty = config_path(None, Some(OsString::new()));
        assert!(
            !matches!(&empty, Ok(p) if p.as_os_str().is_empty()),
            "{empty:?}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let raw = OsString::from_vec(b"/tmp/\xff.toml".to_vec());
            assert_eq!(
                config_path(None, Some(raw.clone())).unwrap(),
                PathBuf::from(raw)
            );
        }
    }

    /// `~` is not expanded, so an entry starting with it is refused at load
    /// instead of turning into a literal `~` directory. Other relative paths
    /// are fragile but can be deliberate; they still load.
    #[test]
    fn attachment_dirs_refuse_an_unexpanded_tilde() {
        let scratch = Scratch::new();
        let path = scratch.0.join("config.toml");
        let write = |dirs: &str| {
            fs::write(
                &path,
                format!(
                    "allowed_attachment_dirs = {dirs}\n\n[[accounts]]\nname = \"A\"\n\
                     host = \"h\"\nusername = \"u@h\"\npassword = \"p\"\n"
                ),
            )
            .unwrap();
        };
        for bad in [r#"["~/Downloads"]"#, r#"["/tmp/ok", "~"]"#] {
            write(bad);
            let err = load_config(path.to_str())
                .expect_err("tilde entry accepted")
                .to_string();
            assert!(err.contains("starts with `~`"), "{bad}: {err}");
        }
        write(r#"["/tmp/ok", "relative/dir"]"#);
        assert!(load_config(path.to_str()).is_ok());
    }

    /// A misspelled permission switch must not leave the account writable:
    /// an unknown key in an account block is an error naming it, in the
    /// `OAuth2` section as well.
    #[test]
    fn unknown_account_keys_are_refused() {
        let account = |extra: &str| {
            format!(
                "[[accounts]]\nname = \"A\"\nhost = \"h\"\nusername = \"u@h\"\npassword = \"p\"\n\
                 {extra}\n"
            )
        };
        for (extra, key) in [
            ("read_onyl = true", "read_onyl"),
            ("readonly = true", "readonly"),
            ("allow-delete = false", "allow-delete"),
            (
                "auth_method = \"oauth2\"\n[accounts.oauth2]\nclient-id = \"x\"",
                "client-id",
            ),
        ] {
            let err = toml::from_str::<ServerConfig>(&account(extra))
                .expect_err(extra)
                .to_string();
            assert!(err.contains(key), "{extra}: {err}");
        }
        // Every documented key still loads.
        let full = account(
            "email = \"u@h\"\ndisplay_name = \"U\"\nsignature_html = \"<p>s</p>\"\n\
             signature_text = \"s\"\nmessage_id_domain = \"h\"\nlocale = \"de\"\nport = 993\n\
             read_only = true\nallow_delete = false\nallow_move = false\n\
             allow_flag_change = false\naccept_invalid_certs = false\n\
             allow_unsafe_expunge = false\nallowed_folders = [\"INBOX\"]\n\
             auth_method = \"oauth2\"\n[accounts.oauth2]\nprovider = \"outlook365\"\n\
             client_id = \"c\"\nclient_secret = \"s\"\nrefresh_token = \"r\"\ntenant = \"t\"\n\
             token_url = \"https://h/t\"",
        );
        toml::from_str::<ServerConfig>(&full).expect("every documented key");
    }

    /// `tool_set` chose between two tool surfaces before there was one for
    /// every client. A config that still carries it must keep loading.
    #[test]
    fn a_leftover_tool_set_key_is_ignored() {
        let account =
            "[[accounts]]\nname = \"A\"\nhost = \"h\"\nusername = \"u@h\"\npassword = \"p\"\n";
        assert!(
            toml::from_str::<ServerConfig>(&format!("tool_set = \"voice\"\n{account}")).is_ok()
        );
    }

    #[test]
    fn token_url_matches_configured_provider() {
        let outlook: OAuth2Config = toml::from_str(
            r#"
            provider = "outlook365"
            tenant = "tid"
            "#,
        )
        .unwrap();
        assert_eq!(
            outlook.token_url().unwrap(),
            "https://login.microsoftonline.com/tid/oauth2/v2.0/token"
        );

        let gmail: OAuth2Config = toml::from_str(r#"provider = "gmail""#).unwrap();
        assert_eq!(
            gmail.token_url().unwrap(),
            "https://oauth2.googleapis.com/token"
        );

        // Custom without an explicit URL is a config error, not a guess.
        let custom: OAuth2Config = toml::from_str(r#"provider = "custom""#).unwrap();
        assert!(custom.token_url().is_err());
    }

    fn account_toml(extra: &str) -> AccountConfig {
        toml::from_str(&format!(
            "name = \"Test\"\n\
             host = \"imap.example.com\"\n\
             username = \"user@example.com\"\n\
             password = \"x\"\n\
             {extra}"
        ))
        .unwrap()
    }

    #[test]
    fn message_id_domain_defaults_to_sender_domain() {
        let account = account_toml("");
        assert_eq!(account.message_id_domain(), "example.com");
        // `email` overrides `username` for the sender address — and with it
        // the derived Message-ID domain.
        let account = account_toml("email = \"other@example.org\"\n");
        assert_eq!(account.message_id_domain(), "example.org");
    }

    #[test]
    fn message_id_domain_explicit_config_wins() {
        let account = account_toml("message_id_domain = \"mail.example.net\"\n");
        assert_eq!(account.message_id_domain(), "mail.example.net");
    }

    #[test]
    fn message_id_domain_treats_empty_config_value_as_unset() {
        // Honouring `message_id_domain = ""` would mint malformed ids like
        // `<uuid@>` in every Message-ID and Content-ID header.
        let account = account_toml("message_id_domain = \"\"\n");
        assert_eq!(account.message_id_domain(), "example.com");
    }

    #[test]
    fn message_id_domain_falls_back_to_invalid_tld() {
        // A username without an @ (some providers use bare logins) must not
        // produce an empty domain — the RFC 2606 `.invalid` TLD marks it.
        let account: AccountConfig = toml::from_str(
            "name = \"Test\"\n\
             host = \"imap.example.com\"\n\
             username = \"bareuser\"\n\
             password = \"x\"\n",
        )
        .unwrap();
        assert_eq!(account.message_id_domain(), "message-id.invalid");
    }

    /// Scratch base for the attachment-dir tests; removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "imap-mcp-rs-attdir-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prepare_attachment_dir_creates_and_tightens_to_0700() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;

        let fresh = scratch.0.join("nested/attachments");
        prepare_attachment_dir(&fresh).await.unwrap();
        assert_eq!(mode(&fresh), 0o700);

        // An existing directory of our own that is open gets restricted.
        let open = scratch.0.join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
        prepare_attachment_dir(&open).await.unwrap();
        assert_eq!(mode(&open), 0o700);
    }

    /// A directory private to another user passes the mode check, yet its
    /// owner can open it again at any time: refused by its owner.
    #[cfg(unix)]
    #[test]
    fn a_private_directory_of_another_user_is_refused() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let scratch = Scratch::new();
        let dir = scratch.0.join("downloads");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let meta = fs::symlink_metadata(&dir).unwrap();
        refuse_foreign(&dir, &meta, meta.uid()).expect("the owner's own directory");
        let err = refuse_foreign(&dir, &meta, meta.uid().wrapping_add(1))
            .expect_err("another user's private directory")
            .to_string();
        assert!(err.contains("belongs to another user"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prepare_attachment_dir_refuses_symlinks_and_files() {
        let scratch = Scratch::new();
        let target = scratch.0.join("elsewhere");
        fs::create_dir(&target).unwrap();
        let link = scratch.0.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = prepare_attachment_dir(&link).await.unwrap_err().to_string();
        assert!(err.contains("symlink"), "{err}");

        let file = scratch.0.join("file");
        fs::write(&file, b"x").unwrap();
        assert!(prepare_attachment_dir(&file).await.is_err());
    }
}
