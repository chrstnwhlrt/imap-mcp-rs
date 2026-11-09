//! Mutating MCP tools: flag, mark read/unread, move, delete.
//!
//! Each tool checks `account_config.read_only` and the per-account
//! `allow_move` / `allow_delete` / `allow_flag_change` switches before
//! issuing IMAP commands — defense-in-depth so an LLM that ignores the schema
//! description can't corrupt mailboxes that the user marked off-limits.

use rmcp::schemars;
use serde::Deserialize;

use super::{ImapMcpServer, ToolResult, error_json};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MoveEmailRequest {
    #[schemars(
        description = "Account name (from list_accounts), matched case-insensitively. Optional only when a single account is configured; with multiple accounts it is required — omitting it errors and lists the names."
    )]
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
    #[schemars(
        description = "Account name (from list_accounts), matched case-insensitively. Optional only when a single account is configured; with multiple accounts it is required — omitting it errors and lists the names."
    )]
    pub account: Option<String>,
    #[schemars(description = "Folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(
        description = "Email UIDs to mark as read (from list_emails or search_emails results)"
    )]
    pub uids: Vec<u32>,
    #[schemars(
        description = "If true, validate permissions + inputs but don't touch IMAP; returns a preview payload. Default: false."
    )]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MarkUnreadRequest {
    #[schemars(
        description = "Account name (from list_accounts), matched case-insensitively. Optional only when a single account is configured; with multiple accounts it is required — omitting it errors and lists the names."
    )]
    pub account: Option<String>,
    #[schemars(description = "Folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(
        description = "Email UIDs to mark as unread (from list_emails or search_emails results)"
    )]
    pub uids: Vec<u32>,
    #[schemars(
        description = "If true, validate permissions + inputs but don't touch IMAP; returns a preview payload. Default: false."
    )]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FlagEmailRequest {
    #[schemars(
        description = "Account name (from list_accounts), matched case-insensitively. Optional only when a single account is configured; with multiple accounts it is required — omitting it errors and lists the names."
    )]
    pub account: Option<String>,
    #[schemars(description = "Folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(description = "Email UIDs to flag (from list_emails or search_emails results)")]
    pub uids: Vec<u32>,
    #[schemars(
        description = "If true, validate permissions + inputs but don't touch IMAP; returns a preview payload. Default: false."
    )]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UnflagEmailRequest {
    #[schemars(
        description = "Account name (from list_accounts), matched case-insensitively. Optional only when a single account is configured; with multiple accounts it is required — omitting it errors and lists the names."
    )]
    pub account: Option<String>,
    #[schemars(description = "Folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(description = "Email UIDs to unflag (from list_emails or search_emails results)")]
    pub uids: Vec<u32>,
    #[schemars(
        description = "If true, validate permissions + inputs but don't touch IMAP; returns a preview payload. Default: false."
    )]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteEmailRequest {
    #[schemars(
        description = "Account name (from list_accounts), matched case-insensitively. Optional only when a single account is configured; with multiple accounts it is required — omitting it errors and lists the names."
    )]
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

/// Response of the four flag tools. `account` is always included so the LLM
/// can disambiguate when calling tools on multiple accounts in parallel.
/// `succeeded` lists the UIDs whose flags the server acknowledged changing
/// (see `imap_client::acknowledged_uids`); a UID already in the target state
/// or unknown (stale after UIDVALIDITY rotation, externally expunged, typo)
/// is omitted rather than echoed. There is deliberately no `failed` field
/// here — see [`write_result`] for why the destructive tools have one and
/// these do not.
fn write_ok(account: &str, succeeded: &[u32]) -> ToolResult {
    serde_json::to_string(&serde_json::json!({
        "account": account,
        "succeeded": succeeded,
    }))
    .map_err(|e| error_json(&e.to_string()))
}

/// Result shape for the existence-verified destructive tools (move/delete):
/// `failed` spells out `input − succeeded`, because a caller checking only
/// "no error came back" reads a partial success as a full one — an empty
/// array is a statement, a gap in `succeeded` is not. The flag tools keep
/// the two-field shape: for them the difference means "already in the
/// target state OR unknown", and calling that `failed` would be the next
/// misdirection.
pub(super) fn write_result(account: &str, succeeded: &[u32], input: &[u32]) -> ToolResult {
    let done: std::collections::HashSet<u32> = succeeded.iter().copied().collect();
    let mut failed: Vec<u32> = input
        .iter()
        .copied()
        .filter(|u| !done.contains(u))
        .collect();
    failed.sort_unstable();
    failed.dedup();
    serde_json::to_string(&serde_json::json!({
        "account": account,
        "succeeded": succeeded,
        "failed": failed,
    }))
    .map_err(|e| error_json(&e.to_string()))
}

/// Upper bound on UIDs per write call. A prompt-injected LLM passing a
/// ludicrously long list (e.g. 10M UIDs) would force a ~110 MB
/// `uid_set_string` allocation and a gigantic IMAP STORE/COPY command the
/// server likely rejects anyway. MCP JSON-RPC has a rough ceiling from the
/// transport layer, but that's not a hard guarantee. 1000 is several times
/// more than any legitimate batch operation.
const MAX_UIDS_PER_CALL: usize = 1000;

fn uid_cap_error() -> ToolResult {
    Err(error_json(&format!(
        "uids list exceeds {MAX_UIDS_PER_CALL}-item cap — batch into smaller calls"
    )))
}

// Shared head of the four flag tools (mark read/unread, flag/unflag):
// cap, account resolution, read_only, and the `allow_flag_change` gate —
// the unread state is often a human's work queue, so flag writes get the
// same per-account switch move and delete always had.
macro_rules! resolve_write {
    ($server:expr, $req:expr) => {{
        if $req.uids.len() > MAX_UIDS_PER_CALL {
            return uid_cap_error();
        }
        let (config, client_arc) = match $server.resolve_client($req.account.as_deref()) {
            Ok(r) => r,
            Err(e) => return Err(error_json(&e)),
        };
        if config.read_only {
            return Err(error_json("Account is configured as read-only"));
        }
        if !config.allow_flag_change {
            return Err(error_json(
                "Flag changes are disabled for this account (allow_flag_change = false)",
            ));
        }
        (config.name.clone(), client_arc)
    }};
}

pub async fn move_email(server: &ImapMcpServer, req: MoveEmailRequest) -> ToolResult {
    if req.uids.len() > MAX_UIDS_PER_CALL {
        return uid_cap_error();
    }
    let (config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return Err(error_json(&e)),
    };
    if config.read_only {
        return Err(error_json("Account is configured as read-only"));
    }
    if !config.allow_move {
        return Err(error_json(
            "Moving emails is disabled for this account (allow_move = false)",
        ));
    }
    let account_name = config.name.clone();
    // Checked before the preview too: a dry run must not promise a move the
    // real call refuses (see `ImapClient::move_emails`). By the names as
    // given — the preview makes no IMAP call, so a role word the real call
    // resolves to the source folder ("Posteingang" for INBOX) is caught only
    // there.
    if crate::imap_client::same_folder(&req.folder, &req.target_folder) {
        return Err(error_json(&format!(
            "Source and target folder are the same (\"{}\") — there is nothing to move",
            crate::email::sanitize_external_str(&req.folder)
        )));
    }
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
        .map_err(|e| error_json(&e.to_string()));
    }
    let mut client = client_arc.lock().await;
    let folder = super::folder_for(&mut client, &req.folder).await?;
    let target = super::folder_for(&mut client, &req.target_folder).await?;
    match client.move_emails(&folder, &req.uids, &target).await {
        Ok(succeeded) => write_result(&account_name, &succeeded, &req.uids),
        Err(e) => Err(error_json(&client.check_error(e).to_string())),
    }
}

pub async fn mark_as_read(server: &ImapMcpServer, req: MarkReadRequest) -> ToolResult {
    let (account_name, client_arc) = resolve_write!(server, req);
    if req.dry_run.unwrap_or(false) {
        // No IMAP roundtrip; the gates above still fired, so the preview
        // also confirms the action would be allowed.
        return serde_json::to_string(&serde_json::json!({
            "account": account_name,
            "dry_run": true,
            "folder": req.folder,
            "uids": req.uids,
            "would_mark_read": req.uids.len(),
        }))
        .map_err(|e| error_json(&e.to_string()));
    }
    let mut client = client_arc.lock().await;
    let folder = super::folder_for(&mut client, &req.folder).await?;
    match client.mark_flags(&folder, &req.uids, "\\Seen", true).await {
        Ok(succeeded) => write_ok(&account_name, &succeeded),
        Err(e) => Err(error_json(&client.check_error(e).to_string())),
    }
}

pub async fn mark_as_unread(server: &ImapMcpServer, req: MarkUnreadRequest) -> ToolResult {
    let (account_name, client_arc) = resolve_write!(server, req);
    if req.dry_run.unwrap_or(false) {
        // No IMAP roundtrip; the gates above still fired, so the preview
        // also confirms the action would be allowed.
        return serde_json::to_string(&serde_json::json!({
            "account": account_name,
            "dry_run": true,
            "folder": req.folder,
            "uids": req.uids,
            "would_mark_unread": req.uids.len(),
        }))
        .map_err(|e| error_json(&e.to_string()));
    }
    let mut client = client_arc.lock().await;
    let folder = super::folder_for(&mut client, &req.folder).await?;
    match client.mark_flags(&folder, &req.uids, "\\Seen", false).await {
        Ok(succeeded) => write_ok(&account_name, &succeeded),
        Err(e) => Err(error_json(&client.check_error(e).to_string())),
    }
}

pub async fn flag_email(server: &ImapMcpServer, req: FlagEmailRequest) -> ToolResult {
    let (account_name, client_arc) = resolve_write!(server, req);
    if req.dry_run.unwrap_or(false) {
        // No IMAP roundtrip; the gates above still fired, so the preview
        // also confirms the action would be allowed.
        return serde_json::to_string(&serde_json::json!({
            "account": account_name,
            "dry_run": true,
            "folder": req.folder,
            "uids": req.uids,
            "would_flag": req.uids.len(),
        }))
        .map_err(|e| error_json(&e.to_string()));
    }
    let mut client = client_arc.lock().await;
    let folder = super::folder_for(&mut client, &req.folder).await?;
    match client
        .mark_flags(&folder, &req.uids, "\\Flagged", true)
        .await
    {
        Ok(succeeded) => write_ok(&account_name, &succeeded),
        Err(e) => Err(error_json(&client.check_error(e).to_string())),
    }
}

pub async fn unflag_email(server: &ImapMcpServer, req: UnflagEmailRequest) -> ToolResult {
    let (account_name, client_arc) = resolve_write!(server, req);
    if req.dry_run.unwrap_or(false) {
        // No IMAP roundtrip; the gates above still fired, so the preview
        // also confirms the action would be allowed.
        return serde_json::to_string(&serde_json::json!({
            "account": account_name,
            "dry_run": true,
            "folder": req.folder,
            "uids": req.uids,
            "would_unflag": req.uids.len(),
        }))
        .map_err(|e| error_json(&e.to_string()));
    }
    let mut client = client_arc.lock().await;
    let folder = super::folder_for(&mut client, &req.folder).await?;
    match client
        .mark_flags(&folder, &req.uids, "\\Flagged", false)
        .await
    {
        Ok(succeeded) => write_ok(&account_name, &succeeded),
        Err(e) => Err(error_json(&client.check_error(e).to_string())),
    }
}

pub async fn delete_email(server: &ImapMcpServer, req: DeleteEmailRequest) -> ToolResult {
    if req.uids.len() > MAX_UIDS_PER_CALL {
        return uid_cap_error();
    }
    let (config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return Err(error_json(&e)),
    };
    if config.read_only {
        return Err(error_json("Account is configured as read-only"));
    }
    if !config.allow_delete {
        return Err(error_json(
            "Deleting emails is disabled for this account (allow_delete = false)",
        ));
    }
    let account_name = config.name.clone();
    let permanent = req.permanent.unwrap_or(false);
    // The refusal the real call makes for the Trash folder itself (see
    // `ImapClient::delete_emails`), decided from the folder name here so the
    // preview stays IMAP-free and does not promise a move the call refuses.
    // A name only the real call resolves to the trash (a role word such as
    // "Müll") is refused there.
    if !permanent && crate::imap_client::detect_folder_role(&req.folder) == Some("trash") {
        return Err(error_json(&format!(
            "\"{}\" is the Trash folder — moving its messages to Trash changes nothing. Pass \
             `permanent: true` to delete them for good",
            crate::email::sanitize_external_str(&req.folder)
        )));
    }
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
        .map_err(|e| error_json(&e.to_string()));
    }
    let mut client = client_arc.lock().await;
    let folder = super::folder_for(&mut client, &req.folder).await?;
    match client.delete_emails(&folder, &req.uids, permanent).await {
        Ok(succeeded) => write_result(&account_name, &succeeded, &req.uids),
        Err(e) => Err(error_json(&client.check_error(e).to_string())),
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
            dry_run: None,
        }
    }
    fn mark_unread_req(uids: Vec<u32>) -> MarkUnreadRequest {
        MarkUnreadRequest {
            account: None,
            folder: "INBOX".into(),
            uids,
            dry_run: None,
        }
    }
    fn flag_req(uids: Vec<u32>) -> FlagEmailRequest {
        FlagEmailRequest {
            account: None,
            folder: "INBOX".into(),
            uids,
            dry_run: None,
        }
    }
    fn unflag_req(uids: Vec<u32>) -> UnflagEmailRequest {
        UnflagEmailRequest {
            account: None,
            folder: "INBOX".into(),
            uids,
            dry_run: None,
        }
    }

    /// The text of an error result — the call must have failed, i.e. reached
    /// the client as `isError: true`.
    fn refused(out: ToolResult) -> String {
        out.expect_err("expected an error result")
    }

    /// The parsed JSON of a success result — the call must have succeeded.
    fn ok_json(out: ToolResult) -> serde_json::Value {
        serde_json::from_str(&out.expect("expected a success result")).expect("valid json")
    }

    /// The text of either outcome, for checks that a gate did NOT fire: with
    /// the gate open the call goes on to the (closed) server port and fails
    /// there, which is fine — only the gate's own refusal is ruled out.
    fn text(out: &ToolResult) -> &str {
        match out {
            Ok(s) | Err(s) => s,
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
        for (i, out) in refusals.into_iter().enumerate() {
            let err = refused(out);
            assert!(
                err.contains("read-only"),
                "tool #{i} did not refuse a read-only account: {err}"
            );
        }
    }

    /// The finer-grained gates: an account may allow flag changes while
    /// refusing to move or delete. Each must be refused on its own switch,
    /// and — equally important — must not be refused when it is enabled.
    #[tokio::test]
    async fn move_and_delete_honour_their_own_switches() {
        let no_move = server_with("allow_move = false\nallow_delete = true");
        let err = refused(move_email(&no_move, move_req(vec![1])).await);
        assert!(err.contains("allow_move"), "{err}");

        let no_delete = server_with("allow_move = true\nallow_delete = false");
        let err = refused(delete_email(&no_delete, delete_req(vec![1], false)).await);
        assert!(err.contains("allow_delete"), "{err}");

        // With the switch on, the refusal must be gone — otherwise the gate
        // would be indistinguishable from the feature being broken.
        let allowed = server_with("allow_move = true\nallow_delete = true");
        let out = move_email(&allowed, move_req(vec![1])).await;
        let t = text(&out);
        assert!(
            !t.contains("allow_move") && !t.contains("read-only"),
            "an enabled account must not be refused: {t}"
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
            let err = refused(out);
            assert!(err.contains("cap"), "oversized list not refused: {err}");
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
        let v = ok_json(move_email(&s, req).await);
        assert_eq!(v["dry_run"], true, "{v}");
        assert_eq!(v["would_move"], 2, "{v}");
        assert_eq!(v["target_folder"], "Archive", "{v}");

        let mut req = delete_req(vec![7], true);
        req.dry_run = Some(true);
        let v = ok_json(delete_email(&s, req).await);
        assert_eq!(v["dry_run"], true, "{v}");
        assert_eq!(v["would_expunge_permanently"], 1, "{v}");
    }

    /// A preview must still respect the gates: previewing an action the
    /// account may not perform would suggest it is available.
    #[tokio::test]
    async fn dry_run_still_respects_the_permission_gates() {
        let s = server_with("read_only = true");
        let mut req = move_req(vec![1]);
        req.dry_run = Some(true);
        assert!(refused(move_email(&s, req).await).contains("read-only"));

        let s = server_with("allow_delete = false");
        let mut req = delete_req(vec![1], false);
        req.dry_run = Some(true);
        assert!(refused(delete_email(&s, req).await).contains("allow_delete"));

        let s = server_with("allow_flag_change = false");
        let mut req = mark_read_req(vec![1]);
        req.dry_run = Some(true);
        assert!(refused(mark_as_read(&s, req).await).contains("allow_flag_change"));
    }

    /// The unread state is often a human's work queue: a bulk `mark_as_read`
    /// erases it with no trash and no record of which messages it hit. The
    /// flag tools therefore get the same per-account gate move and delete
    /// always had — and it must cover all four, in both directions.
    #[tokio::test]
    async fn allow_flag_change_gates_all_four_flag_tools() {
        let s = server_with("allow_flag_change = false");
        for out in [
            mark_as_read(&s, mark_read_req(vec![1])).await,
            mark_as_unread(&s, mark_unread_req(vec![1])).await,
            flag_email(&s, flag_req(vec![1])).await,
            unflag_email(&s, unflag_req(vec![1])).await,
        ] {
            let err = refused(out);
            assert!(err.contains("allow_flag_change"), "{err}");
        }
        // …and it must not block the unrelated tools.
        let out = move_email(&s, move_req(vec![1])).await;
        assert!(!text(&out).contains("allow_flag_change"), "{out:?}");

        // Enabled (the default): the refusal must be gone.
        let s = server_with("");
        let out = mark_as_read(&s, mark_read_req(vec![1])).await;
        assert!(!text(&out).contains("allow_flag_change"), "{out:?}");
    }

    /// Every flag tool previews without touching IMAP — against a closed
    /// port a real attempt could not succeed, so a well-formed preview
    /// proves nothing reached the network.
    #[tokio::test]
    async fn flag_tools_preview_with_dry_run() {
        let s = server_with("");
        for (out, field) in [
            (
                {
                    let mut r = mark_read_req(vec![7, 8]);
                    r.dry_run = Some(true);
                    mark_as_read(&s, r).await
                },
                "would_mark_read",
            ),
            (
                {
                    let mut r = mark_unread_req(vec![7]);
                    r.dry_run = Some(true);
                    mark_as_unread(&s, r).await
                },
                "would_mark_unread",
            ),
            (
                {
                    let mut r = flag_req(vec![7]);
                    r.dry_run = Some(true);
                    flag_email(&s, r).await
                },
                "would_flag",
            ),
            (
                {
                    let mut r = unflag_req(vec![7]);
                    r.dry_run = Some(true);
                    unflag_email(&s, r).await
                },
                "would_unflag",
            ),
        ] {
            let v = ok_json(out);
            assert_eq!(v["dry_run"], true, "{v}");
            assert!(v.get(field).is_some(), "{field} missing: {v}");
        }
    }

    /// Moving a folder into itself re-creates its messages under new UIDs
    /// while nothing moves; refused before any IMAP traffic — the preview
    /// included, which would otherwise promise a move the real call refuses.
    #[tokio::test]
    async fn move_into_the_source_folder_is_refused_even_as_preview() {
        let s = server_with("");
        let mut req = move_req(vec![1]);
        req.target_folder = "INBOX".into();
        assert!(refused(move_email(&s, req).await).contains("same"));
        let mut req = move_req(vec![1]);
        req.target_folder = "inbox".into();
        req.dry_run = Some(true);
        assert!(refused(move_email(&s, req).await).contains("same"));
    }

    /// A soft delete from the Trash would move its messages onto themselves;
    /// the preview refuses it like the real call, without IMAP.
    #[tokio::test]
    async fn soft_delete_from_trash_is_refused_even_as_preview() {
        let s = server_with("");
        let mut req = delete_req(vec![1], false);
        req.folder = "Papierkorb".into();
        req.dry_run = Some(true);
        assert!(refused(delete_email(&s, req).await).contains("permanent"));
        // Permanent deletion from the Trash stays possible.
        let mut req = delete_req(vec![1], true);
        req.folder = "Trash".into();
        req.dry_run = Some(true);
        let v = ok_json(delete_email(&s, req).await);
        assert_eq!(v["would_expunge_permanently"], 1, "{v}");
    }

    /// `failed` spells out the difference so a partial success can never be
    /// read as a full one by a caller that only checks for an error.
    #[test]
    fn write_result_reports_the_failed_difference() {
        let v = ok_json(write_result("A", &[1, 3], &[1, 2, 3, 4]));
        assert_eq!(v["succeeded"], serde_json::json!([1, 3]));
        assert_eq!(v["failed"], serde_json::json!([2, 4]));

        // Full success: failed is PRESENT and empty — an empty array is a
        // statement, an absent field is not.
        let v = ok_json(write_result("A", &[1, 2], &[1, 2]));
        assert_eq!(v["failed"], serde_json::json!([]));
    }
}
