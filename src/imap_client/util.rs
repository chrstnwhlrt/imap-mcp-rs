//! Pure helpers used by the IMAP client and the tools layer. Free of any
//! dependency on `ImapClient` state or the network — kept here so they can be
//! unit-tested in isolation.

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;

/// Escape a string for use in IMAP search quoted strings.
/// Strips control characters and escapes backslash + double quote.
fn escape_imap_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => result.push_str("\\\\"),
            '"' => result.push_str("\\\""),
            c if c.is_control() => {} // strip NUL, CR, LF, etc.
            c => result.push(c),
        }
    }
    result
}

/// Whether an IMAP host reliably supports SEARCH with `CHARSET UTF-8` +
/// LITERAL+ for non-ASCII string arguments. Outlook 365 / Exchange accept the
/// syntax (no BAD response) but always return zero matches — callers must
/// apply non-ASCII filters client-side instead.
///
/// We host-detect rather than capability-check because the CAPABILITY
/// response on Office 365 doesn't accurately reflect this quirk.
pub fn host_supports_unicode_search(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    !(h.contains("office365.com") || h.contains("outlook.com") || h.contains("outlook.office.com"))
}

/// Format a string as an IMAP astring for SEARCH arguments. ASCII uses the
/// quoted form; non-ASCII uses a LITERAL+ non-synchronizing literal (RFC 7888,
/// `{N+}\r\n<bytes>`) because IMAP quoted strings are 7-bit ASCII only per
/// RFC 3501 §4.3. Gmail, Outlook 365, Dovecot, and Cyrus all support LITERAL+.
///
/// When any criterion uses a literal, callers must also prepend `CHARSET UTF-8`
/// to the SEARCH command so the server decodes the bytes correctly.
pub fn imap_astring(value: &str) -> String {
    // Strip control chars in both paths: required for correctness (CR/LF would
    // break literal length accounting; NUL is invalid in quoted strings).
    let clean: String = value.chars().filter(|c| !c.is_control()).collect();
    if clean.is_ascii() {
        let mut out = String::with_capacity(clean.len() + 2);
        out.push('"');
        for ch in clean.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    } else {
        format!("{{{}+}}\r\n{}", clean.len(), clean)
    }
}

/// Convert ISO 8601 date (YYYY-MM-DD) to IMAP date format (DD-Mon-YYYY).
pub fn iso_to_imap_date(iso: &str) -> Result<String> {
    let parts: Vec<&str> = iso.split('-').collect();
    if parts.len() != 3 {
        anyhow::bail!("Invalid date format: {iso}. Expected YYYY-MM-DD");
    }
    let year = parts[0];
    let month_num: u32 = parts[1].parse().context("Invalid month")?;
    let day: u32 = parts[2].parse().context("Invalid day")?;

    let month_name = match month_num {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => anyhow::bail!("Invalid month: {month_num}"),
    };

    Ok(format!("{day}-{month_name}-{year}"))
}

/// Build an IMAP OR chain from multiple search criteria.
/// IMAP OR is prefix notation: `OR crit1 OR crit2 crit3` = crit1 OR (crit2 OR crit3).
/// Returns `None` if the input is empty.
///
/// Single-pass O(n) construction with pre-sized capacity — the naive
/// "reduce with `format!`" pattern is O(n²) on the growing string for large
/// OR-groups (e.g. `from_any: [50 sender names]`).
pub fn build_or_criteria(criteria: &[String]) -> Option<String> {
    match criteria.len() {
        0 => None,
        1 => Some(criteria[0].clone()),
        n => {
            // Output: "OR c0 OR c1 ... OR c_{n-2} c_{n-1}"
            //         = (n-1) × "OR " prefixes, n criteria separated by " ".
            let content_len: usize = criteria.iter().map(String::len).sum();
            let cap = content_len + 3 * (n - 1) /* "OR " */ + n - 1 /* separators */;
            let mut result = String::with_capacity(cap);
            for c in &criteria[..n - 1] {
                result.push_str("OR ");
                result.push_str(c);
                result.push(' ');
            }
            result.push_str(&criteria[n - 1]);
            Some(result)
        }
    }
}

/// Sanitize a string for safe inclusion in a log line. Replaces ASCII control
/// chars (CR, LF, ESC, NUL) and Unicode line separators with `\xNN` escapes so
/// an adversarial IMAP server can't inject fake log records via `%err_str`
/// formatting into stderr.
pub fn sanitize_log_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '\u{FEFF}') {
            use std::fmt::Write;
            let _ = write!(out, "\\x{:02X}", c as u32);
        } else {
            out.push(c);
        }
    }
    out
}

/// Whether retrying the same call later is sensible. Broader than
/// [`is_connection_error`]: every dead-session error is retryable (the next
/// call reconnects), and so are server-side transient states that leave the
/// session alive — Dovecot's `[UNAVAILABLE]`/"Server Unavailable", mailbox
/// `[INUSE]`, "try again". Feeds the `retryable` field every tool error
/// carries: without it a caller cannot tell "folder doesn't exist" (never
/// retry) from "temporarily unavailable" (retry) — field reports showed an
/// unattended run skipping a folder for a day over exactly that.
///
/// Error text interpolates names the sender or the caller chose (folder
/// names, attachment filenames), so the markers are full server phrases and
/// RFC 5530 response codes, not bare words: a folder called "Temporary
/// Projects" inside "Unknown Mailbox: …" must not read as transient — an
/// unattended caller would retry a fact forever. The classification runs on
/// the raw message, before [`clean_imap_error`] strips the `[CODE]` prefix,
/// so the bracketed codes are reliably present.
pub fn is_retryable_error(msg: &str) -> bool {
    if is_connection_error(msg) {
        return true;
    }
    let lower = msg.to_lowercase();
    // RFC 5530 response codes, verbatim with brackets.
    lower.contains("[unavailable]")
        || lower.contains("[inuse]")
        // Full server phrases (Dovecot, Office 365, Gmail wordings).
        || lower.contains("server unavailable")
        || lower.contains("service unavailable")
        || lower.contains("temporarily")
        || lower.contains("temporary error")
        || lower.contains("temporary failure")
        || lower.contains("temporary problem")
        || lower.contains("mailbox is in use")
        || lower.contains("try again")
        || lower.contains("too many connections")
}

/// Heuristic to detect errors that mean the IMAP session is unusable and
/// should be recycled via reconnect. This includes obvious transport errors
/// (broken pipe, connection reset) but also cases where the session is alive
/// at the TCP level but effectively desynced:
///
/// - **`connection lost` / `BYE`** — the server initiated a clean shutdown
///   that async-imap surfaces as `ConnectionLost`.
/// - **Parse errors** — usually leftover bytes in the stream (e.g. after a
///   cancelled operation) desync our reader from the server's output. The
///   session is nominally alive but every subsequent command will fail.
/// - **`no mailbox selected`** — rare, but happens if the server internally
///   deselects without dropping TCP. A reconnect + fresh SELECT recovers.
pub(super) fn is_connection_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    // OS / transport errors
    lower.contains("broken pipe")
        || lower.contains("connection reset")
        || lower.contains("connection refused")
        || lower.contains("connection closed")
        || lower.contains("connection is closed")      // IMAP BYE response: "Connection is closed"
        || lower.contains("connection aborted")
        || lower.contains("connection lost")
        || lower.contains("peer closed")                // TLS peer closed without close_notify
        || lower.contains("close_notify")               // rustls TLS early-close
        || lower.contains("unexpected eof")
        || lower.contains("timed out")
        || lower.contains("network is unreachable")
        || lower.contains("no route to host")
        || lower.contains("status: bye")                // IMAP server BYE response
        || lower.contains("* bye")                      // IMAP BYE protocol tag
        // Stream corruption / desync — reconnect to clear the buffer
        || lower.contains("unable to parse")
        || lower.contains("invalid response")
        // Session-state desync — reconnect forces a fresh SELECT
        || lower.contains("no mailbox selected")
}

/// Clean and escape a Message-ID for safe use in IMAP HEADER search.
/// Strips angle brackets, then escapes quotes/backslashes/control chars
/// to prevent IMAP injection via crafted Message-IDs in received emails.
pub(super) fn clean_message_id(id: &str) -> String {
    escape_imap_string(id.trim_matches(|c| c == '<' || c == '>'))
}

/// Decode an IMAP folder name from modified UTF-7 (RFC 3501 section 5.1.3)
/// into what the user actually sees in their mail client.
///
/// IMAP encodes non-ASCII folder names so that `Entwürfe` travels the wire as
/// `Entw&APw-rfe` and `Gelöschte Elemente` as `Gel&APY-schte Elemente`. That
/// wire form is the only name the server accepts, so it stays authoritative —
/// this is purely for display.
///
/// Returns `None` when the name is plain ASCII (nothing gained) or when the
/// encoding is malformed, so callers can simply omit the field.
///
/// Differences from standard base64: `,` replaces `/`, there is no padding,
/// the payload is UTF-16BE, and `&-` encodes a literal `&`.
pub(super) fn decode_modified_utf7(name: &str) -> Option<String> {
    if !name.contains('&') {
        return None;
    }
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let Some(end) = after.find('-') else {
            return None; // unterminated shift sequence
        };
        if end == 0 {
            out.push('&'); // `&-` is a literal ampersand
        } else {
            // Modified UTF-7 uses the standard base64 alphabet with `,` in
            // place of `/`, and never pads.
            let bytes = STANDARD_NO_PAD
                .decode(after[..end].replace(',', "/"))
                .ok()?;
            if bytes.len() % 2 != 0 {
                return None; // not whole UTF-16 code units
            }
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|p| u16::from_be_bytes([p[0], p[1]]))
                .collect();
            out.push_str(&String::from_utf16(&units).ok()?);
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    (out != name).then_some(out)
}

/// Read the `Message-ID` out of a MIME message we just built.
///
/// `APPEND` is the only way to store a draft, and `async-imap` discards the
/// `APPENDUID` response code, so the UID of a freshly saved draft has to be
/// looked up afterwards — by the one header that identifies it uniquely.
///
/// Scans only the header block (up to the first empty line): a draft may
/// carry tens of megabytes of attachments, and the answer is always in the
/// first few hundred bytes. Returns the value including angle brackets, as it
/// appears in the header.
pub(super) fn extract_message_id(message_bytes: &[u8]) -> Option<String> {
    const HEADER: &[u8] = b"message-id:";
    for line in message_bytes.split(|&b| b == b'\n') {
        // An empty line (bare or CRLF) ends the header block; everything
        // after it is body and could contain anything, including text that
        // merely looks like a header.
        let trimmed = line.strip_suffix(b"\r").unwrap_or(line);
        if trimmed.is_empty() {
            return None;
        }
        if trimmed.len() > HEADER.len()
            && trimmed[..HEADER.len()].eq_ignore_ascii_case(HEADER)
            && let Ok(value) = std::str::from_utf8(&trimmed[HEADER.len()..])
        {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Locale-aware reply prefixes, lowercase. Shared between subject-stripping
/// (for thread search) and reply-subject detection (for draft composition)
/// so the two can never drift apart.
pub const REPLY_PREFIXES: &[&str] = &["re:", "aw:", "antw:", "antwort:"];

/// Locale-aware forward prefixes, lowercase. Same consolidation rationale
/// as [`REPLY_PREFIXES`].
pub const FORWARD_PREFIXES: &[&str] = &["fwd:", "fw:", "wg:", "weitergeleitet:"];

/// Case-insensitive `starts_with` for ASCII prefixes without allocating.
/// (`str::eq_ignore_ascii_case` exists but there's no `str::starts_with_ignore_ascii_case`.)
pub fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// Strip `Re:` / `Fwd:` / locale equivalents repeatedly until the subject
/// has no further known prefix. Consolidated from a 12-case chained
/// `strip_prefix` into a list-driven loop so adding a new locale in one
/// place ([`REPLY_PREFIXES`] / [`FORWARD_PREFIXES`]) updates every consumer.
pub(super) fn strip_email_prefixes(subject: &str) -> &str {
    let mut s = subject;
    loop {
        let trimmed = s.trim_start();
        let matched = REPLY_PREFIXES
            .iter()
            .chain(FORWARD_PREFIXES.iter())
            .find(|p| starts_with_ignore_ascii_case(trimmed, p));
        match matched {
            Some(prefix) => s = &trimmed[prefix.len()..],
            None => return trimmed,
        }
    }
}

/// Reformat a raw async-imap error string into a concise user-facing message.
///
/// async-imap's `Display` for `Error::No` / `Error::Bad` emits its own
/// `Option`-debug shape:
/// `no response: code: None, info: Some("[NONEXISTENT] Unknown Mailbox: X (now in authenticated state) (Failure)")`
/// which leaks internal framing to the LLM. Extract the actual server
/// response text, strip the `[CODE]` prefix and the trailing `(Failure)` /
/// `(now in ... state)` noise, and return just the useful part.
///
/// Leaves non-matching inputs unchanged, so this is safe to apply to every
/// error message (including our own static strings) at the `error_json`
/// boundary.
pub fn clean_imap_error(raw: &str) -> String {
    let info = raw
        .split_once("info: Some(\"")
        .and_then(|(_, rest)| rest.rsplit_once("\")"))
        .map(|(inner, _)| inner);
    let Some(info) = info else {
        return raw.to_string();
    };

    // Strip a well-known response-code prefix: "[NONEXISTENT] ", "[TRYCREATE] ", etc.
    let info = info.find("] ").map_or(info, |end| &info[end + 2..]);

    // Drop trailing framing noise that async-imap / Dovecot / Cyrus append.
    let info = info
        .split(" (now in authenticated state)")
        .next()
        .unwrap_or(info);
    let info = info.trim_end_matches(" (Failure)");

    info.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imap_astring_ascii_is_quoted() {
        assert_eq!(imap_astring("hello"), "\"hello\"");
        assert_eq!(imap_astring("a b"), "\"a b\"");
    }

    #[test]
    fn imap_astring_escapes_quotes_and_backslash() {
        assert_eq!(imap_astring("a\"b"), "\"a\\\"b\"");
        assert_eq!(imap_astring("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn imap_astring_strips_control_chars() {
        assert_eq!(imap_astring("a\r\nb"), "\"ab\"");
        assert_eq!(imap_astring("a\x00b"), "\"ab\"");
    }

    #[test]
    fn imap_astring_non_ascii_uses_literal_plus() {
        assert_eq!(imap_astring("Bestätigung"), "{12+}\r\nBestätigung");
        assert_eq!(imap_astring("für"), "{4+}\r\nfür");
    }

    #[test]
    fn imap_astring_literal_length_matches_bytes_not_chars() {
        let s = "日本語"; // 9 bytes, 3 chars
        assert_eq!(imap_astring(s), "{9+}\r\n日本語");
    }

    #[test]
    fn escape_imap_string_basic() {
        assert_eq!(escape_imap_string("hello"), "hello");
        assert_eq!(escape_imap_string("a\"b"), "a\\\"b");
        assert_eq!(escape_imap_string("a\\b"), "a\\\\b");
        assert_eq!(escape_imap_string("a\r\nb"), "ab");
        assert_eq!(escape_imap_string(""), "");
    }

    #[test]
    fn iso_to_imap_date_valid() {
        assert_eq!(iso_to_imap_date("2026-01-15").unwrap(), "15-Jan-2026");
        assert_eq!(iso_to_imap_date("2026-12-01").unwrap(), "1-Dec-2026");
        assert_eq!(iso_to_imap_date("2000-06-30").unwrap(), "30-Jun-2000");
    }

    #[test]
    fn iso_to_imap_date_invalid() {
        assert!(iso_to_imap_date("2026-1-1").is_ok()); // single-digit ok
        assert!(iso_to_imap_date("2026/01/15").is_err());
        assert!(iso_to_imap_date("2026-13-01").is_err());
        assert!(iso_to_imap_date("not-a-date").is_err());
        assert!(iso_to_imap_date("2026").is_err());
    }

    #[test]
    fn build_or_criteria_empty_returns_none() {
        assert_eq!(build_or_criteria(&[]), None);
    }

    #[test]
    fn build_or_criteria_single_returns_unwrapped() {
        let parts = vec!["FROM \"a\"".to_string()];
        assert_eq!(build_or_criteria(&parts).unwrap(), "FROM \"a\"");
    }

    #[test]
    fn build_or_criteria_multiple_uses_prefix_or() {
        let parts = vec![
            "FROM \"a\"".to_string(),
            "FROM \"b\"".to_string(),
            "FROM \"c\"".to_string(),
        ];
        assert_eq!(
            build_or_criteria(&parts).unwrap(),
            "OR FROM \"a\" OR FROM \"b\" FROM \"c\""
        );
    }

    #[test]
    fn build_or_criteria_two_terms() {
        let parts = vec!["FROM \"a\"".to_string(), "FROM \"b\"".to_string()];
        assert_eq!(
            build_or_criteria(&parts).unwrap(),
            "OR FROM \"a\" FROM \"b\""
        );
    }

    #[test]
    fn is_connection_error_recognises_transport_errors() {
        assert!(is_connection_error("broken pipe"));
        assert!(is_connection_error("Broken Pipe"));
        assert!(is_connection_error("Connection reset by peer"));
        assert!(is_connection_error("connection refused"));
        assert!(is_connection_error("connection closed"));
        assert!(is_connection_error("Connection is closed"));
        assert!(is_connection_error("connection aborted"));
        assert!(is_connection_error("connection lost"));
        assert!(is_connection_error("peer closed connection"));
        assert!(is_connection_error("close_notify alert"));
        assert!(is_connection_error("unexpected EOF"));
        assert!(is_connection_error("operation timed out"));
        assert!(is_connection_error("network is unreachable"));
        assert!(is_connection_error("no route to host"));
    }

    #[test]
    fn is_connection_error_recognises_imap_protocol_errors() {
        assert!(is_connection_error("status: BYE"));
        assert!(is_connection_error("* BYE server going down"));
        assert!(is_connection_error("unable to parse response"));
        assert!(is_connection_error("invalid response from server"));
        assert!(is_connection_error("no mailbox selected"));
    }

    #[test]
    fn is_retryable_error_classifies_transient_vs_permanent() {
        // Transient: connection class plus alive-session server states.
        for msg in [
            "broken pipe",
            "Server Unavailable. 15",
            "[INUSE] Mailbox is in use",
            "Temporary failure, try again later",
            "too many connections",
        ] {
            assert!(is_retryable_error(msg), "{msg} should be retryable");
        }
        // Permanent: repeating these would hammer a fact.
        for msg in [
            "Unknown Mailbox: DoesNotExist",
            "Email with UID 99999999 not found in INBOX",
            "Account \"foo\" not found",
            "Moving emails is disabled for this account (allow_move = false)",
        ] {
            assert!(!is_retryable_error(msg), "{msg} should not be retryable");
        }
    }

    /// Error text interpolates sender- and caller-chosen names. A name that
    /// happens to contain a transient-looking word must not flip the bit —
    /// an unattended caller would retry a fact forever.
    #[test]
    fn is_retryable_error_ignores_transient_words_inside_names() {
        for msg in [
            "Unknown Mailbox: Temporary Projects",
            "Attachment \"unavailable\" not found in email 42",
            "Folder \"Machines in use\" not found",
        ] {
            assert!(!is_retryable_error(msg), "{msg} should not be retryable");
        }
        // The bracketed RFC 5530 codes stay reliable signals: they sit in
        // the raw message (classification runs before clean_imap_error).
        for msg in [
            "[UNAVAILABLE] Temporary System Error (Failure)",
            "no response: code: None, info: Some(\"[INUSE] Mailbox is in use\")",
        ] {
            assert!(is_retryable_error(msg), "{msg} should be retryable");
        }
    }

    #[test]
    fn is_connection_error_rejects_unrelated_errors() {
        assert!(!is_connection_error("permission denied"));
        assert!(!is_connection_error("folder not found"));
        assert!(!is_connection_error("invalid uid"));
        assert!(!is_connection_error("authentication failed"));
        assert!(!is_connection_error(""));
    }

    /// Real folder names as Gmail and Outlook send them over the wire.
    /// Parsers see hostile input: folder names come from the server, MIME from
    /// whoever sent the mail. None of these may panic, whatever they are fed.
    #[test]
    fn parsers_never_panic_on_arbitrary_input() {
        let seeds: &[&str] = &[
            "",
            "&",
            "&-",
            "&&",
            "-",
            "&-&-",
            "&A",
            "&AAAA",
            "&AAA-",
            "&////-",
            "&,,,,-",
            "INBOX",
            "a&b",
            "&\u{202E}-",
            "ä&ö-ü",
            "&AAAAAAAAAAAAAAAA-",
            "&\u{0}-",
            "&%%%%-",
            "\u{FEFF}&AAA-",
            "&-&-&-&-&-",
            "&AAAAA-",
        ];
        let mut cases: Vec<String> = seeds.iter().map(|s| (*s).to_string()).collect();
        // Deterministic combinations — every seed against every other, plus
        // truncations, which is where index arithmetic tends to break.
        for a in seeds {
            for b in seeds {
                cases.push(format!("{a}{b}"));
            }
        }
        for s in seeds {
            for cut in 0..s.len() {
                if s.is_char_boundary(cut) {
                    cases.push(s[..cut].to_string());
                }
            }
        }
        for c in &cases {
            let _ = decode_modified_utf7(c);
            let _ = extract_message_id(c.as_bytes());
            let _ = clean_message_id(c);
            let _ = sanitize_log_str(c);
            let _ = imap_astring(c);
        }
        // Raw bytes too: MIME is not guaranteed to be valid UTF-8.
        for b in 0u8..=255 {
            let _ = extract_message_id(&[b, b'\n', b, b':', b]);
        }
        let _ = extract_message_id(&[0xff, 0xfe, b'\n', b'\n', 0x80]);
    }

    #[test]
    fn decode_modified_utf7_handles_real_folder_names() {
        assert_eq!(
            decode_modified_utf7("Entw&APw-rfe").as_deref(),
            Some("Entwürfe")
        );
        assert_eq!(
            decode_modified_utf7("Gel&APY-schte Elemente").as_deref(),
            Some("Gelöschte Elemente")
        );
        assert_eq!(
            decode_modified_utf7("[Google Mail]/Entw&APw-rfe").as_deref(),
            Some("[Google Mail]/Entwürfe")
        );
        // `&2DzfhQ-` is a surrogate pair (an emoji) — must survive intact.
        assert!(decode_modified_utf7("&2DzfhQ-Diese Woche").is_some());
    }

    /// `None` means "nothing to display differently", so the caller can just
    /// omit the field — plain ASCII must not produce a redundant copy.
    #[test]
    fn decode_modified_utf7_returns_none_when_nothing_changes() {
        assert_eq!(decode_modified_utf7("INBOX"), None);
        assert_eq!(decode_modified_utf7("Clients/Acme"), None);
        // `&-` is a literal ampersand: decodes to the same string.
        assert_eq!(decode_modified_utf7("A&-B"), Some("A&B".to_string()));
    }

    /// Malformed input must fail closed rather than produce a half-decoded
    /// name that no longer matches any real folder.
    #[test]
    fn decode_modified_utf7_rejects_malformed_input() {
        assert_eq!(decode_modified_utf7("Entw&APw"), None); // unterminated
        assert_eq!(decode_modified_utf7("&!!!-x"), None); // not base64
        assert_eq!(decode_modified_utf7("&AP-x"), None); // odd byte count
    }

    /// The security-relevant case: the raw name is pure ASCII and sails past
    /// the control/bidi filter, while its decoded form carries a
    /// right-to-left override. Decoding must surface it so the caller can
    /// reject it — `mod.rs` re-runs `sanitize_external_str` on the result.
    #[test]
    fn decode_modified_utf7_surfaces_hidden_bidi_for_the_caller_to_reject() {
        // U+202E RIGHT-TO-LEFT OVERRIDE encoded as modified UTF-7.
        let raw = "INBOX/&IC4-evil";
        assert!(raw.is_ascii(), "raw name passes an ASCII-only check");
        let decoded = decode_modified_utf7(raw).expect("should decode");
        assert!(
            decoded.chars().any(|c| c == '\u{202E}'),
            "decoded form must expose the override: {decoded:?}"
        );
        assert_ne!(
            crate::email::sanitize_external_str(&decoded),
            decoded,
            "sanitizer must flag it, which is what mod.rs filters on"
        );
    }

    #[test]
    fn extract_message_id_reads_the_header_case_insensitively() {
        let msg = b"From: a@example.com\r\nMessage-ID: <abc.123@nix>\r\nSubject: x\r\n\r\nbody";
        assert_eq!(extract_message_id(msg), Some("<abc.123@nix>".to_string()));
        // Real messages vary the casing of this header freely.
        let lower = b"message-id: <x@y>\r\n\r\nbody";
        assert_eq!(extract_message_id(lower), Some("<x@y>".to_string()));
        let upper = b"MESSAGE-ID: <x@y>\r\n\r\nbody";
        assert_eq!(extract_message_id(upper), Some("<x@y>".to_string()));
    }

    /// The body is attacker-influenced (a quoted email, a pasted log). A line
    /// there that looks like the header must not be mistaken for it, or a
    /// draft could be reported under a UID belonging to another message.
    #[test]
    fn extract_message_id_stops_at_the_body() {
        let msg = b"From: a@example.com\r\nSubject: x\r\n\r\nMessage-ID: <forged@evil>";
        assert_eq!(extract_message_id(msg), None);
    }

    #[test]
    fn extract_message_id_handles_absent_and_malformed_headers() {
        assert_eq!(extract_message_id(b"From: a@b\r\n\r\nbody"), None);
        assert_eq!(extract_message_id(b""), None);
        // Present but empty — nothing to search for.
        assert_eq!(extract_message_id(b"Message-ID:   \r\n\r\nbody"), None);
        // Bare LF instead of CRLF, as some builders emit.
        assert_eq!(
            extract_message_id(b"Message-ID: <a@b>\nSubject: x\n\nbody"),
            Some("<a@b>".to_string())
        );
    }

    /// A name that merely starts the same way is a different header.
    #[test]
    fn extract_message_id_does_not_match_similar_header_names() {
        let msg = b"Message-ID-Extra: <no@match>\r\nIn-Reply-To: <other@x>\r\n\r\nbody";
        assert_ne!(extract_message_id(msg), Some("<no@match>".to_string()));
    }

    #[test]
    fn clean_message_id_strips_brackets_and_escapes() {
        assert_eq!(clean_message_id("<abc@example.com>"), "abc@example.com");
        assert_eq!(clean_message_id("abc@example.com"), "abc@example.com");
        assert_eq!(clean_message_id("<<nested>>"), "nested");
        assert_eq!(
            clean_message_id("<good@id\r\nBcc: evil@evil.com>"),
            "good@idBcc: evil@evil.com"
        );
    }

    #[test]
    fn strip_email_prefixes_removes_known_variants() {
        assert_eq!(strip_email_prefixes("Re: Hello"), "Hello");
        assert_eq!(strip_email_prefixes("RE: Hello"), "Hello");
        assert_eq!(strip_email_prefixes("re: Hello"), "Hello");
        assert_eq!(strip_email_prefixes("Fwd: Hello"), "Hello");
        assert_eq!(strip_email_prefixes("FWD: Hello"), "Hello");
        assert_eq!(strip_email_prefixes("fwd: Hello"), "Hello");
        assert_eq!(strip_email_prefixes("Fw: Hello"), "Hello");
        assert_eq!(strip_email_prefixes("AW: Hello"), "Hello");
        assert_eq!(strip_email_prefixes("WG: Hello"), "Hello");
    }

    #[test]
    fn strip_email_prefixes_strips_recursively() {
        assert_eq!(strip_email_prefixes("Re: Re: Fwd: Hello"), "Hello");
        assert_eq!(strip_email_prefixes("AW: WG: AW: Test"), "Test");
    }

    #[test]
    fn strip_email_prefixes_leaves_unprefixed_subjects() {
        assert_eq!(strip_email_prefixes("Hello world"), "Hello world");
        assert_eq!(strip_email_prefixes(""), "");
        assert_eq!(
            strip_email_prefixes("Reply but no colon"),
            "Reply but no colon"
        );
    }

    #[test]
    fn host_supports_unicode_search_outlook365() {
        assert!(!host_supports_unicode_search("outlook.office365.com"));
        assert!(!host_supports_unicode_search("imap.outlook.com"));
        assert!(!host_supports_unicode_search("OUTLOOK.OFFICE365.COM"));
    }

    #[test]
    fn host_supports_unicode_search_other_providers() {
        assert!(host_supports_unicode_search("imap.gmail.com"));
        assert!(host_supports_unicode_search("imap.fastmail.com"));
        assert!(host_supports_unicode_search("dovecot.example.com"));
        assert!(host_supports_unicode_search(""));
    }

    #[test]
    fn clean_imap_error_extracts_info_and_strips_code_prefix() {
        let raw = r#"no response: code: None, info: Some("[NONEXISTENT] Unknown Mailbox: DoesNotExist (now in authenticated state) (Failure)")"#;
        assert_eq!(clean_imap_error(raw), "Unknown Mailbox: DoesNotExist");
    }

    #[test]
    fn clean_imap_error_strips_trycreate_prefix() {
        let raw = r#"no response: code: None, info: Some("[TRYCREATE] Mailbox doesn't exist: foo (Failure)")"#;
        assert_eq!(clean_imap_error(raw), "Mailbox doesn't exist: foo");
    }

    #[test]
    fn clean_imap_error_handles_missing_code_prefix() {
        let raw =
            r#"no response: code: None, info: Some("Server temporarily unavailable (Failure)")"#;
        assert_eq!(clean_imap_error(raw), "Server temporarily unavailable");
    }

    #[test]
    fn clean_imap_error_passes_through_unrelated_messages() {
        assert_eq!(
            clean_imap_error("Email UID 42 not found in INBOX"),
            "Email UID 42 not found in INBOX"
        );
        assert_eq!(clean_imap_error(""), "");
        assert_eq!(
            clean_imap_error("Account \"foo\" not found"),
            "Account \"foo\" not found"
        );
    }

    #[test]
    fn clean_imap_error_leaves_info_none_case_unchanged() {
        // async-imap emits `info: None` when the server sent no text.
        // Nothing to extract — pass through.
        let raw = "no response: code: None, info: None";
        assert_eq!(clean_imap_error(raw), raw);
    }
}
