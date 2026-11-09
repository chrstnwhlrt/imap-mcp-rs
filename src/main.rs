//! Binary entry point for `imap-mcp-rs`. Orchestrates config loading,
//! per-account IMAP connections, and the MCP server lifecycle; all the
//! actual logic lives in the library crate ([`imap_mcp_rs`]). The README's
//! "Architecture" section maps its modules.

use std::collections::HashMap;
use std::env;
use std::sync::Arc;

use rmcp::{ServiceExt, transport::stdio};
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use imap_mcp_rs::{config, imap_client::ImapClient, tools::ImapMcpServer};

const HELP: &str = "\
imap-mcp-rs — IMAP access for LLM assistants over the Model Context Protocol

USAGE:
    imap-mcp-rs [--config <path>]           run as MCP server on stdin/stdout
    imap-mcp-rs reauth <account> [OPTIONS]  (re-)authorize an OAuth2 account

OPTIONS:
    --config <path>    config file to use; without it $IMAP_MCP_CONFIG is
                       used when set, otherwise the standard locations are
                       searched ($XDG_CONFIG_HOME/imap-mcp-rs/config.toml,
                       i.e. ~/.config/… by default, then
                       /etc/imap-mcp-rs/config.toml)
    -h, --help         print this help
    -V, --version      print version

The server is normally launched by an MCP client rather than by hand: it
speaks the protocol on stdin/stdout and will appear to hang if started in a
terminal. `reauth` is the one thing you run yourself — once per OAuth2
account at setup, and again only if its token is revoked or lost.

Documentation: https://github.com/chrstnwhlrt/imap-mcp-rs";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let rest = args.get(1..).unwrap_or_default();
    let is_reauth = rest.first().is_some_and(|a| a == "reauth");

    // Answered before the tracing subscriber is installed: help and version
    // go to stdout unaccompanied by INFO lines on stderr, so they stay
    // pipeable. `reauth --help` is handled by the subcommand itself.
    if !is_reauth {
        if rest.iter().any(|a| a == "-h" || a == "--help") {
            println!("{HELP}");
            return Ok(());
        }
        if rest.iter().any(|a| a == "-V" || a == "--version") {
            println!("imap-mcp-rs {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
    }

    // INFO unless RUST_LOG says otherwise (`RUST_LOG=debug`, or per crate
    // `RUST_LOG=info,imap_mcp_rs=debug`). Adding INFO on top of the env
    // filter instead overwrote any global level RUST_LOG set, so
    // `RUST_LOG=debug` stayed at INFO.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    // Subcommand dispatch before server startup. `reauth` runs the
    // interactive OAuth2 authorization flow and exits — it never speaks MCP.
    if is_reauth {
        return imap_mcp_rs::reauth::run(&rest[1..]).await;
    }

    let config_path = parse_server_args(rest)?;
    let config = config::load_config(config_path.as_deref())?;

    // Ensure the first configured attachment dir exists so the whitelist
    // check can canonicalize it, and that it is a private directory of our
    // own: downloads save here, drafts read from here, and the prune below
    // deletes in it. See `prepare_attachment_dir` for what is refused.
    let default_dir = config::default_attachment_dir();
    let attachment_dir = config
        .allowed_attachment_dirs
        .first()
        .cloned()
        .unwrap_or(default_dir);
    config::prepare_attachment_dir(std::path::Path::new(&attachment_dir)).await?;

    // Prune stale downloads at startup — download_attachment writes per-call
    // UUID subdirs that nothing else reaps. 7 days is far longer than any
    // realistic draft-review cycle; on systemd hosts XDG_RUNTIME_DIR is also
    // wiped on logout, so this is a belt-and-braces for long-running sessions
    // and non-systemd systems. Non-fatal on errors.
    let max_age = std::time::Duration::from_hours(7 * 24);
    if let Err(e) = prune_stale_attachments(&attachment_dir, max_age).await {
        tracing::warn!(error = %e, "attachment cleanup skipped");
    }

    // Connect all accounts in parallel — otherwise a slow server on one account
    // would delay startup of all others. Each has its own timeout (see
    // `ImapClient::connect_bounded`). Soft errors: failed accounts reconnect
    // on first use.
    // `iter().cloned()` (not into_iter) because we still need config.accounts
    // afterwards to build ImapMcpServer.
    #[allow(clippy::redundant_iter_cloned)]
    let connect_futures = config.accounts.iter().cloned().map(|account| async move {
        let name = account.name.clone();
        let mut client = ImapClient::new(account);
        client.adopt_stored_token();
        let result = client.connect_bounded().await;
        (name, client, result)
    });

    let mut clients = HashMap::new();
    for (name, client, result) in futures_util::future::join_all(connect_futures).await {
        match result {
            Ok(()) => tracing::info!(account = %name, "Connected"),
            Err(e) => tracing::warn!(
                account = %name,
                error = %e,
                "Failed to connect (will retry on first use)"
            ),
        }
        clients.insert(name.to_lowercase(), Arc::new(Mutex::new(client)));
    }

    let server = ImapMcpServer::new(config, clients);
    let disconnect_clients: Vec<_> = server.clients.values().cloned().collect();

    let service = match server.serve(stdio()).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("serving error: {:?}", e);
            disconnect_all(&disconnect_clients).await;
            return Err(e.into());
        }
    };

    let result = service.waiting().await;

    disconnect_all(&disconnect_clients).await;
    tracing::info!("All accounts disconnected, shutting down");

    result?;
    Ok(())
}

/// Disconnect all accounts in parallel so shutdown time is bounded by the
/// slowest account (capped at ~5s per the logout timeout in `ImapClient`),
/// not the sum of all accounts. A total-timeout wraps the whole batch so
/// that a tool call still holding a per-account mutex at shutdown time
/// can't stall the process indefinitely — the OS will close the TCP on
/// process exit.
async fn disconnect_all(clients: &[Arc<Mutex<ImapClient>>]) {
    let futures = clients
        .iter()
        .map(|c| async move { c.lock().await.disconnect().await });
    let total = std::time::Duration::from_secs(15);
    if tokio::time::timeout(total, futures_util::future::join_all(futures))
        .await
        .is_err()
    {
        tracing::warn!("disconnect_all timed out, abandoning remaining sessions");
    }
}

/// Remove our own download directories in `dir` whose modification time is
/// older than `max_age`.
///
/// Only per-download `<uuid>` subdirectories are candidates — the format
/// `download_attachment` writes. `dir` is the first `allowed_attachment_dirs`
/// entry, which may well be a directory the user keeps their own files in
/// (the sample config suggests a Documents folder): an earlier version
/// deleted *every* entry older than `max_age` there, subdirectories
/// recursively. Symlinks are skipped (`prepare_attachment_dir` refuses a
/// symlinked `dir`; a symlink inside would be planted and must not be
/// followed).
async fn prune_stale_attachments(dir: &str, max_age: std::time::Duration) -> anyhow::Result<()> {
    let now = std::time::SystemTime::now();
    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut pruned = 0usize;
    while let Some(entry) = entries.next_entry().await? {
        if !is_download_dir_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        // `DirEntry::metadata` does not traverse symlinks (lstat semantics),
        // so a planted link is seen as the link it is.
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let Ok(mtime) = meta.modified() else { continue };
        let age = now
            .duration_since(mtime)
            .unwrap_or(std::time::Duration::ZERO);
        if age <= max_age {
            continue;
        }
        if tokio::fs::remove_dir_all(entry.path()).await.is_ok() {
            pruned += 1;
        }
    }
    if pruned > 0 {
        tracing::info!(
            dir = %dir,
            count = pruned,
            max_age_days = max_age.as_secs() / 86_400,
            "Pruned stale attachment downloads"
        );
    }
    Ok(())
}

/// Whether a directory entry name is one of `download_attachment`'s
/// per-download directories: a UUID exactly as `Uuid::to_string` writes it
/// (hyphenated, lowercase). Anything else is not ours to prune.
fn is_download_dir_name(name: &str) -> bool {
    uuid::Uuid::try_parse(name).is_ok_and(|id| id.hyphenated().to_string() == name)
}

/// The server's own arguments (everything except `reauth`, `--help` and
/// `--version`, which are dispatched before this): `--config <path>` and
/// nothing else.
///
/// Strict on purpose. An unrecognized argument used to be ignored, so a typo
/// such as `--confg read-only.toml` started the server on the default config
/// — possibly one with write permissions the operator meant to withhold —
/// and a `--config` without its path fell back to the default the same way.
fn parse_server_args(args: &[String]) -> anyhow::Result<Option<String>> {
    let mut config = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => {
                let Some(path) = it.next() else {
                    anyhow::bail!("--config needs a path: imap-mcp-rs --config <path>");
                };
                if config.replace(path.clone()).is_some() {
                    anyhow::bail!("--config was given more than once");
                }
            }
            // A misplaced subcommand would otherwise read as an unknown
            // argument; say what to type instead.
            "reauth" => anyhow::bail!(
                "the subcommand must come first: imap-mcp-rs reauth <account> [--config <path>]"
            ),
            other => anyhow::bail!(
                "unknown argument \"{other}\" — usage: imap-mcp-rs [--config <path>] \
                 (see imap-mcp-rs --help)"
            ),
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn server_args_accept_config_and_nothing_else() {
        assert_eq!(parse_server_args(&args(&[])).unwrap(), None);
        assert_eq!(
            parse_server_args(&args(&["--config", "/x/config.toml"])).unwrap(),
            Some("/x/config.toml".to_string())
        );
        // A typo must not silently fall back to the default config.
        for (bad, expect) in [
            (vec!["--confg", "/x.toml"], "unknown argument"),
            (vec!["--config"], "needs a path"),
            (vec!["--config", "/a", "--config", "/b"], "more than once"),
            (
                vec!["--config", "/x", "reauth", "Office"],
                "must come first",
            ),
        ] {
            let err = parse_server_args(&args(&bad)).unwrap_err().to_string();
            assert!(err.contains(expect), "{bad:?} -> {err}");
        }
    }

    #[test]
    fn only_hyphenated_uuid_names_count_as_download_dirs() {
        let id = uuid::Uuid::new_v4().to_string();
        assert!(is_download_dir_name(&id));
        assert!(!is_download_dir_name(&id.to_uppercase()));
        assert!(!is_download_dir_name(&id.replace('-', "")));
        assert!(!is_download_dir_name(&format!("{id}.pdf")));
        assert!(!is_download_dir_name("Documents"));
        assert!(!is_download_dir_name(""));
    }

    /// The prune runs on the first `allowed_attachment_dirs` entry, which
    /// can be a directory holding the user's own files. Only stale
    /// per-download directories may go — never anything else, however old.
    #[tokio::test]
    async fn prune_removes_only_stale_download_dirs() {
        let base = std::env::temp_dir().join(format!(
            "imap-mcp-rs-prune-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_hours(30 * 24);
        let age = |path: &std::path::Path| {
            std::fs::File::open(path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        };

        let stale_download = base.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir(&stale_download).unwrap();
        std::fs::write(stale_download.join("report.pdf"), b"x").unwrap();
        age(&stale_download);

        let fresh_download = base.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir(&fresh_download).unwrap();

        let user_dir = base.join("Projects");
        std::fs::create_dir(&user_dir).unwrap();
        age(&user_dir);
        let user_file = base.join("notes.txt");
        std::fs::write(&user_file, b"keep me").unwrap();
        age(&user_file);
        let uuid_named_file = base.join(format!("{}.pdf", uuid::Uuid::new_v4()));
        std::fs::write(&uuid_named_file, b"keep me too").unwrap();
        age(&uuid_named_file);

        prune_stale_attachments(
            base.to_str().unwrap(),
            std::time::Duration::from_hours(7 * 24),
        )
        .await
        .unwrap();

        assert!(!stale_download.exists(), "stale download dir must go");
        assert!(fresh_download.exists(), "fresh download dir must stay");
        assert!(
            user_dir.exists(),
            "a user's own directory must never be pruned"
        );
        assert!(user_file.exists(), "a user's own file must never be pruned");
        assert!(
            uuid_named_file.exists(),
            "only directories are download entries"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
