use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub accounts: Vec<AccountConfig>,
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
    #[serde(default)]
    pub read_only: bool,
    #[serde(default = "default_true")]
    pub allow_delete: bool,
    #[serde(default = "default_true")]
    pub allow_move: bool,
    #[serde(default)]
    pub accept_invalid_certs: bool,
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

fn default_port() -> u16 {
    993
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    Password,
    OAuth2,
}

fn default_auth_method() -> AuthMethod {
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

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OAuth2Provider {
    Gmail,
    Outlook365,
    Custom,
}

fn default_provider() -> OAuth2Provider {
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
    let candidates = [
        Some(PathBuf::from("config.toml")),
        dirs::config_dir().map(|d| d.join("imap-mcp-rs").join("config.toml")),
        Some(PathBuf::from("/etc/imap-mcp-rs/config.toml")),
    ];

    candidates.into_iter().flatten().find(|p| p.exists())
}
