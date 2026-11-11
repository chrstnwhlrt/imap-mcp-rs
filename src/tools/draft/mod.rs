//! Draft composition: reply, forward, and fresh-compose.
//!
//! All three follow the same skeleton:
//! 1. Resolve account + check `read_only`, then validate the caller's input
//!    (size caps, recipients) — before any IMAP traffic.
//! 2. (Reply / forward only) fetch the original — under a short mutex hold.
//! 3. Read the attachments and build the plaintext + HTML bodies — pure CPU
//!    work, mutex released.
//! 4. APPEND to the account's Drafts folder.
//!
//! Sanitization runs at the boundary between untrusted input (parsed mail
//! headers, LLM tool args) and `mail-builder`: every recipient and
//! Message-ID passes through `sanitize_header_value` to strip CR/LF that
//! would otherwise inject extra headers (e.g. a silent `Bcc:`).
//!
//! Rendering helpers (Locale presets, Outlook Web HTML) live in `render`.

use std::sync::Arc;

use mail_builder::MessageBuilder;
use mail_builder::headers::content_type::ContentType;
use mail_builder::mime::MimePart;
use rmcp::schemars;
use serde::Deserialize;

use tokio::sync::Mutex;

use crate::email::EmailFull;
use crate::imap_client::ImapClient;

use super::{ImapMcpServer, ToolResult, error_json};

mod render;
use render::{
    InlineRef, Locale, MAX_MARKER_ALT_BYTES, MAX_MARKER_CID_BYTES, Signatures, apply_from,
    build_compose_bodies, build_forward_bodies, build_reply_bodies, inspect_markers,
};

/// One entry of a draft's `attachments` list.
///
/// Accepts either a bare path — the original and still most common form — or
/// an object that additionally marks the file as an inline image the body
/// refers to. Both spellings coexist in the same array, so callers using
/// either shape keep working:
///
/// ```json
/// ["/path/report.pdf", {"path": "/path/shot.png", "inline": true, "cid": "shot"}]
/// ```
///
/// Deserialization is hand-written rather than `#[serde(untagged)]`-derived,
/// for two failure modes the derive gets wrong: it ignores unknown fields, so
/// a typo like `"inlin": true` silently degrades the entry to a regular
/// attachment, and when nothing matches it reports only "data did not match
/// any variant" — no field name, no hint. The manual impl rejects unknown
/// fields with serde's precise message and names the accepted shapes. That
/// rejection is deliberate wire strictness: an object carrying extra fields
/// fails rather than having them silently ignored.
///
/// The `untagged` attribute below is NOT dead serde config: schemars reads
/// it to emit the `anyOf` schema. Removing it would flip the served schema
/// to an externally-tagged `oneOf` and break every currently valid
/// `attachments` array against strict client-side validation — the incident
/// class of the 2026-07-28 handshake, which `draft_request_schemas_are_ref_free`
/// pins.
///
/// The schema carries a short description of its own: schemars would
/// otherwise serve this entire comment, serde notes included, to every client
/// in all three draft tools.
#[derive(Debug, schemars::JsonSchema)]
#[serde(untagged)]
#[schemars(inline)]
#[schemars(description = "A file path, or {path, inline, cid} for an image shown inside the body.")]
pub enum AttachmentSpec {
    /// A file path, attached as a file.
    Path(String),
    /// A file path and where the file shows.
    Detailed(DetailedAttachment),
}

/// Object form of an [`AttachmentSpec`] entry.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(inline)]
pub struct DetailedAttachment {
    #[schemars(description = "The file's path, as saved_to gives it.")]
    pub path: String,
    #[schemars(description = "true: show the image inside the text; cid implies it.")]
    pub inline: Option<bool>,
    #[schemars(
        description = "The name body uses for the image, as in ![alt](cid:name): letters, digits, '.', '_' and '-'; taken from the file name when left out."
    )]
    pub cid: Option<String>,
}

impl<'de> Deserialize<'de> for AttachmentSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(path) => Ok(Self::Path(path)),
            serde_json::Value::Object(_) => serde_json::from_value::<DetailedAttachment>(value)
                .map(Self::Detailed)
                .map_err(|e| D::Error::custom(format_args!("invalid attachment object: {e}"))),
            other => Err(D::Error::custom(format_args!(
                "attachment entries must be a path string or an object with `path` \
                 (plus optional `inline`, `cid`), got {}",
                json_type_name(&other)
            ))),
        }
    }
}

/// Human word for a JSON value's type, for the error above.
const fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

impl AttachmentSpec {
    fn path(&self) -> &str {
        match self {
            Self::Path(p) => p,
            Self::Detailed(d) => &d.path,
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
            Self::Detailed(d) => d.inline.unwrap_or_else(|| d.cid.is_some()),
        }
    }

    fn explicit_cid(&self) -> Option<&str> {
        match self {
            Self::Path(_) => None,
            Self::Detailed(d) => d.cid.as_deref(),
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
/// Message-ID on `localhost` (on the **machine's hostname** with its default
/// `gethostname` feature, which Cargo.toml disables for exactly that leak)
/// and the Date in UTC, while desktop clients write their sending domain and
/// the local UTC offset. Setting them explicitly makes the draft
/// indistinguishable from a hand-written one.
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

// Field order and texts as for the read tools (see `read.rs`): what a model
// reads, in the order it should fill them. The `body` texts end in a full
// stop that `surface.rs` turns into the signature clause where the server
// signs the draft. `to` and `subject` are required in the schema but
// `Option` here: a revision that leaves them out takes them from the draft
// it replaces (see `Revision`).

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftReplyRequest {
    #[schemars(
        description = "The email's account, as its row names it; the draft is saved there."
    )]
    pub account: Option<String>,
    #[schemars(description = "Folder containing the email to reply to (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(description = "Email UID to reply to (from list_emails or search_emails results)")]
    pub uid: u32,
    #[schemars(description = "The message in the user's words.")]
    pub body: String,
    #[schemars(description = "true: also to everyone else the email went to.")]
    pub reply_all: Option<bool>,
    #[schemars(description = "More addresses to put in copy (CC), one per entry.")]
    pub cc: Option<Vec<String>>,
    #[schemars(
        description = "Files to attach, one per entry: a saved_to path from download_attachment. For an image inside the text, pass {path, inline: true, cid: \"logo\"} and write ![Logo](cid:logo) in body where it belongs."
    )]
    pub attachments: Option<Vec<AttachmentSpec>>,
    #[schemars(
        description = "To change a draft the user already has: its uid from list_drafts. The new version takes its place."
    )]
    pub replaces_uid: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftForwardRequest {
    #[schemars(
        description = "The email's account, as its row names it; the draft is saved there."
    )]
    pub account: Option<String>,
    #[schemars(description = "Folder containing the email to forward (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(description = "Email UID to forward (from list_emails or search_emails results)")]
    pub uid: u32,
    #[schemars(
        required,
        description = "Email addresses, one per entry, such as \"ben@example.de\" or \"Ben <ben@example.de>\". Look an address up before asking the user: search_emails from: the name; for a person named by role, list_emails INBOX shows who wrote."
    )]
    pub to: Option<Vec<String>>,
    #[schemars(description = "A note above the forwarded email, in the user's words.")]
    pub body: Option<String>,
    #[schemars(description = "Addresses to put in copy (CC), one per entry.")]
    pub cc: Option<Vec<String>>,
    #[schemars(
        description = "Files to attach, one per entry: a saved_to path from download_attachment. For an image inside the text, pass {path, inline: true, cid: \"logo\"} and write ![Logo](cid:logo) in body where it belongs."
    )]
    pub attachments: Option<Vec<AttachmentSpec>>,
    #[schemars(
        description = "To change a draft the user already has: its uid from list_drafts. The new version takes its place."
    )]
    pub replaces_uid: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteDraftRequest {
    #[schemars(description = "The drafts' account, as list_drafts names it.")]
    pub account: Option<String>,
    #[schemars(description = "Draft UIDs to delete (from list_drafts results). Pass one or many.")]
    pub uids: Vec<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftEmailRequest {
    #[schemars(description = "The account to write from.")]
    pub account: Option<String>,
    #[schemars(
        required,
        description = "Email addresses, one per entry, such as \"ben@example.de\" or \"Ben <ben@example.de>\". Look an address up before asking the user: search_emails from: the name; for a person named by role, list_emails INBOX shows who wrote."
    )]
    pub to: Option<Vec<String>>,
    #[schemars(required, description = "Email subject line (plain text)")]
    pub subject: Option<String>,
    #[schemars(description = "The message in the user's words.")]
    pub body: String,
    #[schemars(description = "Addresses to put in copy (CC), one per entry.")]
    pub cc: Option<Vec<String>>,
    #[schemars(description = "Addresses in blind copy (BCC), hidden from the other recipients.")]
    pub bcc: Option<Vec<String>>,
    #[schemars(
        description = "Files to attach, one per entry: a saved_to path from download_attachment. For an image inside the text, pass {path, inline: true, cid: \"logo\"} and write ![Logo](cid:logo) in body where it belongs."
    )]
    pub attachments: Option<Vec<AttachmentSpec>>,
    #[schemars(
        description = "To change a draft the user already has: its uid from list_drafts. The new version takes its place."
    )]
    pub replaces_uid: Option<u32>,
}

// ========== Tool implementations ==========

/// Fetch the message a reply or forward is based on, parsed and as the
/// server stores it (the images a quote shows and the attachments a forward
/// carries exist only there). The error side is the finished tool response.
///
/// Locks only for the fetch — CPU work (HTML escape, quote building, MIME
/// serialization) happens outside the mutex so parallel tool calls on the
/// same account aren't blocked on this draft.
async fn fetch_original_with_raw(
    client_arc: &Arc<Mutex<ImapClient>>,
    folder: &str,
    uid: u32,
) -> Result<(EmailFull, Vec<u8>), String> {
    let mut client = client_arc.lock().await;
    let folder = super::folder_for(&mut client, folder).await?;
    match client.get_email_with_raw(&folder, uid).await {
        Ok(Some(found)) => Ok(found),
        Ok(None) => Err(error_json(&format!(
            "Email with UID {uid} not found in {}",
            crate::email::sanitize_external_str(&folder)
        ))),
        Err(e) => Err(error_json(&client.check_error(e).to_string())),
    }
}

/// The attachments of a message being forwarded or replied to, as a mail
/// client carries them along: every part `get_email` lists (see
/// [`crate::email::attachment_parts`]), decoded, under the name and type it
/// shows there. A part the original's HTML shows by its Content-ID (the logo
/// in an Outlook signature, a pasted screenshot) stays inline under that id,
/// next to the quote that shows it; carried as a file it became an extra
/// attachment beside a broken image in the quote. Every other part is a
/// regular attachment, which a forward carries and a reply leaves out.
fn original_attachments(raw: &[u8], html: Option<&str>) -> (AttachmentData, Vec<InlineImage>) {
    let mut regular = Vec::new();
    let mut inline = Vec::new();
    let Some(message) = mail_parser::MessageParser::default().parse(raw) else {
        return (regular, inline);
    };
    for part in crate::email::attachment_parts(&message) {
        let content_type = crate::email::format_content_type(part.content_type());
        let filename =
            crate::email::sanitize_external_str(part.attachment_name().unwrap_or("attachment"));
        let bytes = part.decoded().into_owned();
        let shown = part
            .content_id()
            .map(|id| sanitize_header_value(id.trim().trim_matches(['<', '>'])))
            .filter(|id| !id.is_empty() && html.is_some_and(|h| h.contains(&format!("cid:{id}"))));
        match shown {
            Some(id) => inline.push(InlineImage {
                cid: id.clone(),
                content_id: id,
                filename,
                content_type,
                bytes,
            }),
            None => regular.push((content_type, filename, bytes)),
        }
    }
    (regular, inline)
}

/// Put the original's attachments ahead of the files a forward passes. A
/// passed file with the same content as a carried one is left out: a model
/// that downloaded the invoice to forward it, as `download_attachment`
/// suggests, would otherwise attach it twice.
fn carry_along(passed: &mut AttachmentData, carried: AttachmentData) {
    passed.retain(|(_, _, bytes)| !carried.iter().any(|(_, _, carried)| carried == bytes));
    passed.splice(0..0, carried);
}

/// The `saved` of every draft response.
const SAVED_AS_DRAFT: &str = "as a draft in Drafts; nothing was sent, the user sends it";

/// What a revision leaves out, taken from the draft it replaces. A model
/// revising a draft sends the new text and `replaces_uid`, not the
/// addresses and the subject again: Gemma failed twice on the missing
/// `to` and `subject` before its third call carried them.
struct Revision {
    to: Vec<String>,
    cc: Vec<String>,
    subject: String,
}

impl Revision {
    /// The recipients and the subject of draft `uid`, as entries the
    /// recipient check takes (`Name <address>`); `None` when there is no
    /// such draft.
    async fn of(client_arc: &Arc<Mutex<ImapClient>>, uid: u32) -> Result<Option<Self>, String> {
        let entry = |a: &crate::email::EmailAddress| match a.name.as_deref() {
            Some(name) if !name.trim().is_empty() => format!("{name} <{}>", a.address),
            _ => a.address.clone(),
        };
        let mut client = client_arc.lock().await;
        let drafts = match client.detect_drafts_folder().await {
            Ok(Some(folder)) => folder,
            Ok(None) => {
                return Err(error_json(
                    "No Drafts folder found on server. Create one manually via your webmail \
                     client.",
                ));
            }
            Err(e) => return Err(error_json(&client.check_error(e).to_string())),
        };
        match client.get_email(&drafts, uid).await {
            Ok(draft) => Ok(draft.map(|draft| Self {
                to: draft.to.iter().map(entry).collect(),
                cc: draft.cc.iter().map(entry).collect(),
                subject: draft.subject,
            })),
            Err(e) => Err(error_json(&client.check_error(e).to_string())),
        }
    }
}

/// The addressing of a draft as a call gives it.
struct DraftFields {
    to: Option<Vec<String>>,
    cc: Option<Vec<String>>,
    subject: Option<String>,
    /// Whether the tool takes a subject; a forward derives its own.
    takes_subject: bool,
}

/// `fields` complete. A revision (`replaces_uid`) keeps what it leaves out
/// of `to`, `cc` and the subject from the draft it replaces; a new draft
/// that leaves out `to` or the subject is the error naming everything
/// missing at once.
async fn revised(
    client_arc: &Arc<Mutex<ImapClient>>,
    replaces_uid: Option<u32>,
    mut fields: DraftFields,
) -> Result<DraftFields, String> {
    fields.to = fields.to.filter(|t| !t.is_empty());
    let subject_missing = fields.takes_subject && fields.subject.is_none();
    let required_missing = fields.to.is_none() || subject_missing;
    let (missing, what) = match (fields.to.is_none(), subject_missing) {
        (true, true) => ("to and subject are", "recipients and the subject"),
        (true, false) => ("to is", "recipients"),
        _ => ("subject is", "subject"),
    };
    let Some(uid) = replaces_uid else {
        if required_missing {
            return Err(error_json(&format!(
                "{missing} missing: pass the {what}. A revision with replaces_uid keeps those of \
                 the draft it replaces."
            )));
        }
        return Ok(fields);
    };
    if !required_missing && fields.cc.is_some() {
        return Ok(fields);
    }
    let Some(old) = Revision::of(client_arc, uid).await? else {
        if required_missing {
            return Err(error_json(&format!(
                "{missing} missing, and there is no draft with uid {uid} to keep them from: pass \
                 the {what}, or a draft's uid from list_drafts as replaces_uid."
            )));
        }
        // Only the cc was left to keep: the draft goes ahead without one,
        // and the reply says the uid named no draft (`replace_warning`).
        return Ok(fields);
    };
    fields.to = fields.to.or(Some(old.to));
    fields.cc = fields.cc.or(Some(old.cc));
    if fields.takes_subject {
        fields.subject = fields.subject.or(Some(old.subject));
    }
    Ok(fields)
}

/// Common tail of all three draft tools: report the new draft's UID, honour
/// `replaces_uid` (if given) and serialize. Keeping it here means the
/// append-then-expunge order is implemented once rather than three times.
///
/// `uid` is absent when the server did not let us identify the appended
/// message; the draft was still saved, so it is omitted rather than nulled.
///
/// A `replaces_uid` equal to the new draft's own UID deletes nothing: the
/// UID did not exist when the call came in (the server hands out the next
/// free one), so it named no older draft, and deleting it would throw away
/// the draft just written.
async fn finish_draft(
    client_arc: &Arc<Mutex<ImapClient>>,
    uid: Option<u32>,
    replaces_uid: Option<u32>,
    mut response: serde_json::Value,
) -> ToolResult {
    if let Some(fields) = response.as_object_mut() {
        // What happened, in words, right after `status`: told only "ok", a
        // model reported a forward as "weitergeleitet" and a reply as sent.
        let at = fields
            .keys()
            .position(|k| k == "status")
            .map_or(0, |i| i + 1);
        fields.shift_insert(at, "saved".into(), SAVED_AS_DRAFT.into());
        // Right after `account`, before the preview text: the handle a
        // revision passes back as `replaces_uid`.
        if let Some(uid) = uid {
            let at = fields
                .keys()
                .position(|k| k == "account")
                .map_or(fields.len(), |i| i + 1);
            fields.shift_insert(at, "uid".into(), uid.into());
        }
    }
    match replaces_uid {
        Some(old_uid) if uid == Some(old_uid) => {
            response["replace_warning"] = serde_json::json!(format!(
                "replaces_uid {old_uid} is the UID this new draft was saved under, so it named \
                 no older draft and nothing was deleted — check list_drafts for the version \
                 you meant to replace"
            ));
        }
        Some(old_uid) => remove_replaced_draft(client_arc, old_uid, &mut response).await,
        None => {}
    }
    serde_json::to_string(&response).map_err(|e| error_json(&e.to_string()))
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

pub async fn draft_reply(server: &ImapMcpServer, req: DraftReplyRequest) -> ToolResult {
    let (account_config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return Err(error_json(&e)),
    };
    if account_config.read_only {
        return Err(error_json("Account is configured as read-only"));
    }
    if req.body.len() > MAX_BODY_BYTES {
        return Err(error_json(&format!(
            "Reply body exceeds {MAX_BODY_BYTES}-byte cap"
        )));
    }
    // Validated before the original is fetched: a malformed address is the
    // caller's to fix, no IMAP round trip needed to say so.
    let extra_cc = match parse_recipients(req.cc.as_deref()) {
        Ok(list) => list,
        Err(e) => return Err(error_json(&e)),
    };
    let acct = DraftAccount::from_config(account_config);

    let (original, raw) = fetch_original_with_raw(&client_arc, &req.folder, req.uid).await?;
    // A reply carries no attachments, but the images its quote shows (a
    // signature's logo) stay, as in every mail client: without them the
    // quote showed broken pictures.
    let (_, quoted_images) = original_attachments(&raw, original.body_html.as_deref());
    drop(raw);

    let reply_all = req.reply_all.unwrap_or(false);
    let (to_list, cc_list) =
        match build_reply_recipients(&original, reply_all, &acct.from, extra_cc) {
            Ok(pair) => pair,
            Err(e) => return Err(error_json(e)),
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
    let (mut prepared, inline_notice) = match prepare_attachments(
        req.attachments.as_deref(),
        &server.config.allowed_attachment_dirs,
        &req.body,
        &acct.message_id_domain,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return Err(error_json(&e)),
    };
    prepared.inline.extend(quoted_images);

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
        Err(e) => return Err(error_json(&format!("Failed to build MIME message: {e}"))),
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
        Err(e) => Err(error_json(&format!("Failed to save draft: {e}"))),
    }
}

pub async fn draft_forward(server: &ImapMcpServer, req: DraftForwardRequest) -> ToolResult {
    let (account_config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return Err(error_json(&e)),
    };
    if account_config.read_only {
        return Err(error_json("Account is configured as read-only"));
    }
    if let Some(b) = &req.body
        && b.len() > MAX_BODY_BYTES
    {
        return Err(error_json(&format!(
            "Forward body exceeds {MAX_BODY_BYTES}-byte cap"
        )));
    }
    let fields = DraftFields {
        to: req.to,
        cc: req.cc,
        subject: None,
        takes_subject: false,
    };
    let fields = revised(&client_arc, req.replaces_uid, fields).await?;
    // Before the original is fetched — see draft_reply.
    let (to, cc) = match (
        required_recipients(fields.to.as_deref().unwrap_or_default()),
        parse_recipients(fields.cc.as_deref()),
    ) {
        (Ok(to), Ok(cc)) => (to, cc),
        (Err(e), _) | (_, Err(e)) => return Err(error_json(&e)),
    };
    let acct = DraftAccount::from_config(account_config);

    let (original, raw) = fetch_original_with_raw(&client_arc, &req.folder, req.uid).await?;
    // A forward carries the original's attachments, as every mail client
    // does: forwarding an invoice without the invoice helped nobody, and the
    // draft quoted the text alone. Ahead of the files the call adds.
    let (carried, carried_inline) = original_attachments(&raw, original.body_html.as_deref());
    drop(raw);

    let subject_raw = if has_forward_prefix(&original.subject) {
        original.subject.clone()
    } else {
        format!("{}{}", acct.locale.forward_prefix(), original.subject)
    };
    // `original.subject` can contain `\r\n` header-injection payloads.
    let subject = sanitize_header_value(&subject_raw);

    // Read before rendering — see the note in draft_reply.
    let (mut prepared, inline_notice) = match prepare_attachments(
        req.attachments.as_deref(),
        &server.config.allowed_attachment_dirs,
        req.body.as_deref().unwrap_or(""),
        &acct.message_id_domain,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return Err(error_json(&e)),
    };
    carry_along(&mut prepared.regular, carried);
    prepared.inline.extend(carried_inline);

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

    // Pass each list once; `.to()` / `.cc()` overwrite on repeat calls (same
    // bug that affected draft_reply before the fix). The response below
    // echoes the parsed addresses — echoing the RAW request values instead
    // would claim recipients the saved headers do not carry (e.g. with an
    // injected `\r\nBcc:` stripped out).
    builder = builder.to(to_address_list(&to));
    if !cc.is_empty() {
        builder = builder.cc(to_address_list(&cc));
    }

    builder = apply_bodies_and_attachments(builder, &plain_body, &html_body, prepared);

    let message_bytes = match builder.write_to_vec() {
        Ok(bytes) => bytes,
        Err(e) => return Err(error_json(&format!("Failed to build MIME message: {e}"))),
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
                "to": addresses_only(&to),
                "cc": addresses_only(&cc),
                "subject": subject,
                "body_preview": truncate(&plain_body, 500),
            });
            if let Some(notice) = inline_notice {
                response["inline_warning"] = serde_json::Value::String(notice);
            }
            finish_draft(&client_arc, new_uid, req.replaces_uid, response).await
        }
        Err(e) => Err(error_json(&format!("Failed to save draft: {e}"))),
    }
}

pub async fn draft_email(server: &ImapMcpServer, req: DraftEmailRequest) -> ToolResult {
    let (account_config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return Err(error_json(&e)),
    };
    if account_config.read_only {
        return Err(error_json("Account is configured as read-only"));
    }
    if req.body.len() > MAX_BODY_BYTES {
        return Err(error_json(&format!(
            "Draft body exceeds {MAX_BODY_BYTES}-byte cap"
        )));
    }
    let fields = DraftFields {
        to: req.to,
        cc: req.cc,
        subject: req.subject,
        takes_subject: true,
    };
    let fields = revised(&client_arc, req.replaces_uid, fields).await?;
    let subject = fields.subject.unwrap_or_default();
    if subject.len() > MAX_SUBJECT_BYTES {
        return Err(error_json(&format!(
            "Draft subject exceeds {MAX_SUBJECT_BYTES}-byte cap"
        )));
    }
    let (to, cc, bcc) = match (
        required_recipients(fields.to.as_deref().unwrap_or_default()),
        parse_recipients(fields.cc.as_deref()),
        parse_recipients(req.bcc.as_deref()),
    ) {
        (Ok(to), Ok(cc), Ok(bcc)) => (to, cc, bcc),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return Err(error_json(&e)),
    };
    let acct = DraftAccount::from_config(account_config);

    // Read before rendering — see the note in draft_reply.
    let (prepared, inline_notice) = match prepare_attachments(
        req.attachments.as_deref(),
        &server.config.allowed_attachment_dirs,
        &req.body,
        &acct.message_id_domain,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return Err(error_json(&e)),
    };

    let (plain_body, html_body) = {
        let refs = inline_refs(&prepared.inline);
        build_compose_bodies(&req.body, acct.locale, &acct.signatures, &refs)
    };

    // Sanitize subject + recipients against header injection from LLM input.
    let subject = sanitize_header_value(&subject);
    let mut builder = MessageBuilder::new().subject(&subject);
    builder = apply_from(builder, &acct.from, acct.display_name.as_deref());
    builder = stamp_identity_headers(builder, &acct.message_id_domain);

    // Pass each list once — mail-builder's `.to()` / `.cc()` / `.bcc()`
    // OVERWRITE on repeat calls, so the per-address loop silently dropped
    // every recipient except the last. The response echoes the parsed
    // addresses, i.e. what the saved headers actually carry.
    builder = builder.to(to_address_list(&to));
    if !cc.is_empty() {
        builder = builder.cc(to_address_list(&cc));
    }
    if !bcc.is_empty() {
        builder = builder.bcc(to_address_list(&bcc));
    }

    builder = apply_bodies_and_attachments(builder, &plain_body, &html_body, prepared);

    let message_bytes = match builder.write_to_vec() {
        Ok(bytes) => bytes,
        Err(e) => return Err(error_json(&format!("Failed to build MIME message: {e}"))),
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
                // The sanitized values that went into the headers, matching
                // reply — echoing the raw input would claim recipients or a
                // subject the saved draft does not carry.
                "to": addresses_only(&to),
                "cc": addresses_only(&cc),
                "bcc": addresses_only(&bcc),
                "subject": subject,
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
        Err(e) => Err(error_json(&format!("Failed to save draft: {e}"))),
    }
}

pub async fn delete_draft(server: &ImapMcpServer, req: DeleteDraftRequest) -> ToolResult {
    // Deliberately far below `tools/write.rs::MAX_UIDS_PER_CALL` (1000):
    // deleting drafts bypasses `allow_delete` (the Drafts folder is the
    // user's workspace) and uses EXPUNGE, so there is no Trash to recover
    // from. Replacing a draft touches one UID and tidying up a handful is
    // normal; wiping a whole Drafts folder in a single call has no
    // legitimate use and is exactly what a prompt-injected model would try.
    const MAX_DRAFT_UIDS_PER_CALL: usize = 25;
    if req.uids.len() > MAX_DRAFT_UIDS_PER_CALL {
        return Err(error_json(&format!(
            "uids list exceeds {MAX_DRAFT_UIDS_PER_CALL}-item cap for draft deletion — \
             delete in smaller batches (drafts are expunged, not moved to Trash)"
        )));
    }
    let (account_config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return Err(error_json(&e)),
    };
    if account_config.read_only {
        return Err(error_json("Account is configured as read-only"));
    }
    let account_name = account_config.name.clone();
    let mut client = client_arc.lock().await;
    match client.delete_draft(&req.uids).await {
        // `failed` = input − existing, the same shape (and the same code) as
        // move/delete: an empty array is a statement, a gap in `succeeded`
        // is not.
        Ok(succeeded) => super::write::write_result(&account_name, &succeeded, &req.uids),
        Err(e) => Err(error_json(&client.check_error(e).to_string())),
    }
}

// ========== Reply / draft helpers ==========

/// Build `(to_list, cc_list)` for a reply. Addresses from the original pass
/// through `sanitize_header_value` to strip `\r\n` header-injection payloads;
/// `extra_cc` arrives already parsed and validated ([`parse_recipients`]).
///
/// The reply goes where the sender asks replies to go: the `Reply-To`
/// addresses when the original names any, as every mail client reads it,
/// otherwise its `From`. Ignoring `Reply-To` sent replies to a `noreply@`
/// sender instead of the service desk the mail named.
///
/// In reply-all, the user's own `from` address is excluded (no self-addressed
/// drafts) and entries without an address (a bare group name, a display name
/// with nothing behind it) are skipped rather than written as an empty `<>`.
/// An address appears only once across To and Cc, `extra_cc` included: a
/// sender who copied themselves, or an extra CC naming someone already
/// addressed, would otherwise be listed twice.
fn build_reply_recipients(
    original: &EmailFull,
    reply_all: bool,
    from: &str,
    extra_cc: Vec<Recipient>,
) -> Result<(Vec<Recipient>, Vec<Recipient>), &'static str> {
    let named: Vec<&crate::email::EmailAddress> = original
        .reply_to
        .iter()
        .filter(|a| !a.address.is_empty())
        .collect();
    let primary = if named.is_empty() {
        match original.from.as_ref() {
            Some(a) if !a.address.is_empty() => vec![a],
            _ => return Err("Cannot reply: original email has no sender address"),
        }
    } else {
        named
    };

    let mut seen = std::collections::HashSet::new();
    let mut to_list: Vec<Recipient> = Vec::new();
    for addr in primary {
        if seen.insert(addr.address.to_ascii_lowercase()) {
            to_list.push(recipient_from(addr));
        }
    }
    let mut keep = |addr: &crate::email::EmailAddress| {
        !addr.address.is_empty()
            && !addr.address.eq_ignore_ascii_case(from)
            && seen.insert(addr.address.to_ascii_lowercase())
    };
    let mut cc_list: Vec<Recipient> = Vec::new();
    if reply_all {
        to_list.extend(original.to.iter().filter(|a| keep(a)).map(recipient_from));
        cc_list.extend(original.cc.iter().filter(|a| keep(a)).map(recipient_from));
    }
    cc_list.extend(
        extra_cc
            .into_iter()
            .filter(|(_, addr)| seen.insert(addr.to_ascii_lowercase())),
    );
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
    // RFC 5322 3.6.4: the parent's References, or where it has none, its
    // In-Reply-To, then its own Message-ID. Without the fallback a reply to a
    // reply that carried only In-Reply-To lost the thread's start.
    let ancestors: &[String] = if original.references.is_empty() {
        original.in_reply_to.as_slice()
    } else {
        &original.references
    };
    let refs: Vec<String> = ancestors
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
/// APPEND, wasting server storage and bandwidth. 10 MiB fits every
/// realistic body including formatted ones; a subject is one header line
/// (RFC 5322 caps a line at 998 characters, folding allows more), and no
/// legitimate one comes near 2 KiB even in a multi-byte script.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
const MAX_SUBJECT_BYTES: usize = 2 * 1024;

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..s.floor_char_boundary(max_len)])
    }
}

// ========== Attachment helpers ==========

/// Regular (appended) attachments as `(content_type, filename, bytes)`.
type AttachmentData = Vec<(String, String, Vec<u8>)>;

/// An attachment that is embedded in the body rather than appended to it.
pub(super) struct InlineImage {
    /// User-facing id: what body markers, error messages and
    /// `inline_warning` call this image.
    pub cid: String,
    /// Wire identity: the `Content-ID` header value the HTML's `src="cid:…"`
    /// references. RFC 2045 msg-id shape (`local@domain`) and globally
    /// unique — see `read_attachments` for why it differs from `cid`.
    pub content_id: String,
    pub filename: String,
    pub content_type: String,
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
                            "{} -> cid:{} as <{}> ({}, {} bytes)",
                            i.filename,
                            i.cid,
                            i.content_id,
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

/// THE rule for a marker id, shared by the body scanner, the explicit-cid
/// check and (via normalization in [`derive_cid`]) the derived ids — three
/// call sites, one predicate, so no accepted attachment id can ever be
/// unreferenceable from a body marker, and vice versa. An earlier split
/// version had exactly that bug: `derive_cid` had no length cap, so a long
/// file-name stem minted an id the scanner rejected by construction — a
/// dead end with a misleading "does not parse as a marker" error.
///
/// The dot rules exist for the wire format: the id becomes the first atom
/// of a `Content-ID` local part (`<id.uuid@domain>`), and RFC 5322
/// dot-atoms permit no leading, trailing or doubled dots.
fn is_valid_cid(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_MARKER_CID_BYTES
        && s.chars().all(is_cid_char)
        && !s.starts_with('.')
        && !s.ends_with('.')
        && !s.contains("..")
}

/// Turn a file name into a content id: drop the extension, map everything
/// outside [`is_cid_char`] to `-`, collapse repeats, then normalize until
/// [`is_valid_cid`] holds (length cap, dot placement). Falls back to `image`
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
    // Dot-atom hygiene: `shot..png` has the stem `shot.`, and doubled dots
    // can come straight from the file name.
    while out.contains("..") {
        out = out.replace("..", ".");
    }
    // ASCII by construction, so byte truncation cannot split a character.
    out.truncate(MAX_MARKER_CID_BYTES);
    let trimmed = out.trim_matches(['-', '.']).to_string();
    debug_assert!(trimmed.is_empty() || is_valid_cid(&trimmed));
    if trimmed.is_empty() {
        "image".to_string()
    } else {
        trimmed
    }
}

async fn read_attachments(
    attachments: Option<&[AttachmentSpec]>,
    allowed_dirs: &[String],
    message_id_domain: &str,
) -> Result<PreparedAttachments, String> {
    // Per-file cap prevents a single huge file from OOMing. Aggregate cap
    // prevents the "many medium files" path: 50 files × 50 MiB = 2.5 GiB of
    // RAM before the MIME builder even runs. 100 MiB total covers every
    // realistic email workflow and most provider send limits anyway. The
    // count cap closes the remaining corner the byte caps miss: thousands of
    // one-byte files, each costing canonicalize/stat/read syscalls, a MIME
    // part, and an entry in every per-ref lookup.
    const MAX_ATTACHMENT_SIZE: usize = 50 * 1024 * 1024;
    const MAX_TOTAL_ATTACHMENTS_SIZE: usize = 100 * 1024 * 1024;
    const MAX_ATTACHMENT_COUNT: usize = 100;

    let Some(specs) = attachments else {
        return Ok(PreparedAttachments {
            regular: vec![],
            inline: vec![],
        });
    };
    if specs.len() > MAX_ATTACHMENT_COUNT {
        return Err(format!(
            "attachments list exceeds the {MAX_ATTACHMENT_COUNT}-entry cap — no realistic \
             email carries more files; split into several drafts"
        ));
    }

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

        // Everything decidable without the file's bytes is decided here,
        // before the read pulls up to 50 MiB into memory for nothing.
        //
        // A body marker always renders an `<img>` tag, so anything that is
        // not an image would produce a broken picture at the recipient's end.
        // Type detection is extension-based, so a correctly named file is
        // required — saying so beats letting the draft go out looking wrong.
        //
        // SVG is excluded on purpose although it is an image type: it can
        // carry script, and inline images may well originate from a received
        // mail via `download_attachment`. Embedding one would forward that
        // payload under our own name, for a format nobody needs for
        // screenshots.
        let is_inline = spec.is_inline();
        if is_inline && (!content_type.starts_with("image/") || content_type == "image/svg+xml") {
            return Err(format!(
                "Attachment \"{path_str}\" is marked inline but its type is {content_type}. Only \
                 raster images can be embedded in the body (a marker renders an <img> tag); SVG is \
                 excluded because it can carry script. Drop `inline` to send it as a regular \
                 attachment."
            ));
        }

        // Size precheck on metadata, so an oversized file on an allowed path
        // is refused without first reading all of it into memory. Advisory —
        // the file can change between stat and read — so the authoritative
        // check below stays on the bytes actually read.
        if let Ok(meta) = tokio::fs::metadata(&canonical).await
            && meta.len() > MAX_ATTACHMENT_SIZE as u64
        {
            return Err(format!(
                "Attachment \"{path_str}\" is {} bytes — exceeds the \
                 {MAX_ATTACHMENT_SIZE}-byte per-file cap",
                meta.len()
            ));
        }

        let bytes = tokio::fs::read(&canonical)
            .await
            .map_err(|e| format!("Failed to read attachment \"{path_str}\": {e}"))?;
        if bytes.len() > MAX_ATTACHMENT_SIZE {
            return Err(format!(
                "Attachment \"{path_str}\" is {} bytes — exceeds the \
                 {MAX_ATTACHMENT_SIZE}-byte per-file cap",
                bytes.len()
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

        if !is_inline {
            regular.push((content_type.to_string(), filename, bytes));
            continue;
        }

        let (cid, content_id) =
            inline_identity(spec, &filename, path_str, &inline, message_id_domain)?;
        inline.push(InlineImage {
            cid,
            content_id,
            filename,
            content_type: content_type.to_string(),
            bytes,
        });
    }
    Ok(PreparedAttachments { regular, inline })
}

/// Resolve one inline attachment's `(cid, content_id)` pair.
///
/// An explicit id is rejected rather than silently cleaned: the caller wrote
/// the same string into the body marker, so quietly changing it here would
/// produce a draft whose image never resolves — the exact failure this
/// feature exists to avoid. The length/alphabet rule matches the body
/// scanner's, so no accepted attachment id is ever unreferenceable.
///
/// The wire `content_id` is distinct from the user-facing `cid`: RFC 2045
/// wants a msg-id-shaped (`local@domain`), globally unique Content-ID.
/// Reusing the marker id verbatim put the same `<screenshot>` into every
/// draft — clients and gateways that cache or deduplicate inline parts by
/// Content-ID then show the wrong image, and a value that also occurs as a
/// `cid:` reference inside a quoted original collides outright. The uuid
/// provides uniqueness; the domain is the same config value the Message-ID
/// uses.
fn inline_identity(
    spec: &AttachmentSpec,
    filename: &str,
    path_str: &str,
    taken: &[InlineImage],
    message_id_domain: &str,
) -> Result<(String, String), String> {
    let cid = match spec.explicit_cid() {
        Some(raw) => {
            if !is_valid_cid(raw) {
                return Err(format!(
                    "Invalid cid \"{raw}\" for attachment \"{path_str}\": use letters, digits, \
                     '.', '_' or '-' only, at most {MAX_MARKER_CID_BYTES} characters, with no \
                     leading, trailing or doubled '.' (the id also appears in the body marker \
                     and in the Content-ID header)"
                ));
            }
            raw.to_string()
        }
        None => derive_cid(filename),
    };

    if taken.iter().any(|i| i.cid == cid) {
        return Err(format!(
            "Duplicate cid \"{cid}\" (attachment \"{path_str}\"): each inline image needs its \
             own id, otherwise a body marker cannot say which one it means"
        ));
    }

    let content_id = format!(
        "{cid}.{}@{}",
        uuid::Uuid::new_v4().simple(),
        sanitize_header_value(message_id_domain)
    );
    Ok((cid, content_id))
}

/// Parse a caller-supplied recipient list ([`parse_recipient`] per entry).
fn parse_recipients(addrs: Option<&[String]>) -> Result<Vec<Recipient>, String> {
    addrs
        .unwrap_or_default()
        .iter()
        .map(|a| parse_recipient(a))
        .collect()
}

/// [`parse_recipients`] for the `to` of a forward or a fresh mail, which
/// must name at least one recipient — the schema says so, and a draft without
/// one is not what the caller asked for.
fn required_recipients(addrs: &[String]) -> Result<Vec<Recipient>, String> {
    let list = parse_recipients(Some(addrs))?;
    if list.is_empty() {
        return Err("At least one `to` recipient is required".to_string());
    }
    Ok(list)
}

/// Parse one caller-supplied recipient: a bare `alice@example.com`, or the
/// `Alice <alice@example.com>` form callers naturally reach for. Sanitized
/// against header injection first (`\r\n` in an address would smuggle a
/// silent `Bcc:` into the saved draft).
///
/// Validated, because mail-builder writes whatever it is handed between `<`
/// and `>`: `Alice <a@b>` passed as a bare address became `<Alice <a@b>>`,
/// a comma-joined list became one broken mailbox, and an entry that
/// sanitized down to nothing became an empty `<>` — each a draft that cannot
/// be sent as is, noticed only when the user opens it.
fn parse_recipient(raw: &str) -> Result<Recipient, String> {
    let clean = sanitize_header_value(raw);
    let entry = clean.trim();
    let (name, addr) = match entry.strip_suffix('>').and_then(|s| s.rsplit_once('<')) {
        Some((name, addr)) => {
            let name = name.trim().trim_matches('"').trim();
            ((!name.is_empty()).then(|| name.to_string()), addr.trim())
        }
        None => (None, entry),
    };
    let well_formed = addr.split_once('@').is_some_and(|(local, domain)| {
        !local.is_empty() && !domain.is_empty() && !domain.contains('@')
    }) && !addr
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | ',' | ';' | '"'));
    if !well_formed {
        // A bare name is what a model passes when it only heard the name:
        // say where the address comes from, not just how one looks.
        if name.is_none() && !addr.is_empty() && !addr.contains('@') {
            return Err(format!(
                "Invalid recipient \"{entry}\": a name alone is not an address. Look it up first \
                 (search_emails with from: \"{entry}\" shows the address of the sender), then pass \
                 \"{entry} <address>\""
            ));
        }
        return Err(format!(
            "Invalid recipient \"{entry}\": expected an address like alice@example.com or \
             \"Alice <alice@example.com>\", one recipient per list entry"
        ));
    }
    Ok((name, addr.to_string()))
}

/// Read the attachments and cross-check the body's inline markers in one step.
///
/// Bundled because all three draft flows need exactly this pair before they
/// can render, and the pair has an order that must not be swapped: markers can
/// only be validated once the attachment list is known. `message_id_domain`
/// feeds the inline parts' Content-ID generation.
async fn prepare_attachments(
    attachments: Option<&[AttachmentSpec]>,
    allowed_dirs: &[String],
    body: &str,
    message_id_domain: &str,
) -> Result<(PreparedAttachments, Option<String>), String> {
    let prepared = read_attachments(attachments, allowed_dirs, message_id_domain).await?;
    let notice = check_cid_markers(body, &prepared.inline)?;
    Ok((prepared, notice))
}

/// Borrowed view of the inline images, for the body renderer.
fn inline_refs(inline: &[InlineImage]) -> Vec<InlineRef<'_>> {
    inline
        .iter()
        .map(|i| InlineRef {
            cid: &i.cid,
            content_id: &i.content_id,
            filename: &i.filename,
        })
        .collect()
}

/// Cross-check the body's `cid:` markers against the inline attachments.
///
/// The mismatches are treated differently on purpose:
///
/// * A **marker without an attachment** is an error. The draft would be saved
///   with an image that resolves to nothing, and neither the caller nor the
///   recipient sees why — exactly the silent failure this feature is meant to
///   prevent. Better to refuse and say which ids exist.
/// * A **malformed marker attempt** — a `](cid:` fragment the strict scanner
///   rejected (id with spaces, alt spanning lines) — is an error *when
///   inline images are in play* (an inline attachment or a valid marker
///   exists): the caller clearly meant a marker, and the draft would be
///   saved showing raw marker source with no hint why. With no inline
///   context at all it degrades to a warning: prose that merely *mentions*
///   the syntax (a draft explaining the feature) must not be unsendable.
/// * An **attachment nobody references** is only a warning. The file still
///   reaches the recipient; it simply lands wherever their client puts
///   unreferenced parts instead of at the intended spot. Returning it as
///   `inline_warning` lets the caller notice and fix the body without losing
///   the draft they just wrote.
fn check_cid_markers(body: &str, inline: &[InlineImage]) -> Result<Option<String>, String> {
    // One scan answers both questions — ids and stray fragments.
    let inspection = inspect_markers(body);
    let markers = inspection.unique_ids;

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

    if let Some(fragment) = inspection.stray_fragment {
        let explanation = format!(
            "a `](cid:` sequence that does not parse as an image marker (near: \"{fragment}\"). \
             Markers are `![alt](cid:<id>)` with an id of letters, digits, '.', '_' or '-' \
             (max {MAX_MARKER_CID_BYTES} chars, no leading/trailing/doubled '.') and a \
             single-line alt text (max {MAX_MARKER_ALT_BYTES} bytes)"
        );
        if !inline.is_empty() || !markers.is_empty() {
            return Err(format!(
                "Body contains {explanation} — the draft would show the raw marker text. \
                 Fix the marker or remove the fragment."
            ));
        }
        // No inline attachments and no valid markers: most likely prose
        // about the syntax. Save, but say what was seen.
        return Ok(Some(format!(
            "Body contains {explanation}. No inline attachments were passed, so it was left \
             as literal text — if an embedded image was intended, fix the marker and pass \
             the image with `inline: true`."
        )));
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
                // The globally unique wire id the HTML's `src="cid:…"` uses,
                // not the user-facing marker id — see `read_attachments`.
                .cid(img.content_id)
                .header(
                    "Content-Disposition",
                    ContentType::new("inline").attribute("filename", img.filename),
                ),
        );
    }
    // RFC 2387 makes the `type` parameter mandatory: it names the root
    // part's media type so a client knows what to render before walking the
    // children. Tolerant clients guess it; strict ones may treat the
    // container as malformed and fall back to showing the alternative as an
    // attachment — the exact failure this hand-built tree exists to avoid.
    let related = MimePart::new(
        ContentType::new("multipart/related").attribute("type", "multipart/alternative"),
        related,
    );

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
            reply_to: vec![],
            subject: subject.to_string(),
            date: None,
            date_original: None,
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

    /// A forward carries what `get_email` lists as attachments, decoded,
    /// under the names shown there: forwarding an invoice mail forwards the
    /// invoice.
    #[test]
    fn a_forward_carries_the_original_attachments() {
        let raw = "From: a@example.com\r\nSubject: Rechnung\r\nMIME-Version: 1.0\r\n\
                   Content-Type: multipart/mixed; boundary=\"B\"\r\n\r\n\
                   --B\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nAnbei die Rechnung.\r\n\
                   --B\r\nContent-Type: application/pdf; name=\"Rechnung.pdf\"\r\n\
                   Content-Disposition: attachment; filename=\"Rechnung.pdf\"\r\n\
                   Content-Transfer-Encoding: base64\r\n\r\nJVBERi0xLjQ=\r\n\
                   --B--\r\n";
        let (carried, inline) = original_attachments(raw.as_bytes(), None);
        assert_eq!(carried.len(), 1, "{carried:?}");
        assert_eq!(carried[0].0, "application/pdf");
        assert_eq!(carried[0].1, "Rechnung.pdf");
        assert_eq!(carried[0].2, b"%PDF-1.4", "decoded, not base64");
        assert!(inline.is_empty(), "{} inline parts", inline.len());
        let (carried, inline) =
            original_attachments(b"From: a@b\r\nSubject: s\r\n\r\nText\r\n", None);
        assert_eq!(carried, Vec::new(), "a plain message carries nothing");
        assert!(inline.is_empty(), "{} inline parts", inline.len());
    }

    /// The original's attachments come first; a passed file with the same
    /// content as one of them (the invoice, downloaded to forward it) is not
    /// attached a second time, a different one is.
    #[test]
    fn carried_attachments_are_not_attached_twice() {
        let file = |name: &str, bytes: &[u8]| {
            (
                "application/pdf".to_string(),
                name.to_string(),
                bytes.to_vec(),
            )
        };
        let mut passed = vec![
            file("Rechnung.pdf", b"%PDF-1"),
            file("Notiz.pdf", b"%PDF-2"),
        ];
        carry_along(&mut passed, vec![file("Rechnung.pdf", b"%PDF-1")]);
        let names: Vec<&str> = passed.iter().map(|(_, name, _)| name.as_str()).collect();
        assert_eq!(names, ["Rechnung.pdf", "Notiz.pdf"]);
    }

    /// The logo in an Outlook signature sits inline under a Content-ID that
    /// the HTML shows: forwarded, it stays inline under that id, so the quote
    /// shows it, instead of turning into an extra attachment beside a broken
    /// image. A file the HTML does not show stays an attachment.
    #[test]
    fn a_forward_keeps_what_the_quote_shows_inline() {
        let html = "<p>Gruss</p><img src=\"cid:image001.png@01DA0000.11112222\">";
        let raw = format!(
            "From: a@example.com\r\nSubject: Logo\r\nMIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"M\"\r\n\r\n\
             --M\r\nContent-Type: multipart/related; boundary=\"R\"\r\n\r\n\
             --R\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{html}\r\n\
             --R\r\nContent-Type: image/png; name=\"image001.png\"\r\n\
             Content-ID: <image001.png@01DA0000.11112222>\r\n\
             Content-Disposition: inline; filename=\"image001.png\"\r\n\
             Content-Transfer-Encoding: base64\r\n\r\niVBORw0KGgo=\r\n--R--\r\n\
             --M\r\nContent-Type: application/pdf; name=\"Angebot.pdf\"\r\n\
             Content-Disposition: attachment; filename=\"Angebot.pdf\"\r\n\
             Content-Transfer-Encoding: base64\r\n\r\nJVBERi0xLjQ=\r\n--M--\r\n"
        );
        let (carried, inline) = original_attachments(raw.as_bytes(), Some(html));
        assert_eq!(carried.len(), 1, "{carried:?}");
        assert_eq!(carried[0].1, "Angebot.pdf");
        assert_eq!(inline.len(), 1, "{} inline parts", inline.len());
        assert_eq!(inline[0].content_id, "image001.png@01DA0000.11112222");
        assert_eq!(inline[0].filename, "image001.png");
        assert_eq!(inline[0].content_type, "image/png");
        // Without HTML that shows it, the image is just another attachment.
        let (carried, inline) = original_attachments(raw.as_bytes(), None);
        assert_eq!(carried.len(), 2, "{carried:?}");
        assert!(inline.is_empty(), "{} inline parts", inline.len());
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
        let (to, cc) = build_reply_recipients(&original, false, "me@example.com", vec![]).unwrap();
        assert_eq!(addresses_only(&to), vec!["alice@example.com"]);
        assert_eq!(cc, vec![]);
    }

    /// A reply goes where the sender asks replies to go: the Reply-To
    /// addresses, not a noreply From. Reply-all adds the other recipients,
    /// the user left out and nobody twice.
    #[test]
    fn build_reply_recipients_follows_reply_to() {
        let mut original = email(
            "Test",
            Some("noreply@shop.example"),
            vec!["me@example.com"],
            vec!["bob@example.com"],
        );
        original.reply_to = vec![addr("service@shop.example"), addr("SERVICE@shop.example")];
        let (to, cc) = build_reply_recipients(&original, false, "me@example.com", vec![]).unwrap();
        assert_eq!(addresses_only(&to), vec!["service@shop.example"]);
        assert_eq!(cc, vec![]);
        let (to, cc) = build_reply_recipients(&original, true, "me@example.com", vec![]).unwrap();
        assert_eq!(addresses_only(&to), vec!["service@shop.example"]);
        assert_eq!(addresses_only(&cc), vec!["bob@example.com"]);
        // A Reply-To is enough to answer a mail without a From.
        original.from = None;
        assert!(build_reply_recipients(&original, false, "me@example.com", vec![]).is_ok());
    }

    /// RFC 5322 3.6.4: a parent without References hands on its In-Reply-To,
    /// so the reply keeps the start of the thread.
    #[test]
    fn references_fall_back_to_in_reply_to() {
        let mut original = email("Test", Some("a@b.com"), vec![], vec![]);
        original.message_id = Some("<m2@example.com>".to_string());
        original.in_reply_to = Some("<m1@example.com>".to_string());
        let (builder, threaded) = apply_threading_headers(
            MessageBuilder::new()
                .from("me@example.com")
                .subject("s")
                .text_body("b"),
            &original,
        );
        assert!(threaded);
        let raw = String::from_utf8(builder.write_to_vec().unwrap()).unwrap();
        let refs = raw
            .lines()
            .find(|l| l.starts_with("References: "))
            .expect("References header");
        assert_eq!(refs, "References: <m1@example.com> <m2@example.com>");
    }

    #[test]
    fn build_reply_recipients_no_sender_errors() {
        let original = email("Test", None, vec!["me@example.com"], vec![]);
        let result = build_reply_recipients(&original, false, "me@example.com", vec![]);
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
        let (to, cc) = build_reply_recipients(&original, true, "me@example.com", vec![]).unwrap();
        assert_eq!(
            addresses_only(&to),
            vec!["alice@example.com", "bob@example.com"]
        );
        assert_eq!(addresses_only(&cc), vec!["carol@example.com"]);
    }

    #[test]
    fn build_reply_recipients_extra_cc_appended() {
        let original = email("Test", Some("alice@example.com"), vec![], vec![]);
        let extra = parse_recipients(Some(&["dave@example.com".to_string()])).unwrap();
        let (_to, cc) = build_reply_recipients(&original, false, "me@example.com", extra).unwrap();
        assert_eq!(addresses_only(&cc), vec!["dave@example.com"]);
    }

    /// An extra CC naming someone the reply already addresses is not listed a
    /// second time; one naming the user stays, since it was asked for.
    #[test]
    fn build_reply_recipients_extra_cc_skips_who_is_already_addressed() {
        let original = email(
            "Test",
            Some("alice@example.com"),
            vec!["bob@example.com"],
            vec![],
        );
        let extra = parse_recipients(Some(&[
            "Alice <ALICE@example.com>".to_string(),
            "bob@example.com".to_string(),
            "dave@example.com".to_string(),
            "Dave <dave@example.com>".to_string(),
            "me@example.com".to_string(),
        ]))
        .unwrap();
        let (to, cc) = build_reply_recipients(&original, true, "me@example.com", extra).unwrap();
        assert_eq!(
            addresses_only(&to),
            vec!["alice@example.com", "bob@example.com"]
        );
        assert_eq!(
            addresses_only(&cc),
            vec!["dave@example.com", "me@example.com"]
        );
    }

    /// A `replaces_uid` that turns out to be the UID the new draft was saved
    /// under names no older draft; deleting it would throw away the draft
    /// just written. Nothing may reach the server (the port is closed: an
    /// attempt would come back as a failed deletion instead).
    #[tokio::test]
    async fn finish_draft_never_deletes_the_draft_it_just_saved() {
        let config: crate::config::ServerConfig = toml::from_str(
            "[[accounts]]\nname = \"T\"\nhost = \"127.0.0.1\"\nport = 1\n\
             username = \"u@h\"\nauth_method = \"password\"\npassword = \"p\"\n",
        )
        .unwrap();
        let client = Arc::new(Mutex::new(ImapClient::new(config.accounts[0].clone())));
        let out = finish_draft(
            &client,
            Some(9),
            Some(9),
            serde_json::json!({"status": "ok"}),
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["uid"], 9);
        assert!(v.get("replaced_uid").is_none(), "{v}");
        // Right behind the status, in words: nothing was sent.
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys[..2], ["status", "saved"], "{v}");
        assert!(
            v["saved"].as_str().unwrap().contains("nothing was sent"),
            "{v}"
        );
        let warning = v["replace_warning"].as_str().unwrap();
        assert!(warning.contains("nothing was deleted"), "{warning}");
    }

    /// Reply-all must address everyone once: a sender who copied themselves
    /// (or appears in To and Cc) is not listed twice, and an entry without
    /// an address is skipped instead of becoming an empty `<>`.
    #[test]
    fn build_reply_recipients_reply_all_dedupes_and_skips_empty_addresses() {
        let original = email(
            "Test",
            Some("alice@example.com"),
            vec!["ALICE@example.com", "bob@example.com", ""],
            vec!["bob@example.com", "carol@example.com"],
        );
        let (to, cc) = build_reply_recipients(&original, true, "me@example.com", vec![]).unwrap();
        assert_eq!(
            addresses_only(&to),
            vec!["alice@example.com", "bob@example.com"]
        );
        assert_eq!(addresses_only(&cc), vec!["carol@example.com"]);
    }

    #[test]
    fn parse_recipient_accepts_bare_and_named_addresses() {
        assert_eq!(
            parse_recipient("alice@example.com").unwrap(),
            (None, "alice@example.com".to_string())
        );
        assert_eq!(
            parse_recipient("  Alice Example <alice@example.com> ").unwrap(),
            (
                Some("Alice Example".to_string()),
                "alice@example.com".to_string()
            )
        );
        assert_eq!(
            parse_recipient("\"Müller, Jörg\" <j@example.de>").unwrap(),
            (Some("Müller, Jörg".to_string()), "j@example.de".to_string())
        );
        // Header injection is stripped before parsing, as everywhere else.
        assert_eq!(
            parse_recipient("a@example.com\r\n").unwrap().1,
            "a@example.com"
        );
    }

    /// A display name with a comma must reach the header quoted, or the
    /// recipient's client reads `Doe, John <j@…>` as two addresses.
    #[test]
    fn display_names_with_specials_are_quoted_in_the_header() {
        for (name, expected) in [
            ("Doe, John", "\"Doe, John\" <j@example.com>"),
            (
                "Dr. A. Smith (Sales)",
                "\"Dr. A. Smith (Sales)\" <j@example.com>",
            ),
        ] {
            let recipients = vec![parse_recipient(&format!("\"{name}\" <j@example.com>")).unwrap()];
            let bytes = MessageBuilder::new()
                .from("me@example.com")
                .to(to_address_list(&recipients))
                .subject("s")
                .text_body("b")
                .write_to_vec()
                .unwrap();
            let raw = String::from_utf8(bytes).unwrap();
            let to_line = raw
                .lines()
                .find(|l| l.starts_with("To: "))
                .expect("To header");
            assert_eq!(to_line, format!("To: {expected}"), "{raw}");
        }
    }

    /// Everything mail-builder would have written as a broken mailbox.
    #[test]
    fn parse_recipient_rejects_what_would_break_the_header() {
        for bad in [
            "",
            "\r\n",
            "Alice Example",
            "a@example.com, b@example.com",
            "a@example.com; b@example.com",
            "@example.com",
            "alice@",
            "a@b@c",
            "Alice <>",
            "<alice@example.com",
        ] {
            let err = parse_recipient(bad).expect_err(bad);
            assert!(err.contains("Invalid recipient"), "{bad:?}: {err}");
        }
    }

    /// A bare name gets the next step, not an address format lesson: a
    /// model that only heard "Ben" asked the user for the address instead of
    /// looking it up in Ben's mail.
    #[test]
    fn parse_recipient_names_the_lookup_for_a_bare_name() {
        for name in ["Ben", "Selin Kaya"] {
            let err = parse_recipient(name).unwrap_err();
            assert!(err.contains("a name alone is not an address"), "{err}");
            assert!(err.contains(&format!("from: \"{name}\"")), "{err}");
        }
        // A broken address keeps the format hint.
        let err = parse_recipient("a@b@c").unwrap_err();
        assert!(err.contains("expected an address like"), "{err}");
    }

    /// An account on a closed port: a check that ran after the first IMAP
    /// command would surface as a connection error instead of its own.
    fn offline_server() -> ImapMcpServer {
        let config: crate::config::ServerConfig = toml::from_str(
            "[[accounts]]\nname = \"T\"\nhost = \"127.0.0.1\"\nport = 1\n\
             username = \"u@h\"\nauth_method = \"password\"\npassword = \"p\"\n",
        )
        .expect("test config");
        let clients = config
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

    /// Malformed input is the caller's to fix and is reported before any
    /// IMAP traffic — before the original is even fetched for a forward.
    #[tokio::test]
    async fn draft_input_errors_come_before_any_imap_traffic() {
        let s = offline_server();
        let email = |to: &[&str], subject: String| DraftEmailRequest {
            account: None,
            to: Some(to.iter().map(|t| (*t).to_string()).collect()),
            subject: Some(subject),
            body: "b".into(),
            cc: None,
            bcc: None,
            attachments: None,
            replaces_uid: None,
        };
        // Each refusal must reach the client as an error result
        // (`isError: true`), not as a success that happens to say "error".
        let err = draft_email(&s, email(&[], "s".into())).await.unwrap_err();
        assert!(err.contains("to is missing"), "{err}");
        let err = draft_email(&s, email(&["Alice Example"], "s".into()))
            .await
            .unwrap_err();
        assert!(err.contains("Invalid recipient"), "{err}");
        let err = draft_email(
            &s,
            email(&["a@example.com"], "x".repeat(MAX_SUBJECT_BYTES + 1)),
        )
        .await
        .unwrap_err();
        assert!(err.contains("subject exceeds"), "{err}");

        let err = draft_forward(
            &s,
            DraftForwardRequest {
                account: None,
                folder: "INBOX".into(),
                uid: 1,
                to: Some(vec![]),
                body: None,
                cc: None,
                attachments: None,
                replaces_uid: None,
            },
        )
        .await
        .unwrap_err();
        assert!(err.contains("to is missing"), "{err}");

        // Both left out of a new email: one error names both, so the next
        // call can carry them at once.
        let missing = DraftEmailRequest {
            to: None,
            subject: None,
            ..email(&[], String::new())
        };
        let err = draft_email(&s, missing).await.unwrap_err();
        assert!(err.contains("to and subject are missing"), "{err}");
        assert!(err.contains("replaces_uid"), "{err}");
    }

    #[test]
    fn required_recipients_needs_at_least_one() {
        assert!(
            required_recipients(&[])
                .unwrap_err()
                .contains("At least one")
        );
        assert_eq!(
            required_recipients(&["a@example.com".to_string()])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn build_reply_recipients_sanitizes_addresses() {
        let original = email("Test", Some("alice\r\nBcc: evil@evil.com"), vec![], vec![]);
        let (to, _) = build_reply_recipients(&original, false, "me@example.com", vec![]).unwrap();
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
            content_id: format!("{cid}.fixed0@unit.invalid"),
            filename: format!("{cid}.png"),
            content_type: "image/png".to_string(),
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
    fn attachment_spec_rejects_unknown_fields_by_name() {
        // A typo like `"inlin": true` previously matched the object variant
        // anyway (serde ignores unknown fields by default), silently turning
        // an intended inline image into a regular attachment. It must fail,
        // and the error must name the field so the caller can fix it.
        let err = serde_json::from_str::<Vec<AttachmentSpec>>(
            r#"[{"path": "/tmp/a.png", "inlin": true}]"#,
        )
        .expect_err("typo field must be rejected")
        .to_string();
        assert!(err.contains("inlin"), "field name missing from: {err}");
        assert!(err.contains("path"), "expected fields missing from: {err}");
    }

    #[test]
    fn attachment_spec_rejects_wrong_types_with_a_usable_message() {
        // The untagged derive reported "data did not match any variant" here
        // — no shapes, no hint. The message must say what is accepted.
        for bad in ["[42]", "[true]", "[null]", "[[]]"] {
            let err = serde_json::from_str::<Vec<AttachmentSpec>>(bad)
                .expect_err("non-string, non-object must be rejected")
                .to_string();
            assert!(err.contains("path string"), "{bad} produced: {err}");
            assert!(err.contains("`path`"), "{bad} produced: {err}");
        }
        // An object without `path` gets serde's precise complaint.
        let err = serde_json::from_str::<Vec<AttachmentSpec>>(r#"[{"inline": true}]"#)
            .expect_err("object without path must be rejected")
            .to_string();
        assert!(err.contains("path"), "{err}");
    }

    #[test]
    fn draft_request_schemas_are_ref_free() {
        // rmcp serves these schemas to MCP clients, and `attachments` is the
        // first non-scalar parameter shape in the server. A `$ref` into a
        // definitions map is the kind of construct strict client-side
        // validators choke on (the 2026-07-28 protocol incident was exactly
        // a client discarding a whole tools/list over one field) — keep the
        // schemas self-contained. Checked on the REQUEST types, not on
        // `AttachmentSpec` alone: as a schema root the enum needs no `$ref`
        // to itself, so that narrower check stayed green while the served
        // schema still pointed into `$defs`.
        for (name, schema) in [
            (
                "draft_email",
                serde_json::to_value(schemars::schema_for!(DraftEmailRequest)).unwrap(),
            ),
            (
                "draft_reply",
                serde_json::to_value(schemars::schema_for!(DraftReplyRequest)).unwrap(),
            ),
            (
                "draft_forward",
                serde_json::to_value(schemars::schema_for!(DraftForwardRequest)).unwrap(),
            ),
        ] {
            let text = schema.to_string();
            assert!(
                !text.contains("$ref"),
                "{name} schema must be inline: {text}"
            );
            // Doc comments become schema descriptions; developer notes must
            // not reach the model (they once did, 1.3 KB per draft tool).
            for internal in ["serde", "schemars", "untagged", "\\n\\n"] {
                assert!(
                    !text.contains(internal),
                    "{name} leaks {internal:?}: {text}"
                );
            }
            // Both accepted attachment spellings are visible to the client.
            assert!(text.contains("anyOf"), "{name}: {text}");
            assert!(text.contains("path"), "{name}: {text}");
        }
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
    }

    /// Every derived id must satisfy the exact rule explicit ids and body
    /// markers are held to — a derived id the scanner rejects is a dead end:
    /// the attachment is accepted, but no marker can ever reference it, and
    /// the resulting error blames the marker.
    #[test]
    fn derive_cid_always_yields_a_referenceable_id() {
        // Length: a long cid-safe stem must be capped, not passed through —
        // the scanner rejects ids over MAX_MARKER_CID_BYTES by construction.
        let long = format!("{}.png", "a".repeat(MAX_MARKER_CID_BYTES + 50));
        let derived = derive_cid(&long);
        assert!(is_valid_cid(&derived), "over-long stem: {derived:?}");
        assert_eq!(derived.len(), MAX_MARKER_CID_BYTES);

        // Dot-atom rules: `shot..png` has the stem `shot.`, and dots can
        // double inside a name — the Content-ID local part (`<id.uuid@…>`)
        // permits none of that.
        assert_eq!(derive_cid("shot..png"), "shot");
        assert_eq!(derive_cid("v1..2-shot.png"), "v1.2-shot");
        assert_eq!(derive_cid(".hidden.png"), "hidden");

        for name in [
            "a b.png",
            "ä.png",
            "x!y.png",
            "no-extension",
            "shot..png",
            "...png",
            &long,
        ] {
            let d = derive_cid(name);
            assert!(is_valid_cid(&d), "derived id for {name:?} invalid: {d:?}");
        }
    }

    #[test]
    fn is_valid_cid_enforces_alphabet_length_and_dot_rules() {
        assert!(is_valid_cid("shot"));
        assert!(is_valid_cid("v1.2-shot_3"));
        assert!(is_valid_cid(&"a".repeat(MAX_MARKER_CID_BYTES)));
        for bad in ["", "a b", "a/b", ".a", "a.", "a..b"] {
            assert!(!is_valid_cid(bad), "{bad:?} must be invalid");
        }
        assert!(!is_valid_cid(&"a".repeat(MAX_MARKER_CID_BYTES + 1)));
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
    fn malformed_marker_attempts_are_rejected_not_saved_as_text() {
        // The scanner leaves a rejected candidate as ordinary text; without
        // this check the draft would be saved showing raw marker source. The
        // README-shaped trap — a screenshot file name with spaces used as the
        // id — must produce an error naming the offending line.
        let err = check_cid_markers("see ![x](cid:Bildschirmfoto 2026-08-14)", &[img("shot")])
            .expect_err("must be rejected");
        assert!(err.contains("Bildschirmfoto"), "{err}");
        assert!(err.contains("![alt](cid:"), "syntax help missing: {err}");

        // A valid marker alongside also establishes inline context — the
        // stray fragment is an error even without attachments passed.
        assert!(check_cid_markers("![x](cid:shot) and ![y](cid:bad id)", &[img("shot")]).is_err());

        // WITHOUT any inline context (no attachments, no valid markers) the
        // fragment is most likely prose about the syntax: the draft saves,
        // but with a warning naming what was seen — never silently.
        let notice = check_cid_markers("x ![a](cid:bad id) y", &[])
            .expect("prose about the syntax must not be unsendable")
            .expect("…but it must warn");
        assert!(notice.contains("bad id"), "{notice}");
        assert!(notice.contains("literal text"), "{notice}");

        // A valid marker next to prose stays fine.
        assert!(
            check_cid_markers("![x](cid:shot) plain prose", &[img("shot")])
                .unwrap()
                .is_none()
        );
    }

    /// Unfold MIME header continuation lines so `contains` assertions cannot
    /// break on where mail-builder happens to wrap a long Content-Type.
    fn unfold(mime: &str) -> String {
        mime.replace("\r\n\t", " ").replace("\r\n ", " ")
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
            "<html><body><img src=\"cid:shot.fixed0@unit.invalid\"></body></html>",
            prepared,
        );
        let mime = unfold(&String::from_utf8(builder.write_to_vec().expect("builds")).unwrap());

        assert!(mime.contains("multipart/related"), "{mime}");
        // RFC 2387: the `type` parameter naming the root part is mandatory —
        // without it strict clients may render the alternative as a detached
        // attachment.
        assert!(mime.contains("type=\"multipart/alternative\""), "{mime}");
        assert!(mime.contains("multipart/alternative"), "{mime}");
        // The Content-ID must be angle-bracketed and carry the msg-id-shaped
        // wire id — a bare `<shot>` repeats across drafts and breaks clients
        // that cache inline parts by Content-ID.
        assert!(
            mime.contains("Content-ID: <shot.fixed0@unit.invalid>"),
            "{mime}"
        );
        assert!(!mime.contains("Content-ID: <shot>"), "{mime}");
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
            regular: vec![(
                "application/pdf".to_string(),
                "report.pdf".to_string(),
                vec![9, 9],
            )],
            inline: vec![img("shot")],
        };
        let builder = apply_bodies_and_attachments(
            MessageBuilder::new().subject("t"),
            "plain",
            "<html><body>x</body></html>",
            prepared,
        );
        let mime = unfold(&String::from_utf8(builder.write_to_vec().expect("builds")).unwrap());

        assert!(mime.contains("multipart/mixed"), "{mime}");
        assert!(mime.contains("multipart/related"), "{mime}");
        assert!(mime.contains("type=\"multipart/alternative\""), "{mime}");
        assert!(
            mime.contains("Content-ID: <shot.fixed0@unit.invalid>"),
            "{mime}"
        );
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
            regular: vec![(
                "application/pdf".to_string(),
                "report.pdf".to_string(),
                vec![9],
            )],
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
    /// test so parallel runs cannot collide; removed on drop so a failing
    /// assertion (an early panic) cannot leave litter in the temp dir.
    struct ScratchDir(std::path::PathBuf);

    impl ScratchDir {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("imap-mcp-rs-test-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Self(dir)
        }
        fn join(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
        fn allowed(&self) -> Vec<String> {
            vec![self.0.to_string_lossy().into_owned()]
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn inline_spec(path: &std::path::Path, cid: Option<&str>) -> AttachmentSpec {
        AttachmentSpec::Detailed(DetailedAttachment {
            path: path.to_string_lossy().into_owned(),
            inline: Some(true),
            cid: cid.map(str::to_string),
        })
    }

    const TEST_DOMAIN: &str = "unit.invalid";

    #[tokio::test]
    async fn attachment_count_is_capped_before_any_file_io() {
        // The byte caps alone left "thousands of one-byte files" open: each
        // entry costs syscalls, a MIME part and a lookup-table slot. The cap
        // fires before path validation, so nonexistent paths prove no I/O
        // was attempted first.
        let specs: Vec<AttachmentSpec> = (0..101)
            .map(|i| AttachmentSpec::Path(format!("/nonexistent/{i}.png")))
            .collect();
        let err = read_attachments(Some(&specs), &["/tmp".to_string()], TEST_DOMAIN)
            .await
            .expect_err("over-cap list must be refused");
        assert!(err.contains("cap"), "{err}");
        assert!(
            !err.contains("Cannot resolve"),
            "cap must fire before path I/O: {err}"
        );
    }

    #[tokio::test]
    async fn inline_rejects_types_that_cannot_render_as_an_image() {
        let dir = ScratchDir::new("inline-type");
        let pdf = dir.join("report.pdf");
        std::fs::write(&pdf, b"%PDF-1.4").expect("write");
        let svg = dir.join("logo.svg");
        std::fs::write(&svg, b"<svg/>").expect("write");

        for (path, expected) in [(&pdf, "application/pdf"), (&svg, "image/svg+xml")] {
            let specs = vec![inline_spec(path, None)];
            let err = read_attachments(Some(&specs), &dir.allowed(), TEST_DOMAIN)
                .await
                .expect_err("must be rejected");
            assert!(err.contains(expected), "{err}");
        }
    }

    #[tokio::test]
    async fn inline_png_is_split_out_with_a_derived_cid_and_unique_content_id() {
        let dir = ScratchDir::new("inline-png");
        let png = dir.join("Rollen und Rechte.png");
        std::fs::write(&png, b"\x89PNG").expect("write");
        let pdf = dir.join("report.pdf");
        std::fs::write(&pdf, b"%PDF").expect("write");

        let specs = vec![
            AttachmentSpec::Path(pdf.to_string_lossy().into_owned()),
            inline_spec(&png, None),
        ];
        let prepared = read_attachments(Some(&specs), &dir.allowed(), TEST_DOMAIN)
            .await
            .expect("accepted");

        assert_eq!(prepared.regular.len(), 1, "the pdf stays a regular file");
        assert_eq!(prepared.regular[0].1, "report.pdf");
        assert_eq!(prepared.inline.len(), 1);
        assert_eq!(prepared.inline[0].cid, "Rollen-und-Rechte");
        assert_eq!(prepared.inline[0].content_type, "image/png");
        // The wire id is msg-id shaped: marker id, dot, unique token, @domain.
        let content_id = &prepared.inline[0].content_id;
        assert!(content_id.starts_with("Rollen-und-Rechte."), "{content_id}");
        assert!(content_id.ends_with("@unit.invalid"), "{content_id}");

        // A second read of the same file must mint a DIFFERENT wire id —
        // that uniqueness is the whole point (client-side Content-ID caches).
        let specs2 = vec![inline_spec(&dir.join("Rollen und Rechte.png"), None)];
        let prepared2 = read_attachments(Some(&specs2), &dir.allowed(), TEST_DOMAIN)
            .await
            .expect("accepted");
        assert_ne!(prepared2.inline[0].content_id, *content_id);
        assert_eq!(prepared2.inline[0].cid, "Rollen-und-Rechte");
    }

    #[tokio::test]
    async fn duplicate_cids_are_rejected() {
        // Two files with the same stem in different folders derive the same
        // id; a marker could then no longer say which one it means.
        let dir = ScratchDir::new("dup-cid");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).expect("subdir");
        std::fs::write(dir.join("shot.png"), b"a").expect("write");
        std::fs::write(sub.join("shot.png"), b"b").expect("write");

        let specs = vec![
            inline_spec(&dir.join("shot.png"), None),
            inline_spec(&sub.join("shot.png"), None),
        ];
        let err = read_attachments(Some(&specs), &dir.allowed(), TEST_DOMAIN)
            .await
            .expect_err("duplicate must be rejected");
        assert!(err.contains("Duplicate cid"), "{err}");
        assert!(err.contains("shot"), "{err}");
    }

    #[tokio::test]
    async fn explicit_cid_with_unsafe_characters_is_rejected() {
        let dir = ScratchDir::new("bad-cid");
        let png = dir.join("shot.png");
        std::fs::write(&png, b"x").expect("write");

        let specs = vec![inline_spec(&png, Some("a b\"c"))];
        let err = read_attachments(Some(&specs), &dir.allowed(), TEST_DOMAIN)
            .await
            .expect_err("must be rejected");
        assert!(err.contains("Invalid cid"), "{err}");

        // Over-long ids are rejected by the same rule the body scanner uses —
        // otherwise an attachment could carry an id no marker can reference.
        let long = "a".repeat(MAX_MARKER_CID_BYTES + 1);
        let specs = vec![inline_spec(&png, Some(&long))];
        let err = read_attachments(Some(&specs), &dir.allowed(), TEST_DOMAIN)
            .await
            .expect_err("over-long id must be rejected");
        assert!(err.contains("Invalid cid"), "{err}");
    }

    #[tokio::test]
    async fn inline_type_check_fires_before_the_file_is_read() {
        // The type is decidable from the extension alone; rejecting before
        // the read means even an unreadable file gets the *type* error, which
        // proves no read was attempted first.
        let dir = ScratchDir::new("type-before-read");
        let pdf = dir.join("report.pdf");
        std::fs::write(&pdf, b"%PDF").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&pdf, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        }

        let specs = vec![inline_spec(&pdf, None)];
        let err = read_attachments(Some(&specs), &dir.allowed(), TEST_DOMAIN)
            .await
            .expect_err("must be rejected");
        assert!(
            err.contains("application/pdf"),
            "expected the type error, not a read error: {err}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&pdf, std::fs::Permissions::from_mode(0o600));
        }
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
                content_id: "rollen.fixed0@unit.invalid".to_string(),
                filename: "Rollenübersicht.png".to_string(),
                content_type: "image/png".to_string(),
                bytes: png,
            }],
        };
        let builder = apply_bodies_and_attachments(
            MessageBuilder::new().subject("t"),
            "plain",
            "<html><body><img src=\"cid:rollen.fixed0@unit.invalid\"></body></html>",
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
        // Header stays 7-bit clean: the umlaut is encoded, not passed through
        // — as an RFC 2231 extended parameter, the form RFC 2183 prescribes
        // for parameter values (an RFC 2047 encoded-word inside a quoted
        // parameter is forbidden by RFC 2047 §5).
        assert!(!mime.contains("Rollenübersicht.png"), "{mime}");
        assert!(
            mime.contains("filename*=UTF-8''Rollen%C3%BCbersicht.png"),
            "{mime}"
        );
    }
}
