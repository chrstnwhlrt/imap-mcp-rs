//! Binary entry point for `imap-mcp-rs`. Orchestrates config loading,
//! per-account IMAP connections, and the MCP server lifecycle; all the
//! actual logic lives in the library crate (`src/lib.rs`). See the
//! library-level docs on [`imap_mcp_rs`] for the module layout.

use std::collections::HashMap;
use std::env;
use std::sync::Arc;

use rmcp::{ServiceExt, transport::stdio};
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use imap_mcp_rs::{config, imap_client::ImapClient, tools::ImapMcpServer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let config_path = {
        let args: Vec<String> = env::args().collect();
        args.iter()
            .position(|a| a == "--config")
            .and_then(|i| args.get(i + 1).cloned())
    };

    let config = config::load_config(config_path.as_deref())?;

    // Ensure the first configured attachment dir exists so the whitelist
    // check can canonicalize it. Downloads save here; drafts read from
    // here. Set permissions to 0700 so attachment contents on shared
    // systems aren't world-readable. Reject pre-existing symlinks at this
    // path — on a legacy `/tmp`-based location another user could otherwise
    // redirect our writes by pre-creating the symlink.
    let default_dir = config::default_attachment_dir();
    let attachment_dir = config
        .allowed_attachment_dirs
        .first()
        .cloned()
        .unwrap_or(default_dir);
    // Explicit match: silently swallowing `Err` could let a permission-denied
    // stat (e.g. transiently-chmoded parent on a multi-user host) skip the
    // symlink check. Only "file does not exist yet" is a safe pass-through.
    match std::fs::symlink_metadata(&attachment_dir) {
        Ok(m) if m.file_type().is_symlink() => {
            anyhow::bail!(
                "attachment dir \"{attachment_dir}\" is a symlink — refusing to use; \
                 remove it or change `allowed_attachment_dirs` in the config"
            );
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => anyhow::bail!(
            "cannot stat attachment dir \"{attachment_dir}\": {e} — \
             refusing to proceed without symlink verification"
        ),
    }
    let _ = tokio::fs::create_dir_all(&attachment_dir).await;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ =
            tokio::fs::set_permissions(&attachment_dir, std::fs::Permissions::from_mode(0o700))
                .await;
    }

    // Prune stale downloads at startup — download_attachment writes per-call
    // UUID subdirs that nothing else reaps. 7 days is far longer than any
    // realistic draft-review cycle; on systemd hosts XDG_RUNTIME_DIR is also
    // wiped on logout, so this is a belt-and-braces for long-running sessions
    // and non-systemd systems. Non-fatal on errors.
    // `from_days` is nightly-only in current stable; use `from_secs`
    // with the lint allowed locally.
    #[allow(clippy::duration_suboptimal_units)]
    let max_age = std::time::Duration::from_secs(7 * 24 * 60 * 60);
    if let Err(e) = prune_stale_attachments(&attachment_dir, max_age).await {
        tracing::warn!(error = %e, "attachment cleanup skipped");
    }

    // Connect all accounts in parallel — otherwise a slow server on one account
    // would delay startup of all others. Each has an independent 15s timeout.
    // Soft errors: failed accounts reconnect on first use.
    // `iter().cloned()` (not into_iter) because we still need config.accounts
    // afterwards to build ImapMcpServer.
    #[allow(clippy::redundant_iter_cloned)]
    let connect_futures = config.accounts.iter().cloned().map(|account| async move {
        let name = account.name.clone();
        let mut client = ImapClient::new(account);
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(15), client.connect()).await;
        (name, client, result)
    });

    let mut clients = HashMap::new();
    for (name, client, result) in futures_util::future::join_all(connect_futures).await {
        match result {
            Ok(Ok(())) => tracing::info!(account = %name, "Connected"),
            Ok(Err(e)) => tracing::warn!(
                account = %name,
                error = %e,
                "Failed to connect (will retry on first use)"
            ),
            Err(_) => tracing::warn!(
                account = %name,
                "Connection timed out (will retry on first use)"
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

/// Remove entries in `dir` whose modification time is older than `max_age`.
/// Handles both the new per-download subdirectory format and any legacy
/// flat files (`<UUID>.<ext>`) left over from earlier versions. Symlinks
/// are skipped (the startup check refuses a symlinked `dir`; any symlink
/// inside would be attacker-planted and shouldn't be followed).
async fn prune_stale_attachments(dir: &str, max_age: std::time::Duration) -> anyhow::Result<()> {
    let now = std::time::SystemTime::now();
    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut pruned = 0usize;
    while let Some(entry) = entries.next_entry().await? {
        // `symlink_metadata` so we don't follow symlinks into other trees.
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let Ok(mtime) = meta.modified() else { continue };
        let age = now.duration_since(mtime).unwrap_or(std::time::Duration::ZERO);
        if age <= max_age {
            continue;
        }
        let path = entry.path();
        let removed = if meta.is_dir() {
            tokio::fs::remove_dir_all(&path).await
        } else {
            tokio::fs::remove_file(&path).await
        };
        if removed.is_ok() {
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
