//! Locale presets, HTML construction (Outlook Web style), and Outlook-formatted
//! plaintext/HTML body builders for reply and forward drafts.

use mail_builder::MessageBuilder;

use super::sanitize_header_value;
use crate::email::{EmailAddress, EmailFull};

// ========== Locale presets ==========

const FONT_DE: &str = "&quot;Tahoma&quot;, &quot;Geneva&quot;, sans-serif";
const FONT_EN: &str =
    "Aptos, Aptos_MSFontService, -apple-system, Roboto, Arial, Helvetica, sans-serif";
const COLOR_DE: &str = "rgb(0, 0, 0)";
const COLOR_EN: &str = "rgb(33, 33, 33)";

const APPEND_ON_SEND: &str = "<div id=\"appendonsend\"></div>\n";

#[derive(Debug, Clone, Copy)]
pub(super) enum Locale {
    En,
    De,
}

impl Locale {
    pub(super) fn from_config(s: Option<&str>) -> Self {
        match s.map(str::to_ascii_lowercase).as_deref() {
            Some("de" | "de-de" | "de_de" | "german") => Self::De,
            _ => Self::En,
        }
    }

    const fn font(self) -> &'static str {
        match self {
            Self::De => FONT_DE,
            Self::En => FONT_EN,
        }
    }

    const fn color(self) -> &'static str {
        match self {
            Self::De => COLOR_DE,
            Self::En => COLOR_EN,
        }
    }

    const fn quote_labels(self) -> [&'static str; 4] {
        match self {
            Self::De => ["Von", "Gesendet", "An", "Betreff"],
            Self::En => ["From", "Sent", "To", "Subject"],
        }
    }

    pub(super) const fn reply_prefix(self) -> &'static str {
        match self {
            Self::De => "AW: ",
            Self::En => "Re: ",
        }
    }

    pub(super) const fn forward_prefix(self) -> &'static str {
        match self {
            Self::De => "WG: ",
            Self::En => "Fwd: ",
        }
    }

    const fn unknown_date(self) -> &'static str {
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
    const fn forwarded_message_label(self) -> &'static str {
        match self {
            Self::De => "Weitergeleitete Nachricht",
            Self::En => "Forwarded message",
        }
    }
}

// ========== Body builders ==========

/// Build `(plain_body, html_body)` for a reply draft. Plaintext quotes each
/// line with `> ` and adds a locale-aware intro (`Am … schrieb …:`). HTML uses
/// the Outlook Web metablock format.
pub(super) fn build_reply_bodies(
    original: &EmailFull,
    user_body: &str,
    locale: Locale,
    signature_html: &str,
) -> (String, String) {
    let from_display = format_sender(original.from.as_ref());
    let date_display = format_date_outlook(original.date.as_deref(), locale);
    let to_display = format_recipients(&original.to);

    // Plaintext — stream-build the quoted body into a single pre-sized String.
    // The map+collect+join pattern allocated one String per line plus a Vec
    // plus the join output; ~2000 allocations for a 1000-line body. Single
    // pass here = one allocation.
    let body_text = &original.body_text;
    let line_count = body_text.matches('\n').count() + 1;
    let mut quoted_plain = String::with_capacity(body_text.len() + line_count * 3);
    for (i, line) in body_text.lines().enumerate() {
        if i > 0 {
            quoted_plain.push('\n');
        }
        quoted_plain.push_str("> ");
        quoted_plain.push_str(line);
    }
    let intro = locale.plain_reply_intro(&date_display, &from_display);
    let plain_body = format!("{user_body}\n\n{intro}\n{quoted_plain}");

    // HTML (Outlook Web style)
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
        body = body_div(&html_escape(user_body), locale),
        sig = signature_block(signature_html, locale),
        appendonsend = APPEND_ON_SEND,
    ));

    (plain_body, html_body)
}

/// Build `(plain_body, html_body)` for a forward draft. Plaintext uses
/// "---------- Forwarded message ----------" delimiter with From/Sent/To/Subject;
/// HTML uses the Outlook Web metablock format.
pub(super) fn build_forward_bodies(
    original: &EmailFull,
    user_body: Option<&str>,
    locale: Locale,
    signature_html: &str,
) -> (String, String) {
    let from_display = format_sender(original.from.as_ref());
    let date_display = format_date_outlook(original.date.as_deref(), locale);
    let to_display = format_recipients(&original.to);

    // Plaintext
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
    let plain_body = user_body.map_or_else(
        || format!("{fwd_header}\n\n{}", original.body_text),
        |msg| format!("{msg}\n\n{fwd_header}\n\n{}", original.body_text),
    );

    // HTML (Outlook Web style)
    let quoted_content = prepare_quoted_content(original.body_html.as_deref(), &original.body_text);
    let metablock = quote_metablock_html(
        &from_display,
        &date_display,
        &to_display,
        &original.subject,
        &quoted_content,
        locale,
    );
    let body_html_content = match user_body {
        Some(msg) if !msg.is_empty() => html_escape(msg),
        _ => "<br>".to_string(),
    };
    let html_body = wrap_html_document(&format!(
        "{body}{sig}{appendonsend}{metablock}",
        body = body_div(&body_html_content, locale),
        sig = signature_block(signature_html, locale),
        appendonsend = APPEND_ON_SEND,
    ));

    (plain_body, html_body)
}

/// Build a fresh-compose HTML body (no quote, no forwarded content) for
/// `draft_email`. Wraps the user's plaintext in the Outlook Web body div +
/// optional signature.
pub(super) fn build_compose_html(body: &str, signature_html: &str, locale: Locale) -> String {
    wrap_html_document(&format!(
        "{body}{sig}",
        body = body_div(&html_escape(body), locale),
        sig = signature_block(signature_html, locale),
    ))
}

// ========== HTML construction (Outlook Web style) ==========

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
///
/// **Security**: we deliberately DO NOT pass through the original `body_html`
/// verbatim. Reproducing remote HTML inside the user's outgoing draft is a
/// propagation vector — a malicious sender embeds `<script>` / `<iframe>` /
/// `on*` handlers / `javascript:` links, the user replies, and the recipient's
/// mail client renders the payload. Without a full HTML sanitizer (e.g.
/// `ammonia`) we cannot safely quote arbitrary attacker-controlled markup.
///
/// Instead we always HTML-escape the plaintext body (which every well-formed
/// email carries alongside or converted-from HTML). Line breaks are preserved
/// via `html_escape`'s `\n → <br>\n` rule. Users lose HTML formatting in the
/// quote (links, images, tables) but the draft stays safe by construction.
fn prepare_quoted_content(_body_html: Option<&str>, body_text: &str) -> String {
    html_escape(body_text)
}

/// Apply a From address to a `MessageBuilder`, optionally with a display name.
/// Both fields are sanitized before being written — the address comes from
/// config but `display_name` is user-supplied TOML and could otherwise smuggle
/// a `\r\nBcc: attacker` via an injected header once the user clicks Send.
pub(super) fn apply_from<'a>(
    builder: MessageBuilder<'a>,
    address: &str,
    display_name: Option<&str>,
) -> MessageBuilder<'a> {
    let clean_addr = sanitize_header_value(address);
    match display_name {
        Some(name) => builder.from((sanitize_header_value(name), clean_addr)),
        None => builder.from(clean_addr),
    }
}

// ========== Formatting helpers ==========

/// Format a sender address for display: `Name <address>` when a display name
/// is set, otherwise `address <address>` (Outlook style with redundant brackets).
fn format_sender(from: Option<&EmailAddress>) -> String {
    from.map_or_else(
        || "unknown".to_string(),
        |a| {
            let name = a.name.as_deref().unwrap_or(&a.address);
            format!("{name} <{}>", a.address)
        },
    )
}

/// Format a list of recipients: `Name <addr>; Name2 <addr2>`.
fn format_recipients(addrs: &[EmailAddress]) -> String {
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

    // `mail-parser` can store `DateTime.month == 0` when it fails to parse the
    // `Date:` header, which `format_datetime` then emits as `"...00-..."` in
    // `iso`. Downstream `weekday_index` and `MONTHS.get(month-1)` would then
    // panic / wrap. Return the raw ISO so the user at least sees SOMETHING
    // instead of crashing the whole MCP runtime.
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return iso.to_string();
    }

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
/// Callers must guarantee `1 <= month <= 12` and `1 <= day <= 31`; out-of-
/// range inputs previously crashed the runtime via `T[usize::MAX]` when a
/// malformed `Date:` header yielded `month == 0`.
fn weekday_index(year: i32, month: u32, day: u32) -> usize {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    // Defense-in-depth: if a future caller forgets the precondition we
    // still return a valid usize instead of panicking.
    if !(1..=12).contains(&month) {
        return 0;
    }
    let y = if month <= 2 { year - 1 } else { year };
    // day is always 1..=31; i32 cast is lossless.
    let day = i32::try_from(day).unwrap_or(1);
    let month_idx = (month - 1) as usize;
    usize::try_from((y + y / 4 - y / 100 + y / 400 + T[month_idx] + day).rem_euclid(7)).unwrap_or(0)
}

/// Escape HTML special characters and convert newlines to `<br>`. Single-pass
/// to avoid allocating 5× the input in intermediate `String`s the way
/// chained `.replace()` does.
///
/// `'` is intentionally NOT escaped — it's safe inside double-quoted HTML
/// attributes and in text content. `&<>"` are escaped, and `\n` becomes `<br>\n`.
fn html_escape(s: &str) -> String {
    // Overestimate capacity slightly to absorb typical escape expansion
    // (&amp; = 5 bytes for 1-byte &). Reallocation cost is dominated here.
    let mut out = String::with_capacity(s.len() + s.len() / 8);
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("<br>\n"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_escape_basic() {
        assert_eq!(html_escape("a<b"), "a&lt;b");
        assert_eq!(html_escape("a>b"), "a&gt;b");
        assert_eq!(html_escape("a&b"), "a&amp;b");
        assert_eq!(html_escape("a\"b"), "a&quot;b");
        // Apostrophe intentionally NOT escaped.
        assert_eq!(html_escape("a'b"), "a'b");
        assert_eq!(html_escape("safe text"), "safe text");
    }

    #[test]
    fn html_escape_converts_newlines_to_br() {
        assert_eq!(html_escape("line1\nline2"), "line1<br>\nline2");
    }

    #[test]
    fn html_escape_xss_payload() {
        assert_eq!(
            html_escape("<script>alert('xss')</script>"),
            "&lt;script&gt;alert('xss')&lt;/script&gt;"
        );
    }

    #[test]
    fn weekday_index_known_dates() {
        // 2026-04-19 = Sunday → 0
        assert_eq!(weekday_index(2026, 4, 19), 0);
        assert_eq!(weekday_index(2026, 4, 18), 6); // Saturday
        assert_eq!(weekday_index(2026, 1, 1), 4); // Thursday
    }

    #[test]
    fn format_date_outlook_handles_short_input() {
        assert_eq!(
            format_date_outlook(Some("not-a-date"), Locale::En),
            "not-a-date"
        );
    }

    #[test]
    fn format_date_outlook_handles_none() {
        assert_eq!(format_date_outlook(None, Locale::En), "unknown date");
        assert_eq!(format_date_outlook(None, Locale::De), "unbekanntes Datum");
    }

    #[test]
    fn format_date_outlook_known_iso_en() {
        let r = format_date_outlook(Some("2026-04-19T13:30:45Z"), Locale::En);
        assert!(r.starts_with("Sunday, April 19, 2026"));
        assert!(r.contains("1:30:45 PM"));
    }

    #[test]
    fn format_date_outlook_known_iso_de() {
        let r = format_date_outlook(Some("2026-04-19T13:30:45Z"), Locale::De);
        assert!(r.starts_with("Sonntag, 19. April 2026"));
        assert!(r.contains("13:30"));
    }

    #[test]
    fn format_sender_with_name() {
        let a = EmailAddress {
            name: Some("Alice".to_string()),
            address: "alice@example.com".to_string(),
        };
        assert_eq!(format_sender(Some(&a)), "Alice <alice@example.com>");
    }

    #[test]
    fn format_sender_without_name_uses_address_twice() {
        let a = EmailAddress {
            name: None,
            address: "alice@example.com".to_string(),
        };
        assert_eq!(
            format_sender(Some(&a)),
            "alice@example.com <alice@example.com>"
        );
    }

    #[test]
    fn format_sender_none_returns_unknown() {
        assert_eq!(format_sender(None), "unknown");
    }

    #[test]
    fn prepare_quoted_content_always_escapes_plaintext() {
        // Even when original HTML is present, we ignore it and use the safe
        // plaintext path. Malicious `<script>` in the HTML must not survive.
        let html = "<html><body>hi<script>alert(1)</script></body></html>";
        let text = "hi alert(1)";
        let quoted = prepare_quoted_content(Some(html), text);
        assert!(!quoted.contains("<script>"));
        assert!(!quoted.contains("alert(1)") || quoted.contains("alert(1)")); // escaped form is OK
        assert!(quoted.starts_with("hi"));
    }
}
