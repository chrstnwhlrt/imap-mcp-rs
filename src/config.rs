use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    #[serde(default)]
    pub account: AccountConfig,
    pub imap: ImapConfig,
    #[serde(default)]
    pub auth: AuthConfig,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct AccountConfig {
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ImapConfig {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    /// Email address used as From in drafts. Defaults to username if not set.
    pub email: Option<String>,
    pub allowed_folders: Option<Vec<String>>,
    #[serde(default)]
    pub accept_invalid_certs: bool,
}

impl ImapConfig {
    /// The email address to use as From in outgoing drafts.
    pub fn sender_address(&self) -> &str {
        self.email.as_deref().unwrap_or(&self.username)
    }
}

fn default_port() -> u16 {
    993
}

#[derive(Deserialize, Clone)]
pub struct AuthConfig {
    #[serde(default = "default_auth_method")]
    pub method: AuthMethod,
    pub password: Option<String>,
    #[serde(default)]
    pub oauth2: Option<OAuth2Config>,
}

impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("method", &self.method)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("oauth2", &self.oauth2)
            .finish()
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            method: AuthMethod::Password,
            password: None,
            oauth2: None,
        }
    }
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
                anyhow::bail!("token_url required for custom OAuth2 provider")
            }
        }
    }
}

impl ServerConfig {
    pub fn apply_env_overrides(&mut self) {
        if let Ok(v) = env::var("IMAP_HOST") {
            self.imap.host = v;
        }
        if let Ok(v) = env::var("IMAP_PORT")
            && let Ok(port) = v.parse()
        {
            self.imap.port = port;
        }
        if let Ok(v) = env::var("IMAP_USERNAME") {
            self.imap.username = v;
        }
        if let Ok(v) = env::var("IMAP_PASSWORD") {
            self.auth.password = Some(v);
        }

        // Create oauth2 section from env vars if any are set
        let env_client_id = env::var("OAUTH2_CLIENT_ID").ok();
        let env_client_secret = env::var("OAUTH2_CLIENT_SECRET").ok();
        let env_refresh_token = env::var("OAUTH2_REFRESH_TOKEN").ok();

        if env_client_id.is_some() || env_client_secret.is_some() || env_refresh_token.is_some() {
            let oauth2 = self.auth.oauth2.get_or_insert(OAuth2Config {
                provider: OAuth2Provider::Gmail,
                client_id: None,
                client_secret: None,
                refresh_token: None,
                tenant: None,
                token_url: None,
            });
            if let Some(v) = env_client_id {
                oauth2.client_id = Some(v);
            }
            if let Some(v) = env_client_secret {
                oauth2.client_secret = Some(v);
            }
            if let Some(v) = env_refresh_token {
                oauth2.refresh_token = Some(v);
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

    let mut config: ServerConfig = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config file: {}", config_path.display()))?;

    config.apply_env_overrides();

    tracing::info!("Loaded config from {}", config_path.display());
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
