//! Mutating MCP tools: flag, mark read/unread, move, delete.
//!
//! Each tool checks `account_config.read_only` and the per-account
//! `allow_move` / `allow_delete` switches before issuing IMAP commands —
//! defense-in-depth so an LLM that ignores the schema description can't
//! corrupt mailboxes that the user marked off-limits.

use rmcp::schemars;
use serde::Deserialize;

use super::{ImapMcpServer, error_json};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MoveEmailRequest {
    #[schemars(description = "Account name (from list_accounts); default: first configured.")]
    pub account: Option<String>,
    #[schemars(description = "Source folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(description = "Email UIDs to move (from list_emails or search_emails results)")]
    pub uids: Vec<u32>,
    #[schemars(
        description = "Destination folder name. Must exist — use list_folders to find valid targets."
    )]
    pub target_folder: String,
    #[schemars(
        description = "If true, validate permissions + inputs but don't actually move; returns a preview payload the LLM can show the user for confirmation. Default: false."
    )]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MarkReadRequest {
    #[schemars(description = "Account name (from list_accounts); default: first configured.")]
    pub account: Option<String>,
    #[schemars(description = "Folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(
        description = "Email UIDs to mark as read (from list_emails or search_emails results)"
    )]
    pub uids: Vec<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MarkUnreadRequest {
    #[schemars(description = "Account name (from list_accounts); default: first configured.")]
    pub account: Option<String>,
    #[schemars(description = "Folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(
        description = "Email UIDs to mark as unread (from list_emails or search_emails results)"
    )]
    pub uids: Vec<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FlagEmailRequest {
    #[schemars(description = "Account name (from list_accounts); default: first configured.")]
    pub account: Option<String>,
    #[schemars(description = "Folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(description = "Email UIDs to flag (from list_emails or search_emails results)")]
    pub uids: Vec<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UnflagEmailRequest {
    #[schemars(description = "Account name (from list_accounts); default: first configured.")]
    pub account: Option<String>,
    #[schemars(description = "Folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(description = "Email UIDs to unflag (from list_emails or search_emails results)")]
    pub uids: Vec<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteEmailRequest {
    #[schemars(description = "Account name (from list_accounts); default: first configured.")]
    pub account: Option<String>,
    #[schemars(description = "Folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(description = "Email UIDs to delete (from list_emails or search_emails results)")]
    pub uids: Vec<u32>,
    #[schemars(
        description = "true = EXPUNGE immediately (unrecoverable), false (default) = move to Trash"
    )]
    pub permanent: Option<bool>,
    #[schemars(
        description = "If true, validate permissions + inputs but don't actually delete; returns a preview payload the LLM can show the user for confirmation. Default: false."
    )]
    pub dry_run: Option<bool>,
}

/// Build the standard write-response. `account` is always included so the LLM
/// can disambiguate when calling tools on multiple accounts in parallel.
/// IMAP STORE/COPY/EXPUNGE are atomic at the server — operations either
/// fully succeed or return an error. There is no partial-failure case, so
/// we don't surface a `failed` field.
fn write_ok(account: &str, succeeded: &[u32]) -> String {
    serde_json::to_string(&serde_json::json!({
        "account": account,
        "succeeded": succeeded,
    }))
    .unwrap()
}

/// Upper bound on UIDs per write call. A prompt-injected LLM passing a
/// ludicrously long list (e.g. 10M UIDs) would force a ~110 MB
/// `uid_set_string` allocation and a gigantic IMAP STORE/COPY command the
/// server likely rejects anyway. MCP JSON-RPC has a rough ceiling from the
/// transport layer, but that's not a hard guarantee. 1000 is several times
/// more than any legitimate batch operation.
const MAX_UIDS_PER_CALL: usize = 1000;

fn uid_cap_error() -> String {
    error_json(&format!(
        "uids list exceeds {MAX_UIDS_PER_CALL}-item cap — batch into smaller calls"
    ))
}

macro_rules! resolve_write {
    ($server:expr, $req:expr) => {{
        if $req.uids.len() > MAX_UIDS_PER_CALL {
            return uid_cap_error();
        }
        let (config, client_arc) = match $server.resolve_client($req.account.as_deref()) {
            Ok(r) => r,
            Err(e) => return error_json(&e),
        };
        if config.read_only {
            return error_json("Account is configured as read-only");
        }
        (config.name.clone(), client_arc)
    }};
}

pub async fn move_email(server: &ImapMcpServer, req: MoveEmailRequest) -> String {
    if req.uids.len() > MAX_UIDS_PER_CALL {
        return uid_cap_error();
    }
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
    let account_name = config.name.clone();
    if req.dry_run.unwrap_or(false) {
        // No IMAP roundtrip — returns the LLM a preview it can show before
        // calling the real op. Permission checks above still fired, so this
        // also confirms the action *would* be allowed.
        return serde_json::to_string(&serde_json::json!({
            "account": account_name,
            "dry_run": true,
            "folder": req.folder,
            "target_folder": req.target_folder,
            "uids": req.uids,
            "would_move": req.uids.len(),
        }))
        .unwrap_or_else(|e| error_json(&e.to_string()));
    }
    let mut client = client_arc.lock().await;
    match client
        .move_emails(&req.folder, &req.uids, &req.target_folder)
        .await
    {
        Ok(succeeded) => write_ok(&account_name, &succeeded),
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

pub async fn mark_as_read(server: &ImapMcpServer, req: MarkReadRequest) -> String {
    let (account_name, client_arc) = resolve_write!(server, req);
    let mut client = client_arc.lock().await;
    match client
        .mark_flags(&req.folder, &req.uids, "\\Seen", true)
        .await
    {
        Ok(succeeded) => write_ok(&account_name, &succeeded),
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

pub async fn mark_as_unread(server: &ImapMcpServer, req: MarkUnreadRequest) -> String {
    let (account_name, client_arc) = resolve_write!(server, req);
    let mut client = client_arc.lock().await;
    match client
        .mark_flags(&req.folder, &req.uids, "\\Seen", false)
        .await
    {
        Ok(succeeded) => write_ok(&account_name, &succeeded),
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

pub async fn flag_email(server: &ImapMcpServer, req: FlagEmailRequest) -> String {
    let (account_name, client_arc) = resolve_write!(server, req);
    let mut client = client_arc.lock().await;
    match client
        .mark_flags(&req.folder, &req.uids, "\\Flagged", true)
        .await
    {
        Ok(succeeded) => write_ok(&account_name, &succeeded),
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

pub async fn unflag_email(server: &ImapMcpServer, req: UnflagEmailRequest) -> String {
    let (account_name, client_arc) = resolve_write!(server, req);
    let mut client = client_arc.lock().await;
    match client
        .mark_flags(&req.folder, &req.uids, "\\Flagged", false)
        .await
    {
        Ok(succeeded) => write_ok(&account_name, &succeeded),
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

pub async fn delete_email(server: &ImapMcpServer, req: DeleteEmailRequest) -> String {
    if req.uids.len() > MAX_UIDS_PER_CALL {
        return uid_cap_error();
    }
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
    let account_name = config.name.clone();
    let permanent = req.permanent.unwrap_or(false);
    if req.dry_run.unwrap_or(false) {
        // No IMAP roundtrip. Clear preview of whether this would move to
        // Trash (recoverable) or EXPUNGE permanently.
        let action = if permanent {
            "would_expunge_permanently"
        } else {
            "would_move_to_trash"
        };
        return serde_json::to_string(&serde_json::json!({
            "account": account_name,
            "dry_run": true,
            "folder": req.folder,
            "uids": req.uids,
            "permanent": permanent,
            action: req.uids.len(),
        }))
        .unwrap_or_else(|e| error_json(&e.to_string()));
    }
    let mut client = client_arc.lock().await;
    match client
        .delete_emails(&req.folder, &req.uids, permanent)
        .await
    {
        Ok(succeeded) => write_ok(&account_name, &succeeded),
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}
