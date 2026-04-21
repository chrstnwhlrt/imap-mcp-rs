//! `OAuth2` refresh-token flow for Gmail and Outlook 365.
//!
//! Exchanges the long-lived refresh token (stored in config) for a short-lived
//! access token, which the IMAP layer then sends as `XOAUTH2`. We talk HTTPS
//! directly — no HTTP client crate dependency.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::config::OAuth2Config;

const OAUTH2_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Seconds until the access token expires. Gmail and Outlook 365 both
    /// return 3600 (1 hour). Treated as a hint — the IMAP server is the
    /// authoritative source of truth if we see an auth error.
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Access token with its absolute expiry deadline, used by callers to cache
/// across reconnects and avoid burning a 100-500ms HTTPS roundtrip to the
/// OAuth provider every time the IMAP session drops.
#[derive(Debug, Clone)]
pub struct AccessToken {
    pub token: String,
    pub expires_at: std::time::Instant,
}

impl AccessToken {
    /// Conservative expiry check: 1 minute of slack so we don't present a
    /// token that expires while in flight.
    pub fn is_valid(&self) -> bool {
        const SLACK: Duration = Duration::from_mins(1);
        self.expires_at > std::time::Instant::now() + SLACK
    }
}

/// Fetch a fresh access token. Returns the raw token plus its expiry deadline
/// so callers can cache and reuse it until expiry.
pub async fn refresh_access_token(config: &OAuth2Config) -> Result<AccessToken> {
    let token_url = config.token_url()?;
    let client_id = config
        .client_id
        .as_deref()
        .context("OAuth2 client_id not configured")?;
    let client_secret = config
        .client_secret
        .as_deref()
        .context("OAuth2 client_secret not configured")?;
    let refresh_token = config
        .refresh_token
        .as_deref()
        .context("OAuth2 refresh_token not configured")?;

    let body = format!(
        "grant_type=refresh_token&client_id={}&client_secret={}&refresh_token={}",
        urlencoded(client_id),
        urlencoded(client_secret),
        urlencoded(refresh_token),
    );

    let response = tokio::time::timeout(OAUTH2_TIMEOUT, minimal_https_post(&token_url, &body))
        .await
        .context("OAuth2 token refresh timed out")?
        .map_err(|e| {
            // Use `{e}` (not `{e:#}`) so the full chain — which can embed
            // server-returned body bytes — doesn't hit the stderr log.
            tracing::error!("OAuth2 HTTP request failed: {e}");
            e
        })
        .context("OAuth2 token refresh failed")?;

    // Do NOT embed the response body in the error message: a success-shaped
    // but unparseable response could contain the access_token itself, and
    // anyhow error chains get surfaced through tool responses + tracing.
    let token_response: TokenResponse = serde_json::from_str(&response)
        .context("Failed to parse OAuth2 token response (body omitted from log)")?;

    // Default to 30 minutes if the server omits `expires_in` — conservative
    // compared to Gmail/Outlook's typical 3600s, prevents stale-token reuse.
    let ttl = Duration::from_secs(token_response.expires_in.unwrap_or(1800));
    tracing::debug!(
        expires_in_secs = ttl.as_secs(),
        "OAuth2 access token refreshed"
    );
    Ok(AccessToken {
        token: token_response.access_token,
        expires_at: std::time::Instant::now() + ttl,
    })
}

fn urlencoded(s: &str) -> String {
    use std::fmt::Write;
    let mut result = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(b as char);
            }
            _ => {
                let _ = write!(result, "%{b:02X}");
            }
        }
    }
    result
}

async fn minimal_https_post(url: &str, body: &str) -> Result<String> {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;

    // Cap the buffer: a compromised token endpoint or TLS-terminating proxy
    // could stream gigabytes pre-EOF to OOM the process. Legitimate token
    // responses are <2 KB.
    const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

    let url_parsed: url_parts::UrlParts = url.parse().context("Invalid token URL")?;

    let tcp = TcpStream::connect(format!("{}:{}", url_parsed.host, url_parsed.port))
        .await
        .context("Failed to connect to OAuth2 token endpoint")?;

    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(tls_config));
    let domain = rustls::pki_types::ServerName::try_from(url_parsed.host.clone())?;
    let mut tls = connector.connect(domain, tcp).await?;

    let request = format!(
        "POST {} HTTP/1.1\r\n\
         Host: {}\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        url_parsed.path,
        url_parsed.host,
        body.len(),
        body
    );

    tls.write_all(request.as_bytes()).await?;
    tls.flush().await?;

    // Read response in chunks. Some servers (Microsoft) close the
    // connection without TLS close_notify, causing UnexpectedEof.
    // We read until EOF or error, keeping whatever data we received.
    let mut response_bytes = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match tls.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                response_bytes.extend_from_slice(&buf[..n]);
                if response_bytes.len() > MAX_RESPONSE_BYTES {
                    anyhow::bail!("OAuth2 response exceeded {MAX_RESPONSE_BYTES} bytes — aborting");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
    }
    let response = String::from_utf8_lossy(&response_bytes).to_string();

    // Extract HTTP status line
    let first_line = response.lines().next().unwrap_or("");
    let status_code: u16 = first_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    if !(200..300).contains(&status_code) {
        anyhow::bail!("OAuth2 token endpoint returned HTTP {status_code}");
    }

    // Extract body (after \r\n\r\n)
    let body_start = response
        .find("\r\n\r\n")
        .context("Invalid HTTP response: no header/body separator")?
        + 4;
    Ok(response[body_start..].to_string())
}

mod url_parts {
    use std::str::FromStr;

    pub struct UrlParts {
        pub host: String,
        pub port: u16,
        pub path: String,
    }

    impl FromStr for UrlParts {
        type Err = anyhow::Error;

        fn from_str(url: &str) -> Result<Self, Self::Err> {
            let url = url
                .strip_prefix("https://")
                .ok_or_else(|| anyhow::anyhow!("Only HTTPS URLs supported"))?;

            let (host_port, path) = url.split_once('/').unwrap_or((url, ""));
            let path = format!("/{path}");

            let (host, port) = if let Some((h, p)) = host_port.split_once(':') {
                (h.to_string(), p.parse()?)
            } else {
                (host_port.to_string(), 443)
            };

            // Reject control/CR/LF/whitespace in host + path. The `token_url`
            // from config is splatted into a raw HTTP request line AND the
            // Host header; without this check a malicious `custom` OAuth2
            // URL like `https://host/path\r\nX-Injected: ...` would splice
            // extra headers or smuggle a second request into the TLS session.
            let invalid = |c: char| c.is_control() || c == ' ' || c == '\t';
            if host.chars().any(invalid) {
                anyhow::bail!("token_url host contains invalid characters");
            }
            if path.chars().any(invalid) {
                anyhow::bail!("token_url path contains invalid characters");
            }

            Ok(Self { host, port, path })
        }
    }
}
