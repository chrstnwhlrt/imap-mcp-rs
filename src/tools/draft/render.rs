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
        body = body_div(&render_body_html(user_body, inline), locale),
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
        Some(msg) if !msg.is_empty() => render_body_html(msg, inline),
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
        body = body_div(&render_body_html(body, inline), locale),
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

/// Format a date for the Outlook-style quote header, in the READER's (this
/// machine's) timezone — desktop clients render the quoted mail's time in
/// the local zone, not the sender's. The input is the UTC-normalized `date`
/// (offset forms parse too); unparseable input is returned verbatim, since
/// wrong-looking beats absent.
/// EN: "Tuesday, March 24, 2026 1:56:47 PM" (12h with seconds)
/// DE: "Dienstag, 24. März 2026 13:56" (24h, no seconds)
fn format_date_outlook(iso: Option<&str>, locale: Locale) -> String {
    format_date_outlook_in(iso, locale, &jiff::tz::TimeZone::system())
}

/// [`format_date_outlook`] with an explicit zone, so tests are not hostage
/// to the machine's timezone.
fn format_date_outlook_in(iso: Option<&str>, locale: Locale, tz: &jiff::tz::TimeZone) -> String {
    let Some(iso) = iso else {
        return locale.unknown_date().to_string();
    };
    let norm = iso
        .strip_suffix('Z')
        .map_or_else(|| iso.to_string(), |stem| format!("{stem}+00:00"));
    let Ok(ts) = jiff::Timestamp::strptime("%Y-%m-%dT%H:%M:%S%:z", &norm) else {
        return iso.to_string();
    };
    let z = ts.to_zoned(tz.clone());
    let year = i32::from(z.year());
    let month = u32::try_from(z.month()).unwrap_or(1);
    let day = u32::try_from(z.day()).unwrap_or(1);
    let (hour, minute, second) = (z.hour(), z.minute(), z.second());
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
            let month_name = MONTHS[(month - 1) as usize];
            let weekday = WEEKDAYS[weekday_idx];
            let (h12, ampm) = match hour {
                0 => (12, "AM"),
                1..=11 => (hour, "AM"),
                12 => (12, "PM"),
                _ => (hour - 12, "PM"),
            };
            format!("{weekday}, {month_name} {day}, {year} {h12}:{minute:02}:{second:02} {ampm}")
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
            let month_name = MONTHS[(month - 1) as usize];
            let weekday = WEEKDAYS[weekday_idx];
            format!("{weekday}, {day}. {month_name} {year} {hour:02}:{minute:02}")
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
    /// The id as it appears in body markers — the user-facing handle.
    pub cid: &'a str,
    /// The globally unique value written into the part's `Content-ID` header
    /// and the `src="cid:…"` referencing it. See `read_attachments` for why
    /// this differs from `cid`.
    pub content_id: &'a str,
    pub filename: &'a str,
}

/// One `![alt](cid:<id>)` occurrence: its byte range plus the parsed parts.
struct CidMarker {
    start: usize,
    end: usize,
    alt: String,
    cid: String,
}

/// Longest accepted id inside a `(cid:<id>)` marker. Real ids are short
/// file-name stems; anything longer is prose that happens to contain the
/// delimiters. The cap also bounds the per-candidate validation work.
pub(super) const MAX_MARKER_CID_BYTES: usize = 128;

/// Longest accepted alt text, which must also stay on one line. An alt is a
/// one-phrase description; without the bound, a stray `![` in prose followed
/// much later by `](cid:<id>)` would swallow everything in between into an
/// `alt` attribute — whole paragraphs silently vanishing from the visible
/// HTML body.
pub(super) const MAX_MARKER_ALT_BYTES: usize = 300;

/// Forward-only cached byte finder: `at_or_after(from)` returns the first
/// position at or past `from` holding one of `needles`.
///
/// Queries must arrive with non-decreasing `from`; the cache then never
/// re-reads a byte, so all lookups over one scan cost O(n) *together*. This
/// is what keeps [`scan_cid_markers`] linear on adversarial input: a body of
/// repeated `![` sharing one distant `]` made the previous per-candidate
/// `find` rescan the same span every time — O(n²), minutes of CPU inside the
/// 10 MiB body cap, on the async worker thread.
struct NextByte<'a> {
    haystack: &'a [u8],
    needles: &'static [u8],
    cached: NextByteCache,
}

/// Search state of a [`NextByte`]. `Exhausted` is final: `from` only grows,
/// so once a search ran off the end no later query can hit either.
enum NextByteCache {
    Unqueried,
    Exhausted,
    /// Hit at this position — valid for every `from <= position`.
    Hit(usize),
}

impl<'a> NextByte<'a> {
    const fn new(haystack: &'a [u8], needles: &'static [u8]) -> Self {
        Self {
            haystack,
            needles,
            cached: NextByteCache::Unqueried,
        }
    }

    fn at_or_after(&mut self, from: usize) -> Option<usize> {
        match self.cached {
            NextByteCache::Exhausted => None,
            NextByteCache::Hit(p) if p >= from => Some(p),
            NextByteCache::Unqueried | NextByteCache::Hit(_) => {
                let start = from.min(self.haystack.len());
                let found = self.haystack[start..]
                    .iter()
                    .position(|b| self.needles.contains(b))
                    .map(|i| start + i);
                self.cached = found.map_or(NextByteCache::Exhausted, NextByteCache::Hit);
                found
            }
        }
    }
}

/// Middle delimiter of a marker. Shared between the scanner and the stray-
/// fragment detection in [`inspect_markers`] — as two literals, a grammar
/// change touching one but not the other would silently stop flagging
/// malformed markers.
const MID: &str = "](cid:";

/// Scan a body for `![alt](cid:<id>)` markers.
///
/// Hand-rolled rather than regex-based: the grammar is three fixed delimiters
/// and the crate carries no regex dependency. Deliberately strict:
///
/// - The id must satisfy [`super::is_valid_cid`] — the exact rule an
///   attachment's `cid` is held to (alphabet, length, dot placement), so the
///   scanner can never accept a reference no attachment could carry. The
///   accepted characters are also invariant under [`html_escape`], which
///   closes the gap where validation (on the raw body) and rendering
///   (previously on the escaped one) could disagree about what is a marker.
/// - The alt text must stay on one line and within
///   [`MAX_MARKER_ALT_BYTES`] — see there.
///
/// Candidates the grammar rejects stay ordinary text for *this* function;
/// [`inspect_markers`] reports them so validation can refuse (or warn)
/// loudly instead of saving a draft with visible marker source.
fn scan_cid_markers(body: &str) -> Vec<CidMarker> {
    const OPEN: &str = "![";

    let bytes = body.as_bytes();
    // All positions these return are ASCII bytes, hence char boundaries.
    let mut next_rbracket = NextByte::new(bytes, b"]");
    let mut next_paren = NextByte::new(bytes, b")");
    let mut next_newline = NextByte::new(bytes, b"\r\n");

    let mut out = Vec::new();
    let mut cursor = 0usize;

    while let Some(rel) = body[cursor..].find(OPEN) {
        let start = cursor + rel;
        let alt_start = start + OPEN.len();

        // Alt text runs to the first `]`. No `]` left at all means no further
        // marker can complete either, so stop rather than rescan.
        let Some(alt_end) = next_rbracket.at_or_after(alt_start) else {
            break;
        };

        // Oversized or multi-line alt: prose, not a marker. Resume right
        // after the opener (see the id branch below for why not further).
        if alt_end - alt_start > MAX_MARKER_ALT_BYTES
            || next_newline
                .at_or_after(alt_start)
                .is_some_and(|nl| nl < alt_end)
        {
            cursor = alt_start;
            continue;
        }

        if !body[alt_end..].starts_with(MID) {
            // `![…]` without the `(cid:` tail — ordinary text. Resume after the
            // opener so an overlapping marker later in the line is still found.
            cursor = alt_start;
            continue;
        }

        let id_start = alt_end + MID.len();
        let Some(id_end) = next_paren.at_or_after(id_start) else {
            break;
        };
        let id = &body[id_start..id_end];

        // `is_valid_cid` checks length before content, so a slice reaching a
        // `)` far down the mail costs O(1), not a scan of everything between.
        if !super::is_valid_cid(id) {
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

/// Everything validation needs to know about a body's markers, from ONE scan.
pub(super) struct MarkerInspection {
    /// The referenced ids, in order of first appearance, deduplicated.
    pub unique_ids: Vec<String>,
    /// The (capped) line around the first `](cid:` occurrence that is not
    /// part of any accepted marker, when one exists.
    pub stray_fragment: Option<String>,
}

/// Inspect a body's markers for validation: collect the referenced ids and
/// detect malformed marker attempts.
///
/// The scanner's grammar is strict, and a rejected candidate simply stays
/// text. For a caller that *meant* to place an image — an id with spaces, an
/// alt spanning lines — that silence is the worst outcome: the draft would
/// be saved showing raw marker source, with nothing pointing at the cause.
/// The `](cid:` sequence does not occur in prose, so surfacing a stray one
/// makes every malformed marker loud.
///
/// Complexity notes, both learned the hard way in review:
/// - id dedup goes through a `HashSet`; a `Vec::contains` walk was O(u²) in
///   distinct ids and reachable with zero attachments.
/// - the stray check walks occurrences and markers with two pointers —
///   both are position-sorted, so it is O(n). Re-testing every occurrence
///   against the whole marker list was O(k²): ~7 minutes of blocking CPU
///   for a 10 MiB body of repeated valid markers, on the SUCCESS path.
pub(super) fn inspect_markers(body: &str) -> MarkerInspection {
    use std::collections::HashSet;

    let markers = scan_cid_markers(body);

    let mut seen: HashSet<&str> = HashSet::with_capacity(markers.len().min(64));
    let mut unique_ids = Vec::new();
    for marker in &markers {
        if seen.insert(&marker.cid) {
            unique_ids.push(marker.cid.clone());
        }
    }

    let mut stray_fragment = None;
    let mut from = 0usize;
    let mut mi = 0usize; // markers are position-sorted; advances only forward
    while let Some(rel) = body[from..].find(MID) {
        let pos = from + rel;
        while mi < markers.len() && markers[mi].end <= pos {
            mi += 1;
        }
        let inside = mi < markers.len() && pos >= markers[mi].start;
        if !inside {
            let line_start = body[..pos].rfind(['\n', '\r']).map_or(0, |i| i + 1);
            let line_end = body[pos..]
                .find(['\n', '\r'])
                .map_or(body.len(), |i| pos + i);
            stray_fragment = Some(body[line_start..line_end].chars().take(120).collect());
            break;
        }
        from = pos + MID.len();
    }

    MarkerInspection {
        unique_ids,
        stray_fragment,
    }
}

/// Render a raw body to HTML: replace markers with `<img src="cid:…">` tags
/// and HTML-escape everything around them.
///
/// Scans the RAW body — the same text the caller validated markers on — and
/// escapes the segments between markers afterwards. Scanning an
/// already-escaped body instead (the previous design) let the two passes see
/// different marker sets whenever escaping rewrote a character the grammar
/// cares about, and a marker only one side accepted was either silently
/// dropped or saved as visible source text. One scan, one truth.
///
/// `max-width` keeps a phone screenshot from blowing up the mail layout;
/// `alt` falls back to the file name so the image still announces itself in
/// clients that block remote content or in screen readers. The alt text is
/// single-line by grammar, so escaping it can never introduce a `<br>` into
/// the attribute. `src` carries the part's globally unique Content-ID, not
/// the user-facing marker id — see `read_attachments`.
pub(super) fn render_body_html(raw: &str, refs: &[InlineRef]) -> String {
    let markers = scan_cid_markers(raw);
    if markers.is_empty() {
        return html_escape(raw);
    }
    // Map lookup, not a linear `find` per marker: repeated markers are
    // unbounded (the same image may legitimately appear many times), and
    // markers × refs comparisons would grow quadratic-ish with them.
    let by_cid: std::collections::HashMap<&str, &InlineRef> =
        refs.iter().map(|r| (r.cid, r)).collect();

    let mut out = String::with_capacity(raw.len() + markers.len() * 64);
    let mut last = 0usize;
    for marker in markers {
        let Some(found) = by_cid.get(marker.cid.as_str()) else {
            // Unknown id: leave the marker untouched. The caller rejects this
            // case up front; keeping the text verbatim here means a future
            // caller that skips validation degrades to visible text rather
            // than to a broken image icon.
            continue;
        };
        out.push_str(&html_escape(&raw[last..marker.start]));
        let alt = if marker.alt.is_empty() {
            found.filename
        } else {
            marker.alt.as_str()
        };
        // Built by pushes rather than `format!` into the buffer: the tag is
        // assembled once per image and this avoids the intermediate String.
        out.push_str("<img src=\"cid:");
        out.push_str(&html_escape(found.content_id));
        out.push_str("\" alt=\"");
        out.push_str(&html_escape(alt));
        out.push_str("\" style=\"max-width:100%; height:auto;\">");
        last = marker.end;
    }
    out.push_str(&html_escape(&raw[last..]));
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
    // See render_body_html for why this is a map, not a per-marker `find`.
    let by_cid: std::collections::HashMap<&str, &InlineRef> =
        refs.iter().map(|r| (r.cid, r)).collect();

    let mut out = String::with_capacity(body.len());
    let mut last = 0usize;
    for marker in markers {
        let Some(found) = by_cid.get(marker.cid.as_str()) else {
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

    /// Fixed +02:00 — Berlin summer time as a constant offset, so the tests
    /// need no IANA tzdb (the Nix build sandbox has none).
    fn cest() -> jiff::tz::TimeZone {
        jiff::tz::TimeZone::fixed(jiff::tz::Offset::constant(2))
    }

    #[test]
    fn format_date_outlook_known_iso_en() {
        let r = format_date_outlook_in(
            Some("2026-04-19T13:30:45Z"),
            Locale::En,
            &jiff::tz::TimeZone::UTC,
        );
        assert!(r.starts_with("Sunday, April 19, 2026"), "{r}");
        assert!(r.contains("1:30:45 PM"), "{r}");
    }

    #[test]
    fn format_date_outlook_known_iso_de() {
        let r = format_date_outlook_in(
            Some("2026-04-19T13:30:45Z"),
            Locale::De,
            &jiff::tz::TimeZone::UTC,
        );
        assert!(r.starts_with("Sonntag, 19. April 2026"), "{r}");
        assert!(r.contains("13:30"), "{r}");
    }

    /// The quote header shows the reader's clock, the way desktop clients
    /// render it — a UTC instant appears as local wall time, and an instant
    /// late in the UTC day rolls into the reader's next calendar day.
    #[test]
    fn format_date_outlook_renders_in_the_readers_zone() {
        let reader = cest();
        let r = format_date_outlook_in(Some("2026-04-19T13:30:45Z"), Locale::De, &reader);
        assert!(r.contains("15:30"), "reader zone is UTC+2: {r}");

        // 23:30Z on the 19th is already the 20th at +02:00 — day AND
        // weekday must roll, not just the hour.
        let r = format_date_outlook_in(Some("2026-04-19T23:30:00Z"), Locale::De, &reader);
        assert!(r.starts_with("Montag, 20. April 2026"), "{r}");

        // Sender-offset forms (date_original era inputs) parse too.
        let r = format_date_outlook_in(
            Some("2026-04-19T13:30:45+02:00"),
            Locale::De,
            &jiff::tz::TimeZone::UTC,
        );
        assert!(r.contains("11:30"), "{r}");
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
            date_original: None,
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
        // The exact wall time depends on the machine's zone (that is the
        // point — the reader's clock); format correctness is pinned by the
        // fixed-zone tests above. Here: formatted, not passed through raw.
        assert!(plain.contains(". Juli 2026 "), "{plain}");
        assert!(!plain.contains("2026-07-30T"), "raw ISO leaked: {plain}");
        assert!(plain.contains("\nAn: me@example.com <me@example.com>\n"));
        assert!(plain.contains("\nBetreff: Hello\n\noriginal text"));
        assert!(!plain.contains("> original"), "no > prefixes: {plain}");
        assert!(!plain.contains("schrieb"), "no legacy intro line: {plain}");
        // HTML part still carries the signature block.
        assert!(html.contains("id=\"Signature\""));
    }

    // ===== inline image markers =====

    /// Test shims over [`inspect_markers`], keeping the assertions phrased
    /// in terms of the two questions it answers.
    fn collect_cid_markers(body: &str) -> Vec<String> {
        inspect_markers(body).unique_ids
    }
    fn find_unparsed_cid_fragment(body: &str) -> Option<String> {
        inspect_markers(body).stray_fragment
    }

    /// Test refs with a fixed, recognizable wire id per marker id.
    fn make_ref(cid: &'static str, filename: &'static str) -> InlineRef<'static> {
        // Leak is fine in tests; keeps InlineRef borrowing simple.
        let content_id: &'static str =
            Box::leak(format!("{cid}.fixed0@unit.invalid").into_boxed_str());
        InlineRef {
            cid,
            content_id,
            filename,
        }
    }

    #[test]
    fn cid_markers_are_collected_in_order_without_duplicates() {
        let body = "one ![](cid:a) two ![alt](cid:b) three ![](cid:a)";
        assert_eq!(collect_cid_markers(body), vec!["a", "b"]);
    }

    #[test]
    fn cid_marker_scan_rejects_malformed_ids() {
        // The id alphabet is exactly what an attachment's `cid` may carry
        // (letters, digits, `.`, `_`, `-`): nothing else can ever resolve,
        // and every accepted id is invariant under HTML escaping.
        let long_id = format!("![](cid:{})", "a".repeat(MAX_MARKER_CID_BYTES + 1));
        for body in [
            "![](cid:)",
            "![](cid:a b)",
            "![](cid:a<b)",
            "![](cid:a>b)",
            "![](cid:a\"b)",
            "![](cid:a&b)",
            "![](cid:a/b)",
            // Dot-atom rules, shared with attachment cids: an id the
            // Content-ID local part cannot legally carry never parses.
            "![](cid:.a)",
            "![](cid:a.)",
            "![](cid:a..b)",
            long_id.as_str(),
        ] {
            assert!(
                collect_cid_markers(body).is_empty(),
                "should not parse as a marker: {body}"
            );
        }
        // At the cap it still parses.
        let max_id = "a".repeat(MAX_MARKER_CID_BYTES);
        assert_eq!(
            collect_cid_markers(&format!("![](cid:{max_id})")),
            vec![max_id]
        );
    }

    #[test]
    fn cid_marker_alt_must_stay_on_one_line_and_bounded() {
        // A `![` in prose followed paragraphs later by `](cid:x)` must not
        // swallow the text in between into an alt attribute.
        assert!(collect_cid_markers("Na toll![\nZeile zwei](cid:x)").is_empty());
        assert!(collect_cid_markers("a![\r\nb](cid:x)").is_empty());
        let oversized = format!("![{}](cid:x)", "a".repeat(MAX_MARKER_ALT_BYTES + 1));
        assert!(collect_cid_markers(&oversized).is_empty());
        // At the cap, and with arbitrary single-line prose, it parses.
        let max_alt = format!("![{}](cid:x)", "ä".repeat(MAX_MARKER_ALT_BYTES / 2));
        assert_eq!(collect_cid_markers(&max_alt), vec!["x"]);
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

    /// Adversarial-shape regression: many openers sharing one distant `]`
    /// made the scan quadratic before the forward-cached byte finder. At
    /// O(n²) this input costs ~10¹⁰ steps and times the test out; linear, it
    /// finishes in microseconds.
    #[test]
    fn cid_marker_scan_stays_linear_on_pathological_input() {
        let mut body = "![".repeat(100_000);
        body.push_str("end ] and one real ![x](cid:ok)");
        assert_eq!(collect_cid_markers(&body), vec!["ok"]);

        // Same shape for the `)` lookup: many complete-looking prefixes, one
        // far parenthesis.
        let mut body = "![a](cid:x ".repeat(50_000);
        body.push(')');
        assert!(collect_cid_markers(&body).is_empty());
    }

    /// The SECOND pair of quadratics, found in review after the first was
    /// fixed: (a) the stray-fragment check re-tested every `](cid:`
    /// occurrence against the whole marker list — O(k²) on the SUCCESS path
    /// of a body with many repeated valid markers (~7 min CPU at 10 MiB);
    /// (b) id dedup via `Vec::contains` was O(u²) in distinct ids, reachable
    /// with zero attachments. Both shapes at this size finish in
    /// milliseconds linear and would time the test out quadratic.
    #[test]
    fn marker_inspection_stays_linear_on_repeated_and_distinct_markers() {
        // (a) 150k repetitions of one valid marker: two-pointer walk.
        let repeated = "![](cid:a)".repeat(150_000);
        let inspection = inspect_markers(&repeated);
        assert_eq!(inspection.unique_ids, vec!["a"]);
        assert!(inspection.stray_fragment.is_none());

        // (b) 100k distinct ids: HashSet dedup.
        let mut distinct = String::with_capacity(2_400_000);
        for i in 0..100_000 {
            use std::fmt::Write;
            let _ = write!(distinct, "![](cid:id{i})");
        }
        let inspection = inspect_markers(&distinct);
        assert_eq!(inspection.unique_ids.len(), 100_000);
        assert!(inspection.stray_fragment.is_none());

        // Two-pointer correctness at the edge: a stray AFTER many valid
        // markers is still found.
        let mut tail_stray = "![](cid:a)".repeat(1_000);
        tail_stray.push_str(" ![x](cid:bad id)");
        assert!(inspect_markers(&tail_stray).stray_fragment.is_some());
    }

    #[test]
    fn find_unparsed_cid_fragment_reports_rejected_candidates() {
        // Space in the id — the README's screenshot-name trap.
        let frag = find_unparsed_cid_fragment("see ![x](cid:Bildschirmfoto 2026-08-14) here")
            .expect("must be reported");
        assert!(frag.contains("Bildschirmfoto"), "{frag}");

        // Multi-line alt: rejected candidate, must be reported too.
        assert!(find_unparsed_cid_fragment("a![\nb](cid:x)").is_some());

        // A valid marker is not a stray fragment; neither is marker-free text.
        assert!(find_unparsed_cid_fragment("ok ![x](cid:shot) done").is_none());
        assert!(find_unparsed_cid_fragment("no markers at all").is_none());

        // Valid marker plus a stray fragment: the stray is still found.
        assert!(find_unparsed_cid_fragment("![x](cid:ok) and ![y](cid:bad id)").is_some());
    }

    #[test]
    fn html_marker_becomes_img_tag_with_unique_content_id() {
        let refs = [make_ref("shot", "screenshot.png")];
        let out = render_body_html("before ![](cid:shot) after", &refs);
        // src references the globally unique wire id, never the bare marker
        // id — a bare `cid:shot` would repeat across drafts.
        assert!(
            out.contains("<img src=\"cid:shot.fixed0@unit.invalid\""),
            "{out}"
        );
        assert!(!out.contains("src=\"cid:shot\""), "{out}");
        assert!(out.contains("alt=\"screenshot.png\""), "{out}");
        assert!(out.contains("max-width:100%"), "{out}");
        assert!(out.starts_with("before "), "{out}");
        assert!(out.ends_with(" after"), "{out}");
    }

    #[test]
    fn html_rendering_escapes_text_and_alt_around_markers() {
        let refs = [make_ref("x", "f.png")];
        // The scan runs on the raw body; the surrounding text and the alt are
        // escaped afterwards. Scanning escaped text instead let validation
        // and rendering disagree about the marker set.
        let out = render_body_html("a<b ![c<d](cid:x) e\"f", &refs);
        assert!(out.starts_with("a&lt;b "), "{out}");
        assert!(out.contains("alt=\"c&lt;d\""), "{out}");
        assert!(out.ends_with(" e&quot;f"), "{out}");
        assert!(!out.contains("alt=\"c<d\""), "{out}");
    }

    #[test]
    fn plain_marker_becomes_readable_placeholder() {
        let refs = [make_ref("shot", "screenshot.png")];
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
        let refs = [make_ref("known", "k.png")];
        assert_eq!(
            apply_cid_markers_plain("x ![](cid:other) y", &refs),
            "x ![](cid:other) y"
        );
        assert!(render_body_html("x ![](cid:other) y", &refs).contains("![](cid:other)"));
    }

    #[test]
    fn bodies_without_markers_render_as_plain_escaped_text() {
        let refs = [make_ref("a", "a.png")];
        let body = "just text, no markers at all";
        assert_eq!(apply_cid_markers_plain(body, &refs), body);
        assert_eq!(render_body_html(body, &refs), body);
        // …and the no-marker path still escapes.
        assert_eq!(render_body_html("a<b", &refs), "a&lt;b");
    }

    #[test]
    fn multiple_markers_all_replaced() {
        let refs = [make_ref("a", "a.png"), make_ref("b", "b.png")];
        let out = apply_cid_markers_plain("![](cid:a) middle ![](cid:b)", &refs);
        assert_eq!(out, "[a.png] middle [b.png]");
    }
}
