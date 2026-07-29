//! Draft composition: reply, forward, and fresh-compose.
//!
//! All three follow the same skeleton:
//! 1. Resolve account + check `read_only`.
//! 2. (Reply / forward only) fetch the original — under a short mutex hold.
//! 3. Build sanitized recipient lists and the plaintext + HTML bodies — pure
//!    CPU work, mutex released.
//! 4. APPEND to the account's Drafts folder.
//!
//! Sanitization runs at the boundary between untrusted input (parsed mail
//! headers, LLM tool args) and `mail-builder`: every recipient and
//! Message-ID passes through [`sanitize_header_value`] to strip CR/LF that
//! would otherwise inject extra headers (e.g. a silent `Bcc:`).
//!
//! Rendering helpers (Locale presets, Outlook Web HTML) live in [`render`].

use std::sync::Arc;

use mail_builder::MessageBuilder;
use rmcp::schemars;
use serde::Deserialize;

use tokio::sync::Mutex;

use crate::email::EmailFull;
use crate::imap_client::ImapClient;

use super::{ImapMcpServer, error_json};

mod render;
use render::{Locale, apply_from, build_compose_html, build_forward_bodies, build_reply_bodies};

// ========== Request types ==========

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftReplyRequest {
    #[schemars(description = "Account name (from list_accounts); default: first configured.")]
    pub account: Option<String>,
    #[schemars(description = "Folder containing the email to reply to (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(description = "Email UID to reply to (from list_emails or search_emails results)")]
    pub uid: u32,
    #[schemars(
        description = "Plain-text reply body. Rendered to HTML automatically; the original is quoted with a locale-aware intro below."
    )]
    pub body: String,
    #[schemars(
        description = "Reply-all: include original To and CC recipients (your own address is excluded). Default: false."
    )]
    pub reply_all: Option<bool>,
    #[schemars(
        description = "Additional CC email addresses, e.g. [\"alice@example.com\"]. Appended to any recipients from reply_all."
    )]
    pub cc: Option<Vec<String>>,
    #[schemars(
        description = "Absolute file paths to attach (e.g. from download_attachment's `saved_to`). Must be inside allowed_attachment_dirs (default: `$XDG_RUNTIME_DIR/imap-mcp-rs`)."
    )]
    pub attachments: Option<Vec<String>>,
    #[schemars(
        description = "UID of a draft this one replaces — this tool returns `uid` on every save, so a revision loop can pass the previous one straight back without calling list_drafts. The new draft is saved first, the old one deleted only afterwards — use this instead of delete_draft + draft_* so a failure can never leave you with neither version."
    )]
    pub replaces_uid: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftForwardRequest {
    #[schemars(description = "Account name (from list_accounts); default: first configured.")]
    pub account: Option<String>,
    #[schemars(description = "Folder containing the email to forward (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(description = "Email UID to forward (from list_emails or search_emails results)")]
    pub uid: u32,
    #[schemars(
        description = "Recipient email addresses, e.g. [\"alice@example.com\"]. At least one required — forwarding never auto-selects recipients."
    )]
    pub to: Vec<String>,
    #[schemars(
        description = "Optional plain-text message placed ABOVE the forwarded content. If omitted, only the forwarded content is included."
    )]
    pub body: Option<String>,
    #[schemars(description = "Optional CC email addresses, e.g. [\"alice@example.com\"]")]
    pub cc: Option<Vec<String>>,
    #[schemars(
        description = "Absolute file paths to attach (e.g. from download_attachment's `saved_to`). Must be inside allowed_attachment_dirs (default: `$XDG_RUNTIME_DIR/imap-mcp-rs`)."
    )]
    pub attachments: Option<Vec<String>>,
    #[schemars(
        description = "UID of a draft this one replaces — this tool returns `uid` on every save, so a revision loop can pass the previous one straight back without calling list_drafts. The new draft is saved first, the old one deleted only afterwards — use this instead of delete_draft + draft_* so a failure can never leave you with neither version."
    )]
    pub replaces_uid: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteDraftRequest {
    #[schemars(description = "Account name (from list_accounts); default: first configured.")]
    pub account: Option<String>,
    #[schemars(description = "Draft UIDs to delete (from list_drafts results). Pass one or many.")]
    pub uids: Vec<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftEmailRequest {
    #[schemars(description = "Account name (from list_accounts); default: first configured.")]
    pub account: Option<String>,
    #[schemars(
        description = "Recipient email addresses, e.g. [\"alice@example.com\"]. At least one required."
    )]
    pub to: Vec<String>,
    #[schemars(description = "Email subject line (plain text)")]
    pub subject: String,
    #[schemars(description = "Plain-text email body. Rendered to HTML automatically.")]
    pub body: String,
    #[schemars(description = "CC email addresses, e.g. [\"alice@example.com\"]")]
    pub cc: Option<Vec<String>>,
    #[schemars(
        description = "BCC email addresses (hidden from other recipients), e.g. [\"alice@example.com\"]"
    )]
    pub bcc: Option<Vec<String>>,
    #[schemars(
        description = "Absolute file paths to attach (e.g. from download_attachment's `saved_to`). Must be inside allowed_attachment_dirs (default: `$XDG_RUNTIME_DIR/imap-mcp-rs`)."
    )]
    pub attachments: Option<Vec<String>>,
    #[schemars(
        description = "UID of a draft this one replaces — this tool returns `uid` on every save, so a revision loop can pass the previous one straight back without calling list_drafts. The new draft is saved first, the old one deleted only afterwards — use this instead of delete_draft + draft_* so a failure can never leave you with neither version."
    )]
    pub replaces_uid: Option<u32>,
}

// ========== Tool implementations ==========

/// Common tail of all three draft tools: report the new draft's UID, honour
/// `replaces_uid` (if given) and serialize. Keeping it here means the
/// append-then-expunge order is implemented once rather than three times.
///
/// `uid` is absent when the server did not let us identify the appended
/// message; the draft was still saved, so it is omitted rather than nulled.
async fn finish_draft(
    client_arc: &Arc<Mutex<ImapClient>>,
    uid: Option<u32>,
    replaces_uid: Option<u32>,
    mut response: serde_json::Value,
) -> String {
    if let Some(uid) = uid {
        response["uid"] = serde_json::json!(uid);
    }
    if let Some(old_uid) = replaces_uid {
        remove_replaced_draft(client_arc, old_uid, &mut response).await;
    }
    serde_json::to_string(&response).unwrap_or_else(|e| error_json(&e.to_string()))
}

/// Delete the draft a newly saved one replaces. Called *after* the save
/// succeeded, never before: IMAP cannot update a message in place, so the
/// only safe order is append-then-expunge. A failure here is reported as a
/// warning rather than an error — the new draft exists either way, and the
/// stale one is a nuisance, not a loss.
async fn remove_replaced_draft(
    client_arc: &Arc<Mutex<ImapClient>>,
    replaces_uid: u32,
    response: &mut serde_json::Value,
) {
    // Hold the account mutex only for the IMAP round-trip, not while
    // formatting the response — same pattern as the draft builders above.
    let outcome = {
        let mut client = client_arc.lock().await;
        client
            .delete_draft(&[replaces_uid])
            .await
            .map_err(|e| client.check_error(e).to_string())
    };
    note_replacement(response, replaces_uid, outcome.as_deref());
}

/// Record what became of the replaced draft. Split out from the IMAP call so
/// all three outcomes — removed, silently absent, or refused — can be tested
/// without a server; only the happy path is reachable from the `GreenMail`
/// suite.
fn note_replacement(
    response: &mut serde_json::Value,
    replaces_uid: u32,
    outcome: Result<&[u32], &String>,
) {
    match outcome {
        Ok(succeeded) if succeeded.contains(&replaces_uid) => {
            response["replaced_uid"] = serde_json::json!(replaces_uid);
        }
        // The server accepted the command but did not report the UID gone —
        // most often it was already deleted, or the UID never existed.
        Ok(_) => {
            response["replace_warning"] = serde_json::json!(format!(
                "New draft saved, but draft {replaces_uid} was not deleted (already gone or unknown UID) — check list_drafts for leftovers"
            ));
        }
        Err(msg) => {
            response["replace_warning"] = serde_json::json!(format!(
                "New draft saved, but deleting draft {replaces_uid} failed: {msg} — the old version is still in the Drafts folder"
            ));
        }
    }
}

pub async fn draft_reply(server: &ImapMcpServer, req: DraftReplyRequest) -> String {
    let (account_config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    if account_config.read_only {
        return error_json("Account is configured as read-only");
    }
    if req.body.len() > MAX_BODY_BYTES {
        return error_json(&format!("Reply body exceeds {MAX_BODY_BYTES}-byte cap"));
    }
    let from = account_config.sender_address().to_string();
    let account_name = account_config.name.clone();
    let display_name = account_config.display_name.clone();
    let signature_html = account_config.signature_html.as_deref().unwrap_or("");
    let locale = Locale::from_config(account_config.locale.as_deref());

    // Lock only for the fetch — CPU work (HTML escape, quote building, MIME
    // serialization) happens outside the mutex so parallel tool calls on the
    // same account aren't blocked on this draft.
    let original = {
        let mut client = client_arc.lock().await;
        match client.get_email(&req.folder, req.uid).await {
            Ok(Some(email)) => email,
            Ok(None) => {
                return error_json(&format!(
                    "Email {} not found in {}",
                    req.uid,
                    crate::email::sanitize_external_str(&req.folder)
                ));
            }
            Err(e) => return error_json(&client.check_error(e).to_string()),
        }
    };

    let reply_all = req.reply_all.unwrap_or(false);
    let (to_list, cc_list) =
        match build_reply_recipients(&original, reply_all, &from, req.cc.as_deref()) {
            Ok(pair) => pair,
            Err(e) => return error_json(e),
        };

    let subject_raw = if has_reply_prefix(&original.subject) {
        original.subject.clone()
    } else {
        format!("{}{}", locale.reply_prefix(), original.subject)
    };
    let subject = sanitize_header_value(&subject_raw);

    let (plain_body, html_body) = build_reply_bodies(&original, &req.body, locale, signature_html);

    let mut builder = MessageBuilder::new()
        .subject(&subject)
        .text_body(&plain_body)
        .html_body(&html_body);
    builder = apply_from(builder, &from, display_name.as_deref());

    // to_list / cc_list are already sanitized above at construction time.
    // CRITICAL: mail-builder's `.to()` / `.cc()` OVERWRITE on each call, so
    // the previous per-address loop silently dropped every recipient except
    // the last. Pass a Vec at once — mail-builder converts it to
    // `Address::List` which preserves every entry.
    if !to_list.is_empty() {
        builder = builder.to(to_list.clone());
    }
    if !cc_list.is_empty() {
        builder = builder.cc(cc_list.clone());
    }

    let has_threading;
    (builder, has_threading) = apply_threading_headers(builder, &original);

    let attachment_data = match read_attachments(
        req.attachments.as_deref(),
        &server.config.allowed_attachment_dirs,
    )
    .await
    {
        Ok(a) => a,
        Err(e) => return error_json(&e),
    };
    for (content_type, filename, bytes) in attachment_data {
        builder = builder.attachment(content_type, filename, bytes);
    }

    let message_bytes = match builder.write_to_vec() {
        Ok(bytes) => bytes,
        Err(e) => return error_json(&format!("Failed to build MIME message: {e}")),
    };

    let save_result = {
        let mut client = client_arc.lock().await;
        client
            .save_draft(&message_bytes)
            .await
            .map_err(|e| client.check_error(e))
    };
    match save_result {
        Ok(new_uid) => {
            let mut response = serde_json::json!({
                "status": "ok",
                "account": account_name,
                "from": from,
                "to": to_list,
                "cc": cc_list,
                "subject": subject,
                "body_preview": truncate(&plain_body, 500),
            });
            if !has_threading {
                // Named symmetrically with the `replace_warning` set in
                // `remove_replaced_draft` — both can appear on the same
                // response, so neither may claim the generic key.
                response["threading_warning"] = serde_json::json!(
                    "Original email has no Message-ID. Reply was created without threading headers (In-Reply-To/References) — it may not appear in the same thread in the recipient's mail client."
                );
            }
            finish_draft(&client_arc, new_uid, req.replaces_uid, response).await
        }
        Err(e) => error_json(&format!("Failed to save draft: {e}")),
    }
}

pub async fn draft_forward(server: &ImapMcpServer, req: DraftForwardRequest) -> String {
    let (account_config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    if account_config.read_only {
        return error_json("Account is configured as read-only");
    }
    if let Some(b) = &req.body
        && b.len() > MAX_BODY_BYTES
    {
        return error_json(&format!("Forward body exceeds {MAX_BODY_BYTES}-byte cap"));
    }
    let from = account_config.sender_address().to_string();
    let account_name = account_config.name.clone();
    let display_name = account_config.display_name.clone();
    let signature_html = account_config.signature_html.as_deref().unwrap_or("");
    let locale = Locale::from_config(account_config.locale.as_deref());

    let original = {
        let mut client = client_arc.lock().await;
        match client.get_email(&req.folder, req.uid).await {
            Ok(Some(email)) => email,
            Ok(None) => {
                return error_json(&format!(
                    "Email {} not found in {}",
                    req.uid,
                    crate::email::sanitize_external_str(&req.folder)
                ));
            }
            Err(e) => return error_json(&client.check_error(e).to_string()),
        }
    };

    let subject_raw = if has_forward_prefix(&original.subject) {
        original.subject.clone()
    } else {
        format!("{}{}", locale.forward_prefix(), original.subject)
    };
    // `original.subject` can contain `\r\n` header-injection payloads.
    let subject = sanitize_header_value(&subject_raw);

    let (plain_body, html_body) =
        build_forward_bodies(&original, req.body.as_deref(), locale, signature_html);

    let mut builder = MessageBuilder::new()
        .subject(&subject)
        .text_body(&plain_body)
        .html_body(&html_body);
    builder = apply_from(builder, &from, display_name.as_deref());

    // Sanitize LLM-provided recipient addresses — `\r\n` in any of them would
    // inject extra headers (e.g. a silent Bcc) into the saved draft.
    // Collect into a Vec and pass once; `.to()` / `.cc()` overwrite on repeat
    // calls (same bug that affected draft_reply before the fix).
    let to_clean: Vec<String> = req.to.iter().map(|a| sanitize_header_value(a)).collect();
    if !to_clean.is_empty() {
        builder = builder.to(to_clean);
    }
    let cc_clean: Vec<String> = req
        .cc
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|a| sanitize_header_value(a))
        .collect();
    if !cc_clean.is_empty() {
        builder = builder.cc(cc_clean);
    }

    let attachment_data = match read_attachments(
        req.attachments.as_deref(),
        &server.config.allowed_attachment_dirs,
    )
    .await
    {
        Ok(a) => a,
        Err(e) => return error_json(&e),
    };
    for (content_type, filename, bytes) in attachment_data {
        builder = builder.attachment(content_type, filename, bytes);
    }

    let message_bytes = match builder.write_to_vec() {
        Ok(bytes) => bytes,
        Err(e) => return error_json(&format!("Failed to build MIME message: {e}")),
    };

    let save_result = {
        let mut client = client_arc.lock().await;
        client
            .save_draft(&message_bytes)
            .await
            .map_err(|e| client.check_error(e))
    };
    match save_result {
        Ok(new_uid) => {
            let response = serde_json::json!({
                "status": "ok",
                "account": account_name,
                "from": from,
                "to": req.to,
                "cc": req.cc.as_deref().unwrap_or_default(),
                "subject": subject,
                "body_preview": truncate(&plain_body, 500),
            });
            finish_draft(&client_arc, new_uid, req.replaces_uid, response).await
        }
        Err(e) => error_json(&format!("Failed to save draft: {e}")),
    }
}

pub async fn draft_email(server: &ImapMcpServer, req: DraftEmailRequest) -> String {
    let (account_config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    if account_config.read_only {
        return error_json("Account is configured as read-only");
    }
    if req.body.len() > MAX_BODY_BYTES {
        return error_json(&format!("Draft body exceeds {MAX_BODY_BYTES}-byte cap"));
    }
    let from = account_config.sender_address().to_string();
    let account_name = account_config.name.clone();
    let display_name = account_config.display_name.clone();
    let signature_html = account_config.signature_html.as_deref().unwrap_or("");
    let locale = Locale::from_config(account_config.locale.as_deref());

    let html_body = build_compose_html(&req.body, signature_html, locale);

    // Sanitize subject + recipients against header injection from LLM input.
    let subject = sanitize_header_value(&req.subject);
    let mut builder = MessageBuilder::new()
        .subject(&subject)
        .text_body(&req.body)
        .html_body(&html_body);
    builder = apply_from(builder, &from, display_name.as_deref());

    // Collect recipients into Vecs and pass once each — mail-builder's
    // `.to()` / `.cc()` / `.bcc()` OVERWRITE on repeat calls, so the per-
    // address loop silently dropped every recipient except the last.
    let to_clean: Vec<String> = req.to.iter().map(|a| sanitize_header_value(a)).collect();
    if !to_clean.is_empty() {
        builder = builder.to(to_clean);
    }
    let cc_clean: Vec<String> = req
        .cc
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|a| sanitize_header_value(a))
        .collect();
    if !cc_clean.is_empty() {
        builder = builder.cc(cc_clean);
    }
    let bcc_clean: Vec<String> = req
        .bcc
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|a| sanitize_header_value(a))
        .collect();
    if !bcc_clean.is_empty() {
        builder = builder.bcc(bcc_clean);
    }

    let attachment_data = match read_attachments(
        req.attachments.as_deref(),
        &server.config.allowed_attachment_dirs,
    )
    .await
    {
        Ok(a) => a,
        Err(e) => return error_json(&e),
    };
    for (content_type, filename, bytes) in attachment_data {
        builder = builder.attachment(content_type, filename, bytes);
    }

    let message_bytes = match builder.write_to_vec() {
        Ok(bytes) => bytes,
        Err(e) => return error_json(&format!("Failed to build MIME message: {e}")),
    };

    let save_result = {
        let mut client = client_arc.lock().await;
        client
            .save_draft(&message_bytes)
            .await
            .map_err(|e| client.check_error(e))
    };
    match save_result {
        Ok(new_uid) => {
            let response = serde_json::json!({
                "status": "ok",
                "account": account_name,
                "from": from,
                "to": req.to,
                "cc": req.cc.as_deref().unwrap_or_default(),
                "bcc": req.bcc.as_deref().unwrap_or_default(),
                "subject": req.subject,
                "body_preview": truncate(&req.body, 500),
            });
            finish_draft(&client_arc, new_uid, req.replaces_uid, response).await
        }
        Err(e) => error_json(&format!("Failed to save draft: {e}")),
    }
}

pub async fn delete_draft(server: &ImapMcpServer, req: DeleteDraftRequest) -> String {
    // Deliberately far below `tools/write.rs::MAX_UIDS_PER_CALL` (1000):
    // deleting drafts bypasses `allow_delete` (the Drafts folder is the
    // user's workspace) and uses EXPUNGE, so there is no Trash to recover
    // from. Replacing a draft touches one UID and tidying up a handful is
    // normal; wiping a whole Drafts folder in a single call has no
    // legitimate use and is exactly what a prompt-injected model would try.
    const MAX_DRAFT_UIDS_PER_CALL: usize = 25;
    if req.uids.len() > MAX_DRAFT_UIDS_PER_CALL {
        return error_json(&format!(
            "uids list exceeds {MAX_DRAFT_UIDS_PER_CALL}-item cap for draft deletion — \
             delete in smaller batches (drafts are expunged, not moved to Trash)"
        ));
    }
    let (account_config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    if account_config.read_only {
        return error_json("Account is configured as read-only");
    }
    let account_name = account_config.name.clone();
    let mut client = client_arc.lock().await;
    match client.delete_draft(&req.uids).await {
        Ok(succeeded) => serde_json::to_string(&serde_json::json!({
            "account": account_name,
            "succeeded": succeeded,
        }))
        .unwrap_or_else(|e| error_json(&e.to_string())),
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

// ========== Reply / draft helpers ==========

/// Build `(to_list, cc_list)` for a reply. All addresses pass through
/// `sanitize_header_value` to strip `\r\n` header-injection payloads that could
/// come from the parsed original email or LLM-provided `extra_cc`.
/// Excludes the user's own `from` address from both lists in reply-all to
/// avoid self-addressed drafts.
fn build_reply_recipients(
    original: &EmailFull,
    reply_all: bool,
    from: &str,
    extra_cc: Option<&[String]>,
) -> Result<(Vec<String>, Vec<String>), &'static str> {
    let to_addr = match original.from.as_ref() {
        Some(a) if !a.address.is_empty() => sanitize_header_value(&a.address),
        _ => return Err("Cannot reply: original email has no sender address"),
    };

    // `eq_ignore_ascii_case` is allocation-free — preferred over `to_lowercase`.
    let mut to_list = vec![to_addr];
    let mut cc_list: Vec<String> = Vec::new();
    if reply_all {
        for addr in &original.to {
            if !addr.address.eq_ignore_ascii_case(from) {
                to_list.push(sanitize_header_value(&addr.address));
            }
        }
        for addr in &original.cc {
            if !addr.address.eq_ignore_ascii_case(from) {
                cc_list.push(sanitize_header_value(&addr.address));
            }
        }
    }
    if let Some(cc) = extra_cc {
        cc_list.extend(cc.iter().map(|s| sanitize_header_value(s)));
    }
    Ok((to_list, cc_list))
}

/// Apply In-Reply-To + References threading headers to the builder. Returns
/// the updated builder and whether threading was applied — callers should warn
/// the LLM when `false` so it knows to flag the missing Message-ID.
///
/// Sanitizes Message-IDs first: an attacker-crafted Message-ID containing
/// `"\r\nBcc: evil@attacker"` would otherwise inject an extra header into the
/// draft — silent exfiltration if the user sends without reviewing the source.
fn apply_threading_headers<'a>(
    mut builder: MessageBuilder<'a>,
    original: &'a EmailFull,
) -> (MessageBuilder<'a>, bool) {
    let Some(msg_id) = &original.message_id else {
        return (builder, false);
    };
    // `email::parse_email` stores Message-IDs already angle-bracketed (`<id>`).
    // `mail-builder`'s `in_reply_to` re-wraps with its own `<>`, producing
    // `<<id>>` — cosmetic non-compliance some strict parsers reject. Strip
    // any leading `<` / trailing `>` first so re-wrapping yields `<id>` once.
    let unwrap = |s: &str| s.trim_matches(|c| c == '<' || c == '>').to_string();
    let clean_msg_id = sanitize_header_value(&unwrap(msg_id));
    builder = builder.in_reply_to(clean_msg_id.clone());
    let refs: Vec<String> = original
        .references
        .iter()
        .map(|s| sanitize_header_value(&unwrap(s)))
        .map(|s| format!("<{s}>"))
        .chain(std::iter::once(format!("<{clean_msg_id}>")))
        .collect();
    builder = builder.header(
        "References",
        mail_builder::headers::raw::Raw::new(refs.join(" ")),
    );
    (builder, true)
}

/// Returns true if the subject already starts with a known reply prefix.
/// Used to avoid stacking "Re: AW: ..." when replying. Shares the constant
/// list with `strip_email_prefixes` so both sides can't drift.
fn has_reply_prefix(subject: &str) -> bool {
    let trimmed = subject.trim_start();
    crate::imap_client::REPLY_PREFIXES
        .iter()
        .any(|p| crate::imap_client::starts_with_ignore_ascii_case(trimmed, p))
}

/// Returns true if the subject already starts with a known forward prefix.
fn has_forward_prefix(subject: &str) -> bool {
    let trimmed = subject.trim_start();
    crate::imap_client::FORWARD_PREFIXES
        .iter()
        .any(|p| crate::imap_client::starts_with_ignore_ascii_case(trimmed, p))
}

/// Strip CR/LF/NUL and other control chars from a value that will be written
/// into an RFC 5322 header. Prevents header injection via malicious
/// Message-IDs or other untrusted fields parsed out of incoming mail.
///
/// Also strips Unicode line separators U+2028 (LS) and U+2029 (PS) plus the
/// BOM U+FEFF — these are category `Cf`, not `Cc`, so `char::is_control`
/// misses them, but some MIME folders / header writers treat them as line
/// breaks, reopening the CRLF-injection risk through a different channel.
pub(super) fn sanitize_header_value(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() && !matches!(*c, '\u{2028}' | '\u{2029}' | '\u{FEFF}'))
        .collect()
}

/// Caps on user-supplied composition input — a prompt-injected LLM could
/// otherwise pass a 100 MiB subject or body and generate a huge MIME
/// APPEND, wasting server storage and bandwidth. RFC 5322 line limit is 998;
/// 10 MiB body fits every realistic email including formatted ones.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..s.floor_char_boundary(max_len)])
    }
}

// ========== Attachment helpers ==========

/// Read attachment files from disk. Returns `(content_type, filename, bytes)` tuples.
type AttachmentData = Vec<(&'static str, String, Vec<u8>)>;

async fn read_attachments(
    attachments: Option<&[String]>,
    allowed_dirs: &[String],
) -> Result<AttachmentData, String> {
    // Per-file cap prevents a single huge file from OOMing. Aggregate cap
    // prevents the "many medium files" path: 50 files × 50 MiB = 2.5 GiB of
    // RAM before the MIME builder even runs. 100 MiB total covers every
    // realistic email workflow and most provider send limits anyway.
    const MAX_ATTACHMENT_SIZE: usize = 50 * 1024 * 1024;
    const MAX_TOTAL_ATTACHMENTS_SIZE: usize = 100 * 1024 * 1024;

    let Some(paths) = attachments else {
        return Ok(vec![]);
    };

    // Canonicalize the whitelist ONCE, not per-attachment. For a draft with 5
    // attachments and 2 allowed_dirs, this drops 10 FS syscalls to 2.
    // Non-existent or un-canonicalizable entries are dropped here (same
    // permissive behaviour as before, just evaluated eagerly).
    let mut canonical_allowed = Vec::with_capacity(allowed_dirs.len());
    for allowed in allowed_dirs {
        if let Ok(c) = tokio::fs::canonicalize(allowed).await {
            canonical_allowed.push(c);
        }
    }

    let mut result = Vec::new();
    let mut total_bytes: usize = 0;
    for path_str in paths {
        let path = std::path::Path::new(path_str);
        // Validate returns the canonical path; we read FROM that (not the raw
        // input) to close the TOCTOU gap: if the user-supplied path pointed
        // to a file that has since been replaced by a symlink to /etc/shadow,
        // reading the post-canonicalize path still hits the originally
        // resolved file.
        let canonical = validate_attachment_path(path, &canonical_allowed, allowed_dirs).await?;
        let bytes = tokio::fs::read(&canonical)
            .await
            .map_err(|e| format!("Failed to read attachment \"{path_str}\": {e}"))?;
        if bytes.len() > MAX_ATTACHMENT_SIZE {
            return Err(format!(
                "Attachment \"{path_str}\" is {} bytes — exceeds the {}-byte per-file cap",
                bytes.len(),
                MAX_ATTACHMENT_SIZE
            ));
        }
        // Saturating_add is defence-in-depth on 32-bit targets; the caps
        // ensure `total_bytes` never approaches `usize::MAX` on 64-bit.
        total_bytes = total_bytes.saturating_add(bytes.len());
        if total_bytes > MAX_TOTAL_ATTACHMENTS_SIZE {
            return Err(format!(
                "Total attachment size exceeds the {MAX_TOTAL_ATTACHMENTS_SIZE}-byte aggregate cap"
            ));
        }
        let raw_filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment");
        // Defense-in-depth: strip control chars + Unicode line separators
        // from the filename before it ends up in the outgoing MIME
        // Content-Disposition header. mail-builder should encode it, but
        // we don't want to rely on that alone.
        let filename: String = raw_filename
            .chars()
            .filter(|c| !c.is_control() && !matches!(*c, '\u{2028}' | '\u{2029}' | '\u{FEFF}'))
            .collect();
        let content_type =
            mime_type_from_extension(path.extension().and_then(|e| e.to_str()).unwrap_or(""));
        result.push((content_type, filename, bytes));
    }
    Ok(result)
}

/// Reject attachment paths outside the configured whitelist. Returns the
/// canonicalized path on success so the caller can read FROM the canonical
/// (closing TOCTOU between check and use). `canonical_allowed` must be
/// pre-canonicalized by the caller (so the same set can be reused across a
/// batch of attachments).
async fn validate_attachment_path(
    path: &std::path::Path,
    canonical_allowed: &[std::path::PathBuf],
    raw_allowed_for_err: &[String],
) -> Result<std::path::PathBuf, String> {
    let canonical = tokio::fs::canonicalize(path)
        .await
        .map_err(|e| format!("Cannot resolve attachment path \"{}\": {e}", path.display()))?;
    for allowed in canonical_allowed {
        if canonical.starts_with(allowed) {
            return Ok(canonical);
        }
    }
    Err(format!(
        "Attachment path \"{}\" is not within any allowed directory. \
         Configured allowed_attachment_dirs: {raw_allowed_for_err:?}",
        canonical.display()
    ))
}

fn mime_type_from_extension(ext: &str) -> &'static str {
    match ext.to_lowercase().as_str() {
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "gzip" => "application/gzip",
        "json" => "application/json",
        "xml" => "application/xml",
        "csv" => "text/csv",
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "ics" => "text/calendar",
        "eml" => "message/rfc822",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::email::EmailAddress;

    fn addr(email: &str) -> EmailAddress {
        EmailAddress {
            name: None,
            address: email.to_string(),
        }
    }

    #[test]
    fn note_replacement_reports_the_removed_uid() {
        let mut r = serde_json::json!({"status": "ok"});
        note_replacement(&mut r, 812, Ok(&[812]));
        assert_eq!(r["replaced_uid"], 812);
        assert!(r.get("replace_warning").is_none());
    }

    #[test]
    fn note_replacement_warns_when_the_uid_was_not_removed() {
        // Server accepted the command but reported nothing gone — the old
        // draft may still be sitting there, so say so instead of claiming
        // success.
        let mut r = serde_json::json!({"status": "ok"});
        note_replacement(&mut r, 812, Ok(&[]));
        assert!(r.get("replaced_uid").is_none());
        let w = r["replace_warning"].as_str().unwrap();
        assert!(w.contains("812") && w.contains("not deleted"));
    }

    #[test]
    fn note_replacement_warns_and_keeps_the_error_text() {
        let msg = "Unknown Mailbox: Drafts".to_string();
        let mut r = serde_json::json!({"status": "ok"});
        note_replacement(&mut r, 7, Err(&msg));
        assert!(r.get("replaced_uid").is_none());
        let w = r["replace_warning"].as_str().unwrap();
        assert!(w.contains("Unknown Mailbox"), "error text dropped: {w}");
        // The new draft exists either way — the wording must not imply loss.
        assert!(w.starts_with("New draft saved"));
    }

    fn email(subject: &str, from: Option<&str>, to: Vec<&str>, cc: Vec<&str>) -> EmailFull {
        EmailFull {
            uid: 1,
            folder: "INBOX".to_string(),
            from: from.map(addr),
            to: to.into_iter().map(addr).collect(),
            cc: cc.into_iter().map(addr).collect(),
            subject: subject.to_string(),
            date: None,
            message_id: None,
            in_reply_to: None,
            references: vec![],
            flags: vec![],
            body_text: String::new(),
            body_html: None,
            attachments: vec![],
            body_parts_diverge: false,
        }
    }

    #[test]
    fn has_reply_prefix_recognises_de_and_en() {
        assert!(has_reply_prefix("Re: hi"));
        assert!(has_reply_prefix("RE: hi"));
        assert!(has_reply_prefix("re: hi"));
        assert!(has_reply_prefix("AW: hi"));
        assert!(has_reply_prefix("aw: hi"));
        assert!(has_reply_prefix("Antw: hi"));
        assert!(has_reply_prefix("Antwort: hi"));
        assert!(has_reply_prefix("  Re: trimmed leading"));
    }

    #[test]
    fn has_reply_prefix_rejects_unrelated() {
        assert!(!has_reply_prefix("Hello"));
        assert!(!has_reply_prefix("Reply"));
        assert!(!has_reply_prefix(""));
        assert!(!has_reply_prefix("Read this"));
    }

    #[test]
    fn has_forward_prefix_recognises_de_and_en() {
        assert!(has_forward_prefix("Fwd: hi"));
        assert!(has_forward_prefix("FWD: hi"));
        assert!(has_forward_prefix("fwd: hi"));
        assert!(has_forward_prefix("WG: hi"));
        assert!(has_forward_prefix("wg: hi"));
    }

    #[test]
    fn sanitize_header_value_strips_control_chars() {
        assert_eq!(
            sanitize_header_value("good@example.com\r\nBcc: evil@evil.com"),
            "good@example.comBcc: evil@evil.com"
        );
        assert_eq!(sanitize_header_value("a\nb"), "ab");
        assert_eq!(sanitize_header_value("a\rb"), "ab");
        assert_eq!(sanitize_header_value("a\x00b"), "ab");
        assert_eq!(sanitize_header_value("a\tb"), "ab"); // tab is control too
        assert_eq!(sanitize_header_value("clean text"), "clean text");
        assert_eq!(sanitize_header_value("ünïcödë"), "ünïcödë");
    }

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("", 10), "");
    }

    #[test]
    fn truncate_long_string_appends_ellipsis() {
        assert_eq!(truncate("hello world", 5), "hello...");
    }

    #[test]
    fn truncate_respects_utf8_boundaries() {
        let result = truncate("äöü extra", 3);
        assert!(result.starts_with("ä"));
        assert!(result.ends_with("..."));
    }

    #[test]
    fn build_reply_recipients_simple() {
        let original = email(
            "Test",
            Some("alice@example.com"),
            vec!["me@example.com"],
            vec![],
        );
        let (to, cc) = build_reply_recipients(&original, false, "me@example.com", None).unwrap();
        assert_eq!(to, vec!["alice@example.com"]);
        assert!(cc.is_empty());
    }

    #[test]
    fn build_reply_recipients_no_sender_errors() {
        let original = email("Test", None, vec!["me@example.com"], vec![]);
        let result = build_reply_recipients(&original, false, "me@example.com", None);
        assert!(result.is_err());
    }

    #[test]
    fn build_reply_recipients_reply_all_excludes_self() {
        let original = email(
            "Test",
            Some("alice@example.com"),
            vec!["me@example.com", "bob@example.com"],
            vec!["carol@example.com", "ME@example.COM"],
        );
        let (to, cc) = build_reply_recipients(&original, true, "me@example.com", None).unwrap();
        assert_eq!(to, vec!["alice@example.com", "bob@example.com"]);
        assert_eq!(cc, vec!["carol@example.com"]);
    }

    #[test]
    fn build_reply_recipients_extra_cc_appended() {
        let original = email("Test", Some("alice@example.com"), vec![], vec![]);
        let extra = vec!["dave@example.com".to_string()];
        let (_to, cc) =
            build_reply_recipients(&original, false, "me@example.com", Some(&extra)).unwrap();
        assert_eq!(cc, vec!["dave@example.com"]);
    }

    #[test]
    fn build_reply_recipients_sanitizes_addresses() {
        let original = email("Test", Some("alice\r\nBcc: evil@evil.com"), vec![], vec![]);
        let (to, _) = build_reply_recipients(&original, false, "me@example.com", None).unwrap();
        assert!(!to[0].contains('\r'));
        assert!(!to[0].contains('\n'));
    }

    #[test]
    fn apply_threading_headers_with_message_id() {
        let mut original = email("Test", Some("a@b.com"), vec![], vec![]);
        original.message_id = Some("<msg-1@example.com>".to_string());
        original.references = vec!["<ref-1@example.com>".to_string()];
        let builder = MessageBuilder::new();
        let (_builder, threaded) = apply_threading_headers(builder, &original);
        assert!(threaded);
    }

    #[test]
    fn apply_threading_headers_without_message_id_returns_false() {
        let original = email("Test", Some("a@b.com"), vec![], vec![]);
        let builder = MessageBuilder::new();
        let (_builder, threaded) = apply_threading_headers(builder, &original);
        assert!(!threaded);
    }

    #[test]
    fn mime_type_from_extension_known() {
        assert_eq!(mime_type_from_extension("pdf"), "application/pdf");
        assert_eq!(mime_type_from_extension("png"), "image/png");
        assert_eq!(mime_type_from_extension("jpg"), "image/jpeg");
        assert_eq!(mime_type_from_extension("txt"), "text/plain");
    }

    #[test]
    fn mime_type_from_extension_unknown_falls_back() {
        assert_eq!(
            mime_type_from_extension("unknown_ext"),
            "application/octet-stream"
        );
        assert_eq!(mime_type_from_extension(""), "application/octet-stream");
    }
}
