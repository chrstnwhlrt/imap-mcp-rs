use std::collections::HashMap;
use std::env;
use std::sync::Arc;

use rmcp::{ServiceExt, transport::stdio};
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

mod config;
mod email;
mod imap_client;
mod oauth2;
mod tools;

use imap_client::ImapClient;
use tools::ImapMcpServer;

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

    // Connect all accounts (soft errors — failed accounts reconnect on first use)
    let mut clients = HashMap::new();
    for account in &config.accounts {
        let mut client = ImapClient::new(account.clone());
        match tokio::time::timeout(std::time::Duration::from_secs(15), client.connect()).await {
            Ok(Ok(())) => {
                tracing::info!(account = %account.name, "Connected");
            }
            Ok(Err(e)) => {
                tracing::warn!(account = %account.name, error = %e, "Failed to connect (will retry on first use)");
            }
            Err(_) => {
                tracing::warn!(account = %account.name, "Connection timed out (will retry on first use)");
            }
        }
        clients.insert(account.name.to_lowercase(), Arc::new(Mutex::new(client)));
    }

    let server = ImapMcpServer::new(config, clients);
    let disconnect_clients: Vec<_> = server.clients.values().cloned().collect();

    let service = match server.serve(stdio()).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("serving error: {:?}", e);
            for client in &disconnect_clients {
                client.lock().await.disconnect().await;
            }
            return Err(e.into());
        }
    };

    let result = service.waiting().await;

    for client in &disconnect_clients {
        client.lock().await.disconnect().await;
    }
    tracing::info!("All accounts disconnected, shutting down");

    result?;
    Ok(())
}
