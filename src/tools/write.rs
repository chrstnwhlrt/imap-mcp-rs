use rmcp::schemars;
use serde::Deserialize;

use super::{ImapMcpServer, error_json};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MoveEmailRequest {
    #[schemars(description = "Account name (from list_accounts). Uses first account if omitted.")]
    pub account: Option<String>,
    #[schemars(description = "Source folder name")]
    pub folder: String,
    #[schemars(description = "One or more email UIDs to move")]
    pub uids: Vec<u32>,
    #[schemars(description = "Destination folder name")]
    pub target_folder: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MarkReadRequest {
    #[schemars(description = "Account name (from list_accounts). Uses first account if omitted.")]
    pub account: Option<String>,
    #[schemars(description = "Folder name")]
    pub folder: String,
    #[schemars(description = "One or more email UIDs to mark as read")]
    pub uids: Vec<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MarkUnreadRequest {
    #[schemars(description = "Account name (from list_accounts). Uses first account if omitted.")]
    pub account: Option<String>,
    #[schemars(description = "Folder name")]
    pub folder: String,
    #[schemars(description = "One or more email UIDs to mark as unread")]
    pub uids: Vec<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FlagEmailRequest {
    #[schemars(description = "Account name (from list_accounts). Uses first account if omitted.")]
    pub account: Option<String>,
    #[schemars(description = "Folder name")]
    pub folder: String,
    #[schemars(description = "One or more email UIDs to flag/unflag")]
    pub uids: Vec<u32>,
    #[schemars(description = "true to flag (star/important), false to unflag")]
    pub flagged: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteEmailRequest {
    #[schemars(description = "Account name (from list_accounts). Uses first account if omitted.")]
    pub account: Option<String>,
    #[schemars(description = "Folder name")]
    pub folder: String,
    #[schemars(description = "One or more email UIDs to delete")]
    pub uids: Vec<u32>,
    #[schemars(description = "true for immediate deletion, false (default) moves to Trash")]
    pub permanent: Option<bool>,
}

macro_rules! resolve_write {
    ($server:expr, $req:expr) => {{
        let (config, client_arc) = match $server.resolve_client($req.account.as_deref()) {
            Ok(r) => r,
            Err(e) => return error_json(&e),
        };
        if config.read_only {
            return error_json("Account is configured as read-only");
        }
        client_arc
    }};
}

pub async fn move_email(server: &ImapMcpServer, req: MoveEmailRequest) -> String {
    let (config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    if config.read_only {
        return error_json("Account is configured as read-only");
    }
    if !config.allow_move {
        return error_json("Moving emails is disabled for this account (allow_move = false)");
    }
    let mut client = client_arc.lock().await;
    match client
        .move_emails(&req.folder, &req.uids, &req.target_folder)
        .await
    {
        Ok(succeeded) => serde_json::to_string(&serde_json::json!({
            "succeeded": succeeded, "failed": []
        }))
        .unwrap(),
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

pub async fn mark_as_read(server: &ImapMcpServer, req: MarkReadRequest) -> String {
    let client_arc = resolve_write!(server, req);
    let mut client = client_arc.lock().await;
    match client
        .mark_flags(&req.folder, &req.uids, "\\Seen", true)
        .await
    {
        Ok(succeeded) => serde_json::to_string(&serde_json::json!({
            "succeeded": succeeded, "failed": []
        }))
        .unwrap(),
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

pub async fn mark_as_unread(server: &ImapMcpServer, req: MarkUnreadRequest) -> String {
    let client_arc = resolve_write!(server, req);
    let mut client = client_arc.lock().await;
    match client
        .mark_flags(&req.folder, &req.uids, "\\Seen", false)
        .await
    {
        Ok(succeeded) => serde_json::to_string(&serde_json::json!({
            "succeeded": succeeded, "failed": []
        }))
        .unwrap(),
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

pub async fn flag_email(server: &ImapMcpServer, req: FlagEmailRequest) -> String {
    let client_arc = resolve_write!(server, req);
    let mut client = client_arc.lock().await;
    match client
        .mark_flags(&req.folder, &req.uids, "\\Flagged", req.flagged)
        .await
    {
        Ok(succeeded) => serde_json::to_string(&serde_json::json!({
            "succeeded": succeeded, "failed": []
        }))
        .unwrap(),
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

pub async fn delete_email(server: &ImapMcpServer, req: DeleteEmailRequest) -> String {
    let (config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    if config.read_only {
        return error_json("Account is configured as read-only");
    }
    if !config.allow_delete {
        return error_json("Deleting emails is disabled for this account (allow_delete = false)");
    }
    let permanent = req.permanent.unwrap_or(false);
    let mut client = client_arc.lock().await;
    match client
        .delete_emails(&req.folder, &req.uids, permanent)
        .await
    {
        Ok(succeeded) => serde_json::to_string(&serde_json::json!({
            "succeeded": succeeded, "failed": []
        }))
        .unwrap(),
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}
