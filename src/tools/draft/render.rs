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
}

// ========== Body builders ==========

/// Signatures for both MIME parts of a draft. Desktop clients put the
/// signature into the text/plain part as well as the HTML part; carrying it
/// only in HTML would make the text part distinguishable from a hand-written
/// draft (and trip divergence heuristics like our own `body_parts_diverge`).
pub(super) struct Signatures {
    pub html: String,
    pub text: String,
}

impl Signatures {
    /// Resolve the signature pair from config: explicit `signature_text`
    /// wins, otherwise a text rendering is derived from `signature_html`.
    pub fn resolve(signature_html: Option<&str>, signature_text: Option<&str>) -> Self {
        let html = signature_html.unwrap_or("");
        let text = signature_text.map_or_else(|| signature_text_from_html(html), str::to_string);
        Self {
            html: html.to_string(),
            text,
        }
    }

    /// `{body}\n{signature}` when a text signature exists, `{body}` otherwise.
    fn apply_plain(&self, body: &str) -> String {
        if self.text.is_empty() {
            body.to_string()
        } else {
            format!("{body}\n{}", self.text)
        }
    }
}

/// The Outlook-style plaintext quote header:
/// `Von: … / Gesendet: … / An: … / Betreff: …` followed by a blank line.
/// Desktop clients repeat exactly this block (not a `> `-quoted body with an
/// "On … wrote:" intro) in the text part of replies and forwards.
fn plain_quote_header(
    locale: Locale,
    from_display: &str,
    date_display: &str,
    to_display: &str,
    subject: &str,
) -> String {
    let labels = locale.quote_labels();
    format!(
        "{from_label}: {from_display}\n\
         {sent_label}: {date_display}\n\
         {to_label}: {to_display}\n\
         {subj_label}: {subject}",
        from_label = labels[0],
        sent_label = labels[1],
        to_label = labels[2],
        subj_label = labels[3],
    )
}

/// Build `(plain_body, html_body)` for a reply draft. Both parts follow the
/// Outlook format: the plaintext part repeats the From/Sent/To/Subject header
/// block above the unprefixed original text, the HTML part uses the Outlook
/// Web metablock.
pub(super) fn build_reply_bodies(
    original: &EmailFull,
    user_body: &str,
    locale: Locale,
    signatures: &Signatures,
    inline: &[InlineRef],
) -> (String, String) {
    let from_display = format_sender(original.from.as_ref());
    let date_display = format_date_outlook(original.date.as_deref(), locale);
    let to_display = format_recipients(&original.to);

    // Plaintext (Outlook style: no `> ` prefixes, header block instead)
    let quote_header = plain_quote_header(
        locale,
        &from_display,
        &date_display,
        &to_display,
        &original.subject,
    );
    let plain_body = format!(
        "{body}\n\n{quote_header}\n\n{original_text}",
        body = signatures.apply_plain(&apply_cid_markers_plain(user_body, inline)),
        original_text = original.body_text,
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
    let html_body = wrap_html_document(&format!(
        "{body}{sig}{appendonsend}{metablock}",
        body = body_div(
            &apply_cid_markers_html(&html_escape(user_body), inline),
            locale
        ),
        sig = signature_block(&signatures.html, locale),
        appendonsend = APPEND_ON_SEND,
    ));

    (plain_body, html_body)
}

/// Build `(plain_body, html_body)` for a forward draft. Same Outlook format
/// as replies — desktop clients do not distinguish the two in body layout.
pub(super) fn build_forward_bodies(
    original: &EmailFull,
    user_body: Option<&str>,
    locale: Locale,
    signatures: &Signatures,
    inline: &[InlineRef],
) -> (String, String) {
    let from_display = format_sender(original.from.as_ref());
    let date_display = format_date_outlook(original.date.as_deref(), locale);
    let to_display = format_recipients(&original.to);

    // Plaintext (Outlook style, same header block as replies)
    let quote_header = plain_quote_header(
        locale,
        &from_display,
        &date_display,
        &to_display,
        &original.subject,
    );
    let plain_body = format!(
        "{body}\n\n{quote_header}\n\n{original_text}",
        body = signatures.apply_plain(&apply_cid_markers_plain(user_body.unwrap_or(""), inline)),
        original_text = original.body_text,
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
        Some(msg) if !msg.is_empty() => apply_cid_markers_html(&html_escape(msg), inline),
        _ => "<br>".to_string(),
    };
    let html_body = wrap_html_document(&format!(
        "{body}{sig}{appendonsend}{metablock}",
        body = body_div(&body_html_content, locale),
        sig = signature_block(&signatures.html, locale),
        appendonsend = APPEND_ON_SEND,
    ));

    (plain_body, html_body)
}

/// Build `(plain_body, html_body)` for a fresh compose (no quote). The
/// signature goes into both parts, matching what desktop clients save.
pub(super) fn build_compose_bodies(
    body: &str,
    locale: Locale,
    signatures: &Signatures,
    inline: &[InlineRef],
) -> (String, String) {
    let plain_body = signatures.apply_plain(&apply_cid_markers_plain(body, inline));
    let html_body = wrap_html_document(&format!(
        "{body}{sig}",
        body = body_div(&apply_cid_markers_html(&html_escape(body), inline), locale),
        sig = signature_block(&signatures.html, locale),
    ));
    (plain_body, html_body)
}

/// Derive a plaintext signature from the configured HTML signature: block
/// tags become newlines, remaining tags are stripped, basic entities are
/// decoded, and runs of blank lines collapse. Good enough for signatures —
/// which are trusted config, not arbitrary mail content.
fn signature_text_from_html(html: &str) -> String {
    if html.is_empty() {
        return String::new();
    }
    // HTML source whitespace collapses the way a browser renders it: raw
    // newlines inside the markup are formatting of the *source*, not line
    // breaks. Without this, a signature whose markup wraps mid-sentence
    // produced broken lines and stray blank lines in the text part. Line
    // structure below comes exclusively from tags (`<br>`, closing blocks).
    let collapsed = html.split_whitespace().collect::<Vec<_>>().join(" ");

    let mut out = String::with_capacity(collapsed.len());
    let mut rest = collapsed.as_str();
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            // Unterminated tag: treat the remainder as text.
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let tag = after[..end].trim().to_ascii_lowercase();
        let name = tag
            .trim_start_matches('/')
            .split([' ', '\t', '/'])
            .next()
            .unwrap_or("");
        // Opening <br>, and closing block tags, produce line breaks.
        if name == "br" || (tag.starts_with('/') && matches!(name, "p" | "div" | "tr" | "li")) {
            out.push('\n');
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);

    // Decode the entities that realistically appear in signature markup.
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");

    // Trim each line and collapse blank-line runs. A line consisting of
    // `--` is the RFC 3676 signature separator and is restored to `-- `
    // (trailing space included) — that is how clients write it, and some
    // treat the space as significant when detecting the signature start.
    let mut lines: Vec<&str> = Vec::new();
    let mut blank_run = 0usize;
    for line in decoded.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        lines.push(trimmed);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    while lines.first().is_some_and(|l| l.is_empty()) {
        lines.remove(0);
    }
    lines
        .iter()
        .map(|l| if *l == "--" { "-- " } else { *l })
        .collect::<Vec<_>>()
        .join("\n")
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

/// Quoted original content for the HTML part: either sanitized original HTML
/// (embedded directly, the way desktop clients quote HTML mail) or escaped
/// plaintext (wrapped in the `BodyFragment`/`PlainText` structure desktop
/// clients use for text-only originals).
enum QuotedContent {
    Html(String),
    Plain(String),
}

/// Outlook Web quote-message block: hr separator + `divRplyFwdMsg` header
/// (with `<font>` wrapper) + the quoted original content.
fn quote_metablock_html(
    from_display: &str,
    sent: &str,
    to_display: &str,
    subject: &str,
    quoted_content: &QuotedContent,
    locale: Locale,
) -> String {
    let labels = locale.quote_labels();
    let quoted_block = match quoted_content {
        // Sanitized original HTML is embedded as-is — matching how desktop
        // clients splice the original HTML body below the header block.
        QuotedContent::Html(html) => format!("<div>\n{html}\n</div>\n"),
        QuotedContent::Plain(text) => format!(
            "<div class=\"BodyFragment\"><font size=\"2\"><span style=\"font-size:11pt;\">\n\
             <div class=\"PlainText\">{text}</div>\n\
             </span></font></div>\n"
        ),
    };
    format!(
        "<hr style=\"display:inline-block;width:98%\" tabindex=\"-1\"><div id=\"divRplyFwdMsg\" dir=\"ltr\"><font face=\"Calibri, sans-serif\" style=\"font-size:11pt\" color=\"#000000\">\
         <b>{l0}:</b> {from}<br>\n\
         <b>{l1}:</b> {sent}<br>\n\
         <b>{l2}:</b> {to}<br>\n\
         <b>{l3}:</b> {subj}</font>\n\
         <div>&nbsp;</div>\n\
         </div>\n\
         {quoted_block}",
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
/// **Security**: the original `body_html` is attacker-controlled — quoting it
/// verbatim would propagate `<script>` / `<iframe>` / `on*` handlers /
/// `javascript:` links into the user's outgoing draft. It therefore runs
/// through `ammonia` first: scripts, event handlers, dangerous URL schemes
/// and unknown tags are removed while formatting (links, tables, inline
/// styles, images) survives — so the quote looks like what the user saw,
/// the way desktop clients quote HTML mail.
///
/// When the original has no HTML part, the plaintext body is HTML-escaped
/// (line breaks preserved via `html_escape`'s `\n → <br>\n` rule).
fn prepare_quoted_content(body_html: Option<&str>, body_text: &str) -> QuotedContent {
    match body_html {
        Some(html) if !html.trim().is_empty() => QuotedContent::Html(sanitize_quoted_html(html)),
        _ => QuotedContent::Plain(html_escape(body_text)),
    }
}

/// Sanitize untrusted original HTML for embedding in a draft quote.
///
/// Beyond ammonia's defaults (which already remove scripts, event handlers
/// and unsafe URL schemes) this allows the presentational tags and
/// attributes that real HTML email is built from — `div`/`span`/`font`,
/// tables with layout attributes, inline `style` — so the quoted mail keeps
/// its appearance. `style` cannot execute script in any modern client; the
/// worst it can do is load remote background images, which mail clients
/// already gate behind their remote-content setting.
fn sanitize_quoted_html(html: &str) -> String {
    use std::collections::HashSet;
    let mut builder = ammonia::Builder::default();
    builder
        .add_tags(["div", "span", "font", "center", "u", "big", "small"])
        .add_generic_attributes([
            "style",
            "class",
            "dir",
            "align",
            "valign",
            "width",
            "height",
            "border",
            "cellpadding",
            "cellspacing",
            "bgcolor",
            "color",
            "face",
            "size",
            "lang",
        ])
        .url_schemes(HashSet::from(["http", "https", "mailto", "tel", "cid"]))
        // Desktop clients do not decorate quoted links with rel attributes;
        // adding them would mark the draft as machine-processed.
        .link_rel(None);
    builder.clean(html).to_string()
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

// ========== Inline image markers ==========

/// An inline image the body may reference as `![alt](cid:<id>)`.
pub(super) struct InlineRef<'a> {
    pub cid: &'a str,
    pub filename: &'a str,
}

/// One `![alt](cid:<id>)` occurrence: its byte range plus the parsed parts.
struct CidMarker {
    start: usize,
    end: usize,
    alt: String,
    cid: String,
}

/// Scan a body for `![alt](cid:<id>)` markers.
///
/// Hand-rolled rather than regex-based: the grammar is three fixed delimiters
/// and the crate carries no regex dependency. Deliberately strict — an id may
/// not be empty and may not contain whitespace, `<`, `>` or `)`. That way a
/// stray `![](cid:` in prose cannot swallow the rest of the message, and an id
/// can never break out of the `src="cid:…"` attribute it lands in.
///
/// Runs identically on the raw body and on the HTML-escaped one: escaping
/// rewrites `& < > "` and newlines, none of which appear in the delimiters or
/// in a valid id.
fn scan_cid_markers(body: &str) -> Vec<CidMarker> {
    const OPEN: &str = "![";
    const MID: &str = "](cid:";

    let mut out = Vec::new();
    let mut cursor = 0usize;

    while let Some(rel) = body[cursor..].find(OPEN) {
        let start = cursor + rel;
        let alt_start = start + OPEN.len();

        // Alt text runs to the first `]`. No `]` left at all means no further
        // marker can complete either, so stop rather than rescan.
        let Some(alt_rel) = body[alt_start..].find(']') else {
            break;
        };
        let alt_end = alt_start + alt_rel;

        if !body[alt_end..].starts_with(MID) {
            // `![…]` without the `(cid:` tail — ordinary text. Resume after the
            // opener so an overlapping marker later in the line is still found.
            cursor = alt_start;
            continue;
        }

        let id_start = alt_end + MID.len();
        let Some(id_rel) = body[id_start..].find(')') else {
            break;
        };
        let id_end = id_start + id_rel;
        let id = &body[id_start..id_end];

        if id.is_empty()
            || id
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | '"'))
        {
            // Resume right after this opener, NOT after the `)` that ended the
            // candidate: an unterminated `![](cid:` in prose reaches forward to
            // the next `)` anywhere in the mail, and skipping that far would
            // swallow every valid marker in between. Advancing past the opener
            // still guarantees progress.
            cursor = alt_start;
            continue;
        }

        out.push(CidMarker {
            start,
            end: id_end + 1,
            alt: body[alt_start..alt_end].to_string(),
            cid: id.to_string(),
        });
        cursor = id_end + 1;
    }

    out
}

/// The content ids the body references, in order of first appearance and
/// without duplicates. Used by the caller to verify that every marker has a
/// matching inline attachment before a draft is built.
pub(super) fn collect_cid_markers(body: &str) -> Vec<String> {
    let mut seen = Vec::new();
    for marker in scan_cid_markers(body) {
        if !seen.contains(&marker.cid) {
            seen.push(marker.cid);
        }
    }
    seen
}

/// Replace markers with `<img src="cid:…">` tags.
///
/// The input must ALREADY be HTML-escaped — the alt text is copied through
/// verbatim into the attribute, so escaping has to happen before this runs,
/// never after (afterwards would escape the generated tag itself).
///
/// `max-width` keeps a phone screenshot from blowing up the mail layout;
/// `alt` falls back to the file name so the image still announces itself in
/// clients that block remote content or in screen readers.
pub(super) fn apply_cid_markers_html(escaped: &str, refs: &[InlineRef]) -> String {
    let markers = scan_cid_markers(escaped);
    if markers.is_empty() {
        return escaped.to_string();
    }

    let mut out = String::with_capacity(escaped.len() + markers.len() * 64);
    let mut last = 0usize;
    for marker in markers {
        let Some(found) = refs.iter().find(|r| r.cid == marker.cid) else {
            // Unknown id: leave the marker untouched. The caller rejects this
            // case up front; keeping the text verbatim here means a future
            // caller that skips validation degrades to visible text rather
            // than to a broken image icon.
            continue;
        };
        out.push_str(&escaped[last..marker.start]);
        let alt = if marker.alt.is_empty() {
            html_escape(found.filename)
        } else {
            marker.alt.clone()
        };
        // Built by pushes rather than `format!` into the buffer: the tag is
        // assembled once per image and this avoids the intermediate String.
        out.push_str("<img src=\"cid:");
        out.push_str(found.cid);
        out.push_str("\" alt=\"");
        out.push_str(&alt);
        out.push_str("\" style=\"max-width:100%; height:auto;\">");
        last = marker.end;
    }
    out.push_str(&escaped[last..]);
    out
}

/// Replace markers with a readable placeholder for the plaintext part, so a
/// text-only reader learns that an image sits at this position instead of
/// seeing raw markup.
pub(super) fn apply_cid_markers_plain(body: &str, refs: &[InlineRef]) -> String {
    let markers = scan_cid_markers(body);
    if markers.is_empty() {
        return body.to_string();
    }

    let mut out = String::with_capacity(body.len());
    let mut last = 0usize;
    for marker in markers {
        let Some(found) = refs.iter().find(|r| r.cid == marker.cid) else {
            continue;
        };
        out.push_str(&body[last..marker.start]);
        let label = if marker.alt.is_empty() {
            found.filename
        } else {
            marker.alt.as_str()
        };
        out.push('[');
        out.push_str(label);
        out.push(']');
        last = marker.end;
    }
    out.push_str(&body[last..]);
    out
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

    fn quoted_as_html(q: &QuotedContent) -> &str {
        match q {
            QuotedContent::Html(s) => s,
            QuotedContent::Plain(_) => panic!("expected HTML quote path"),
        }
    }

    #[test]
    fn prepare_quoted_content_sanitizes_html_keeps_formatting() {
        // Formatting survives sanitization; script payloads do not.
        let html = "<div>hi <b>bold</b><script>alert(1)</script></div>";
        let quoted = prepare_quoted_content(Some(html), "hi bold");
        let out = quoted_as_html(&quoted);
        assert!(!out.contains("script"), "script must be stripped: {out}");
        assert!(!out.contains("alert(1)"), "script body must go too: {out}");
        assert!(
            out.contains("<b>bold</b>"),
            "formatting must survive: {out}"
        );
    }

    #[test]
    fn prepare_quoted_content_strips_event_handlers_and_js_urls() {
        let html = r#"<div onclick="alert(1)"><a href="javascript:alert(2)">x</a></div>"#;
        let quoted = prepare_quoted_content(Some(html), "x");
        let out = quoted_as_html(&quoted);
        assert!(!out.contains("onclick"), "event handler survived: {out}");
        assert!(!out.contains("javascript:"), "js URL survived: {out}");
    }

    #[test]
    fn prepare_quoted_content_keeps_inline_styles_without_link_rel() {
        let html = r#"<div style="color: red"><a href="https://example.com">link</a></div>"#;
        let quoted = prepare_quoted_content(Some(html), "link");
        let out = quoted_as_html(&quoted);
        assert!(out.contains("style=\"color: red\""), "style dropped: {out}");
        assert!(!out.contains("rel="), "rel decoration added: {out}");
        assert!(out.contains("https://example.com"), "href dropped: {out}");
    }

    #[test]
    fn prepare_quoted_content_falls_back_to_escaped_text() {
        let quoted = prepare_quoted_content(None, "a<b\nnext");
        match quoted {
            QuotedContent::Plain(s) => assert_eq!(s, "a&lt;b<br>\nnext"),
            QuotedContent::Html(_) => panic!("expected plaintext quote path"),
        }
    }

    #[test]
    fn signature_text_from_html_strips_tags_and_decodes_entities() {
        let html = "<p>--&nbsp;</p><p>Example Corp &amp; Co<br>Line two</p>";
        assert_eq!(
            signature_text_from_html(html),
            "-- \nExample Corp & Co\nLine two"
        );
    }

    #[test]
    fn signature_text_from_html_collapses_source_newlines() {
        // Raw newlines inside the markup are source formatting, not line
        // breaks — only tags create lines. This exact shape (a phone number
        // wrapped mid-line in the HTML source) previously broke the text
        // signature apart.
        let html = "<p><span>Phone:\r\n +49 40 0000</span><span>&nbsp;<br></span><span>Line two \r\n</span></p>";
        assert_eq!(
            signature_text_from_html(html),
            "Phone: +49 40 0000\nLine two"
        );
    }

    #[test]
    fn signature_text_from_html_collapses_blank_runs() {
        let html = "<div>top</div><div><br></div><div><br></div><div>bottom</div>";
        let text = signature_text_from_html(html);
        assert!(!text.contains("\n\n\n"), "blank run survived: {text:?}");
        assert!(text.starts_with("top"));
        assert!(text.ends_with("bottom"));
    }

    #[test]
    fn signatures_resolve_prefers_explicit_text() {
        let sigs = Signatures::resolve(Some("<p>HTML sig</p>"), Some("custom text sig"));
        assert_eq!(sigs.text, "custom text sig");
        assert_eq!(sigs.html, "<p>HTML sig</p>");
    }

    #[test]
    fn reply_plaintext_uses_outlook_header_block_and_signature() {
        let mut original = EmailFull {
            uid: 1,
            folder: "INBOX".to_string(),
            from: Some(EmailAddress {
                name: Some("Alice".to_string()),
                address: "alice@example.com".to_string(),
            }),
            to: vec![EmailAddress {
                name: None,
                address: "me@example.com".to_string(),
            }],
            cc: vec![],
            subject: "Hello".to_string(),
            date: Some("2026-07-30T21:18:00+02:00".to_string()),
            message_id: None,
            in_reply_to: None,
            references: vec![],
            flags: vec![],
            body_text: "original text".to_string(),
            body_html: None,
            attachments: vec![],
            body_parts_diverge: false,
        };
        original.subject = "Hello".to_string();
        let sigs = Signatures::resolve(Some("<p>-- </p><p>Sig line</p>"), None);
        let (plain, html) = build_reply_bodies(&original, "my reply", Locale::De, &sigs, &[]);

        // Outlook text part: body, signature, blank line, Von/Gesendet/An/
        // Betreff block, blank line, unprefixed original text.
        assert!(plain.starts_with("my reply\n-- \nSig line\n\nVon: Alice <alice@example.com>\n"));
        assert!(plain.contains("\nGesendet: Donnerstag, 30. Juli 2026 21:18\n"));
        assert!(plain.contains("\nAn: me@example.com <me@example.com>\n"));
        assert!(plain.contains("\nBetreff: Hello\n\noriginal text"));
        assert!(!plain.contains("> original"), "no > prefixes: {plain}");
        assert!(!plain.contains("schrieb"), "no legacy intro line: {plain}");
        // HTML part still carries the signature block.
        assert!(html.contains("id=\"Signature\""));
    }

    // ===== inline image markers =====

    #[test]
    fn cid_markers_are_collected_in_order_without_duplicates() {
        let body = "one ![](cid:a) two ![alt](cid:b) three ![](cid:a)";
        assert_eq!(collect_cid_markers(body), vec!["a", "b"]);
    }

    #[test]
    fn cid_marker_scan_rejects_malformed_ids() {
        // Empty id, whitespace, angle brackets and quotes would all let an id
        // break out of `src="cid:…"`, so none of them may parse.
        for body in [
            "![](cid:)",
            "![](cid:a b)",
            "![](cid:a<b)",
            "![](cid:a>b)",
            "![](cid:a\"b)",
        ] {
            assert!(
                collect_cid_markers(body).is_empty(),
                "should not parse as a marker: {body}"
            );
        }
    }

    #[test]
    fn cid_marker_scan_ignores_plain_markdown_and_prose() {
        // A normal markdown image, a bare bracket pair, and an unterminated
        // marker must all survive as text.
        assert!(collect_cid_markers("![shot](https://example.com/a.png)").is_empty());
        assert!(collect_cid_markers("costs ![] and more").is_empty());
        assert!(collect_cid_markers("![](cid:unterminated").is_empty());
        // Prose containing the opener must not swallow the rest of the mail.
        let body = "see ![](cid: and then a real one ![](cid:real)";
        assert_eq!(collect_cid_markers(body), vec!["real"]);
    }

    #[test]
    fn markers_survive_html_escaping_unchanged() {
        // The HTML pass runs on escaped text, so escaping must not disturb the
        // delimiters — otherwise the marker would never be found there.
        let escaped = html_escape("text ![](cid:shot) more");
        assert_eq!(collect_cid_markers(&escaped), vec!["shot"]);
    }

    #[test]
    fn html_marker_becomes_img_tag_with_filename_alt() {
        let refs = [InlineRef {
            cid: "shot",
            filename: "screenshot.png",
        }];
        let out = apply_cid_markers_html(&html_escape("before ![](cid:shot) after"), &refs);
        assert!(out.contains("<img src=\"cid:shot\""), "{out}");
        assert!(out.contains("alt=\"screenshot.png\""), "{out}");
        assert!(out.contains("max-width:100%"), "{out}");
        assert!(out.starts_with("before "), "{out}");
        assert!(out.ends_with(" after"), "{out}");
    }

    #[test]
    fn html_marker_keeps_author_alt_text_escaped() {
        let refs = [InlineRef {
            cid: "x",
            filename: "f.png",
        }];
        // The alt text is escaped by the earlier html_escape pass; the tag
        // must carry that escaped form, never the raw one.
        let out = apply_cid_markers_html(&html_escape("![a<b](cid:x)"), &refs);
        assert!(out.contains("alt=\"a&lt;b\""), "{out}");
        assert!(!out.contains("alt=\"a<b\""), "{out}");
    }

    #[test]
    fn plain_marker_becomes_readable_placeholder() {
        let refs = [InlineRef {
            cid: "shot",
            filename: "screenshot.png",
        }];
        assert_eq!(
            apply_cid_markers_plain("before ![](cid:shot) after", &refs),
            "before [screenshot.png] after"
        );
        // An author-supplied alt wins over the file name.
        assert_eq!(
            apply_cid_markers_plain("![Rollenübersicht](cid:shot)", &refs),
            "[Rollenübersicht]"
        );
    }

    #[test]
    fn unknown_cid_is_left_verbatim_in_both_parts() {
        // Callers reject this case up front; if one ever forgets, degrading to
        // visible text beats shipping a broken image.
        let refs = [InlineRef {
            cid: "known",
            filename: "k.png",
        }];
        assert_eq!(
            apply_cid_markers_plain("x ![](cid:other) y", &refs),
            "x ![](cid:other) y"
        );
        assert!(apply_cid_markers_html("x ![](cid:other) y", &refs).contains("![](cid:other)"));
    }

    #[test]
    fn bodies_without_markers_are_untouched() {
        let refs = [InlineRef {
            cid: "a",
            filename: "a.png",
        }];
        let body = "just text, no markers at all";
        assert_eq!(apply_cid_markers_plain(body, &refs), body);
        assert_eq!(apply_cid_markers_html(body, &refs), body);
    }

    #[test]
    fn multiple_markers_all_replaced() {
        let refs = [
            InlineRef {
                cid: "a",
                filename: "a.png",
            },
            InlineRef {
                cid: "b",
                filename: "b.png",
            },
        ];
        let out = apply_cid_markers_plain("![](cid:a) middle ![](cid:b)", &refs);
        assert_eq!(out, "[a.png] middle [b.png]");
    }
}
