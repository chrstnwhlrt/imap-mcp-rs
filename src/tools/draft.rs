use mail_builder::MessageBuilder;
use rmcp::schemars;
use serde::Deserialize;

use super::{ImapMcpServer, error_json};

// ========== Locale presets ==========

const FONT_DE: &str = "&quot;Tahoma&quot;, &quot;Geneva&quot;, sans-serif";
const FONT_EN: &str =
    "Aptos, Aptos_MSFontService, -apple-system, Roboto, Arial, Helvetica, sans-serif";
const COLOR_DE: &str = "rgb(0, 0, 0)";
const COLOR_EN: &str = "rgb(33, 33, 33)";

#[derive(Debug, Clone, Copy)]
enum Locale {
    En,
    De,
}

impl Locale {
    fn from_config(s: Option<&str>) -> Self {
        match s.map(str::to_ascii_lowercase).as_deref() {
            Some("de" | "de-de" | "de_de" | "german") => Self::De,
            _ => Self::En,
        }
    }

    fn font(self) -> &'static str {
        match self {
            Self::De => FONT_DE,
            Self::En => FONT_EN,
        }
    }

    fn color(self) -> &'static str {
        match self {
            Self::De => COLOR_DE,
            Self::En => COLOR_EN,
        }
    }

    fn quote_labels(self) -> [&'static str; 4] {
        match self {
            Self::De => ["Von", "Gesendet", "An", "Betreff"],
            Self::En => ["From", "Sent", "To", "Subject"],
        }
    }

    fn reply_prefix(self) -> &'static str {
        match self {
            Self::De => "AW: ",
            Self::En => "Re: ",
        }
    }

    fn forward_prefix(self) -> &'static str {
        match self {
            Self::De => "WG: ",
            Self::En => "Fwd: ",
        }
    }

    fn unknown_date(self) -> &'static str {
        match self {
            Self::De => "unbekanntes Datum",
            Self::En => "unknown date",
        }
    }

    /// Locale-aware intro line for plaintext reply quotes.
    /// EN: "On {date}, {from} wrote:"
    /// DE: "Am {date} schrieb {from}:"
    fn plain_reply_intro(self, date: &str, from: &str) -> String {
        match self {
            Self::De => format!("Am {date} schrieb {from}:"),
            Self::En => format!("On {date}, {from} wrote:"),
        }
    }

    /// Locale-aware "Forwarded message" label for plaintext forwards.
    fn forwarded_message_label(self) -> &'static str {
        match self {
            Self::De => "Weitergeleitete Nachricht",
            Self::En => "Forwarded message",
        }
    }
}

// ========== Request types ==========

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

// ========== Tool implementations ==========

pub async fn draft_reply(server: &ImapMcpServer, req: DraftReplyRequest) -> String {
    let (account_config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    if account_config.read_only {
        return error_json("Account is configured as read-only");
    }
    let from = account_config.sender_address().to_string();
    let display_name = account_config.display_name.clone();
    let signature_html = account_config.signature_html.as_deref().unwrap_or("");
    let locale = Locale::from_config(account_config.locale.as_deref());

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

    let from_lower = from.to_lowercase();
    let mut to_list = vec![to_addr.clone()];
    if reply_all {
        for addr in &original.to {
            if addr.address.to_lowercase() != from_lower {
                to_list.push(addr.address.clone());
            }
        }
    }

    let mut cc_list: Vec<String> = Vec::new();
    if reply_all {
        for addr in &original.cc {
            if addr.address.to_lowercase() != from_lower {
                cc_list.push(addr.address.clone());
            }
        }
    }
    if let Some(extra_cc) = &req.cc {
        cc_list.extend(extra_cc.iter().cloned());
    }

    let subject = if has_reply_prefix(&original.subject) {
        original.subject.clone()
    } else {
        format!("{}{}", locale.reply_prefix(), original.subject)
    };

    let from_display = format_sender(original.from.as_ref());
    let date_display = format_date_outlook(original.date.as_deref(), locale);
    let to_display = format_recipients(&original.to);

    // Plaintext body
    let quoted_plain = original
        .body_text
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let intro = locale.plain_reply_intro(&date_display, &from_display);
    let plain_body = format!("{body}\n\n{intro}\n{quoted_plain}", body = req.body);

    // HTML body (Outlook Web style)
    let quoted_content = prepare_quoted_content(original.body_html.as_deref(), &original.body_text);
    let metablock = quote_metablock_html(
        &from_display,
        &date_display,
        &to_display,
        &original.subject,
        &quoted_content,
        locale,
    );
    let html_body = wrap_html_document(&format!(
        "{body}{sig}{appendonsend}{metablock}",
        body = body_div(&html_escape(&req.body), locale),
        sig = signature_block(signature_html, locale),
        appendonsend = APPEND_ON_SEND,
    ));

    let mut builder = MessageBuilder::new()
        .subject(&subject)
        .text_body(&plain_body)
        .html_body(&html_body);
    builder = apply_from(builder, &from, display_name.as_deref());

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

    let attachment_data = match read_attachments(
        req.attachments.as_deref(),
        &server.config.allowed_attachment_dirs,
    ) {
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

    match client.save_draft(&message_bytes).await {
        Ok(()) => {
            let mut response = serde_json::json!({
                "status": "ok",
                "from": from,
                "to": to_list,
                "cc": cc_list,
                "subject": subject,
                "body_preview": truncate(&plain_body, 500),
            });
            if !has_threading {
                response["warning"] = serde_json::json!(
                    "Original email has no Message-ID. Reply was created without threading headers (In-Reply-To/References) — it may not appear in the same thread in the recipient's mail client."
                );
            }
            serde_json::to_string(&response).unwrap_or_else(|e| error_json(&e.to_string()))
        }
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
    let display_name = account_config.display_name.clone();
    let signature_html = account_config.signature_html.as_deref().unwrap_or("");
    let locale = Locale::from_config(account_config.locale.as_deref());

    let mut client = client_arc.lock().await;

    let original = match client.get_email(&req.folder, req.uid).await {
        Ok(Some(email)) => email,
        Ok(None) => return error_json(&format!("Email {} not found in {}", req.uid, req.folder)),
        Err(e) => return error_json(&client.check_error(e).to_string()),
    };

    let subject = if has_forward_prefix(&original.subject) {
        original.subject.clone()
    } else {
        format!("{}{}", locale.forward_prefix(), original.subject)
    };

    let from_display = format_sender(original.from.as_ref());
    let date_display = format_date_outlook(original.date.as_deref(), locale);
    let to_display = format_recipients(&original.to);

    // Plaintext forward — From / Sent / To / Subject order
    let labels = locale.quote_labels();
    let fwd_header = format!(
        "---------- {header} ----------\n\
         {from_label}: {from_display}\n\
         {sent_label}: {date_display}\n\
         {to_label}: {to_display}\n\
         {subj_label}: {subject}",
        header = locale.forwarded_message_label(),
        from_label = labels[0],
        sent_label = labels[1],
        to_label = labels[2],
        subj_label = labels[3],
        subject = original.subject,
    );
    let plain_body = if let Some(msg) = &req.body {
        format!("{msg}\n\n{fwd_header}\n\n{}", original.body_text)
    } else {
        format!("{fwd_header}\n\n{}", original.body_text)
    };

    // HTML forward (Outlook Web style)
    let quoted_content = prepare_quoted_content(original.body_html.as_deref(), &original.body_text);
    let metablock = quote_metablock_html(
        &from_display,
        &date_display,
        &to_display,
        &original.subject,
        &quoted_content,
        locale,
    );
    let body_html_content = match req.body.as_deref() {
        Some(msg) if !msg.is_empty() => html_escape(msg),
        _ => "<br>".to_string(),
    };
    let html_body = wrap_html_document(&format!(
        "{body}{sig}{appendonsend}{metablock}",
        body = body_div(&body_html_content, locale),
        sig = signature_block(signature_html, locale),
        appendonsend = APPEND_ON_SEND,
    ));

    let mut builder = MessageBuilder::new()
        .subject(&subject)
        .text_body(&plain_body)
        .html_body(&html_body);
    builder = apply_from(builder, &from, display_name.as_deref());

    for addr in &req.to {
        builder = builder.to(addr.as_str());
    }
    if let Some(cc) = &req.cc {
        for addr in cc {
            builder = builder.cc(addr.as_str());
        }
    }

    let attachment_data = match read_attachments(
        req.attachments.as_deref(),
        &server.config.allowed_attachment_dirs,
    ) {
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

    match client.save_draft(&message_bytes).await {
        Ok(()) => serde_json::to_string(&serde_json::json!({
            "status": "ok",
            "from": from,
            "to": req.to,
            "cc": req.cc.as_deref().unwrap_or_default(),
            "subject": subject,
            "body_preview": truncate(&plain_body, 500),
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
    let display_name = account_config.display_name.clone();
    let signature_html = account_config.signature_html.as_deref().unwrap_or("");
    let locale = Locale::from_config(account_config.locale.as_deref());

    let html_body = wrap_html_document(&format!(
        "{body}{sig}",
        body = body_div(&html_escape(&req.body), locale),
        sig = signature_block(signature_html, locale),
    ));

    let mut builder = MessageBuilder::new()
        .subject(&req.subject)
        .text_body(&req.body)
        .html_body(&html_body);
    builder = apply_from(builder, &from, display_name.as_deref());

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

    let attachment_data = match read_attachments(
        req.attachments.as_deref(),
        &server.config.allowed_attachment_dirs,
    ) {
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

// ========== HTML construction (Outlook Web style) ==========

const APPEND_ON_SEND: &str = "<div id=\"appendonsend\"></div>\n";

/// Wrap HTML body content in a full Outlook Web–style document.
fn wrap_html_document(body_content: &str) -> String {
    format!(
        "<html>\n<head>\n\
         <meta http-equiv=\"Content-Type\" content=\"text/html; charset=utf-8\">\n\
         <style type=\"text/css\" style=\"display:none;\"> P {{margin-top:0;margin-bottom:0;}} </style>\n\
         </head>\n<body dir=\"ltr\">\n\
         {body_content}\
         </body>\n</html>\n"
    )
}

/// Outlook Web body div with locale-specific font and color, and the
/// `elementToProof` class that OWA marks editable content with.
fn body_div(content: &str, locale: Locale) -> String {
    format!(
        "<div style=\"font-family: {font}; font-size: 12pt; color: {color};\" class=\"elementToProof\">\n\
         {content}</div>\n",
        font = locale.font(),
        color = locale.color(),
    )
}

/// Outlook Web signature block: `<div id="Signature">` containing a blank
/// spacer line and a `divtagdefaultwrapper` with the actual signature HTML.
fn signature_block(signature_html: &str, locale: Locale) -> String {
    if signature_html.is_empty() {
        return String::new();
    }
    format!(
        "<div id=\"Signature\" class=\"elementToProof\">\n\
         <div style=\"font-family: {font}; font-size: 12pt; color: {color};\">\n\
         <br>\n\
         </div>\n\
         <div id=\"divtagdefaultwrapper\">\n\
         {signature_html}\n\
         </div>\n\
         </div>\n",
        font = locale.font(),
        color = locale.color(),
    )
}

/// Outlook Web quote-message block: hr separator + `divRplyFwdMsg` header
/// (with `<font>` wrapper) + `BodyFragment` quoted content.
fn quote_metablock_html(
    from_display: &str,
    sent: &str,
    to_display: &str,
    subject: &str,
    quoted_content: &str,
    locale: Locale,
) -> String {
    let labels = locale.quote_labels();
    format!(
        "<hr style=\"display:inline-block;width:98%\" tabindex=\"-1\"><div id=\"divRplyFwdMsg\" dir=\"ltr\"><font face=\"Calibri, sans-serif\" style=\"font-size:11pt\" color=\"#000000\">\
         <b>{l0}:</b> {from}<br>\n\
         <b>{l1}:</b> {sent}<br>\n\
         <b>{l2}:</b> {to}<br>\n\
         <b>{l3}:</b> {subj}</font>\n\
         <div>&nbsp;</div>\n\
         </div>\n\
         <div class=\"BodyFragment\"><font size=\"2\"><span style=\"font-size:11pt;\">\n\
         <div class=\"PlainText\">{quoted_content}</div>\n\
         </span></font></div>\n",
        l0 = labels[0],
        l1 = labels[1],
        l2 = labels[2],
        l3 = labels[3],
        from = html_escape(from_display),
        to = html_escape(to_display),
        subj = html_escape(subject),
    )
}

/// Prepare the original email content for quoting in HTML.
/// Uses the HTML body (stripped of `<html>/<body>` wrappers) if available,
/// otherwise HTML-escapes the plaintext body. Normalises all `<br>` variants
/// (`<br>`, `<br/>`, `<br />`) to `<br>\n` (matching Outlook Web's
/// pretty-printed quote output).
fn prepare_quoted_content(body_html: Option<&str>, body_text: &str) -> String {
    let raw = match body_html {
        Some(html) => strip_html_wrapper(html).to_string(),
        None => html_escape(body_text),
    };
    // Normalise self-closing variants first, then ensure exactly one newline
    // after each `<br>`. The final collapse handles inputs that already had
    // `<br>\n`.
    raw.replace("<br/>", "<br>\n")
        .replace("<br />", "<br>\n")
        .replace("<br>", "<br>\n")
        .replace("<br>\n\n", "<br>\n")
}

/// Apply a From address to a MessageBuilder, optionally with a display name.
fn apply_from<'a>(
    builder: MessageBuilder<'a>,
    address: &'a str,
    display_name: Option<&'a str>,
) -> MessageBuilder<'a> {
    match display_name {
        Some(name) => builder.from((name, address)),
        None => builder.from(address),
    }
}

// ========== Formatting helpers ==========

/// Format a sender address for display: "Name <address>" when a display name
/// is set, otherwise "address <address>" (Outlook style with redundant brackets).
fn format_sender(from: Option<&crate::email::EmailAddress>) -> String {
    from.map_or_else(
        || "unknown".to_string(),
        |a| {
            let name = a.name.as_deref().unwrap_or(&a.address);
            format!("{name} <{}>", a.address)
        },
    )
}

/// Format a list of recipients: "Name <addr>; Name2 <addr2>".
fn format_recipients(addrs: &[crate::email::EmailAddress]) -> String {
    addrs
        .iter()
        .map(|a| {
            let name = a.name.as_deref().unwrap_or(&a.address);
            format!("{name} <{}>", a.address)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Format an ISO 8601 date string into Outlook-style human-readable format.
/// EN: "Tuesday, March 24, 2026 1:56:47 PM" (12h with seconds, uppercase)
/// DE: "Dienstag, 24. März 2026 13:56" (24h, no seconds)
fn format_date_outlook(iso: Option<&str>, locale: Locale) -> String {
    let Some(iso) = iso else {
        return locale.unknown_date().to_string();
    };
    if iso.len() < 16 {
        return iso.to_string();
    }
    let year: i32 = iso[0..4].parse().unwrap_or(0);
    let month: u32 = iso[5..7].parse().unwrap_or(0);
    let day: u32 = iso[8..10].parse().unwrap_or(0);
    let hour: u32 = iso[11..13].parse().unwrap_or(0);
    let minute = &iso[14..16];
    let second = if iso.len() >= 19 { &iso[17..19] } else { "00" };

    let weekday_idx = weekday_index(year, month, day);

    match locale {
        Locale::En => {
            const MONTHS: [&str; 12] = [
                "January",
                "February",
                "March",
                "April",
                "May",
                "June",
                "July",
                "August",
                "September",
                "October",
                "November",
                "December",
            ];
            const WEEKDAYS: [&str; 7] = [
                "Sunday",
                "Monday",
                "Tuesday",
                "Wednesday",
                "Thursday",
                "Friday",
                "Saturday",
            ];
            let month_name = MONTHS.get(month.wrapping_sub(1) as usize).unwrap_or(&"???");
            let weekday = WEEKDAYS[weekday_idx];
            let (h12, ampm) = match hour {
                0 => (12, "AM"),
                1..=11 => (hour, "AM"),
                12 => (12, "PM"),
                _ => (hour - 12, "PM"),
            };
            format!("{weekday}, {month_name} {day}, {year} {h12}:{minute}:{second} {ampm}")
        }
        Locale::De => {
            const MONTHS: [&str; 12] = [
                "Januar",
                "Februar",
                "März",
                "April",
                "Mai",
                "Juni",
                "Juli",
                "August",
                "September",
                "Oktober",
                "November",
                "Dezember",
            ];
            const WEEKDAYS: [&str; 7] = [
                "Sonntag",
                "Montag",
                "Dienstag",
                "Mittwoch",
                "Donnerstag",
                "Freitag",
                "Samstag",
            ];
            let month_name = MONTHS.get(month.wrapping_sub(1) as usize).unwrap_or(&"???");
            let weekday = WEEKDAYS[weekday_idx];
            // German: 24h, no seconds, "Dienstag, 24. März 2026 13:56"
            format!("{weekday}, {day}. {month_name} {year} {hour:02}:{minute}")
        }
    }
}

/// Day-of-week index (0=Sunday) using Tomohiko Sakamoto's algorithm.
fn weekday_index(year: i32, month: u32, day: u32) -> usize {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if month <= 2 { year - 1 } else { year };
    (y + y / 4 - y / 100 + y / 400 + T[month.wrapping_sub(1) as usize] + day as i32).rem_euclid(7)
        as usize
}

/// Returns true if the subject already starts with a known reply prefix.
/// Used to avoid stacking "Re: AW: ..." when replying.
fn has_reply_prefix(subject: &str) -> bool {
    let lower = subject.trim_start().to_ascii_lowercase();
    ["re:", "aw:", "antw:", "antwort:"]
        .iter()
        .any(|p| lower.starts_with(p))
}

/// Returns true if the subject already starts with a known forward prefix.
fn has_forward_prefix(subject: &str) -> bool {
    let lower = subject.trim_start().to_ascii_lowercase();
    ["fwd:", "fw:", "wg:", "weitergeleitet:"]
        .iter()
        .any(|p| lower.starts_with(p))
}

/// Strip outer `<html>`, `<head>`, `<body>` wrapper tags from HTML content.
/// Returns the inner content between `<body>` and `</body>`, or the full
/// string if no `<body>` tag is found.
fn strip_html_wrapper(html: &str) -> &str {
    let s = html.trim();
    let start = s
        .find("<body")
        .and_then(|pos| s[pos..].find('>').map(|gt| pos + gt + 1))
        .unwrap_or(0);
    let end = s.rfind("</body>").unwrap_or(s.len());
    if start < end { s[start..end].trim() } else { s }
}

/// Escape HTML special characters and convert newlines to `<br>`.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\n', "<br>\n")
}

// ========== Attachment helpers ==========

/// Read attachment files from disk. Returns (content_type, filename, bytes) tuples.
type AttachmentData = Vec<(&'static str, String, Vec<u8>)>;

fn read_attachments(
    attachments: Option<&[String]>,
    allowed_dirs: &[String],
) -> Result<AttachmentData, String> {
    let Some(paths) = attachments else {
        return Ok(vec![]);
    };
    let mut result = Vec::new();
    for path_str in paths {
        let path = std::path::Path::new(path_str);
        validate_attachment_path(path, allowed_dirs)?;
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

/// Reject attachment paths outside the configured whitelist. Uses `canonicalize`
/// to follow symlinks and resolve `..`, preventing path-traversal or symlink
/// escapes out of the allowed directories.
fn validate_attachment_path(path: &std::path::Path, allowed_dirs: &[String]) -> Result<(), String> {
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("Cannot resolve attachment path \"{}\": {e}", path.display()))?;
    for allowed in allowed_dirs {
        if let Ok(allowed_canonical) = std::path::Path::new(allowed).canonicalize()
            && canonical.starts_with(&allowed_canonical)
        {
            return Ok(());
        }
    }
    Err(format!(
        "Attachment path \"{}\" is not within any allowed directory. \
         Configured allowed_attachment_dirs: {allowed_dirs:?}",
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

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..s.floor_char_boundary(max_len)])
    }
}
