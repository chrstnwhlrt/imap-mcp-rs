use mail_builder::MessageBuilder;
use rmcp::schemars;
use serde::Deserialize;

use super::{ImapMcpServer, error_json};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftReplyRequest {
    #[schemars(description = "Account name (from list_accounts). Uses first account if omitted.")]
    pub account: Option<String>,
    #[schemars(description = "Folder containing the email to reply to")]
    pub folder: String,
    #[schemars(description = "UID of the email to reply to")]
    pub uid: u32,
    #[schemars(description = "Your reply text")]
    pub body: String,
    #[schemars(description = "Reply to all recipients (default: false)")]
    pub reply_all: Option<bool>,
    #[schemars(description = "Additional CC addresses")]
    pub cc: Option<Vec<String>>,
    #[schemars(description = "File paths to attach (e.g. from download_attachment)")]
    pub attachments: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftForwardRequest {
    #[schemars(description = "Account name (from list_accounts). Uses first account if omitted.")]
    pub account: Option<String>,
    #[schemars(description = "Folder containing the email to forward")]
    pub folder: String,
    #[schemars(description = "UID of the email to forward")]
    pub uid: u32,
    #[schemars(description = "Recipient addresses")]
    pub to: Vec<String>,
    #[schemars(description = "Optional message above forwarded content")]
    pub body: Option<String>,
    #[schemars(description = "Optional CC addresses")]
    pub cc: Option<Vec<String>>,
    #[schemars(description = "File paths to attach (e.g. from download_attachment)")]
    pub attachments: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftEmailRequest {
    #[schemars(description = "Account name (from list_accounts). Uses first account if omitted.")]
    pub account: Option<String>,
    #[schemars(description = "Recipient addresses")]
    pub to: Vec<String>,
    #[schemars(description = "Email subject line")]
    pub subject: String,
    #[schemars(description = "Email body text")]
    pub body: String,
    #[schemars(description = "CC addresses")]
    pub cc: Option<Vec<String>>,
    #[schemars(description = "BCC addresses")]
    pub bcc: Option<Vec<String>>,
    #[schemars(description = "File paths to attach (e.g. from download_attachment)")]
    pub attachments: Option<Vec<String>>,
}

#[allow(clippy::too_many_lines)]
pub async fn draft_reply(server: &ImapMcpServer, req: DraftReplyRequest) -> String {
    let (account_config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    if account_config.read_only {
        return error_json("Account is configured as read-only");
    }
    let from = account_config.sender_address().to_string();

    let mut client = client_arc.lock().await;

    let original = match client.get_email(&req.folder, req.uid).await {
        Ok(Some(email)) => email,
        Ok(None) => return error_json(&format!("Email {} not found in {}", req.uid, req.folder)),
        Err(e) => return error_json(&client.check_error(e).to_string()),
    };

    let reply_all = req.reply_all.unwrap_or(false);

    let to_addr = match original.from.as_ref() {
        Some(a) if !a.address.is_empty() => a.address.clone(),
        _ => return error_json("Cannot reply: original email has no sender address"),
    };

    let mut to_list = vec![to_addr.clone()];
    if reply_all {
        for addr in &original.to {
            if addr.address != from {
                to_list.push(addr.address.clone());
            }
        }
    }

    let mut cc_list: Vec<String> = Vec::new();
    if reply_all {
        for addr in &original.cc {
            if addr.address != from {
                cc_list.push(addr.address.clone());
            }
        }
    }
    if let Some(extra_cc) = &req.cc {
        cc_list.extend(extra_cc.iter().cloned());
    }

    let subject = if original.subject.to_ascii_lowercase().starts_with("re:") {
        original.subject.clone()
    } else {
        format!("Re: {}", original.subject)
    };

    let quoted_original = original
        .body_text
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n");

    let from_display = format_sender(original.from.as_ref());

    let date_display = original.date.as_deref().unwrap_or("unknown date");
    let full_body = format!(
        "{}\n\nOn {date_display}, {from_display} wrote:\n{quoted_original}",
        req.body
    );

    let mut builder = MessageBuilder::new()
        .from(from.as_str())
        .subject(&subject)
        .text_body(&full_body);

    for addr in &to_list {
        builder = builder.to(addr.as_str());
    }
    for addr in &cc_list {
        builder = builder.cc(addr.as_str());
    }

    let has_threading = if let Some(msg_id) = &original.message_id {
        builder = builder.in_reply_to(msg_id.as_str());
        let mut refs = original.references.clone();
        refs.push(msg_id.clone());
        builder = builder.header(
            "References",
            mail_builder::headers::raw::Raw::new(refs.join(" ")),
        );
        true
    } else {
        false
    };

    let attachment_data = match read_attachments(req.attachments.as_ref()) {
        Ok(a) => a,
        Err(e) => return error_json(&e),
    };
    for (content_type, filename, bytes) in &attachment_data {
        builder = builder.attachment(*content_type, filename.as_str(), bytes.clone());
    }

    let message_bytes = match builder.write_to_vec() {
        Ok(bytes) => bytes,
        Err(e) => return error_json(&format!("Failed to build MIME message: {e}")),
    };

    match client.save_draft(&message_bytes).await {
        Ok(()) => {
            let mut response = serde_json::json!({
                "status": "ok",
                "from": from,
                "to": to_list,
                "cc": cc_list,
                "subject": subject,
                "body_preview": truncate(&full_body, 500),
            });
            if !has_threading {
                response["warning"] = serde_json::json!(
                    "Original email has no Message-ID. Reply was created without threading headers (In-Reply-To/References) — it may not appear in the same thread in the recipient's mail client."
                );
            }
            serde_json::to_string(&response)
        }
        .unwrap_or_else(|e| error_json(&e.to_string())),
        Err(e) => {
            let e = client.check_error(e);
            error_json(&format!("Failed to save draft: {e}"))
        }
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
    let from = account_config.sender_address().to_string();

    let mut client = client_arc.lock().await;

    let original = match client.get_email(&req.folder, req.uid).await {
        Ok(Some(email)) => email,
        Ok(None) => return error_json(&format!("Email {} not found in {}", req.uid, req.folder)),
        Err(e) => return error_json(&client.check_error(e).to_string()),
    };

    let subject = format!("Fwd: {}", original.subject);

    let from_display = format_sender(original.from.as_ref());

    let date_display = original.date.as_deref().unwrap_or("unknown date");
    let to_display = original
        .to
        .iter()
        .map(|a| a.address.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    let forwarded_content = format!(
        "---------- Forwarded message ----------\n\
         From: {from_display}\n\
         Date: {date_display}\n\
         Subject: {}\n\
         To: {to_display}\n\n\
         {}",
        original.subject, original.body_text,
    );

    let full_body = if let Some(msg) = &req.body {
        format!("{msg}\n\n{forwarded_content}")
    } else {
        forwarded_content
    };

    let mut builder = MessageBuilder::new()
        .from(from.as_str())
        .subject(&subject)
        .text_body(&full_body);

    for addr in &req.to {
        builder = builder.to(addr.as_str());
    }
    if let Some(cc) = &req.cc {
        for addr in cc {
            builder = builder.cc(addr.as_str());
        }
    }

    let attachment_data = match read_attachments(req.attachments.as_ref()) {
        Ok(a) => a,
        Err(e) => return error_json(&e),
    };
    for (content_type, filename, bytes) in &attachment_data {
        builder = builder.attachment(*content_type, filename.as_str(), bytes.clone());
    }

    let message_bytes = match builder.write_to_vec() {
        Ok(bytes) => bytes,
        Err(e) => return error_json(&format!("Failed to build MIME message: {e}")),
    };

    match client.save_draft(&message_bytes).await {
        Ok(()) => serde_json::to_string(&serde_json::json!({
            "status": "ok",
            "from": from,
            "to": req.to,
            "cc": req.cc.as_deref().unwrap_or_default(),
            "subject": subject,
            "body_preview": truncate(&full_body, 500),
        }))
        .unwrap_or_else(|e| error_json(&e.to_string())),
        Err(e) => {
            let e = client.check_error(e);
            error_json(&format!("Failed to save draft: {e}"))
        }
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
    let from = account_config.sender_address().to_string();

    let mut builder = MessageBuilder::new()
        .from(from.as_str())
        .subject(&req.subject)
        .text_body(&req.body);

    for addr in &req.to {
        builder = builder.to(addr.as_str());
    }
    if let Some(cc) = &req.cc {
        for addr in cc {
            builder = builder.cc(addr.as_str());
        }
    }
    if let Some(bcc) = &req.bcc {
        for addr in bcc {
            builder = builder.bcc(addr.as_str());
        }
    }

    let attachment_data = match read_attachments(req.attachments.as_ref()) {
        Ok(a) => a,
        Err(e) => return error_json(&e),
    };
    for (content_type, filename, bytes) in &attachment_data {
        builder = builder.attachment(*content_type, filename.as_str(), bytes.clone());
    }

    let message_bytes = match builder.write_to_vec() {
        Ok(bytes) => bytes,
        Err(e) => return error_json(&format!("Failed to build MIME message: {e}")),
    };

    let mut client = client_arc.lock().await;
    match client.save_draft(&message_bytes).await {
        Ok(()) => serde_json::to_string(&serde_json::json!({
            "status": "ok",
            "from": from,
            "to": req.to,
            "cc": req.cc.as_deref().unwrap_or_default(),
            "bcc": req.bcc.as_deref().unwrap_or_default(),
            "subject": req.subject,
            "body_preview": truncate(&req.body, 500),
        }))
        .unwrap_or_else(|e| error_json(&e.to_string())),
        Err(e) => {
            let e = client.check_error(e);
            error_json(&format!("Failed to save draft: {e}"))
        }
    }
}

fn format_sender(from: Option<&crate::email::EmailAddress>) -> String {
    from.map_or_else(
        || "unknown".to_string(),
        |a| match &a.name {
            Some(name) => format!("{name} <{}>", a.address),
            None => a.address.clone(),
        },
    )
}

/// Read attachment files from disk. Returns (content_type, filename, bytes) tuples.
type AttachmentData = Vec<(&'static str, String, Vec<u8>)>;

fn read_attachments(attachments: Option<&Vec<String>>) -> Result<AttachmentData, String> {
    let Some(paths) = attachments else {
        return Ok(vec![]);
    };
    let mut result = Vec::new();
    for path_str in paths {
        let path = std::path::Path::new(path_str);
        let bytes = std::fs::read(path)
            .map_err(|e| format!("Failed to read attachment \"{path_str}\": {e}"))?;
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment")
            .to_string();
        let content_type =
            mime_type_from_extension(path.extension().and_then(|e| e.to_str()).unwrap_or(""));
        result.push((content_type, filename, bytes));
    }
    Ok(result)
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

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..s.floor_char_boundary(max_len)])
    }
}
