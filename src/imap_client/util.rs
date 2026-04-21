//! Pure helpers used by the IMAP client and the tools layer. Free of any
//! dependency on `ImapClient` state or the network — kept here so they can be
//! unit-tested in isolation.

use anyhow::{Context, Result};

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
    fn is_connection_error_rejects_unrelated_errors() {
        assert!(!is_connection_error("permission denied"));
        assert!(!is_connection_error("folder not found"));
        assert!(!is_connection_error("invalid uid"));
        assert!(!is_connection_error("authentication failed"));
        assert!(!is_connection_error(""));
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
