use mail_parser::MimeHeaders;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct EmailAddress {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub address: String,
}

impl EmailAddress {
    pub fn parse_addr(addr: &mail_parser::Addr<'_>) -> Self {
        Self {
            name: addr
                .name
                .as_deref()
                .filter(|n| !n.is_empty())
                .map(String::from),
            address: addr.address.as_deref().unwrap_or("").to_string(),
        }
    }

    pub fn list_from_address(addr: &mail_parser::Address<'_>) -> Vec<Self> {
        match addr {
            mail_parser::Address::List(list) => list.iter().map(Self::parse_addr).collect(),
            mail_parser::Address::Group(groups) => groups
                .iter()
                .flat_map(|g| g.addresses.iter().map(Self::parse_addr))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EmailSummary {
    pub uid: u32,
    pub folder: String,
    pub from: Option<EmailAddress>,
    pub to: Vec<EmailAddress>,
    pub subject: String,
    pub date: Option<String>,
    pub flags: Vec<String>,
    pub has_attachments: bool,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmailFull {
    pub uid: u32,
    pub folder: String,
    pub from: Option<EmailAddress>,
    pub to: Vec<EmailAddress>,
    pub cc: Vec<EmailAddress>,
    pub subject: String,
    pub date: Option<String>,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub flags: Vec<String>,
    pub body_text: String,
    pub body_html: Option<String>,
    pub attachments: Vec<AttachmentInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttachmentInfo {
    pub filename: String,
    pub content_type: String,
    pub size: usize,
}

pub fn parse_email(uid: u32, folder: &str, raw: &[u8], flags: Vec<String>) -> EmailFull {
    let message = mail_parser::MessageParser::default().parse(raw);

    let Some(message) = message else {
        return EmailFull {
            uid,
            folder: folder.to_string(),
            from: None,
            to: vec![],
            cc: vec![],
            subject: String::new(),
            date: None,
            message_id: None,
            in_reply_to: None,
            references: vec![],
            flags,
            body_text: String::new(),
            body_html: None,
            attachments: vec![],
        };
    };

    let from = message
        .from()
        .and_then(|addr| EmailAddress::list_from_address(addr).into_iter().next());

    let to = message
        .to()
        .map(|addr| EmailAddress::list_from_address(addr))
        .unwrap_or_default();

    let cc = message
        .cc()
        .map(|addr| EmailAddress::list_from_address(addr))
        .unwrap_or_default();

    let subject = message.subject().unwrap_or("").to_string();

    let date = message.date().map(format_datetime);

    let message_id = message.message_id().map(|s| format!("<{s}>"));

    let in_reply_to = message
        .in_reply_to()
        .as_text_list()
        .and_then(|list| list.first().map(|s| format!("<{s}>")));

    let references: Vec<String> = message
        .references()
        .as_text_list()
        .map(|list| list.iter().map(|s| format!("<{s}>")).collect())
        .unwrap_or_default();

    let body_text = extract_body_text(&message);
    let body_html = message.body_html(0).map(|s| s.to_string());

    let attachments = message
        .attachments()
        .map(|att| AttachmentInfo {
            filename: att.attachment_name().unwrap_or("attachment").to_string(),
            content_type: att.content_type().map_or_else(
                || "application/octet-stream".to_string(),
                |ct| {
                    if let Some(sub) = ct.subtype() {
                        format!("{}/{}", ct.ctype(), sub)
                    } else {
                        ct.ctype().to_string()
                    }
                },
            ),
            size: att.len(),
        })
        .collect();

    EmailFull {
        uid,
        folder: folder.to_string(),
        from,
        to,
        cc,
        subject,
        date,
        message_id,
        in_reply_to,
        references,
        flags,
        body_text,
        body_html,
        attachments,
    }
}

/// Format a mail_parser DateTime as ISO 8601 with correct timezone offset.
fn format_datetime(d: &mail_parser::DateTime) -> String {
    if d.tz_hour == 0 && d.tz_minute == 0 && !d.tz_before_gmt {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            d.year, d.month, d.day, d.hour, d.minute, d.second
        )
    } else {
        let sign = if d.tz_before_gmt { '-' } else { '+' };
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{sign}{:02}:{:02}",
            d.year, d.month, d.day, d.hour, d.minute, d.second, d.tz_hour, d.tz_minute,
        )
    }
}

/// Extract plain text body. If only HTML exists, strip tags and decode entities.
fn extract_body_text(message: &mail_parser::Message<'_>) -> String {
    if let Some(text) = message.body_text(0) {
        return text.to_string();
    }
    if let Some(html) = message.body_html(0) {
        return strip_html(&html);
    }
    String::new()
}

/// Strip HTML tags and decode common HTML entities.
fn strip_html(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut last_was_space = false;

    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                if !last_was_space {
                    result.push(' ');
                    last_was_space = true;
                }
            }
            _ if !in_tag => {
                if ch.is_whitespace() {
                    if !last_was_space {
                        result.push(' ');
                        last_was_space = true;
                    }
                } else {
                    result.push(ch);
                    last_was_space = false;
                }
            }
            _ => {}
        }
    }

    decode_html_entities(result.trim())
}

/// Decode common HTML entities.
fn decode_html_entities(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '&' {
            let mut entity = String::new();
            let mut terminated = false;
            for c in chars.by_ref() {
                if c == ';' {
                    terminated = true;
                    break;
                }
                entity.push(c);
                if entity.len() > 10 {
                    // Not a real entity, emit as-is
                    result.push('&');
                    result.push_str(&entity);
                    entity.clear();
                    break;
                }
            }
            if entity.is_empty() {
                if !terminated {
                    result.push('&'); // lone & at end of string
                }
                continue;
            }
            if !terminated {
                // Unterminated entity (e.g. &amp at EOF) — emit literally
                result.push('&');
                result.push_str(&entity);
                continue;
            }
            match entity.as_str() {
                "amp" => result.push('&'),
                "lt" => result.push('<'),
                "gt" => result.push('>'),
                "quot" => result.push('"'),
                "apos" => result.push('\''),
                "nbsp" => result.push(' '),
                s if s.starts_with('#') => {
                    let num_str = &s[1..];
                    let code = if let Some(hex) = num_str.strip_prefix('x') {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        num_str.parse().ok()
                    };
                    if let Some(c) = code.and_then(char::from_u32) {
                        result.push(c);
                    } else {
                        result.push('&');
                        result.push_str(s);
                        result.push(';');
                    }
                }
                other => {
                    result.push('&');
                    result.push_str(other);
                    result.push(';');
                }
            }
        } else {
            result.push(ch);
        }
    }

    result
}

pub fn summarize(email: &EmailFull, snippet_len: usize) -> EmailSummary {
    let snippet = if email.body_text.len() > snippet_len {
        format!(
            "{}...",
            &email.body_text[..email.body_text.floor_char_boundary(snippet_len)]
        )
    } else {
        email.body_text.clone()
    };

    EmailSummary {
        uid: email.uid,
        folder: email.folder.clone(),
        from: email.from.clone(),
        to: email.to.clone(),
        subject: email.subject.clone(),
        date: email.date.clone(),
        flags: email.flags.clone(),
        has_attachments: !email.attachments.is_empty(),
        snippet,
    }
}
