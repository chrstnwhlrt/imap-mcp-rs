use rmcp::schemars;
use serde::Deserialize;

use crate::imap_client::{escape_imap_string, iso_to_imap_date};

use super::{ImapMcpServer, error_json};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListFoldersRequest {
    #[schemars(description = "Account name (from list_accounts). Uses first account if omitted.")]
    pub account: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListEmailsRequest {
    #[schemars(description = "Account name (from list_accounts). Uses first account if omitted.")]
    pub account: Option<String>,
    #[schemars(description = "Folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(description = "Maximum number of results (default: 20)")]
    pub limit: Option<u32>,
    #[schemars(description = "Number of results to skip for pagination (default: 0)")]
    pub offset: Option<u32>,
    #[schemars(description = "Only show unread emails (default: false)")]
    pub unread_only: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetEmailRequest {
    #[schemars(description = "Account name (from list_accounts). Uses first account if omitted.")]
    pub account: Option<String>,
    #[schemars(description = "Folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(description = "Email UID (from list_emails or search_emails results)")]
    pub uid: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetThreadRequest {
    #[schemars(description = "Account name (from list_accounts). Uses first account if omitted.")]
    pub account: Option<String>,
    #[schemars(description = "Folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(description = "Email UID of any message in the thread")]
    pub uid: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchEmailsRequest {
    #[schemars(description = "Account name (from list_accounts). Uses first account if omitted.")]
    pub account: Option<String>,
    #[schemars(description = "Folder to search (omit to search all folders)")]
    pub folder: Option<String>,
    #[schemars(description = "Full-text search in body and headers")]
    pub text: Option<String>,
    #[schemars(description = "Filter by sender address or name")]
    pub from: Option<String>,
    #[schemars(description = "Filter by recipient address")]
    pub to: Option<String>,
    #[schemars(description = "Filter by subject line")]
    pub subject: Option<String>,
    #[schemars(description = "Emails on or after this date (YYYY-MM-DD)")]
    pub since: Option<String>,
    #[schemars(description = "Emails before this date (YYYY-MM-DD)")]
    pub before: Option<String>,
    #[schemars(description = "true for read, false for unread")]
    pub is_read: Option<bool>,
    #[schemars(description = "true for flagged/starred")]
    pub is_flagged: Option<bool>,
    #[schemars(description = "true for replied-to, false for unreplied")]
    pub is_answered: Option<bool>,
    #[schemars(description = "Maximum results (default: 20)")]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DownloadAttachmentRequest {
    #[schemars(description = "Account name (from list_accounts). Uses first account if omitted.")]
    pub account: Option<String>,
    #[schemars(description = "Folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(description = "Email UID (from list_emails or get_email results)")]
    pub uid: u32,
    #[schemars(description = "Attachment filename (from get_email attachments list)")]
    pub filename: String,
}

pub async fn list_folders(server: &ImapMcpServer, req: ListFoldersRequest) -> String {
    let (_, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    let mut client = client_arc.lock().await;
    match client.list_folders().await {
        Ok(folders) => {
            serde_json::to_string(&folders).unwrap_or_else(|e| error_json(&e.to_string()))
        }
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

pub async fn list_emails(server: &ImapMcpServer, req: ListEmailsRequest) -> String {
    let (_, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    let mut client = client_arc.lock().await;
    let limit = req.limit.unwrap_or(20);
    let offset = req.offset.unwrap_or(0);
    let unread_only = req.unread_only.unwrap_or(false);

    match client
        .list_emails(&req.folder, limit, offset, unread_only)
        .await
    {
        Ok((emails, total, matched)) => serde_json::to_string(&serde_json::json!({
            "folder": req.folder,
            "total": total,
            "matched": matched,
            "offset": offset,
            "limit": limit,
            "emails": emails,
        }))
        .unwrap_or_else(|e| error_json(&e.to_string())),
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

pub async fn get_email(server: &ImapMcpServer, req: GetEmailRequest) -> String {
    let (_, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    let mut client = client_arc.lock().await;
    match client.get_email(&req.folder, req.uid).await {
        Ok(Some(email)) => {
            serde_json::to_string(&email).unwrap_or_else(|e| error_json(&e.to_string()))
        }
        Ok(None) => error_json(&format!(
            "Email with UID {} not found in {}",
            req.uid, req.folder
        )),
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

pub async fn get_thread(server: &ImapMcpServer, req: GetThreadRequest) -> String {
    let (_, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    let mut client = client_arc.lock().await;
    match client.get_thread(&req.folder, req.uid).await {
        Ok(emails) => {
            let subject = emails
                .first()
                .map(|e| e.subject.clone())
                .unwrap_or_default();
            serde_json::to_string(&serde_json::json!({
                "subject": subject,
                "message_count": emails.len(),
                "messages": emails,
            }))
            .unwrap_or_else(|e| error_json(&e.to_string()))
        }
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

pub async fn search_emails(server: &ImapMcpServer, req: SearchEmailsRequest) -> String {
    let mut criteria_parts: Vec<String> = Vec::new();

    if let Some(text) = &req.text {
        criteria_parts.push(format!("TEXT \"{}\"", escape_imap_string(text)));
    }
    if let Some(from) = &req.from {
        criteria_parts.push(format!("FROM \"{}\"", escape_imap_string(from)));
    }
    if let Some(to) = &req.to {
        criteria_parts.push(format!("TO \"{}\"", escape_imap_string(to)));
    }
    if let Some(subject) = &req.subject {
        criteria_parts.push(format!("SUBJECT \"{}\"", escape_imap_string(subject)));
    }
    if let Some(since) = &req.since {
        match iso_to_imap_date(since) {
            Ok(d) => criteria_parts.push(format!("SINCE {d}")),
            Err(e) => return error_json(&format!("Invalid 'since' date: {e}")),
        }
    }
    if let Some(before) = &req.before {
        match iso_to_imap_date(before) {
            Ok(d) => criteria_parts.push(format!("BEFORE {d}")),
            Err(e) => return error_json(&format!("Invalid 'before' date: {e}")),
        }
    }
    if let Some(is_read) = req.is_read {
        criteria_parts.push(if is_read {
            "SEEN".to_string()
        } else {
            "UNSEEN".to_string()
        });
    }
    if let Some(is_flagged) = req.is_flagged {
        criteria_parts.push(if is_flagged {
            "FLAGGED".to_string()
        } else {
            "UNFLAGGED".to_string()
        });
    }
    if let Some(is_answered) = req.is_answered {
        criteria_parts.push(if is_answered {
            "ANSWERED".to_string()
        } else {
            "UNANSWERED".to_string()
        });
    }

    if criteria_parts.is_empty() {
        return error_json("At least one search criterion is required");
    }

    let criteria = criteria_parts.join(" ");
    let limit = req.limit.unwrap_or(20);

    let (_, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    let mut client = client_arc.lock().await;

    let folders = if let Some(folder) = &req.folder {
        vec![folder.clone()]
    } else {
        match client.get_folder_names().await {
            Ok(names) => names,
            Err(e) => return error_json(&client.check_error(e).to_string()),
        }
    };

    let mut all_results = Vec::new();
    for folder in &folders {
        match client.search_emails(folder, &criteria, limit).await {
            Ok(results) => all_results.extend(results),
            Err(e) => {
                let _ = client.check_error(e);
                tracing::warn!(folder = %folder, "Search failed for folder");
            }
        }
    }

    all_results.sort_by(|a, b| b.date.cmp(&a.date));
    all_results.truncate(limit as usize);

    serde_json::to_string(&serde_json::json!({
        "total_results": all_results.len(),
        "emails": all_results,
    }))
    .unwrap_or_else(|e| error_json(&e.to_string()))
}

pub async fn download_attachment(server: &ImapMcpServer, req: DownloadAttachmentRequest) -> String {
    use mail_parser::MimeHeaders;
    use std::path::Path;
    use uuid::Uuid;

    let (_, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    let mut client = client_arc.lock().await;

    // Fetch raw email bytes
    let raw = match client.fetch_raw(&req.folder, req.uid).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return error_json(&format!(
                "Email with UID {} not found in {}",
                req.uid, req.folder
            ));
        }
        Err(e) => return error_json(&client.check_error(e).to_string()),
    };

    // Parse and find the attachment
    let Some(message) = mail_parser::MessageParser::default().parse(&raw) else {
        return error_json("Failed to parse email");
    };

    let attachment = message
        .attachments()
        .find(|att| att.attachment_name().unwrap_or("") == req.filename);

    let Some(attachment) = attachment else {
        return error_json(&format!(
            "Attachment \"{}\" not found in email UID {}",
            req.filename, req.uid
        ));
    };

    let content_type = attachment.content_type().map_or_else(
        || "application/octet-stream".to_string(),
        |ct| {
            if let Some(sub) = ct.subtype() {
                format!("{}/{}", ct.ctype(), sub)
            } else {
                ct.ctype().to_string()
            }
        },
    );

    let contents = attachment.contents();
    let size = contents.len();

    // Determine file extension from original filename
    let extension = Path::new(&req.filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");

    // Save to /tmp/imap-mcp-rs/ with UUID filename
    let dir = Path::new("/tmp/imap-mcp-rs");
    if let Err(e) = std::fs::create_dir_all(dir) {
        return error_json(&format!("Failed to create directory: {e}"));
    }

    let uuid = Uuid::new_v4();
    let save_path = dir.join(format!("{uuid}.{extension}"));

    if let Err(e) = std::fs::write(&save_path, contents) {
        return error_json(&format!("Failed to write file: {e}"));
    }

    serde_json::to_string(&serde_json::json!({
        "saved_to": save_path.to_string_lossy(),
        "filename": req.filename,
        "size": size,
        "content_type": content_type,
    }))
    .unwrap_or_else(|e| error_json(&e.to_string()))
}
