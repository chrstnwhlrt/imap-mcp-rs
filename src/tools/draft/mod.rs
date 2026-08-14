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
use mail_builder::headers::content_type::ContentType;
use mail_builder::mime::MimePart;
use rmcp::schemars;
use serde::Deserialize;

use tokio::sync::Mutex;

use crate::email::EmailFull;
use crate::imap_client::ImapClient;

use super::{ImapMcpServer, error_json};

mod render;
use render::{
    InlineRef, Locale, Signatures, apply_from, build_compose_bodies, build_forward_bodies,
    build_reply_bodies, collect_cid_markers,
};

/// One entry of a draft's `attachments` list.
///
/// Accepts either a bare path — the original and still most common form — or
/// an object that additionally marks the file as an inline image the body
/// refers to. Untagged, so both spellings coexist in the same array and every
/// existing caller keeps working unchanged:
///
/// ```json
/// ["/path/report.pdf", {"path": "/path/shot.png", "inline": true, "cid": "shot"}]
/// ```
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum AttachmentSpec {
    /// Bare absolute path — a regular attachment.
    Path(String),
    /// Path plus placement metadata.
    Detailed {
        #[schemars(description = "Absolute file path, same rules as the bare-string form.")]
        path: String,
        #[schemars(
            description = "Embed in the body instead of appending as a file. Implied when `cid` is set."
        )]
        inline: Option<bool>,
        #[schemars(
            description = "Content id referenced from the body as `![alt](cid:<id>)`. Defaults to a value derived from the file name."
        )]
        cid: Option<String>,
    },
}

impl AttachmentSpec {
    fn path(&self) -> &str {
        match self {
            Self::Path(p) => p,
            Self::Detailed { path, .. } => path,
        }
    }

    /// A `cid` alone is enough to mean "inline" — requiring `inline: true`
    /// next to it would be a redundant second switch that callers forget.
    ///
    /// An explicit `inline` still wins over that inference: `{"cid": "x",
    /// "inline": false}` yields a regular attachment. Silently overriding a
    /// stated `false` would be the more surprising behaviour, and a body that
    /// then references `cid:x` is caught by [`check_cid_markers`] with a clear
    /// message rather than shipping a broken image.
    fn is_inline(&self) -> bool {
        match self {
            Self::Path(_) => false,
            Self::Detailed { inline, cid, .. } => inline.unwrap_or_else(|| cid.is_some()),
        }
    }

    fn explicit_cid(&self) -> Option<&str> {
        match self {
            Self::Path(_) => None,
            Self::Detailed { cid, .. } => cid.as_deref(),
        }
    }
}

/// Account-derived values shared by all three draft flows, resolved once per
/// call so the flows stay below clippy's function-length lint and cannot
/// drift apart in how they read the config.
struct DraftAccount {
    from: String,
    name: String,
    display_name: Option<String>,
    message_id_domain: String,
    signatures: Signatures,
    locale: Locale,
}

impl DraftAccount {
    fn from_config(config: &crate::config::AccountConfig) -> Self {
        Self {
            from: config.sender_address().to_string(),
            name: config.name.clone(),
            display_name: config.display_name.clone(),
            message_id_domain: config.message_id_domain().to_string(),
            signatures: Signatures::resolve(
                config.signature_html.as_deref(),
                config.signature_text.as_deref(),
            ),
            locale: Locale::from_config(config.locale.as_deref()),
        }
    }
}

/// Stamp explicit `Message-ID` and `Date` headers on the builder.
///
/// Without these, `mail-builder` generates both at write time — the
/// Message-ID domain falling back to the **machine's hostname** (leaking the
/// local machine name into every draft) and the Date to UTC, while desktop
/// clients write their sending domain and the local UTC offset. Setting them
/// explicitly makes the draft indistinguishable from a hand-written one.
fn stamp_identity_headers<'a>(
    builder: MessageBuilder<'a>,
    message_id_domain: &str,
) -> MessageBuilder<'a> {
    let message_id = format!(
        "{}@{}",
        uuid::Uuid::new_v4().simple(),
        sanitize_header_value(message_id_domain)
    );
    let date = jiff::Zoned::now()
        .strftime("%a, %d %b %Y %H:%M:%S %z")
        .to_string();
    builder
        .message_id(message_id)
        .header("Date", mail_builder::headers::raw::Raw::new(date))
}

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
        description = "Plain-text reply body. Rendered to HTML automatically; the original is quoted below an Outlook-style From/Sent/To/Subject header block in both MIME parts."
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
        description = "Files to attach. Each entry is either an absolute path (e.g. from download_attachment's `saved_to`) or an object `{path, inline, cid}`. With `inline: true` the file is embedded in the body instead of appended: reference it from `body` as `![alt](cid:<id>)` and it appears exactly there. Paths must be inside allowed_attachment_dirs (default: `$XDG_RUNTIME_DIR/imap-mcp-rs`)."
    )]
    pub attachments: Option<Vec<AttachmentSpec>>,
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
        description = "Files to attach. Each entry is either an absolute path (e.g. from download_attachment's `saved_to`) or an object `{path, inline, cid}`. With `inline: true` the file is embedded in the body instead of appended: reference it from `body` as `![alt](cid:<id>)` and it appears exactly there. Paths must be inside allowed_attachment_dirs (default: `$XDG_RUNTIME_DIR/imap-mcp-rs`)."
    )]
    pub attachments: Option<Vec<AttachmentSpec>>,
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
        description = "Files to attach. Each entry is either an absolute path (e.g. from download_attachment's `saved_to`) or an object `{path, inline, cid}`. With `inline: true` the file is embedded in the body instead of appended: reference it from `body` as `![alt](cid:<id>)` and it appears exactly there. Paths must be inside allowed_attachment_dirs (default: `$XDG_RUNTIME_DIR/imap-mcp-rs`)."
    )]
    pub attachments: Option<Vec<AttachmentSpec>>,
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
    let acct = DraftAccount::from_config(account_config);

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
        match build_reply_recipients(&original, reply_all, &acct.from, req.cc.as_deref()) {
            Ok(pair) => pair,
            Err(e) => return error_json(e),
        };

    let subject_raw = if has_reply_prefix(&original.subject) {
        original.subject.clone()
    } else {
        format!("{}{}", acct.locale.reply_prefix(), original.subject)
    };
    let subject = sanitize_header_value(&subject_raw);

    // Attachments are read BEFORE the bodies: inline images have to be known
    // while rendering, because their markers are resolved inside the body
    // text rather than patched in afterwards.
    let (prepared, inline_notice) = match prepare_attachments(
        req.attachments.as_deref(),
        &server.config.allowed_attachment_dirs,
        &req.body,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return error_json(&e),
    };

    // Scoped so the borrows of `prepared` end before it is moved into the
    // MIME assembly below.
    let (plain_body, html_body) = {
        let refs = inline_refs(&prepared.inline);
        build_reply_bodies(&original, &req.body, acct.locale, &acct.signatures, &refs)
    };

    let mut builder = MessageBuilder::new().subject(&subject);
    builder = apply_from(builder, &acct.from, acct.display_name.as_deref());
    builder = stamp_identity_headers(builder, &acct.message_id_domain);

    // to_list / cc_list are already sanitized above at construction time.
    // CRITICAL: mail-builder's `.to()` / `.cc()` OVERWRITE on each call, so
    // the previous per-address loop silently dropped every recipient except
    // the last. Pass a full list at once — mail-builder converts it to
    // `Address::List` which preserves every entry. Display names from the
    // original are carried along (desktop clients keep `Name <addr>` in To).
    if !to_list.is_empty() {
        builder = builder.to(to_address_list(&to_list));
    }
    if !cc_list.is_empty() {
        builder = builder.cc(to_address_list(&cc_list));
    }

    let has_threading;
    (builder, has_threading) = apply_threading_headers(builder, &original);

    builder = apply_bodies_and_attachments(builder, &plain_body, &html_body, prepared);

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
                "account": acct.name,
                "from": acct.from,
                "to": addresses_only(&to_list),
                "cc": addresses_only(&cc_list),
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
            if let Some(notice) = inline_notice {
                response["inline_warning"] = serde_json::Value::String(notice);
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
    let acct = DraftAccount::from_config(account_config);

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
        format!("{}{}", acct.locale.forward_prefix(), original.subject)
    };
    // `original.subject` can contain `\r\n` header-injection payloads.
    let subject = sanitize_header_value(&subject_raw);

    // Read before rendering — see the note in draft_reply.
    let (prepared, inline_notice) = match prepare_attachments(
        req.attachments.as_deref(),
        &server.config.allowed_attachment_dirs,
        req.body.as_deref().unwrap_or(""),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return error_json(&e),
    };

    let (plain_body, html_body) = {
        let refs = inline_refs(&prepared.inline);
        build_forward_bodies(
            &original,
            req.body.as_deref(),
            acct.locale,
            &acct.signatures,
            &refs,
        )
    };

    let mut builder = MessageBuilder::new().subject(&subject);
    builder = apply_from(builder, &acct.from, acct.display_name.as_deref());
    builder = stamp_identity_headers(builder, &acct.message_id_domain);

    // Collect into a Vec and pass once; `.to()` / `.cc()` overwrite on repeat
    // calls (same bug that affected draft_reply before the fix).
    let to_clean = clean_recipients(Some(&req.to));
    if !to_clean.is_empty() {
        builder = builder.to(to_clean);
    }
    let cc_clean = clean_recipients(req.cc.as_deref());
    if !cc_clean.is_empty() {
        builder = builder.cc(cc_clean);
    }

    builder = apply_bodies_and_attachments(builder, &plain_body, &html_body, prepared);

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
                "account": acct.name,
                "from": acct.from,
                "to": req.to,
                "cc": req.cc.as_deref().unwrap_or_default(),
                "subject": subject,
                "body_preview": truncate(&plain_body, 500),
            });
            if let Some(notice) = inline_notice {
                response["inline_warning"] = serde_json::Value::String(notice);
            }
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
    let acct = DraftAccount::from_config(account_config);

    // Read before rendering — see the note in draft_reply.
    let (prepared, inline_notice) = match prepare_attachments(
        req.attachments.as_deref(),
        &server.config.allowed_attachment_dirs,
        &req.body,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return error_json(&e),
    };

    let (plain_body, html_body) = {
        let refs = inline_refs(&prepared.inline);
        build_compose_bodies(&req.body, acct.locale, &acct.signatures, &refs)
    };

    // Sanitize subject + recipients against header injection from LLM input.
    let subject = sanitize_header_value(&req.subject);
    let mut builder = MessageBuilder::new().subject(&subject);
    builder = apply_from(builder, &acct.from, acct.display_name.as_deref());
    builder = stamp_identity_headers(builder, &acct.message_id_domain);

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

    builder = apply_bodies_and_attachments(builder, &plain_body, &html_body, prepared);

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
                "account": acct.name,
                "from": acct.from,
                "to": req.to,
                "cc": req.cc.as_deref().unwrap_or_default(),
                "bcc": req.bcc.as_deref().unwrap_or_default(),
                "subject": req.subject,
                // The rendered text, not the raw input — matching reply and
                // forward. With inline images the two differ: the input still
                // carries `![alt](cid:…)` markers, the saved message carries
                // the `[alt]` placeholders. A preview showing markers would
                // claim the mail contains something it does not.
                "body_preview": truncate(&plain_body, 500),
            });
            if let Some(notice) = inline_notice {
                response["inline_warning"] = serde_json::Value::String(notice);
            }
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
) -> Result<(Vec<Recipient>, Vec<Recipient>), &'static str> {
    let to_recipient = match original.from.as_ref() {
        Some(a) if !a.address.is_empty() => recipient_from(a),
        _ => return Err("Cannot reply: original email has no sender address"),
    };

    // `eq_ignore_ascii_case` is allocation-free — preferred over `to_lowercase`.
    let mut to_list = vec![to_recipient];
    let mut cc_list: Vec<Recipient> = Vec::new();
    if reply_all {
        for addr in &original.to {
            if !addr.address.eq_ignore_ascii_case(from) {
                to_list.push(recipient_from(addr));
            }
        }
        for addr in &original.cc {
            if !addr.address.eq_ignore_ascii_case(from) {
                cc_list.push(recipient_from(addr));
            }
        }
    }
    if let Some(cc) = extra_cc {
        cc_list.extend(cc.iter().map(|s| (None, sanitize_header_value(s))));
    }
    Ok((to_list, cc_list))
}

/// A draft recipient: optional display name + address, both already
/// sanitized against header injection.
type Recipient = (Option<String>, String);

/// Build a sanitized `Recipient` from a parsed address, keeping the display
/// name — desktop clients write `Name <addr>` into To/Cc when the original
/// carried a name, and a bare address where it did not.
fn recipient_from(addr: &crate::email::EmailAddress) -> Recipient {
    let name = addr
        .name
        .as_deref()
        .map(sanitize_header_value)
        .filter(|n| !n.is_empty());
    (name, sanitize_header_value(&addr.address))
}

/// Convert recipients into a mail-builder address list, preserving names.
fn to_address_list(list: &[Recipient]) -> mail_builder::headers::address::Address<'static> {
    use mail_builder::headers::address::Address;
    Address::new_list(
        list.iter()
            .map(|(name, addr)| {
                Address::new_address(name.clone().map(std::borrow::Cow::from), addr.clone())
            })
            .collect(),
    )
}

/// Extract the bare addresses for the JSON tool response (the response
/// format predates display-name support and stays address-only).
fn addresses_only(list: &[Recipient]) -> Vec<String> {
    list.iter().map(|(_, addr)| addr.clone()).collect()
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

/// Regular (appended) attachments as `(content_type, filename, bytes)`.
type AttachmentData = Vec<(&'static str, String, Vec<u8>)>;

/// An attachment that is embedded in the body rather than appended to it.
pub(super) struct InlineImage {
    pub cid: String,
    pub filename: String,
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
}

/// Attachments split by how they are placed in the message. Kept apart
/// because they end up in different MIME subtrees: inline parts belong next
/// to the HTML inside `multipart/related`, appended files next to it inside
/// `multipart/mixed`.
///
/// `Debug` omits the payloads via the manual impl below — a derived one would
/// dump megabytes of image bytes into a failing test's output.
pub(super) struct PreparedAttachments {
    pub regular: AttachmentData,
    pub inline: Vec<InlineImage>,
}

impl std::fmt::Debug for PreparedAttachments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedAttachments")
            .field(
                "regular",
                &self
                    .regular
                    .iter()
                    .map(|(ct, name, bytes)| format!("{name} ({ct}, {} bytes)", bytes.len()))
                    .collect::<Vec<_>>(),
            )
            .field(
                "inline",
                &self
                    .inline
                    .iter()
                    .map(|i| {
                        format!(
                            "{} -> cid:{} ({}, {} bytes)",
                            i.filename,
                            i.cid,
                            i.content_type,
                            i.bytes.len()
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Characters safe to carry inside `src="cid:…"` and a `Content-ID` header
/// without quoting. A deliberately narrow subset of what RFC 2392 permits.
const fn is_cid_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// Turn a file name into a content id: drop the extension, map everything
/// outside [`is_cid_char`] to `-`, collapse repeats. Falls back to `image`
/// for names that reduce to nothing (e.g. purely non-ASCII).
fn derive_cid(filename: &str) -> String {
    let stem = filename.rsplit_once('.').map_or(filename, |(s, _)| s);
    let mut out = String::with_capacity(stem.len());
    let mut last_dash = false;
    for c in stem.chars() {
        if is_cid_char(c) {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "image".to_string()
    } else {
        trimmed
    }
}

async fn read_attachments(
    attachments: Option<&[AttachmentSpec]>,
    allowed_dirs: &[String],
) -> Result<PreparedAttachments, String> {
    // Per-file cap prevents a single huge file from OOMing. Aggregate cap
    // prevents the "many medium files" path: 50 files × 50 MiB = 2.5 GiB of
    // RAM before the MIME builder even runs. 100 MiB total covers every
    // realistic email workflow and most provider send limits anyway.
    const MAX_ATTACHMENT_SIZE: usize = 50 * 1024 * 1024;
    const MAX_TOTAL_ATTACHMENTS_SIZE: usize = 100 * 1024 * 1024;

    let Some(specs) = attachments else {
        return Ok(PreparedAttachments {
            regular: vec![],
            inline: vec![],
        });
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

    let mut regular = Vec::new();
    let mut inline: Vec<InlineImage> = Vec::new();
    let mut total_bytes: usize = 0;
    for spec in specs {
        let path_str = spec.path();
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

        if !spec.is_inline() {
            regular.push((content_type, filename, bytes));
            continue;
        }

        // A body marker always renders an `<img>` tag, so anything that is not
        // an image would produce a broken picture at the recipient's end. Type
        // detection is extension-based, so a correctly named file is required
        // — saying so beats letting the draft go out looking wrong.
        //
        // SVG is excluded on purpose although it is an image type: it can
        // carry script, and inline images may well originate from a received
        // mail via `download_attachment`. Embedding one would forward that
        // payload under our own name, for a format nobody needs for
        // screenshots.
        if !content_type.starts_with("image/") || content_type == "image/svg+xml" {
            return Err(format!(
                "Attachment \"{path_str}\" is marked inline but its type is {content_type}. Only \
                 raster images can be embedded in the body (a marker renders an <img> tag); SVG is \
                 excluded because it can carry script. Drop `inline` to send it as a regular \
                 attachment."
            ));
        }

        // An explicit id is rejected rather than silently cleaned: the caller
        // wrote the same string into the body marker, so quietly changing it
        // here would produce a draft whose image never resolves — the exact
        // failure this feature exists to avoid.
        let cid = match spec.explicit_cid() {
            Some(raw) => {
                if raw.is_empty() || !raw.chars().all(is_cid_char) {
                    return Err(format!(
                        "Invalid cid \"{raw}\" for attachment \"{path_str}\": use letters, digits, \
                         '.', '_' or '-' only (the id also appears in the body marker)"
                    ));
                }
                raw.to_string()
            }
            None => derive_cid(&filename),
        };

        if inline.iter().any(|i| i.cid == cid) {
            return Err(format!(
                "Duplicate cid \"{cid}\" (attachment \"{path_str}\"): each inline image needs its \
                 own id, otherwise a body marker cannot say which one it means"
            ));
        }

        inline.push(InlineImage {
            cid,
            filename,
            content_type,
            bytes,
        });
    }
    Ok(PreparedAttachments { regular, inline })
}

/// Sanitize an LLM-supplied recipient list: `\r\n` in any address would inject
/// extra headers (a silent `Bcc:`, for instance) into the saved draft.
fn clean_recipients(addrs: Option<&[String]>) -> Vec<String> {
    addrs
        .unwrap_or_default()
        .iter()
        .map(|a| sanitize_header_value(a))
        .collect()
}

/// Read the attachments and cross-check the body's inline markers in one step.
///
/// Bundled because all three draft flows need exactly this pair before they
/// can render, and the pair has an order that must not be swapped: markers can
/// only be validated once the attachment list is known.
async fn prepare_attachments(
    attachments: Option<&[AttachmentSpec]>,
    allowed_dirs: &[String],
    body: &str,
) -> Result<(PreparedAttachments, Option<String>), String> {
    let prepared = read_attachments(attachments, allowed_dirs).await?;
    let notice = check_cid_markers(body, &prepared.inline)?;
    Ok((prepared, notice))
}

/// Borrowed view of the inline images, for the body renderer.
fn inline_refs(inline: &[InlineImage]) -> Vec<InlineRef<'_>> {
    inline
        .iter()
        .map(|i| InlineRef {
            cid: &i.cid,
            filename: &i.filename,
        })
        .collect()
}

/// Cross-check the body's `cid:` markers against the inline attachments.
///
/// The two directions are treated differently on purpose:
///
/// * A **marker without an attachment** is an error. The draft would be saved
///   with an image that resolves to nothing, and neither the caller nor the
///   recipient sees why — exactly the silent failure this feature is meant to
///   prevent. Better to refuse and say which ids exist.
/// * An **attachment nobody references** is only a warning. The file still
///   reaches the recipient; it simply lands wherever their client puts
///   unreferenced parts instead of at the intended spot. Returning it as
///   `inline_warning` lets the caller notice and fix the body without losing
///   the draft they just wrote.
fn check_cid_markers(body: &str, inline: &[InlineImage]) -> Result<Option<String>, String> {
    let markers = collect_cid_markers(body);

    for cid in &markers {
        if !inline.iter().any(|i| &i.cid == cid) {
            let available: Vec<&str> = inline.iter().map(|i| i.cid.as_str()).collect();
            return Err(if available.is_empty() {
                format!(
                    "Body references `(cid:{cid})` but no attachment is marked inline. Pass the \
                     image as {{\"path\": \"…\", \"inline\": true, \"cid\": \"{cid}\"}}."
                )
            } else {
                format!(
                    "Body references `(cid:{cid})` but no attachment carries that id. Available: {}",
                    available.join(", ")
                )
            });
        }
    }

    let unused: Vec<&str> = inline
        .iter()
        .filter(|i| !markers.contains(&i.cid))
        .map(|i| i.cid.as_str())
        .collect();
    if unused.is_empty() {
        Ok(None)
    } else {
        Ok(Some(format!(
            "Inline attachment(s) not referenced from the body: {}. They are still sent, but the \
             recipient's client decides where to place them — add `![](cid:<id>)` where each belongs.",
            unused.join(", ")
        )))
    }
}

/// Put bodies and attachments on the builder, choosing the MIME shape that
/// matches what the message actually contains.
///
/// **Without inline images** this is mail-builder's own path: `text_body` +
/// `html_body` + attachments, which it assembles into
/// `multipart/mixed[ multipart/alternative[text, html], files… ]`.
///
/// **With inline images** that shape is wrong. A `cid:` reference is only
/// guaranteed to resolve when the image sits in the same `multipart/related`
/// as the HTML pointing at it (RFC 2387). mail-builder's own `.inline()`
/// helper appends the part *beside* the alternative instead, which most
/// clients tolerate but some render as a detached attachment. So the tree is
/// assembled by hand:
///
/// ```text
/// multipart/mixed              (only when regular attachments exist)
/// ├── multipart/related
/// │   ├── multipart/alternative
/// │   │   ├── text/plain
/// │   │   └── text/html
/// │   └── image/…              (Content-ID + inline disposition)
/// └── report.pdf               (regular attachments)
/// ```
///
/// The inline parts carry `Content-Disposition: inline; filename="…"` in one
/// header rather than `.inline()` plus `.attachment()`, because both helpers
/// push their own `Content-Disposition` and a part would end up with two.
fn apply_bodies_and_attachments<'a>(
    builder: MessageBuilder<'a>,
    plain_body: &'a str,
    html_body: &'a str,
    prepared: PreparedAttachments,
) -> MessageBuilder<'a> {
    if prepared.inline.is_empty() {
        let mut builder = builder.text_body(plain_body).html_body(html_body);
        for (content_type, filename, bytes) in prepared.regular {
            builder = builder.attachment(content_type, filename, bytes);
        }
        return builder;
    }

    let mut related = Vec::with_capacity(prepared.inline.len() + 1);
    related.push(MimePart::new(
        "multipart/alternative",
        vec![
            MimePart::new("text/plain", plain_body),
            MimePart::new("text/html", html_body),
        ],
    ));
    for img in prepared.inline {
        related.push(
            MimePart::new(img.content_type, img.bytes)
                .cid(img.cid)
                .header(
                    "Content-Disposition",
                    ContentType::new("inline").attribute("filename", img.filename),
                ),
        );
    }
    let related = MimePart::new("multipart/related", related);

    if prepared.regular.is_empty() {
        return builder.body(related);
    }

    let mut mixed = Vec::with_capacity(prepared.regular.len() + 1);
    mixed.push(related);
    for (content_type, filename, bytes) in prepared.regular {
        mixed.push(MimePart::new(content_type, bytes).attachment(filename));
    }
    builder.body(MimePart::new("multipart/mixed", mixed))
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
        assert_eq!(addresses_only(&to), vec!["alice@example.com"]);
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
        assert_eq!(
            addresses_only(&to),
            vec!["alice@example.com", "bob@example.com"]
        );
        assert_eq!(addresses_only(&cc), vec!["carol@example.com"]);
    }

    #[test]
    fn build_reply_recipients_extra_cc_appended() {
        let original = email("Test", Some("alice@example.com"), vec![], vec![]);
        let extra = vec!["dave@example.com".to_string()];
        let (_to, cc) =
            build_reply_recipients(&original, false, "me@example.com", Some(&extra)).unwrap();
        assert_eq!(addresses_only(&cc), vec!["dave@example.com"]);
    }

    #[test]
    fn build_reply_recipients_sanitizes_addresses() {
        let original = email("Test", Some("alice\r\nBcc: evil@evil.com"), vec![], vec![]);
        let (to, _) = build_reply_recipients(&original, false, "me@example.com", None).unwrap();
        assert!(!to[0].1.contains('\r'));
        assert!(!to[0].1.contains('\n'));
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

    // ===== inline attachments =====

    fn img(cid: &str) -> InlineImage {
        InlineImage {
            cid: cid.to_string(),
            filename: format!("{cid}.png"),
            content_type: "image/png",
            bytes: vec![1, 2, 3],
        }
    }

    #[test]
    fn attachment_spec_accepts_both_spellings() {
        // Backwards compatibility: every existing caller passes bare strings,
        // and they must keep deserializing into the same list as the new
        // object form.
        let specs: Vec<AttachmentSpec> = serde_json::from_str(
            r#"["/tmp/a.pdf", {"path": "/tmp/b.png", "inline": true}, {"path": "/tmp/c.png", "cid": "shot"}]"#,
        )
        .expect("both spellings parse");

        assert_eq!(specs[0].path(), "/tmp/a.pdf");
        assert!(!specs[0].is_inline());
        assert_eq!(specs[0].explicit_cid(), None);

        assert!(specs[1].is_inline());
        assert_eq!(specs[1].explicit_cid(), None);

        // A cid alone implies inline — no second switch required.
        assert!(specs[2].is_inline());
        assert_eq!(specs[2].explicit_cid(), Some("shot"));
    }

    #[test]
    fn explicit_inline_false_wins_over_cid_inference() {
        // Stating `inline: false` next to a cid is contradictory input. The
        // explicit value decides, rather than the inference silently
        // overruling what the caller wrote.
        let specs: Vec<AttachmentSpec> =
            serde_json::from_str(r#"[{"path": "/tmp/a.png", "inline": false, "cid": "shot"}]"#)
                .expect("parses");
        assert!(!specs[0].is_inline());
    }

    #[test]
    fn derive_cid_strips_extension_and_unsafe_chars() {
        assert_eq!(derive_cid("screenshot.png"), "screenshot");
        assert_eq!(derive_cid("Rollen und Rechte.png"), "Rollen-und-Rechte");
        assert_eq!(derive_cid("a  b.png"), "a-b");
        assert_eq!(derive_cid("-weird-.png"), "weird");
        assert_eq!(derive_cid("übersicht.png"), "bersicht");
        // Nothing usable left over: must still yield a valid id.
        assert_eq!(derive_cid("äöü.png"), "image");
        assert_eq!(derive_cid(".png"), "image");
        // Every derived id has to satisfy the same rule an explicit one does.
        for name in ["a b.png", "ä.png", "x!y.png", "no-extension"] {
            assert!(
                derive_cid(name).chars().all(is_cid_char),
                "derived id for {name} must be safe"
            );
        }
    }

    #[test]
    fn marker_without_attachment_is_rejected() {
        // No inline attachments at all: the message should tell the caller how
        // to pass one, not just that something is missing.
        let err = check_cid_markers("see ![](cid:shot)", &[]).unwrap_err();
        assert!(err.contains("shot"), "{err}");
        assert!(err.contains("inline"), "{err}");

        // Some exist, but not this one: list what is available.
        let err = check_cid_markers("see ![](cid:typo)", &[img("shot")]).unwrap_err();
        assert!(err.contains("typo"), "{err}");
        assert!(err.contains("shot"), "{err}");
    }

    #[test]
    fn unreferenced_inline_attachment_only_warns() {
        let notice = check_cid_markers("no marker here", &[img("shot")])
            .expect("must not be an error")
            .expect("should warn");
        assert!(notice.contains("shot"), "{notice}");

        // Matching marker and attachment: silence.
        assert!(
            check_cid_markers("![](cid:shot)", &[img("shot")])
                .unwrap()
                .is_none()
        );
        // Nothing inline and no markers: also silence.
        assert!(check_cid_markers("plain body", &[]).unwrap().is_none());
    }

    #[test]
    fn inline_images_produce_a_related_tree_with_content_ids() {
        let prepared = PreparedAttachments {
            regular: vec![],
            inline: vec![img("shot")],
        };
        let builder = apply_bodies_and_attachments(
            MessageBuilder::new().subject("t"),
            "plain",
            "<html><body><img src=\"cid:shot\"></body></html>",
            prepared,
        );
        let mime = String::from_utf8(builder.write_to_vec().expect("builds")).expect("utf8");

        assert!(mime.contains("multipart/related"), "{mime}");
        assert!(mime.contains("multipart/alternative"), "{mime}");
        // The Content-ID must be angle-bracketed, otherwise `cid:` lookups
        // fail in strict clients.
        assert!(mime.contains("Content-ID: <shot>"), "{mime}");
        assert!(mime.contains("Content-Disposition: inline"), "{mime}");
        assert!(mime.contains("filename=\"shot.png\""), "{mime}");
        // Exactly one disposition header on the image part — `.inline()` plus
        // `.attachment()` would have produced two.
        assert_eq!(
            mime.matches("Content-Disposition:").count(),
            1,
            "one disposition header expected: {mime}"
        );
    }

    #[test]
    fn inline_and_regular_attachments_nest_correctly() {
        let prepared = PreparedAttachments {
            regular: vec![("application/pdf", "report.pdf".to_string(), vec![9, 9])],
            inline: vec![img("shot")],
        };
        let builder = apply_bodies_and_attachments(
            MessageBuilder::new().subject("t"),
            "plain",
            "<html><body>x</body></html>",
            prepared,
        );
        let mime = String::from_utf8(builder.write_to_vec().expect("builds")).expect("utf8");

        assert!(mime.contains("multipart/mixed"), "{mime}");
        assert!(mime.contains("multipart/related"), "{mime}");
        assert!(mime.contains("Content-ID: <shot>"), "{mime}");
        assert!(mime.contains("filename=\"report.pdf\""), "{mime}");
        // The related subtree has to come before the appended file, otherwise
        // the image is no longer "related" to the HTML that references it.
        let related_at = mime.find("multipart/related").expect("related present");
        let pdf_at = mime.find("report.pdf").expect("pdf present");
        assert!(related_at < pdf_at, "related must precede the attachment");
    }

    #[test]
    fn without_inline_images_the_classic_structure_is_kept() {
        // Regression guard: the common path must not change shape just because
        // the inline feature exists.
        let prepared = PreparedAttachments {
            regular: vec![("application/pdf", "report.pdf".to_string(), vec![9])],
            inline: vec![],
        };
        let builder = apply_bodies_and_attachments(
            MessageBuilder::new().subject("t"),
            "plain",
            "<html><body>x</body></html>",
            prepared,
        );
        let mime = String::from_utf8(builder.write_to_vec().expect("builds")).expect("utf8");

        assert!(mime.contains("multipart/mixed"), "{mime}");
        assert!(mime.contains("multipart/alternative"), "{mime}");
        assert!(!mime.contains("multipart/related"), "{mime}");
        assert!(!mime.contains("Content-ID"), "{mime}");
    }

    /// Scratch directory for the filesystem-backed attachment tests. Named per
    /// test so parallel runs cannot collide.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("imap-mcp-rs-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[tokio::test]
    async fn inline_rejects_types_that_cannot_render_as_an_image() {
        let dir = scratch_dir("inline-type");
        let pdf = dir.join("report.pdf");
        std::fs::write(&pdf, b"%PDF-1.4").expect("write");
        let svg = dir.join("logo.svg");
        std::fs::write(&svg, b"<svg/>").expect("write");
        let allowed = vec![dir.to_string_lossy().into_owned()];

        for (path, expected) in [(&pdf, "application/pdf"), (&svg, "image/svg+xml")] {
            let specs = vec![AttachmentSpec::Detailed {
                path: path.to_string_lossy().into_owned(),
                inline: Some(true),
                cid: None,
            }];
            let err = read_attachments(Some(&specs), &allowed)
                .await
                .expect_err("must be rejected");
            assert!(err.contains(expected), "{err}");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn inline_png_is_split_out_with_a_derived_cid() {
        let dir = scratch_dir("inline-png");
        let png = dir.join("Rollen und Rechte.png");
        std::fs::write(&png, b"\x89PNG").expect("write");
        let pdf = dir.join("report.pdf");
        std::fs::write(&pdf, b"%PDF").expect("write");
        let allowed = vec![dir.to_string_lossy().into_owned()];

        let specs = vec![
            AttachmentSpec::Path(pdf.to_string_lossy().into_owned()),
            AttachmentSpec::Detailed {
                path: png.to_string_lossy().into_owned(),
                inline: Some(true),
                cid: None,
            },
        ];
        let prepared = read_attachments(Some(&specs), &allowed)
            .await
            .expect("accepted");

        assert_eq!(prepared.regular.len(), 1, "the pdf stays a regular file");
        assert_eq!(prepared.regular[0].1, "report.pdf");
        assert_eq!(prepared.inline.len(), 1);
        assert_eq!(prepared.inline[0].cid, "Rollen-und-Rechte");
        assert_eq!(prepared.inline[0].content_type, "image/png");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn duplicate_cids_are_rejected() {
        // Two files with the same stem in different folders derive the same
        // id; a marker could then no longer say which one it means.
        let dir = scratch_dir("dup-cid");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).expect("subdir");
        std::fs::write(dir.join("shot.png"), b"a").expect("write");
        std::fs::write(sub.join("shot.png"), b"b").expect("write");
        let allowed = vec![dir.to_string_lossy().into_owned()];

        let specs = vec![
            AttachmentSpec::Detailed {
                path: dir.join("shot.png").to_string_lossy().into_owned(),
                inline: Some(true),
                cid: None,
            },
            AttachmentSpec::Detailed {
                path: sub.join("shot.png").to_string_lossy().into_owned(),
                inline: Some(true),
                cid: None,
            },
        ];
        let err = read_attachments(Some(&specs), &allowed)
            .await
            .expect_err("duplicate must be rejected");
        assert!(err.contains("Duplicate cid"), "{err}");
        assert!(err.contains("shot"), "{err}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn explicit_cid_with_unsafe_characters_is_rejected() {
        let dir = scratch_dir("bad-cid");
        let png = dir.join("shot.png");
        std::fs::write(&png, b"x").expect("write");
        let allowed = vec![dir.to_string_lossy().into_owned()];

        let specs = vec![AttachmentSpec::Detailed {
            path: png.to_string_lossy().into_owned(),
            inline: Some(true),
            cid: Some("a b\"c".to_string()),
        }];
        let err = read_attachments(Some(&specs), &allowed)
            .await
            .expect_err("must be rejected");
        assert!(err.contains("Invalid cid"), "{err}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn inline_image_bytes_are_base64_and_umlaut_filenames_encoded() {
        // Guards the two things a hand-built MIME tree gets wrong most easily:
        // binary data written verbatim (corrupting the image), and a non-ASCII
        // filename dropped raw into a header (an 8-bit header is invalid and
        // some servers reject the APPEND).
        let png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0xFF, 0xFE];
        let prepared = PreparedAttachments {
            regular: vec![],
            inline: vec![InlineImage {
                cid: "rollen".to_string(),
                filename: "Rollenübersicht.png".to_string(),
                content_type: "image/png",
                bytes: png,
            }],
        };
        let builder = apply_bodies_and_attachments(
            MessageBuilder::new().subject("t"),
            "plain",
            "<html><body><img src=\"cid:rollen\"></body></html>",
            prepared,
        );
        let mime = String::from_utf8_lossy(&builder.write_to_vec().expect("builds")).to_string();

        assert!(
            mime.contains("Content-Transfer-Encoding: base64"),
            "image must be base64, not raw: {mime}"
        );
        // The raw PNG signature must not appear literally anywhere.
        assert!(
            !mime.contains("\u{89}PNG"),
            "binary leaked into the message unencoded: {mime}"
        );
        // Header stays 7-bit clean: the umlaut is encoded, not passed through.
        assert!(!mime.contains("Rollenübersicht.png"), "{mime}");
        assert!(mime.contains("Rollen=C3=BCbersicht.png"), "{mime}");
    }
}
