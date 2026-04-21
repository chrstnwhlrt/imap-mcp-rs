//! MIME parsing into the response types ([`EmailFull`] / [`EmailSummary`])
//! returned by IMAP read operations.
//!
//! Wraps [`mail_parser`] with two performance-shaped variants:
//! - [`parse_email`] — full email including HTML body
//! - [`parse_email_no_html`] — skips `body_html` allocation; used by
//!   `list_emails` / `search_emails` where only the snippet is needed
//!   (40–60 KB saved per HTML newsletter).
//!
//! [`summarize`] is the partner that *consumes* an [`EmailFull`] into an
//! [`EmailSummary`] without cloning, since the full struct is dropped right
//! after.

use mail_parser::{ContentType, MimeHeaders};
use serde::Serialize;

/// Format a MIME `Content-Type` as `type/subtype`, falling back to
/// `application/octet-stream` when absent. The raw bytes are attacker-
/// controlled (remote sender sets `Content-Type`) — strip control chars and
/// Unicode bidirectional overrides so the value can safely land in the
/// JSON response shown to the LLM without smuggling display-manipulation
/// or prompt-injection payloads.
pub fn format_content_type(ct: Option<&ContentType<'_>>) -> String {
    let raw = ct.map_or_else(
        || "application/octet-stream".to_string(),
        |ct| {
            ct.subtype().map_or_else(
                || ct.ctype().to_string(),
                |sub| format!("{}/{sub}", ct.ctype()),
            )
        },
    );
    sanitize_external_str(&raw)
}

/// Sanitize a string that originated from untrusted email headers/metadata
/// before handing it to the LLM or any downstream consumer. Removes:
///   - control chars (including CR/LF/NUL/tab)
///   - Unicode line separators (U+2028 / U+2029) and BOM (U+FEFF)
///   - bidirectional override/isolate characters (U+202A..E, U+2066..9)
///     which can flip visual order to disguise filenames like
///     `invoice<RLO>gpj.exe` (renders as "invoiceexe.jpg")
///   - soft hyphen (U+00AD) and zero-width chars (U+200B..D, LRM/RLM) —
///     invisible-but-present chars that can hide content ("inv\u{200B}oice.exe"
///     renders as "invoice.exe" in most UIs) or break substring-equality
///     checks if one caller strips them and another doesn't.
pub fn sanitize_external_str(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            !c.is_control()
                && !matches!(c, '\u{00AD}' | '\u{2028}' | '\u{2029}' | '\u{FEFF}')
                && !('\u{200B}'..='\u{200F}').contains(&c)
                && !('\u{202A}'..='\u{202E}').contains(&c)
                && !('\u{2060}'..='\u{2064}').contains(&c)
                && !('\u{2066}'..='\u{2069}').contains(&c)
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct EmailAddress {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub address: String,
}

impl EmailAddress {
    pub fn parse_addr(addr: &mail_parser::Addr<'_>) -> Self {
        // Sender-controlled display name + address both reach the LLM via
        // every EmailFull / EmailSummary. Same class of injection surface
        // as subject/snippet/filename — strip control+bidi+zero-width before
        // exposure. `address` is normally ASCII-clean but a malicious server
        // could smuggle bidi chars here too.
        Self {
            name: addr
                .name
                .as_deref()
                .filter(|n| !n.is_empty())
                .map(sanitize_external_str),
            address: sanitize_external_str(addr.address.as_deref().unwrap_or("")),
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

/// Max recipients echoed in a summary's `to` list. Full recipient set is
/// still available via `get_email`. Keeps `list_emails` / `search_emails`
/// responses bounded when a single thread has a 45-recipient mass-mail —
/// those alone can inflate a 20-row triage response by 50+ KB of JSON.
pub const SUMMARY_TO_PREVIEW: usize = 3;

#[derive(Debug, Clone, Serialize)]
pub struct EmailSummary {
    pub uid: u32,
    pub folder: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Threading hints passed through so callers can cluster by
    /// conversation. Only serialised when set (a reply), otherwise
    /// the summary stays compact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
    pub from: Option<EmailAddress>,
    /// Up to `SUMMARY_TO_PREVIEW` recipients for triage context. Real
    /// length is in `to_count`; use `get_email` for the full list.
    pub to: Vec<EmailAddress>,
    pub to_count: usize,
    /// Number of CC recipients. Addresses aren't echoed in summaries to
    /// keep the payload bounded — use `get_email` when you need them.
    pub cc_count: usize,
    pub subject: String,
    pub date: Option<String>,
    pub flags: Vec<String>,
    pub has_attachments: bool,
    pub snippet: String,
    /// Only set when a response was grouped by thread
    /// (`list_emails(group_by_thread=true)`): total messages in this
    /// thread. Absent from ungrouped responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_message_count: Option<usize>,
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
    parse_email_inner(uid, folder, raw, flags, true)
}

/// Parse without allocating `body_html` (used by list/search which only need
/// the snippet — saves 40-60 KB of String copies per HTML email).
pub fn parse_email_no_html(uid: u32, folder: &str, raw: &[u8], flags: Vec<String>) -> EmailFull {
    parse_email_inner(uid, folder, raw, flags, false)
}

fn parse_email_inner(
    uid: u32,
    folder: &str,
    raw: &[u8],
    flags: Vec<String>,
    include_html: bool,
) -> EmailFull {
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

    // Sanitize subject before it lands in the LLM's view — bidi overrides
    // and U+2028/9 in a subject would otherwise let a malicious sender
    // disguise a fake `[SYSTEM] ...` line once the LLM renders an email
    // list. Same class of defense as `filename` / `content_type`.
    let subject = sanitize_external_str(message.subject().unwrap_or(""));

    let date = message.date().map(format_datetime);

    // Message-ID / In-Reply-To / References echo back to the LLM and are
    // ALSO reused when composing replies (for threading headers). Sanitize
    // here at the single parse source; `apply_threading_headers` still
    // re-runs its own sanitize for defense-in-depth, but that was the only
    // gate before.
    let wrap = |s: &str| format!("<{}>", sanitize_external_str(s));
    let message_id = message.message_id().map(wrap);
    let in_reply_to = message
        .in_reply_to()
        .as_text_list()
        .and_then(|list| list.first().map(|s| wrap(s)));
    let references: Vec<String> = message
        .references()
        .as_text_list()
        .map(|list| list.iter().map(|s| wrap(s)).collect())
        .unwrap_or_default();

    let body_text = extract_body_text(&message);
    let body_html = if include_html {
        message.body_html(0).map(|s| s.to_string())
    } else {
        None
    };

    let attachments = message
        .attachments()
        .map(|att| AttachmentInfo {
            // Sanitize the attacker-controlled filename before it lands in the
            // JSON response read by the LLM — bidi overrides here would
            // otherwise let a sender disguise a `.exe` as `.jpg` in the
            // rendered view.
            filename: sanitize_external_str(att.attachment_name().unwrap_or("attachment")),
            content_type: format_content_type(att.content_type()),
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

/// Format a `mail_parser` `DateTime` as ISO 8601 with correct timezone offset.
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
    let mut chars = s.chars();

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
                    let code = num_str.strip_prefix('x').map_or_else(
                        || num_str.parse().ok(),
                        |hex| u32::from_str_radix(hex, 16).ok(),
                    );
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

/// Build a summary by CONSUMING the `EmailFull` — avoids cloning 7 fields per
/// email in list/search results. Callers drop the `EmailFull` right after, so
/// moving is always correct.
pub fn summarize(email: EmailFull, snippet_len: usize) -> EmailSummary {
    // Sanitize + truncate in a single pass into a pre-sized buffer.
    // Earlier flow was three allocations (format! → intermediate → sanitize
    // rebuild); for a 50-row list page on a 20 KB-body inbox that's
    // ~150 small heap allocations we can avoid entirely.
    let snippet = build_snippet(&email.body_text, snippet_len);

    // Truncate recipient list in-place (Vec::truncate doesn't reallocate,
    // it just drops elements) so a 45-way mass-mail doesn't inflate every
    // triage response by ~4 KB per row. `to_count` preserves the real size
    // so callers can still detect broadcast mails at a glance.
    let to_count = email.to.len();
    let cc_count = email.cc.len();
    let mut to = email.to;
    to.truncate(SUMMARY_TO_PREVIEW);

    EmailSummary {
        uid: email.uid,
        folder: email.folder,
        message_id: email.message_id,
        in_reply_to: email.in_reply_to,
        references: email.references,
        from: email.from,
        to,
        to_count,
        cc_count,
        subject: email.subject,
        date: email.date,
        flags: email.flags,
        has_attachments: !email.attachments.is_empty(),
        snippet,
        thread_message_count: None,
    }
}

/// Build a sanitized snippet in one pass: drop chars that
/// `sanitize_external_str` would strip, and stop once we've accumulated
/// `snippet_len` bytes of kept output (char-boundary-safe by construction),
/// appending `"..."` if anything was truncated.
fn build_snippet(body: &str, snippet_len: usize) -> String {
    let mut out = String::with_capacity(snippet_len.min(body.len()) + 3);
    let mut truncated = false;
    for c in body.chars() {
        // Matches the filter in `sanitize_external_str`; kept inline to
        // avoid a second pass over the result.
        let dangerous = c.is_control()
            || matches!(c, '\u{00AD}' | '\u{2028}' | '\u{2029}' | '\u{FEFF}')
            || ('\u{200B}'..='\u{200F}').contains(&c)
            || ('\u{202A}'..='\u{202E}').contains(&c)
            || ('\u{2060}'..='\u{2064}').contains(&c)
            || ('\u{2066}'..='\u{2069}').contains(&c);
        if dangerous {
            continue;
        }
        if out.len() + c.len_utf8() > snippet_len {
            truncated = true;
            break;
        }
        out.push(c);
    }
    if truncated {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_content_type_none_falls_back() {
        assert_eq!(format_content_type(None), "application/octet-stream");
    }

    #[test]
    fn strip_html_removes_tags_and_collapses_whitespace() {
        assert_eq!(strip_html("<p>hello</p>"), "hello");
        assert_eq!(strip_html("<b>a</b> <i>b</i>"), "a b");
        assert_eq!(strip_html("<div>a   b</div>"), "a b"); // collapse runs
        assert_eq!(strip_html("plain"), "plain");
        assert_eq!(strip_html(""), "");
    }

    #[test]
    fn strip_html_handles_nested_tags() {
        assert_eq!(strip_html("<div><p>a<span>b</span>c</p></div>"), "a b c");
    }

    #[test]
    fn decode_html_entities_named() {
        assert_eq!(decode_html_entities("&amp;"), "&");
        assert_eq!(decode_html_entities("&lt;"), "<");
        assert_eq!(decode_html_entities("&gt;"), ">");
        assert_eq!(decode_html_entities("&quot;"), "\"");
        assert_eq!(decode_html_entities("&apos;"), "'");
        assert_eq!(decode_html_entities("&nbsp;"), " ");
    }

    #[test]
    fn decode_html_entities_numeric_decimal() {
        assert_eq!(decode_html_entities("&#65;"), "A");
        assert_eq!(decode_html_entities("&#228;"), "ä");
        assert_eq!(decode_html_entities("&#8364;"), "€");
    }

    #[test]
    fn decode_html_entities_numeric_hex() {
        assert_eq!(decode_html_entities("&#x41;"), "A");
        assert_eq!(decode_html_entities("&#xE4;"), "ä");
    }

    #[test]
    fn decode_html_entities_unknown_passes_through() {
        // Unknown named entity: emit as-is.
        assert_eq!(decode_html_entities("&fakeentity;"), "&fakeentity;");
    }

    #[test]
    fn decode_html_entities_unterminated_emits_literal() {
        // `&amp` (no semicolon) at end of string: emit literally.
        assert_eq!(decode_html_entities("&amp"), "&amp");
        assert_eq!(decode_html_entities("a & b"), "a & b"); // lone &
    }

    #[test]
    fn decode_html_entities_invalid_codepoint() {
        // U+FFFFFFFF is not a valid char — should emit literally.
        assert_eq!(decode_html_entities("&#xFFFFFFFF;"), "&#xFFFFFFFF;");
    }

    #[test]
    fn decode_html_entities_long_run_emits_literal() {
        // Entity longer than 10 chars without `;` → emit literally.
        let result = decode_html_entities("&waytoolongtobeavalidentity;");
        assert!(result.starts_with('&'));
        assert!(result.contains("waytoolong"));
    }

    #[test]
    fn decode_html_entities_mixed_text() {
        assert_eq!(
            decode_html_entities("Hello &amp; goodbye &#228;"),
            "Hello & goodbye ä"
        );
    }

    #[test]
    fn sanitize_external_str_strips_crlf_and_control() {
        assert_eq!(sanitize_external_str("a\r\nb"), "ab");
        assert_eq!(sanitize_external_str("a\x00b\tc"), "abc");
        assert_eq!(sanitize_external_str("normal text"), "normal text");
        assert_eq!(sanitize_external_str("ünïcödë ok"), "ünïcödë ok");
    }

    #[test]
    fn sanitize_external_str_strips_unicode_line_and_bom() {
        // U+2028 LS, U+2029 PS, U+FEFF BOM
        assert_eq!(sanitize_external_str("a\u{2028}b"), "ab");
        assert_eq!(sanitize_external_str("a\u{2029}b"), "ab");
        assert_eq!(sanitize_external_str("\u{FEFF}hello"), "hello");
    }

    #[test]
    fn sanitize_external_str_strips_bidi_and_zero_width() {
        // Bidi overrides
        assert_eq!(
            sanitize_external_str("invoice\u{202E}gpj.exe"),
            "invoicegpj.exe"
        );
        // Zero-width space
        assert_eq!(sanitize_external_str("inv\u{200B}oice"), "invoice");
        // LRM/RLM
        assert_eq!(sanitize_external_str("a\u{200E}b\u{200F}c"), "abc");
        // Soft hyphen
        assert_eq!(sanitize_external_str("inv\u{00AD}oice"), "invoice");
        // Invisible operators
        assert_eq!(sanitize_external_str("a\u{2060}b"), "ab");
        // Bidi isolates
        assert_eq!(sanitize_external_str("a\u{2066}b\u{2069}c"), "abc");
    }

    #[test]
    fn sanitize_external_str_empty_and_all_dangerous() {
        assert_eq!(sanitize_external_str(""), "");
        assert_eq!(sanitize_external_str("\r\n\t\x00"), "");
        assert_eq!(sanitize_external_str("\u{2028}\u{202E}\u{200B}"), "");
    }

    #[test]
    fn build_snippet_short_body_returns_full_content() {
        assert_eq!(build_snippet("hello", 200), "hello");
        assert_eq!(build_snippet("", 200), "");
    }

    #[test]
    fn build_snippet_long_body_truncates_with_ellipsis() {
        let body = "a".repeat(500);
        let snippet = build_snippet(&body, 100);
        // Up to 100 ASCII chars + "..."
        assert!(snippet.ends_with("..."));
        assert_eq!(snippet.len(), 103);
    }

    #[test]
    fn build_snippet_strips_dangerous_chars_inline() {
        // Sanitize happens BEFORE the byte counter, so dangerous chars
        // don't count toward the snippet budget.
        assert_eq!(build_snippet("a\u{202E}b\r\nc", 200), "abc");
    }

    #[test]
    fn build_snippet_respects_utf8_boundary() {
        // Each "ä" is 2 bytes. snippet_len=3 fits exactly one.
        let snippet = build_snippet("äöü rest", 3);
        assert!(snippet.ends_with("..."));
        // The output before "..." must be valid UTF-8 and contain "ä".
        assert!(snippet.starts_with('ä'));
    }

    #[test]
    fn build_snippet_all_dangerous_returns_empty() {
        assert_eq!(build_snippet("\r\n\u{202E}\u{200B}", 200), "");
    }
}
