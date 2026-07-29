//! `OAuth2` refresh-token flow for Gmail and Outlook 365.
//!
//! Exchanges a long-lived refresh token (held in the token state file) for a
//! short-lived access token, which the IMAP layer then sends as `XOAUTH2`.
//! We talk HTTPS directly — no HTTP client crate dependency.
//!
//! Providers with refresh-token rotation (Microsoft Entra) return a NEW
//! refresh token with every grant; only using the new one extends its sliding
//! inactivity window (90 days by default). The rotated token is surfaced via
//! [`RefreshOutcome::rotated_refresh_token`] — callers must persist it (see
//! [`crate::token_state`]) or the account deterministically dies 90 days
//! after the initial authorization (`AADSTS700082`).

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
    /// Rotated refresh token. Entra sends one on every grant; Gmail omits it
    /// (Google refresh tokens are static). Absent ≠ error.
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Error payload of a non-2xx token-endpoint response (RFC 6749 §5.2).
/// Parsed selectively: only `error` and `error_description` are ever
/// extracted — the raw body is never embedded in errors or logs.
#[derive(Debug, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// Structured token-endpoint failure. Carries the `OAuth2` `error` code and a
/// trimmed, control-char-free `error_description` so `account_health` can
/// show the operator the real reason (e.g. `AADSTS700082` = token expired
/// from inactivity) instead of a bare HTTP status.
#[derive(Debug)]
pub struct OAuth2Error {
    pub http_status: u16,
    pub code: Option<String>,
    pub description: Option<String>,
}

impl OAuth2Error {
    /// `invalid_grant` means the refresh token itself is dead (expired,
    /// revoked, or rotated out of its family). Retrying with the same token
    /// is pointless — the caller re-reads the state file once, in case a
    /// parallel process has rotated it since, and otherwise points the
    /// operator at `imap-mcp-rs reauth`.
    pub fn is_invalid_grant(&self) -> bool {
        self.code.as_deref() == Some("invalid_grant")
    }

    /// `invalid_client` points at the app credentials, not the token: an
    /// expired or mistyped `client_secret`, a client id that no longer
    /// exists, or credentials of the wrong shape for the registration (see
    /// `client_type_mismatch`). Re-running `reauth` on its own never
    /// helps — it presents the very same credentials — so the fix is always a
    /// config or app-registration edit first.
    pub fn is_invalid_client(&self) -> bool {
        self.code.as_deref() == Some("invalid_client")
    }

    /// Whether the client authenticated with the wrong *shape* of credentials
    /// for how the app is registered, rather than with bad ones.
    ///
    /// Both arrive as `invalid_client`, so the generic "your secret expired"
    /// advice would send the operator down the wrong path entirely. Matched on
    /// the AADSTS code, which the provider states verbatim in `description`.
    fn client_type_mismatch(&self) -> Option<&'static str> {
        let description = self.description.as_deref()?;
        if description.contains("AADSTS700025") {
            // Sent a secret to an app registered as a public client.
            Some(
                "the app is registered as a public client (its redirect URI sits under \
                 `publicClient` in the Entra manifest), which must authenticate without one — \
                 remove `client_secret` from the account's `[accounts.oauth2]` section. The \
                 authorization code is protected by PKCE instead",
            )
        } else if description.contains("AADSTS7000218") {
            // Withheld a secret from an app registered as confidential.
            Some(
                "the app is registered as a confidential client and requires a secret — set \
                 `client_secret` in the account's `[accounts.oauth2]` section, or move the \
                 redirect URI to `publicClient` in the Entra manifest to run without one",
            )
        } else {
            None
        }
    }

    /// What the operator has to do about this failure, when we can tell.
    /// `account` names the account so the hint can be copy-pasted.
    pub fn remedy(&self, account: &str) -> Option<String> {
        if self.is_invalid_grant() {
            Some(format!(
                "run `imap-mcp-rs reauth {account}` to re-authorize"
            ))
        } else if self.is_invalid_client() {
            Some(self.client_type_mismatch().map_or_else(
                || {
                    "the app credentials were rejected — client secrets expire (Entra: max 24 \
                     months); create a new one in the app registration and update `client_secret` \
                     in the config. `reauth` cannot fix this, it needs the same credentials"
                        .to_string()
                },
                ToString::to_string,
            ))
        } else {
            None
        }
    }
}

impl std::fmt::Display for OAuth2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "OAuth2 token endpoint returned HTTP {}",
            self.http_status
        )?;
        if let Some(code) = &self.code {
            write!(f, " ({code})")?;
        }
        if let Some(desc) = &self.description {
            write!(f, ": {desc}")?;
        }
        Ok(())
    }
}

impl std::error::Error for OAuth2Error {}

/// Build an [`OAuth2Error`] from a non-2xx token-endpoint response.
/// Extracts only the two RFC 6749 error fields; the description is cut
/// before the provider's `Trace ID` noise, stripped of control characters
/// and capped, so it is safe to log and to surface through `last_error`.
pub fn parse_error_body(http_status: u16, body: &str) -> OAuth2Error {
    let parsed: ErrorBody = serde_json::from_str(body).unwrap_or(ErrorBody {
        error: None,
        error_description: None,
    });
    let description = parsed.error_description.map(|d| {
        let cut = d.split(" Trace ID").next().unwrap_or(&d);
        cut.chars()
            .filter(|c| !c.is_control())
            .take(300)
            .collect::<String>()
    });
    OAuth2Error {
        http_status,
        code: parsed.error,
        description,
    }
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

/// Result of a successful refresh: the access token to present as `XOAUTH2`,
/// plus the rotated refresh token when the provider issued one.
#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    pub access: AccessToken,
    /// `Some` when the provider rotated the refresh token. The caller must
    /// adopt it for subsequent refreshes and persist it — see module docs.
    pub rotated_refresh_token: Option<String>,
}

/// Exchange `refresh_token` for a fresh access token. The token is passed
/// explicitly (not read from `config`) because it is server-managed state
/// that follows provider-side rotation — the config only supplies the app
/// credentials.
///
/// Non-2xx responses become a typed [`OAuth2Error`] (downcastable from the
/// returned `anyhow::Error`) so callers can distinguish a terminally dead
/// token (`invalid_grant`) from transient endpoint trouble.
pub async fn refresh_access_token(
    config: &OAuth2Config,
    refresh_token: &str,
) -> Result<RefreshOutcome> {
    let token_url = config.token_url()?;
    let client_id = config
        .client_id
        .as_deref()
        .context("OAuth2 client_id not configured")?;
    // Optional — see `client_secret_param`. A public-client registration must
    // refresh without one, exactly as it exchanged the code without one.
    let body = format!(
        "grant_type=refresh_token&client_id={}&refresh_token={}{}",
        urlencoded(client_id),
        urlencoded(refresh_token),
        client_secret_param(config.client_secret.as_deref()),
    );

    let (status, response) =
        tokio::time::timeout(OAUTH2_TIMEOUT, minimal_https_post(&token_url, &body))
            .await
            .context("OAuth2 token refresh timed out")?
            .map_err(|e| {
                // Use `{e}` (not `{e:#}`) so the full chain — which can embed
                // server-returned body bytes — doesn't hit the stderr log.
                tracing::error!("OAuth2 HTTP request failed: {e}");
                e
            })
            .context("OAuth2 token refresh failed")?;

    if !(200..300).contains(&status) {
        // Error bodies carry no tokens; extracting the two RFC 6749 fields
        // (trimmed + control-stripped in `parse_error_body`) is safe and
        // turns "HTTP 400" into an actionable AADSTS reason.
        let err = parse_error_body(status, &response);
        tracing::error!("OAuth2 token refresh rejected: {err}");
        return Err(anyhow::Error::new(err));
    }

    // Do NOT embed the response body in the error message: a success-shaped
    // but unparseable response could contain the access_token itself, and
    // anyhow error chains get surfaced through tool responses + tracing.
    let token_response: TokenResponse = serde_json::from_str(&response)
        .context("Failed to parse OAuth2 token response (body omitted from log)")?;

    // Default to 30 minutes if the server omits `expires_in` — conservative
    // compared to Gmail/Outlook's typical 3600s, prevents stale-token reuse.
    let ttl = Duration::from_secs(token_response.expires_in.unwrap_or(1800));
    // A provider echoing back the identical token is not a rotation — treat
    // as no-op so callers don't rewrite the state file on every refresh. An
    // empty value is not a token either: adopting one would leave the account
    // without any usable credential until the next restart.
    let rotated_refresh_token = token_response
        .refresh_token
        .filter(|new| !new.is_empty() && new != refresh_token);
    tracing::debug!(
        expires_in_secs = ttl.as_secs(),
        rotated = rotated_refresh_token.is_some(),
        "OAuth2 access token refreshed"
    );
    Ok(RefreshOutcome {
        access: AccessToken {
            token: token_response.access_token,
            expires_at: std::time::Instant::now() + ttl,
        },
        rotated_refresh_token,
    })
}

/// Render the optional `client_secret` form field, ready to append to a token
/// request body.
///
/// Confidential clients must send it; public clients must not — Entra rejects
/// a secret on an app whose redirect URI sits under `publicClient` with
/// AADSTS700025, and omitting it on a confidential app fails with
/// AADSTS7000218. Neither the client nor this crate can tell the two apart, so
/// the presence of `client_secret` in the config decides.
pub fn client_secret_param(secret: Option<&str>) -> String {
    secret.map_or_else(String::new, |s| format!("&client_secret={}", urlencoded(s)))
}

pub fn urlencoded(s: &str) -> String {
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

/// Raw HTTPS POST returning `(status, body)`. Status handling is the
/// caller's job — `refresh_access_token` and the `reauth` code exchange
/// interpret non-2xx differently (typed [`OAuth2Error`] vs. CLI guidance).
pub async fn minimal_https_post(url: &str, body: &str) -> Result<(u16, String)> {
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

    // Extract body (after \r\n\r\n)
    let body_start = response
        .find("\r\n\r\n")
        .context("Invalid HTTP response: no header/body separator")?
        + 4;
    Ok((status_code, response[body_start..].to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_response_with_rotation() {
        let r: TokenResponse = serde_json::from_str(
            r#"{"access_token":"at","expires_in":3600,"refresh_token":"rt-new"}"#,
        )
        .unwrap();
        assert_eq!(r.access_token, "at");
        assert_eq!(r.refresh_token.as_deref(), Some("rt-new"));
    }

    #[test]
    fn token_response_without_rotation_is_fine() {
        // Gmail: no refresh_token in the response — must not fail.
        let r: TokenResponse =
            serde_json::from_str(r#"{"access_token":"at","expires_in":3599}"#).unwrap();
        assert!(r.refresh_token.is_none());
    }

    #[test]
    fn error_body_extracts_code_and_trims_description() {
        let e = parse_error_body(
            400,
            r#"{"error":"invalid_grant","error_description":"AADSTS700082: The refresh token has expired due to inactivity. Trace ID: abc Correlation ID: def"}"#,
        );
        assert_eq!(e.http_status, 400);
        assert!(e.is_invalid_grant());
        let desc = e.description.clone().unwrap();
        assert!(desc.starts_with("AADSTS700082"));
        assert!(!desc.contains("Trace ID"));
        assert_eq!(
            format!("{e}"),
            format!("OAuth2 token endpoint returned HTTP 400 (invalid_grant): {desc}")
        );
    }

    #[test]
    fn remedy_distinguishes_dead_token_from_dead_client_secret() {
        let token_dead = parse_error_body(400, r#"{"error":"invalid_grant"}"#);
        assert_eq!(
            token_dead.remedy("Office").unwrap(),
            "run `imap-mcp-rs reauth Office` to re-authorize"
        );

        // A dead client secret is the one case where reauth does NOT help —
        // the flow needs those very credentials.
        let client_dead = parse_error_body(401, r#"{"error":"invalid_client"}"#);
        let remedy = client_dead.remedy("Office").unwrap();
        assert!(remedy.contains("client_secret"));
        assert!(remedy.contains("cannot fix this"));

        // Unknown/transient errors get no invented advice.
        assert!(parse_error_body(503, "{}").remedy("Office").is_none());
    }

    /// Both mismatches arrive as `invalid_client`; without the AADSTS code the
    /// operator would be told to renew a secret that is either unwanted or
    /// missing. Each must point the opposite way.
    #[test]
    fn remedy_tells_the_two_client_type_mismatches_apart() {
        let too_much = parse_error_body(
            401,
            r#"{"error":"invalid_client","error_description":"AADSTS700025: Client is public so neither 'client_assertion' nor 'client_secret' should be presented."}"#,
        );
        let remedy = too_much.remedy("Office").unwrap();
        assert!(remedy.contains("remove `client_secret`"), "{remedy}");
        assert!(!remedy.contains("secrets expire"), "{remedy}");

        let too_little = parse_error_body(
            401,
            r#"{"error":"invalid_client","error_description":"AADSTS7000218: The request body must contain the following parameter: 'client_assertion' or 'client_secret'."}"#,
        );
        let remedy = too_little.remedy("Office").unwrap();
        assert!(remedy.contains("requires a secret"), "{remedy}");
        assert!(!remedy.contains("secrets expire"), "{remedy}");
    }

    #[test]
    fn client_secret_param_is_omitted_entirely_when_absent() {
        assert_eq!(client_secret_param(None), "");
        assert_eq!(
            client_secret_param(Some("s3c/ret")),
            "&client_secret=s3c%2Fret"
        );
    }

    #[test]
    fn error_body_unparseable_still_yields_status() {
        let e = parse_error_body(502, "<html>Bad Gateway</html>");
        assert_eq!(e.http_status, 502);
        assert!(e.code.is_none());
        assert!(!e.is_invalid_grant());
    }

    #[test]
    fn error_description_strips_control_chars_and_caps() {
        let long = format!("{}\r\nX{}", "A".repeat(10), "B".repeat(500));
        let body = serde_json::json!({"error":"server_error","error_description": long});
        let e = parse_error_body(500, &body.to_string());
        let desc = e.description.unwrap();
        assert!(!desc.contains('\r') && !desc.contains('\n'));
        assert!(desc.chars().count() <= 300);
    }
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
