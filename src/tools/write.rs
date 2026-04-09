use rmcp::schemars;
use serde::Deserialize;

use super::{ImapMcpServer, error_json};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MoveEmailRequest {
    #[schemars(description = "Source folder name")]
    pub folder: String,
    #[schemars(description = "One or more email UIDs to move")]
    pub uids: Vec<u32>,
    #[schemars(description = "Destination folder name")]
    pub target_folder: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MarkReadRequest {
    #[schemars(description = "Folder name")]
    pub folder: String,
    #[schemars(description = "One or more email UIDs to mark as read")]
    pub uids: Vec<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MarkUnreadRequest {
    #[schemars(description = "Folder name")]
    pub folder: String,
    #[schemars(description = "One or more email UIDs to mark as unread")]
    pub uids: Vec<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FlagEmailRequest {
    #[schemars(description = "Folder name")]
    pub folder: String,
    #[schemars(description = "One or more email UIDs to flag/unflag")]
    pub uids: Vec<u32>,
    #[schemars(description = "true to flag (star/important), false to unflag")]
    pub flagged: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteEmailRequest {
    #[schemars(description = "Folder name")]
    pub folder: String,
    #[schemars(description = "One or more email UIDs to delete")]
    pub uids: Vec<u32>,
    #[schemars(description = "true for immediate deletion, false (default) moves to Trash")]
    pub permanent: Option<bool>,
}

pub async fn move_email(server: &ImapMcpServer, req: MoveEmailRequest) -> String {
    if server.config.account.read_only {
        return error_json("Account is configured as read-only");
    }
    let mut client = server.client.lock().await;
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
    if server.config.account.read_only {
        return error_json("Account is configured as read-only");
    }
    let mut client = server.client.lock().await;
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
    if server.config.account.read_only {
        return error_json("Account is configured as read-only");
    }
    let mut client = server.client.lock().await;
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
    if server.config.account.read_only {
        return error_json("Account is configured as read-only");
    }
    let mut client = server.client.lock().await;
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
    if server.config.account.read_only {
        return error_json("Account is configured as read-only");
    }
    let permanent = req.permanent.unwrap_or(false);
    let mut client = server.client.lock().await;
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
