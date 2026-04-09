use std::env;

use rmcp::{ServiceExt, transport::stdio};
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
    tracing::info!(
        host = %config.imap.host,
        user = %config.imap.username,
        read_only = config.account.read_only,
        "Configuration loaded"
    );

    let mut client = ImapClient::new(config.imap.clone(), config.auth.clone());
    client.connect().await?;
    tracing::info!("IMAP connection established");

    let server = ImapMcpServer::new(config, client);
    let disconnect_client = server.client.clone();

    let service = match server.serve(stdio()).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("serving error: {:?}", e);
            disconnect_client.lock().await.disconnect().await;
            return Err(e.into());
        }
    };

    let result = service.waiting().await;

    // Always disconnect, even on error
    disconnect_client.lock().await.disconnect().await;
    tracing::info!("IMAP disconnected, shutting down");

    result?;
    Ok(())
}
