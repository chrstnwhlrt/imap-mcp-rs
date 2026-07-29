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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;
    use crate::imap_client::ImapClient;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// Build a server whose account carries the given permission lines.
    ///
    /// The host points at a closed port on purpose: every check here is meant
    /// to fire *before* any IMAP command goes out. If one silently stopped
    /// doing so, the call would attempt a connection instead of returning the
    /// refusal, and the assertion fails rather than quietly passing.
    fn server_with(permissions: &str) -> ImapMcpServer {
        let toml_src = format!(
            r#"
            [[accounts]]
            name = "T"
            host = "127.0.0.1"
            port = 1
            username = "u@h"
            auth_method = "password"
            password = "p"
            {permissions}
            "#
        );
        let config: ServerConfig = toml::from_str(&toml_src).expect("test config");
        let clients: HashMap<_, _> = config
            .accounts
            .iter()
            .map(|a| {
                (
                    a.name.to_lowercase(),
                    Arc::new(Mutex::new(ImapClient::new(a.clone()))),
                )
            })
            .collect();
        ImapMcpServer::new(config, clients)
    }

    fn move_req(uids: Vec<u32>) -> MoveEmailRequest {
        MoveEmailRequest {
            account: None,
            folder: "INBOX".into(),
            uids,
            target_folder: "Archive".into(),
            dry_run: None,
        }
    }

    fn delete_req(uids: Vec<u32>, permanent: bool) -> DeleteEmailRequest {
        DeleteEmailRequest {
            account: None,
            folder: "INBOX".into(),
            uids,
            permanent: Some(permanent),
            dry_run: None,
        }
    }

    // Each flag tool has its own request type with identical fields.
    fn mark_read_req(uids: Vec<u32>) -> MarkReadRequest {
        MarkReadRequest {
            account: None,
            folder: "INBOX".into(),
            uids,
        }
    }
    fn mark_unread_req(uids: Vec<u32>) -> MarkUnreadRequest {
        MarkUnreadRequest {
            account: None,
            folder: "INBOX".into(),
            uids,
        }
    }
    fn flag_req(uids: Vec<u32>) -> FlagEmailRequest {
        FlagEmailRequest {
            account: None,
            folder: "INBOX".into(),
            uids,
        }
    }
    fn unflag_req(uids: Vec<u32>) -> UnflagEmailRequest {
        UnflagEmailRequest {
            account: None,
            folder: "INBOX".into(),
            uids,
        }
    }

    /// `read_only` is the switch a user sets to make an account observable but
    /// untouchable. It has to hold for *every* mutating tool — one that
    /// forgets to consult it would silently write to a mailbox declared safe.
    #[tokio::test]
    async fn read_only_blocks_every_mutating_tool() {
        let s = server_with("read_only = true");
        let refusals = [
            move_email(&s, move_req(vec![1])).await,
            delete_email(&s, delete_req(vec![1], false)).await,
            mark_as_read(&s, mark_read_req(vec![1])).await,
            mark_as_unread(&s, mark_unread_req(vec![1])).await,
            flag_email(&s, flag_req(vec![1])).await,
            unflag_email(&s, unflag_req(vec![1])).await,
        ];
        for (i, out) in refusals.iter().enumerate() {
            assert!(
                out.contains("read-only"),
                "tool #{i} did not refuse a read-only account: {out}"
            );
        }
    }

    /// The finer-grained gates: an account may allow flag changes while
    /// refusing to move or delete. Each must be refused on its own switch,
    /// and — equally important — must not be refused when it is enabled.
    #[tokio::test]
    async fn move_and_delete_honour_their_own_switches() {
        let no_move = server_with("allow_move = false\nallow_delete = true");
        let out = move_email(&no_move, move_req(vec![1])).await;
        assert!(out.contains("allow_move"), "{out}");

        let no_delete = server_with("allow_move = true\nallow_delete = false");
        let out = delete_email(&no_delete, delete_req(vec![1], false)).await;
        assert!(out.contains("allow_delete"), "{out}");

        // With the switch on, the refusal must be gone — otherwise the gate
        // would be indistinguishable from the feature being broken.
        let allowed = server_with("allow_move = true\nallow_delete = true");
        let out = move_email(&allowed, move_req(vec![1])).await;
        assert!(
            !out.contains("allow_move") && !out.contains("read-only"),
            "an enabled account must not be refused: {out}"
        );
    }

    /// The cap exists so a prompt-injected request cannot ask for a
    /// hundred-thousand-UID command. Rejecting is the point: silently
    /// truncating would act on a different set than the caller asked for.
    #[tokio::test]
    async fn oversized_uid_lists_are_refused_not_truncated() {
        let s = server_with("allow_move = true\nallow_delete = true");
        let over_cap = u32::try_from(MAX_UIDS_PER_CALL)
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        let too_many: Vec<u32> = (1..=over_cap).collect();
        for out in [
            move_email(&s, move_req(too_many.clone())).await,
            delete_email(&s, delete_req(too_many.clone(), false)).await,
            mark_as_read(&s, mark_read_req(too_many)).await,
        ] {
            assert!(out.contains("cap"), "oversized list not refused: {out}");
        }
    }

    /// `dry_run` promises a preview "without touching IMAP". Against a closed
    /// port that promise is verifiable: a real attempt could not succeed, so a
    /// well-formed preview proves nothing reached the network.
    #[tokio::test]
    async fn dry_run_previews_without_contacting_the_server() {
        let s = server_with("allow_move = true\nallow_delete = true");

        let mut req = move_req(vec![7, 8]);
        req.dry_run = Some(true);
        let out = move_email(&s, req).await;
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(v["dry_run"], true, "{out}");
        assert_eq!(v["would_move"], 2, "{out}");
        assert_eq!(v["target_folder"], "Archive", "{out}");
        assert!(v.get("error").is_none(), "preview must not error: {out}");

        let mut req = delete_req(vec![7], true);
        req.dry_run = Some(true);
        let out = delete_email(&s, req).await;
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(v["dry_run"], true, "{out}");
        assert_eq!(v["would_expunge_permanently"], 1, "{out}");
    }

    /// A preview must still respect the gates: previewing an action the
    /// account may not perform would suggest it is available.
    #[tokio::test]
    async fn dry_run_still_respects_the_permission_gates() {
        let s = server_with("read_only = true");
        let mut req = move_req(vec![1]);
        req.dry_run = Some(true);
        assert!(move_email(&s, req).await.contains("read-only"));

        let s = server_with("allow_delete = false");
        let mut req = delete_req(vec![1], false);
        req.dry_run = Some(true);
        assert!(delete_email(&s, req).await.contains("allow_delete"));
    }
}
