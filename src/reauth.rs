//! `imap-mcp-rs reauth <account>` — interactive `OAuth2` authorization-code
//! flow for (re-)obtaining a refresh token.
//!
//! Needed at initial setup and whenever every known refresh token is dead
//! (`invalid_grant`: revoked, or expired after 90 days of a rotation not
//! being persisted by older versions). The regular lifecycle keeps tokens
//! alive without this command — see [`crate::token_state`].
//!
//! Flow: spin up a loopback listener, print (and try to open) the provider's
//! authorization URL, wait for the browser redirect carrying `?code=…`,
//! exchange the code at the token endpoint (PKCE always; `client_secret`
//! only if the config carries one), and persist the refresh token in the
//! token state file. The config file is never touched — it holds the app
//! credentials, not tokens.
//!
//! Provider notes: the redirect URI is `http://127.0.0.1:<port>` and must be
//! registered on the app. Entra rejects an `http` loopback URI in the portal's
//! redirect-URI form, so it has to go into the app manifest under
//! `publicClient.redirectUris` (equivalently `replyUrlsWithType` with type
//! `InstalledClient`). `AADSTS50011` means that entry is missing.
//!
//! That registration also makes the app a public client, which must then
//! authenticate *without* a secret — see [`crate::oauth2::client_secret_param`].

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use aws_lc_rs::digest;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::{AccountConfig, AuthMethod, OAuth2Config, OAuth2Provider};
use crate::imap_client::ImapClient;
use crate::oauth2::{client_secret_param, minimal_https_post, parse_error_body, urlencoded};
use crate::token_state;

const DEFAULT_PORT: u16 = 8365;
/// How long we wait for the operator to finish the browser login.
const AUTHORIZATION_TIMEOUT: Duration = Duration::from_mins(5);
/// Per-connection budget for receiving a complete request head. Guards
/// against connections that are opened but never written to (speculative
/// browser preconnects, local port scanners).
const CONNECTION_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Redirect requests are tiny (`GET /?code=… HTTP/1.1` + browser headers).
const MAX_REQUEST_BYTES: usize = 16 * 1024;
/// Budget for the token endpoint round-trips (code exchange) and for the
/// post-authorization IMAP verification login.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(20);
/// Shown both on a usage error and on `reauth --help`.
const USAGE: &str =
    "usage: imap-mcp-rs reauth <account> [--config <path>] [--port <n>] [--no-browser]";

/// Authorization-code grant response. Only the refresh token matters here —
/// the access token is discarded; the server mints its own on connect.
#[derive(Deserialize)]
struct CodeExchangeResponse {
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Entry point of the `reauth` subcommand: resolve the account, run the
/// browser flow, persist the token and prove it works. Returns `Err` for
/// anything the operator has to act on — the message is what they see.
pub async fn run(args: &[String]) -> Result<()> {
    // Answered, not failed: print the usage and exit 0 rather than treating
    // the request as the parse error an unknown flag would be.
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let opts = CliOptions::parse(args)?;

    let config = crate::config::load_config(opts.config.as_deref())?;
    let account = config
        .accounts
        .iter()
        .find(|a| a.name.to_lowercase() == opts.account.to_lowercase())
        .with_context(|| {
            let names: Vec<&str> = config.accounts.iter().map(|a| a.name.as_str()).collect();
            format!(
                "No account named \"{}\" — configured accounts: {}",
                opts.account,
                names.join(", ")
            )
        })?;

    if !matches!(account.auth_method, AuthMethod::OAuth2) {
        bail!(
            "Account \"{}\" uses password auth — reauth only applies to auth_method = \"oauth2\"",
            account.name
        );
    }
    let oauth2 = account
        .oauth2
        .as_ref()
        .context("Account has no [accounts.oauth2] section")?;
    let client_id = oauth2
        .client_id
        .as_deref()
        .context("OAuth2 client_id not configured")?;
    // Optional: an app whose redirect URI is registered as a public client
    // (Entra: `publicClient.redirectUris`) must NOT send one — it rejects the
    // exchange with AADSTS700025. PKCE below is what secures that case.
    let client_secret = oauth2.client_secret.as_deref();

    let redirect_uri = loopback_redirect_uri(opts.port);
    let state = uuid::Uuid::new_v4().to_string();
    let (verifier, challenge) = pkce_pair()?;
    let url = authorize_url(oauth2, client_id, &redirect_uri, &state, &challenge)?;

    // Bind before printing the URL so the operator never opens a link whose
    // redirect target isn't listening yet. Loopback only.
    let listener = TcpListener::bind(("127.0.0.1", opts.port))
        .await
        .with_context(|| format!("Cannot listen on 127.0.0.1:{}", opts.port))?;

    println!(
        "Open this URL in your browser and sign in as {}:",
        account.username
    );
    println!("\n{url}\n");
    println!(
        "Waiting for the redirect on {redirect_uri} (up to {} minutes) …",
        AUTHORIZATION_TIMEOUT.as_secs() / 60
    );
    if !opts.no_browser {
        open_browser(&url);
    }

    let code = tokio::time::timeout(AUTHORIZATION_TIMEOUT, wait_for_code(&listener, &state))
        .await
        .context("Timed out waiting for the browser authorization")??;

    // Exchange the one-time code. Scope is bound to the code; redirect_uri
    // must byte-match the one used in the authorization request.
    let body = format!(
        "grant_type=authorization_code&client_id={}&code={}&redirect_uri={}&code_verifier={}{}",
        urlencoded(client_id),
        urlencoded(&code),
        urlencoded(&redirect_uri),
        urlencoded(&verifier),
        client_secret_param(client_secret),
    );
    let token_url = oauth2.token_url()?;
    let (status, response) =
        tokio::time::timeout(EXCHANGE_TIMEOUT, minimal_https_post(&token_url, &body))
            .await
            .context("Code exchange timed out")?
            .context("Code exchange failed")?;
    if !(200..300).contains(&status) {
        let err = parse_error_body(status, &response);
        // Append the remedy: at this point the operator is staring at a bare
        // AADSTS code with a half-finished browser flow behind them, and the
        // fix (a config or manifest edit) is not guessable from the code.
        match err.remedy(&account.name) {
            Some(remedy) => bail!("Code exchange rejected: {err}\n\n  → {remedy}"),
            None => bail!("Code exchange rejected: {err}"),
        }
    }

    let parsed: CodeExchangeResponse = serde_json::from_str(&response)
        .context("Failed to parse token response (body omitted from output)")?;
    let refresh_token = parsed.refresh_token.context(
        "Token response contained no refresh_token — ensure the authorization request includes \
         the offline_access scope (Entra) or access_type=offline with prompt=consent (Gmail)",
    )?;

    let key = token_state::account_key(account);
    token_state::store(&key, &refresh_token)?;

    println!("✓ Authorization complete for \"{}\".", account.name);
    println!(
        "  Refresh token stored in {} (key \"{key}\").",
        token_state::state_file_path()?.display()
    );
    println!("  The config file was not modified — tokens live in the state file.");

    verify_login(account).await
}

/// Prove the stored token end-to-end instead of reporting "stored": build a
/// client exactly as the server does, adopt the token just written and
/// complete an XOAUTH2 login. Surfaces scope/permission mistakes now rather
/// than at the next MCP call — and exercises the rotation-persist path once,
/// so what ends up stored is already the freshest token.
async fn verify_login(account: &AccountConfig) -> Result<()> {
    println!("  Verifying IMAP login …");
    let mut client = ImapClient::new(account.clone());
    client.adopt_stored_token();
    match tokio::time::timeout(VERIFY_TIMEOUT, client.connect()).await {
        Ok(Ok(())) => {
            client.disconnect().await;
            println!("✓ IMAP login verified — the account is ready to use.");
            Ok(())
        }
        Ok(Err(e)) => Err(e).context(
            "the refresh token was stored, but the IMAP login failed — check that the app grants \
             IMAP.AccessAsUser.All (Entra) / https://mail.google.com/ (Gmail) and that IMAP is \
             enabled for the mailbox",
        ),
        Err(_) => bail!(
            "the refresh token was stored, but the IMAP login did not complete within {}s — \
             retry or check connectivity to {}",
            VERIFY_TIMEOUT.as_secs(),
            account.host
        ),
    }
}

#[derive(Debug)]
struct CliOptions {
    account: String,
    config: Option<String>,
    port: u16,
    no_browser: bool,
}

impl CliOptions {
    /// Hand-rolled argument parsing, matching the project's no-clap
    /// dependency stance. Rejects anything ambiguous rather than guessing.
    fn parse(args: &[String]) -> Result<Self> {
        let mut account = None;
        let mut config = None;
        let mut port = DEFAULT_PORT;
        let mut no_browser = false;
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--config" => {
                    config = Some(it.next().with_context(|| USAGE)?.clone());
                }
                "--port" => {
                    let raw = it.next().with_context(|| USAGE)?;
                    // Port 0 would bind an OS-chosen port while the redirect
                    // URI still says ":0" — the browser would never reach us
                    // and the command would just sit there. Reject it.
                    port = match raw.parse::<u16>() {
                        Ok(0) | Err(_) => bail!(
                            "--port must be a number between 1 and 65535 (got \"{raw}\"); \
                             the port has to match a redirect URI registered on the app"
                        ),
                        Ok(p) => p,
                    };
                }
                "--no-browser" => no_browser = true,
                other if other.starts_with('-') => bail!("Unknown flag {other}\n{USAGE}"),
                other => {
                    if account.replace(other.to_string()).is_some() {
                        bail!("Only one account may be given\n{USAGE}");
                    }
                }
            }
        }
        Ok(Self {
            account: account.with_context(|| USAGE)?,
            config,
            port,
            no_browser,
        })
    }
}

/// Redirect target handed to the provider. Uses the IP literal rather than
/// `localhost`: the listener binds 127.0.0.1, while `localhost` resolves to
/// `::1` first on dual-stack systems — the browser would then hit a closed
/// IPv6 port and depend on its fallback behaviour. RFC 8252 §7.3 recommends
/// the literal for exactly this reason; Entra and Google both accept
/// loopback redirects on any port.
fn loopback_redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// One RFC 7636 PKCE pair: a 32-byte random verifier and its SHA-256
/// challenge, both base64url without padding.
///
/// Mandatory here because the client may authenticate without a secret: an
/// authorization code alone would then be enough to obtain a token, so any
/// local process that claims the loopback port before us — or reads the code
/// out of the redirect — could redeem it. Binding the code to a verifier that
/// never leaves this process closes that. Harmless for confidential clients,
/// which both providers still accept alongside a secret.
fn pkce_pair() -> Result<(String, String)> {
    let mut bytes = [0u8; 32];
    aws_lc_rs::rand::fill(&mut bytes).map_err(|_| anyhow::anyhow!("CSPRNG unavailable"))?;
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()));
    Ok((verifier, challenge))
}

/// Build the provider's authorization URL. Kept pure for testability.
fn authorize_url(
    oauth2: &OAuth2Config,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> Result<String> {
    match oauth2.provider {
        OAuth2Provider::Outlook365 => {
            let tenant = oauth2.tenant.as_deref().unwrap_or("common");
            Ok(format!(
                "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/authorize\
                 ?client_id={}&response_type=code&redirect_uri={}&response_mode=query\
                 &scope={}&state={}&code_challenge={}&code_challenge_method=S256",
                urlencoded(client_id),
                urlencoded(redirect_uri),
                urlencoded("https://outlook.office365.com/IMAP.AccessAsUser.All offline_access"),
                urlencoded(state),
                urlencoded(challenge),
            ))
        }
        OAuth2Provider::Gmail => Ok(format!(
            "https://accounts.google.com/o/oauth2/v2/auth\
             ?client_id={}&response_type=code&redirect_uri={}\
             &scope={}&state={}&access_type=offline&prompt=consent\
             &code_challenge={}&code_challenge_method=S256",
            urlencoded(client_id),
            urlencoded(redirect_uri),
            urlencoded("https://mail.google.com/"),
            urlencoded(state),
            urlencoded(challenge),
        )),
        OAuth2Provider::Custom => bail!(
            "reauth is not supported for provider = \"custom\" (no authorization endpoint known) \
             — obtain a refresh token manually and place it in the config"
        ),
    }
}

/// Accept loopback connections until one carries the OAuth redirect
/// (`code` or `error` in the query). Unrelated requests (favicon, probes)
/// get a 404 and are ignored; the overall deadline is the caller's timeout.
///
/// Connections are served concurrently, deliberately: browsers routinely
/// open speculative TCP connections that never send a request, and handling
/// one connection at a time would let such a preconnect block the actual
/// redirect until the outer deadline. Each handler additionally bails out
/// after [`CONNECTION_READ_TIMEOUT`] without a complete request head. No cap
/// on in-flight handlers — the listener is loopback-only and lives for a
/// single operator-initiated authorization.
async fn wait_for_code(listener: &TcpListener, expected_state: &str) -> Result<String> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<String>>(4);
    loop {
        tokio::select! {
            // First handler with a definitive verdict decides the flow.
            Some(verdict) = rx.recv() => return verdict,
            accepted = listener.accept() => {
                let (stream, peer) = accepted.context("accept failed")?;
                tracing::debug!(%peer, "Loopback connection accepted");
                let tx = tx.clone();
                let expected_state = expected_state.to_string();
                tokio::spawn(async move {
                    if let Some(verdict) = handle_connection(stream, &expected_state).await {
                        // Full/closed channel means another connection already
                        // decided — drop this verdict rather than block.
                        let _ = tx.try_send(verdict);
                    }
                });
            }
        }
    }
}

/// Serve one loopback connection. `None` means "not the redirect, ignore it";
/// `Some` is a definitive verdict for the whole authorization flow.
async fn handle_connection(mut stream: TcpStream, expected_state: &str) -> Option<Result<String>> {
    let params = match tokio::time::timeout(
        CONNECTION_READ_TIMEOUT,
        read_request_query(&mut stream),
    )
    .await
    {
        Ok(Ok(Some(params))) => params,
        Ok(Ok(None)) => {
            respond(&mut stream, "404 Not Found", "Not the OAuth redirect.").await;
            return None;
        }
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "Unreadable request on loopback listener — ignoring");
            return None;
        }
        Err(_) => {
            tracing::debug!(
                "No request head within the read budget (browser preconnect?) — ignoring"
            );
            return None;
        }
    };

    match validate_redirect(&params, expected_state) {
        Ok(code) => {
            respond(
                &mut stream,
                "200 OK",
                "Authorization received — you can close this window and return to the terminal.",
            )
            .await;
            Some(Ok(code))
        }
        Err(RedirectError::NotOAuth) => {
            respond(&mut stream, "404 Not Found", "Not the OAuth redirect.").await;
            None
        }
        Err(RedirectError::StateMismatch) => {
            respond(&mut stream, "400 Bad Request", "State mismatch — aborted.").await;
            Some(Err(anyhow::anyhow!(
                "state parameter mismatch — possible CSRF or a stale browser tab; retry"
            )))
        }
        Err(RedirectError::Provider { error, description }) => {
            respond(
                &mut stream,
                "400 Bad Request",
                "Authorization failed — see terminal.",
            )
            .await;
            let hint = if description.contains("AADSTS50011") || error == "redirect_uri_mismatch" {
                "\nHint: the redirect URI is not registered on the app. For Entra, add the \
                 \"Mobile and desktop applications\" platform (loopback on any port) or the \
                 exact web redirect that reauth used (printed above)."
            } else {
                ""
            };
            Some(Err(anyhow::anyhow!(
                "provider returned {error}: {description}{hint}"
            )))
        }
    }
}

/// Read one HTTP request's head and extract the query parameters of the
/// request line. `Ok(None)` when the request has no query string at all.
async fn read_request_query(stream: &mut TcpStream) -> Result<Option<BTreeMap<String, String>>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_REQUEST_BYTES {
            bail!("request too large");
        }
        // Search only the freshly read bytes plus a 3-byte overlap, so a
        // client dribbling out a 16 KB head can't make this quadratic.
        let tail = buf.len().saturating_sub(n + 3);
        if buf[tail..].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let request_line = head.lines().next().unwrap_or("");
    // "GET /?code=…&state=… HTTP/1.1"
    let target = request_line.split_whitespace().nth(1).unwrap_or("");
    Ok(target.split_once('?').map(|(_, q)| parse_query(q)))
}

#[derive(Debug)]
enum RedirectError {
    /// Query present but carries neither `code` nor `error` — not ours.
    NotOAuth,
    StateMismatch,
    Provider {
        error: String,
        description: String,
    },
}

/// Decide what a redirect's query means. Pure for testability.
fn validate_redirect(
    params: &BTreeMap<String, String>,
    expected_state: &str,
) -> std::result::Result<String, RedirectError> {
    if let Some(error) = params.get("error") {
        return Err(RedirectError::Provider {
            error: error.clone(),
            description: params.get("error_description").cloned().unwrap_or_default(),
        });
    }
    let Some(code) = params.get("code") else {
        return Err(RedirectError::NotOAuth);
    };
    if params.get("state").map(String::as_str) != Some(expected_state) {
        return Err(RedirectError::StateMismatch);
    }
    Ok(code.clone())
}

fn parse_query(query: &str) -> BTreeMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((percent_decode(k), percent_decode(v)))
        })
        .collect()
}

/// Minimal application/x-www-form-urlencoded decoder: `%XX` and `+`.
/// Invalid escapes pass through literally — provider-generated values are
/// well-formed; this only has to be robust, not strict.
///
/// Decodes over raw bytes and never slices the `&str`: a `%` followed by
/// part of a multi-byte UTF-8 character (`%aä`) would put a `str` slice off
/// a char boundary and panic — reachable from any hand-crafted request to
/// the loopback listener.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

const fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

async fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><html><body style=\"font-family: sans-serif; margin: 3em;\">\
         <h3>imap-mcp-rs</h3><p>{message}</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    const OPENER: &str = "open";
    #[cfg(not(target_os = "macos"))]
    const OPENER: &str = "xdg-open";
    let _ = std::process::Command::new(OPENER)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oauth2(provider: &str) -> OAuth2Config {
        toml::from_str(&format!(
            r#"
            provider = "{provider}"
            tenant = "tid"
            client_id = "cid"
            client_secret = "sec"
            "#
        ))
        .unwrap()
    }

    #[test]
    fn authorize_url_outlook_requests_imap_scope() {
        let url = authorize_url(
            &oauth2("outlook365"),
            "cid",
            "http://localhost:8365",
            "st",
            "ch",
        )
        .unwrap();
        assert!(url.starts_with("https://login.microsoftonline.com/tid/oauth2/v2.0/authorize?"));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8365"));
        assert!(url.contains("IMAP.AccessAsUser.All%20offline_access"));
        assert!(url.contains("state=st"));
    }

    #[test]
    fn authorize_url_gmail_requests_offline_consent() {
        let url =
            authorize_url(&oauth2("gmail"), "cid", "http://localhost:8365", "st", "ch").unwrap();
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
    }

    /// Both providers must receive the challenge — a request without it
    /// silently degrades to a code that anyone holding it can redeem.
    #[test]
    fn authorize_url_carries_pkce_challenge_for_every_provider() {
        for provider in ["outlook365", "gmail"] {
            let url = authorize_url(
                &oauth2(provider),
                "cid",
                "http://localhost:8365",
                "st",
                "ch-42",
            )
            .unwrap();
            assert!(url.contains("code_challenge=ch-42"), "{provider}");
            assert!(url.contains("code_challenge_method=S256"), "{provider}");
        }
    }

    #[test]
    fn pkce_pair_is_rfc7636_shaped_and_unpredictable() {
        let (verifier, challenge) = pkce_pair().unwrap();
        // 32 random bytes -> 43 base64url chars, within RFC 7636's 43..=128.
        assert_eq!(verifier.len(), 43);
        assert_eq!(challenge.len(), 43);
        let unreserved = |s: &str| {
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~'))
        };
        assert!(unreserved(&verifier), "{verifier}");
        assert!(unreserved(&challenge), "{challenge}");
        // The challenge is a digest, never the verifier itself (`plain`).
        assert_ne!(verifier, challenge);
        assert_ne!(pkce_pair().unwrap().0, verifier);
    }

    /// The challenge must be S256 of the verifier's ASCII, per RFC 7636 §4.2 —
    /// checked against the specification's own worked example.
    #[test]
    fn pkce_challenge_matches_rfc7636_test_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge =
            URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    fn parse_args(args: &[&str]) -> Result<CliOptions> {
        CliOptions::parse(&args.iter().map(|s| (*s).to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn cli_options_parse_reads_account_and_flags() {
        let o = parse_args(&["Office", "--port", "9000", "--no-browser"]).unwrap();
        assert_eq!(o.account, "Office");
        assert_eq!(o.port, 9000);
        assert!(o.no_browser);
        assert!(o.config.is_none());
        assert_eq!(parse_args(&["Office"]).unwrap().port, DEFAULT_PORT);
    }

    #[test]
    fn cli_options_parse_rejects_unusable_input() {
        for (args, expect) in [
            (vec![], "usage"),
            (vec!["A", "B"], "Only one account"),
            (vec!["Office", "--nope"], "Unknown flag"),
            (vec!["Office", "--port"], "usage"),
            (vec!["Office", "--port", "0"], "between 1 and 65535"),
            (vec!["Office", "--port", "99999"], "between 1 and 65535"),
            (vec!["Office", "--port", "abc"], "between 1 and 65535"),
            (vec!["Office", "--config"], "usage"),
        ] {
            let err = parse_args(&args).unwrap_err().to_string().to_lowercase();
            assert!(
                err.contains(&expect.to_lowercase()),
                "args {args:?} produced {err:?}, expected it to mention {expect:?}"
            );
        }
    }

    #[test]
    fn loopback_redirect_uri_uses_ip_literal() {
        // Regression guard: the listener binds 127.0.0.1, so the redirect URI
        // must name that address. With `localhost` a dual-stack browser tries
        // ::1 first and lands on a closed port (RFC 8252 §7.3).
        let uri = loopback_redirect_uri(8365);
        assert_eq!(uri, "http://127.0.0.1:8365");
        assert!(!uri.contains("localhost"));
    }

    #[test]
    fn authorize_url_custom_is_rejected() {
        let err = authorize_url(
            &oauth2("custom"),
            "cid",
            "http://localhost:8365",
            "st",
            "ch",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("custom"));
    }

    #[test]
    fn percent_decode_basics() {
        assert_eq!(percent_decode("a%20b+c"), "a b c");
        assert_eq!(
            percent_decode("AADSTS50011%3A%20mismatch"),
            "AADSTS50011: mismatch"
        );
        // Robustness: invalid escapes pass through.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        // Multi-byte sequences survive round-trip.
        assert_eq!(percent_decode("Gr%C3%BC%C3%9Fe"), "Grüße");
    }

    #[test]
    fn percent_decode_multibyte_after_percent_does_not_panic() {
        // Regression: reading the two hex digits by slicing the &str put the
        // slice off a UTF-8 char boundary here and panicked.
        assert_eq!(percent_decode("%aä"), "%aä");
        assert_eq!(percent_decode("%ä"), "%ä");
        assert_eq!(percent_decode("x%✓y"), "x%✓y");
    }

    /// Send a raw HTTP request line to `addr` and keep the connection open
    /// until the response arrives.
    async fn send_request(addr: std::net::SocketAddr, target: &str) {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .unwrap();
        s.flush().await.unwrap();
    }

    /// Asking for help must succeed on a machine with no config at all —
    /// proving the check runs before `load_config`, which would otherwise
    /// turn `--help` into "no configuration file found".
    #[tokio::test]
    async fn help_flag_succeeds_without_reading_any_config() {
        for flag in ["--help", "-h"] {
            let args = vec![flag.to_string(), "--config".into(), "/nonexistent".into()];
            assert!(run(&args).await.is_ok(), "{flag}");
        }
        // An unknown flag stays a failure, with the same text as guidance.
        let err = run(&["--nope".to_string()]).await.unwrap_err().to_string();
        assert!(err.contains("Unknown flag"), "{err}");
        assert!(err.contains("usage:"), "{err}");
    }

    #[tokio::test]
    async fn wait_for_code_extracts_code_from_redirect() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let waiter = tokio::spawn(async move { wait_for_code(&listener, "st").await });
        send_request(addr, "/?code=the-code&state=st").await;
        let code = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("wait_for_code did not finish")
            .unwrap()
            .unwrap();
        assert_eq!(code, "the-code");
    }

    #[tokio::test]
    async fn wait_for_code_survives_silent_preconnect() {
        // Regression: browsers open speculative connections that never send a
        // request. Serving connections sequentially made such a preconnect
        // stall the real redirect until the per-connection timeout (15s).
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let waiter = tokio::spawn(async move { wait_for_code(&listener, "st").await });
        let _silent = tokio::net::TcpStream::connect(addr).await.unwrap();
        send_request(addr, "/?code=c2&state=st").await;
        let code = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("redirect was blocked by the silent connection")
            .unwrap()
            .unwrap();
        assert_eq!(code, "c2");
    }

    #[tokio::test]
    async fn wait_for_code_ignores_unrelated_request() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let waiter = tokio::spawn(async move { wait_for_code(&listener, "st").await });
        send_request(addr, "/favicon.ico").await;
        send_request(addr, "/?code=c3&state=st").await;
        let code = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("wait_for_code did not finish")
            .unwrap()
            .unwrap();
        assert_eq!(code, "c3");
    }

    #[tokio::test]
    async fn wait_for_code_aborts_on_state_mismatch() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let waiter = tokio::spawn(async move { wait_for_code(&listener, "st").await });
        send_request(addr, "/?code=c4&state=forged").await;
        let err = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("wait_for_code did not finish")
            .unwrap()
            .unwrap_err();
        assert!(err.to_string().contains("state parameter mismatch"));
    }

    #[test]
    fn parse_query_splits_pairs() {
        let q = parse_query("code=abc%2Fdef&state=xyz&empty");
        assert_eq!(q.get("code").unwrap(), "abc/def");
        assert_eq!(q.get("state").unwrap(), "xyz");
        assert!(!q.contains_key("empty"));
    }

    #[test]
    fn validate_redirect_happy_path() {
        let mut p = BTreeMap::new();
        p.insert("code".to_string(), "c1".to_string());
        p.insert("state".to_string(), "st".to_string());
        assert_eq!(validate_redirect(&p, "st").unwrap(), "c1");
    }

    #[test]
    fn validate_redirect_state_mismatch() {
        let mut p = BTreeMap::new();
        p.insert("code".to_string(), "c1".to_string());
        p.insert("state".to_string(), "WRONG".to_string());
        assert!(matches!(
            validate_redirect(&p, "st"),
            Err(RedirectError::StateMismatch)
        ));
    }

    #[test]
    fn validate_redirect_provider_error_wins() {
        let mut p = BTreeMap::new();
        p.insert("error".to_string(), "access_denied".to_string());
        assert!(matches!(
            validate_redirect(&p, "st"),
            Err(RedirectError::Provider { .. })
        ));
    }

    #[test]
    fn validate_redirect_unrelated_query() {
        let p = BTreeMap::new();
        assert!(matches!(
            validate_redirect(&p, "st"),
            Err(RedirectError::NotOAuth)
        ));
    }
}
