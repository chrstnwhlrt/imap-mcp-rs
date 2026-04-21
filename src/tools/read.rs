//! Non-mutating MCP tools: account/folder/email listing, get, search, draft
//! listing.
//!
//! `search_emails` carries the most logic: criteria are split between
//! server-side (IMAP `SEARCH`) and an internal [`ClientFilter`] for non-ASCII
//! terms on Outlook 365 (which silently returns 0 matches for `CHARSET UTF-8`).

use std::collections::HashSet;

use rmcp::schemars;
use serde::Deserialize;

use crate::email::EmailSummary;
use crate::imap_client::{
    build_or_criteria, host_supports_unicode_search, imap_astring, iso_to_imap_date,
};

use super::{ImapMcpServer, error_json};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListFoldersRequest {
    #[schemars(description = "Account name (from list_accounts); default: first configured.")]
    pub account: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListEmailsRequest {
    #[schemars(description = "Account name (from list_accounts); default: first configured.")]
    pub account: Option<String>,
    #[schemars(description = "Folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(description = "Maximum number of results (default: 20, hard cap: 500).")]
    pub limit: Option<u32>,
    #[schemars(description = "Number of results to skip for pagination (default: 0)")]
    pub offset: Option<u32>,
    #[schemars(description = "Only show unread emails (default: false)")]
    pub unread_only: Option<bool>,
    #[schemars(
        description = "Collapse results into conversation threads by Message-ID / References (default: false). Returns one row per thread (newest message), with `thread_message_count` indicating thread size. Fetches ~3× the limit internally to compensate for collapsing. Note: `thread_message_count` counts only messages within the fetched window — older thread members outside the window are not included. For the full thread, call `get_thread(uid)` on the representative."
    )]
    pub group_by_thread: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetEmailRequest {
    #[schemars(description = "Account name (from list_accounts); default: first configured.")]
    pub account: Option<String>,
    #[schemars(description = "Folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(description = "Email UID (from list_emails or search_emails results)")]
    pub uid: u32,
    #[schemars(
        description = "Include body_html in response (default: false). HTML bodies of marketing/order emails can be 40–60 KB of inlined styling. Only enable when you need the HTML markup (e.g. to parse tables); body_text is usually sufficient."
    )]
    pub include_html: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetThreadRequest {
    #[schemars(description = "Account name (from list_accounts); default: first configured.")]
    pub account: Option<String>,
    #[schemars(description = "Folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(
        description = "Email UID of any message in the thread (from list_emails or search_emails results)"
    )]
    pub uid: u32,
    #[schemars(
        description = "Strict thread matching via Message-ID / References / In-Reply-To only (default: true). Matches `list_emails(group_by_thread=true)` semantics. Set to `false` to additionally merge messages by subject-kernel for small threads — useful for mailers that omit References headers (Lotus Notes), but can merge unrelated conversations that share subject keywords."
    )]
    pub strict: Option<bool>,
    #[schemars(
        description = "Include full message bodies + attachments per thread message (default: true). Set to `false` for a compact summary-only response (same shape as list_emails entries, ~1–2 KB per message instead of 5–20 KB) when you only need to overview a thread."
    )]
    pub include_body: Option<bool>,
    #[schemars(
        description = "Include body_html in each thread message (default: false). HTML bodies are large; body_text is usually sufficient. Ignored when include_body is false."
    )]
    pub include_html: Option<bool>,
    #[schemars(
        description = "Maximum number of thread messages to return (default: 50, hard cap: 200). Oldest messages are dropped first; response includes `truncated_from` when truncation occurred."
    )]
    pub max_messages: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchEmailsRequest {
    #[schemars(description = "Account name (from list_accounts); default: first configured.")]
    pub account: Option<String>,
    #[schemars(
        description = "Folder name to search (e.g. \"INBOX\"). Omit to search all folders; Gmail duplicates across labels are deduped by Message-ID."
    )]
    pub folder: Option<String>,
    #[schemars(
        description = "Full-text search in body and headers (single term, substring, case-insensitive). Server-side IMAP search — not fuzzy/stemmed."
    )]
    pub text: Option<String>,
    #[schemars(
        description = "Full-text search matching ANY of these terms (OR-combined, substring, case-insensitive). Useful for synonyms: [\"lipo\", \"akku\", \"battery\"]."
    )]
    pub text_any: Option<Vec<String>>,
    #[schemars(
        description = "Full-text search matching ALL of these terms (AND-combined, substring, case-insensitive). Useful for narrowing: [\"praktikum\", \"2027\"]."
    )]
    pub text_all: Option<Vec<String>>,
    #[schemars(
        description = "Filter by sender address or name (substring match, case-insensitive)"
    )]
    pub from: Option<String>,
    #[schemars(
        description = "Filter by sender matching ANY of these values (OR-combined, substring). E.g. [\"amazon.de\", \"paypal.com\"]."
    )]
    pub from_any: Option<Vec<String>>,
    #[schemars(
        description = "Filter by sender matching ALL of these values (AND-combined, substring, case-insensitive). Uncommon — use when sender name AND address parts must both match."
    )]
    pub from_all: Option<Vec<String>>,
    #[schemars(description = "Filter by recipient address (substring match, case-insensitive)")]
    pub to: Option<String>,
    #[schemars(description = "Filter by subject line (substring match, case-insensitive)")]
    pub subject: Option<String>,
    #[schemars(
        description = "Filter by subject matching ALL of these terms (AND-combined, substring, case-insensitive). E.g. [\"invoice\", \"Q4\"]."
    )]
    pub subject_all: Option<Vec<String>>,
    #[schemars(description = "Emails on or after this date (format: YYYY-MM-DD)")]
    pub since: Option<String>,
    #[schemars(description = "Emails strictly before this date (format: YYYY-MM-DD)")]
    pub before: Option<String>,
    #[schemars(description = "Filter by read state: true = read, false = unread")]
    pub is_read: Option<bool>,
    #[schemars(description = "Filter by flag state: true = flagged/starred, false = unflagged")]
    pub is_flagged: Option<bool>,
    #[schemars(
        description = "Filter by reply state: true = replied-to, false = unreplied. Reads the IMAP \\Answered flag, which is not always set by webmail clients — treat results as best-effort."
    )]
    pub is_answered: Option<bool>,
    #[schemars(
        description = "Filter by attachment presence: true = has attachments, false = no attachments. Applied client-side after fetch, so combine with a date/sender filter on large folders to narrow candidates first."
    )]
    pub has_attachments: Option<bool>,
    #[schemars(
        description = "Only emails larger than this many bytes (IMAP `LARGER`). Useful for spotting big space consumers. 1 MiB = 1048576."
    )]
    pub min_size: Option<u32>,
    #[schemars(
        description = "Only emails strictly smaller than this many bytes (IMAP `SMALLER`)."
    )]
    pub max_size: Option<u32>,
    #[schemars(description = "Maximum number of results (default: 20, hard cap: 500).")]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DownloadAttachmentRequest {
    #[schemars(description = "Account name (from list_accounts); default: first configured.")]
    pub account: Option<String>,
    #[schemars(description = "Folder name (e.g. \"INBOX\")")]
    pub folder: String,
    #[schemars(description = "Email UID (from list_emails, search_emails, or get_email results)")]
    pub uid: u32,
    #[schemars(
        description = "Attachment filename as reported by get_email (`attachments[].filename`)"
    )]
    pub filename: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListDraftsRequest {
    #[schemars(description = "Account name (from list_accounts); default: first configured.")]
    pub account: Option<String>,
    #[schemars(description = "Maximum number of results (default: 20, hard cap: 500).")]
    pub limit: Option<u32>,
    #[schemars(description = "Number of results to skip for pagination (default: 0).")]
    pub offset: Option<u32>,
}

pub async fn list_folders(server: &ImapMcpServer, req: ListFoldersRequest) -> String {
    let (account_config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    let account_name = account_config.name.clone();
    let mut client = client_arc.lock().await;
    match client.list_folders().await {
        Ok(folders) => serde_json::to_string(&serde_json::json!({
            "account": account_name,
            "folders": folders,
        }))
        .unwrap_or_else(|e| error_json(&e.to_string())),
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

pub async fn list_emails(server: &ImapMcpServer, req: ListEmailsRequest) -> String {
    let (account_config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    let account_name = account_config.name.clone();
    let mut client = client_arc.lock().await;
    // Clamp to a hard ceiling so a prompt-injected limit can't ask for 100k
    // emails and OOM the host. Users needing more should paginate via offset.
    let limit = req.limit.unwrap_or(20).clamp(1, 500);
    let offset = req.offset.unwrap_or(0);
    let unread_only = req.unread_only.unwrap_or(false);
    let group_by_thread = req.group_by_thread.unwrap_or(false);

    // When grouping by thread, fetch ~3× so collapsed duplicates still leave
    // enough rows to fill the requested `limit`. Still capped at 500.
    let fetch_limit = if group_by_thread {
        limit.saturating_mul(3).min(500)
    } else {
        limit
    };

    match client
        .list_emails(&req.folder, fetch_limit, offset, unread_only)
        .await
    {
        Ok((emails, total, matched)) => {
            let emails = if group_by_thread {
                let mut grouped = group_summaries_by_thread(emails);
                grouped.truncate(limit as usize);
                grouped
            } else {
                emails
            };
            serde_json::to_string(&serde_json::json!({
                "account": account_name,
                "folder": req.folder,
                "total": total,
                "matched": matched,
                "offset": offset,
                "limit": limit,
                "emails": emails,
            }))
            .unwrap_or_else(|e| error_json(&e.to_string()))
        }
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

/// Union-find `find` with path compression over the interned ID space
/// built by [`group_summaries_by_thread`]. Extracted so
/// `clippy::items_after_statements` stays happy.
fn uf_find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

/// Union-find union by setting one root's parent to the other's. Biased
/// toward `b` for simplicity — the size doesn't matter for our tree depth
/// since `uf_find` already path-compresses.
fn uf_union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (uf_find(parent, a), uf_find(parent, b));
    if ra != rb {
        parent[ra] = rb;
    }
}

/// Collapse an `EmailSummary` list into one row per conversation thread.
/// Builds a disjoint-set union over (Message-ID, In-Reply-To, References)
/// so any two messages linked by a shared ID end up in the same group,
/// then keeps the newest-by-date representative per group and sets
/// `thread_message_count` on it.
///
/// Summaries without a Message-ID stay as their own single-message
/// "group" so they're never silently dropped.
fn group_summaries_by_thread(mut summaries: Vec<EmailSummary>) -> Vec<EmailSummary> {
    use std::collections::HashMap;

    if summaries.len() < 2 {
        for s in &mut summaries {
            s.thread_message_count.get_or_insert(1);
        }
        return summaries;
    }

    let mut id_of: HashMap<String, usize> = HashMap::new();
    let mut parent: Vec<usize> = Vec::new();
    // Inline intern — closures with mutable borrows get ugly under clippy.
    let intern = |s: &str, parent: &mut Vec<usize>, id_of: &mut HashMap<String, usize>| -> usize {
        if let Some(&i) = id_of.get(s) {
            return i;
        }
        let i = parent.len();
        parent.push(i);
        id_of.insert(s.to_string(), i);
        i
    };

    // First pass: intern every Message-ID / In-Reply-To / References entry
    // and record which summary owns which ID(s). Summaries without a
    // Message-ID get a synthetic ID keyed on (folder, uid) so they stay
    // groupable against themselves only.
    let mut summary_keys: Vec<Vec<usize>> = Vec::with_capacity(summaries.len());
    for s in &summaries {
        let mut keys: Vec<usize> = Vec::new();
        if let Some(mid) = &s.message_id {
            keys.push(intern(mid, &mut parent, &mut id_of));
        } else {
            let synth = format!("\0synthetic:{}:{}", s.folder, s.uid);
            keys.push(intern(&synth, &mut parent, &mut id_of));
        }
        if let Some(irt) = &s.in_reply_to {
            keys.push(intern(irt, &mut parent, &mut id_of));
        }
        for r in &s.references {
            keys.push(intern(r, &mut parent, &mut id_of));
        }
        summary_keys.push(keys);
    }

    // Second pass: merge all keys belonging to the same summary.
    for keys in &summary_keys {
        if keys.len() < 2 {
            continue;
        }
        let first = keys[0];
        for &k in &keys[1..] {
            uf_union(&mut parent, first, k);
        }
    }

    // Third pass: bucket summary indices by their canonical root.
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for (idx, keys) in summary_keys.iter().enumerate() {
        let root = uf_find(&mut parent, keys[0]);
        groups.entry(root).or_default().push(idx);
    }

    // Pick the newest (by ISO date — lexicographic order is correct for
    // the `YYYY-MM-DDTHH:MM:SS+TZ` format `format_datetime` produces) per
    // group, annotate with count, then restore newest-first ordering by
    // original index (the caller already sorted that way pre-group).
    let mut representatives: Vec<(usize, EmailSummary)> = Vec::with_capacity(groups.len());
    for (_root, mut members) in groups {
        if members.is_empty() {
            continue;
        }
        members.sort_by(|&a, &b| summaries[b].date.cmp(&summaries[a].date));
        let winner_idx = members[0];
        let mut rep = summaries[winner_idx].clone();
        rep.thread_message_count = Some(members.len());
        representatives.push((winner_idx, rep));
    }
    representatives.sort_by_key(|(idx, _)| *idx);
    representatives.into_iter().map(|(_, s)| s).collect()
}

pub async fn get_email(server: &ImapMcpServer, req: GetEmailRequest) -> String {
    let (account_config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    let account_name = account_config.name.clone();
    let include_html = req.include_html.unwrap_or(false);
    let mut client = client_arc.lock().await;
    match client.get_email(&req.folder, req.uid).await {
        Ok(Some(mut email)) => {
            if !include_html {
                email.body_html = None;
            }
            serde_json::to_string(&serde_json::json!({
                "account": account_name,
                "email": email,
            }))
            .unwrap_or_else(|e| error_json(&e.to_string()))
        }
        Ok(None) => error_json(&format!(
            "Email with UID {} not found in {}",
            req.uid,
            crate::email::sanitize_external_str(&req.folder)
        )),
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

pub async fn get_thread(server: &ImapMcpServer, req: GetThreadRequest) -> String {
    let (account_config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    let account_name = account_config.name.clone();
    let strict = req.strict.unwrap_or(true);
    let include_body = req.include_body.unwrap_or(true);
    let include_html = req.include_html.unwrap_or(false);
    let max_messages = req.max_messages.unwrap_or(50).clamp(1, 200) as usize;
    let mut client = client_arc.lock().await;
    match client.get_thread(&req.folder, req.uid, strict).await {
        Ok(mut emails) => {
            let original_count = emails.len();
            let truncated = original_count > max_messages;
            // Drop oldest messages when over budget. `emails` is already sorted
            // chronologically (oldest first) by get_thread_once, so drain the head.
            if truncated {
                emails.drain(..original_count - max_messages);
            }
            let subject = emails
                .first()
                .map(|e| e.subject.clone())
                .unwrap_or_default();

            let emails_value = if include_body {
                if !include_html {
                    for email in &mut emails {
                        email.body_html = None;
                    }
                }
                serde_json::to_value(&emails).unwrap_or(serde_json::Value::Array(vec![]))
            } else {
                let summaries: Vec<_> = emails
                    .into_iter()
                    .map(|e| crate::email::summarize(e, 200))
                    .collect();
                serde_json::to_value(&summaries).unwrap_or(serde_json::Value::Array(vec![]))
            };

            let message_count = emails_value.as_array().map_or(0, Vec::len);
            let mut payload = serde_json::Map::with_capacity(5);
            payload.insert("account".into(), account_name.into());
            payload.insert("subject".into(), subject.into());
            payload.insert("message_count".into(), message_count.into());
            if truncated {
                payload.insert("truncated_from".into(), original_count.into());
            }
            payload.insert("emails".into(), emails_value);

            serde_json::to_string(&serde_json::Value::Object(payload))
                .unwrap_or_else(|e| error_json(&e.to_string()))
        }
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

/// Substring filters applied client-side after fetch. Used as a fallback for
/// IMAP servers (e.g. Outlook 365) that silently return zero matches for
/// SEARCH with `CHARSET UTF-8` instead of rejecting the syntax.
///
/// All matching is case-insensitive. AND-combined within a category; OR-combined
/// within a single `*_any` group. Empty filter matches every email.
///
/// **Invariant: all stored needles are already lowercased.** `build_search_criteria`
/// owns the `.to_lowercase()` call so `matches()` can hot-loop over N emails
/// without re-lowercasing the same needles per email.
#[derive(Default, Debug)]
struct ClientFilter {
    subject: Vec<String>,
    text: Vec<String>,
    from: Vec<String>,
    to: Vec<String>,
    text_any: Vec<Vec<String>>,
    from_any: Vec<Vec<String>>,
    /// When set, post-filter by attachment presence. IMAP SEARCH has no
    /// native "has attachment" operator, so this is always client-side.
    has_attachments: Option<bool>,
}

impl ClientFilter {
    const fn is_empty(&self) -> bool {
        self.subject.is_empty()
            && self.text.is_empty()
            && self.from.is_empty()
            && self.to.is_empty()
            && self.text_any.is_empty()
            && self.from_any.is_empty()
            && self.has_attachments.is_none()
    }

    fn matches(&self, email: &EmailSummary) -> bool {
        // `Vec` fields already AND by construction — every pushed needle
        // must match, so `_all`-style request fields just push multiple
        // entries into the same buckets used by their single-term siblings.
        let subject_l = email.subject.to_lowercase();
        for s in &self.subject {
            if !subject_l.contains(s.as_str()) {
                return false;
            }
        }
        let snippet_l = email.snippet.to_lowercase();
        for s in &self.text {
            if !snippet_l.contains(s.as_str()) {
                return false;
            }
        }
        let from_l = email.from.as_ref().map_or(String::new(), |a| {
            format!(
                "{} {}",
                a.address.to_lowercase(),
                a.name.as_deref().unwrap_or("").to_lowercase()
            )
        });
        for s in &self.from {
            if !from_l.contains(s.as_str()) {
                return false;
            }
        }
        for s in &self.to {
            if !email
                .to
                .iter()
                .any(|a| a.address.to_lowercase().contains(s.as_str()))
            {
                return false;
            }
        }
        for group in &self.text_any {
            if !group.iter().any(|s| snippet_l.contains(s.as_str())) {
                return false;
            }
        }
        for group in &self.from_any {
            if !group.iter().any(|s| from_l.contains(&s.to_lowercase())) {
                return false;
            }
        }
        if let Some(want) = self.has_attachments
            && email.has_attachments != want
        {
            return false;
        }
        true
    }
}

/// Build an IMAP SEARCH criteria string from the request. When `unicode_search`
/// is `false`, non-ASCII string criteria are diverted into a `ClientFilter`
/// (server gets ASCII-only) so they can be applied after fetch — workaround for
/// Outlook 365's broken `CHARSET UTF-8` SEARCH.
///
/// Returns `Err` for user-facing validation failures (bad date, no criterion,
/// non-ASCII-only criteria without a date scope on a non-Unicode server).
/// Server-side / client-side splitter for a single search term. Pulled
/// out of `build_search_criteria` so `clippy::items_after_statements`
/// stays happy when it's referenced from inside the function body.
fn push_search_term(
    parts: &mut Vec<String>,
    bucket: &mut Vec<String>,
    key: &str,
    term: &str,
    unicode: bool,
) {
    if unicode || term.is_ascii() {
        parts.push(format!("{key} {}", imap_astring(term)));
    } else {
        bucket.push(term.to_lowercase());
    }
}

#[allow(clippy::too_many_lines)]
fn build_search_criteria(
    req: &SearchEmailsRequest,
    unicode_search: bool,
) -> Result<(String, ClientFilter), String> {
    let mut parts: Vec<String> = Vec::new();
    let mut filter = ClientFilter::default();
    let to_server = |v: &str| unicode_search || v.is_ascii();

    // Filter-side pushes ALWAYS lowercase the needle upfront — see
    // `ClientFilter`'s invariant. Saves re-lowercasing in the hot `matches`
    // loop (per-email × per-filter).
    //
    // `push_search_term` (module-private) either sends a single term to the
    // server or diverts it to the client-side filter (for non-ASCII on
    // Outlook-style servers). Reused across `_single` and `_all` request
    // slots — the `_all` variants simply push each term into the same
    // bucket, which already AND-combines per `ClientFilter::matches`.
    let push_term = push_search_term;

    if let Some(text) = &req.text {
        push_term(&mut parts, &mut filter.text, "TEXT", text, unicode_search);
    }
    if let Some(text_all) = &req.text_all {
        for term in text_all {
            push_term(&mut parts, &mut filter.text, "TEXT", term, unicode_search);
        }
    }
    if let Some(text_any) = &req.text_any
        && !text_any.is_empty()
    {
        if text_any.iter().all(|t| to_server(t)) {
            let ors: Vec<String> = text_any
                .iter()
                .map(|t| format!("TEXT {}", imap_astring(t)))
                .collect();
            if let Some(combined) = build_or_criteria(&ors) {
                parts.push(combined);
            }
        } else {
            filter
                .text_any
                .push(text_any.iter().map(|s| s.to_lowercase()).collect());
        }
    }
    if let Some(from) = &req.from {
        push_term(&mut parts, &mut filter.from, "FROM", from, unicode_search);
    }
    if let Some(from_all) = &req.from_all {
        for term in from_all {
            push_term(&mut parts, &mut filter.from, "FROM", term, unicode_search);
        }
    }
    if let Some(from_any) = &req.from_any
        && !from_any.is_empty()
    {
        if from_any.iter().all(|t| to_server(t)) {
            let ors: Vec<String> = from_any
                .iter()
                .map(|t| format!("FROM {}", imap_astring(t)))
                .collect();
            if let Some(combined) = build_or_criteria(&ors) {
                parts.push(combined);
            }
        } else {
            filter
                .from_any
                .push(from_any.iter().map(|s| s.to_lowercase()).collect());
        }
    }
    if let Some(to) = &req.to {
        push_term(&mut parts, &mut filter.to, "TO", to, unicode_search);
    }
    if let Some(subject) = &req.subject {
        push_term(
            &mut parts,
            &mut filter.subject,
            "SUBJECT",
            subject,
            unicode_search,
        );
    }
    if let Some(subject_all) = &req.subject_all {
        for term in subject_all {
            push_term(
                &mut parts,
                &mut filter.subject,
                "SUBJECT",
                term,
                unicode_search,
            );
        }
    }
    if let Some(since) = &req.since {
        let d = iso_to_imap_date(since).map_err(|e| format!("Invalid 'since' date: {e}"))?;
        parts.push(format!("SINCE {d}"));
    }
    if let Some(before) = &req.before {
        let d = iso_to_imap_date(before).map_err(|e| format!("Invalid 'before' date: {e}"))?;
        parts.push(format!("BEFORE {d}"));
    }
    if let Some(is_read) = req.is_read {
        parts.push(if is_read { "SEEN" } else { "UNSEEN" }.to_string());
    }
    if let Some(is_flagged) = req.is_flagged {
        parts.push(if is_flagged { "FLAGGED" } else { "UNFLAGGED" }.to_string());
    }
    if let Some(is_answered) = req.is_answered {
        parts.push(
            if is_answered {
                "ANSWERED"
            } else {
                "UNANSWERED"
            }
            .to_string(),
        );
    }
    if let Some(min_size) = req.min_size {
        parts.push(format!("LARGER {min_size}"));
    }
    if let Some(max_size) = req.max_size {
        parts.push(format!("SMALLER {max_size}"));
    }
    // has_attachments is always client-side — no native IMAP SEARCH operator.
    if let Some(want) = req.has_attachments {
        filter.has_attachments = Some(want);
    }

    if parts.is_empty() && filter.is_empty() {
        return Err("At least one search criterion is required".to_string());
    }
    if parts.is_empty() {
        // All criteria were diverted client-side. Fetching the entire mailbox
        // would be prohibitively slow on big folders, so require a date scope.
        return Err(
            "Non-ASCII search on this server requires a date filter (since/before)".to_string(),
        );
    }

    // Prepend `CHARSET UTF-8` only when something non-ASCII actually went to
    // the server (i.e. on Unicode-capable servers).
    let criteria = if parts.iter().any(|p| !p.is_ascii()) {
        format!("CHARSET UTF-8 {}", parts.join(" "))
    } else {
        parts.join(" ")
    };
    Ok((criteria, filter))
}

pub async fn search_emails(server: &ImapMcpServer, req: SearchEmailsRequest) -> String {
    let (account_config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    let account_name = account_config.name.clone();
    let unicode_search = host_supports_unicode_search(&account_config.host);

    let (criteria, filter) = match build_search_criteria(&req, unicode_search) {
        Ok(c) => c,
        Err(e) => return error_json(&e),
    };
    // Clamp to a hard ceiling so a prompt-injected limit can't ask for 100k
    // emails and OOM the host. Users needing more should paginate via offset.
    let limit = req.limit.unwrap_or(20).clamp(1, 500);

    let mut client = client_arc.lock().await;

    let folders = if let Some(folder) = &req.folder {
        vec![folder.clone()]
    } else {
        match client.get_folder_names().await {
            Ok(names) => names,
            Err(e) => return error_json(&client.check_error(e).to_string()),
        }
    };

    // When searching across all folders, put INBOX first so its version of any
    // duplicated Gmail message wins the dedup below (better UX than an
    // `[Gmail]/All Mail` or label-folder UID).
    let searching_all = req.folder.is_none();
    // Move folders (not clone) — we don't use the original vec afterwards.
    let mut ordered_folders = folders;
    if searching_all {
        ordered_folders.sort_by_key(|f| i32::from(!f.eq_ignore_ascii_case("INBOX")));
    }

    let mut all_results = Vec::new();
    // For single-folder searches, surface errors directly — otherwise a
    // disallowed folder (`allowed_folders` violation) or typo'd folder name
    // would silently return empty results, which is misleading.
    let single_folder = ordered_folders.len() == 1;
    let mut single_folder_error: Option<String> = None;
    for folder in &ordered_folders {
        match client.search_emails(folder, &criteria, limit).await {
            Ok(results) => all_results.extend(results),
            Err(e) => {
                let err_str = e.to_string();
                tracing::warn!(
                    folder = %crate::imap_client::sanitize_log_str(folder),
                    error = %crate::imap_client::sanitize_log_str(&err_str),
                    "Search failed for folder"
                );
                let _ = client.check_error(e);
                if single_folder {
                    single_folder_error = Some(err_str);
                }
            }
        }
    }

    // Release the mutex before CPU-bound dedup/sort/serialize so parallel tool
    // calls on the same account aren't blocked.
    drop(client);

    if let Some(err) = single_folder_error {
        return error_json(&err);
    }

    // Apply client-side filters (Outlook 365 UTF-8 fallback). No-op when the
    // server handled all criteria itself.
    if !filter.is_empty() {
        all_results.retain(|e| filter.matches(e));
    }

    // Dedup by Message-ID when searching across folders. Gmail's label system
    // returns the same physical message from every labelled folder (plus
    // `[Gmail]/All Mail`) with different UIDs per folder. Message-ID is the
    // only consistent cross-folder identifier. For emails without a Message-ID
    // (rare — only malformed mails), fall back to (folder, uid) which is
    // always unique and therefore never dedups.
    if searching_all {
        let mut seen: HashSet<String> = HashSet::new();
        all_results.retain(|email| {
            let key = email
                .message_id
                .clone()
                .unwrap_or_else(|| format!("{}\x00{}", email.folder, email.uid));
            seen.insert(key)
        });
    }

    all_results.sort_by(|a, b| b.date.cmp(&a.date));
    all_results.truncate(limit as usize);

    serde_json::to_string(&serde_json::json!({
        "account": account_name,
        "matched": all_results.len(),
        "emails": all_results,
    }))
    .unwrap_or_else(|e| error_json(&e.to_string()))
}

/// Make a filesystem-safe filename out of an LLM-supplied attachment name.
/// Sanitises bidi/control via `sanitize_external_str`, then replaces path
/// separators + NUL with `_` so the result is always a single path
/// component. Falls back to `"attachment"` for empty / `.` / `..` inputs.
fn filesystem_safe_filename(raw: &str) -> String {
    let cleaned = crate::email::sanitize_external_str(raw);
    let safe: String = cleaned
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            other => other,
        })
        .collect();
    let trimmed = safe.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "attachment".to_string()
    } else {
        trimmed.to_string()
    }
}

// Linear download workflow (resolve → fetch raw → parse → find attachment →
// size check → mkdir → write partial → chmod → rename). Splitting would
// fragment a straight pipeline for no readability gain.
#[allow(clippy::too_many_lines)]
pub async fn download_attachment(server: &ImapMcpServer, req: DownloadAttachmentRequest) -> String {
    use mail_parser::MimeHeaders;
    use std::path::Path;
    use uuid::Uuid;

    // Cap attachment size to prevent OOM from malicious / huge attachments.
    // Legitimate attachments above 50 MiB are rare; users needing that can
    // raise the cap or use a dedicated mail client.
    const MAX_ATTACHMENT_SIZE: usize = 50 * 1024 * 1024;

    let (account_config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    let account_name = account_config.name.clone();
    let mut client = client_arc.lock().await;

    // Fetch raw email bytes
    let raw = match client.fetch_raw(&req.folder, req.uid).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return error_json(&format!(
                "Email with UID {} not found in {}",
                req.uid,
                crate::email::sanitize_external_str(&req.folder)
            ));
        }
        Err(e) => return error_json(&client.check_error(e).to_string()),
    };

    // Parse and find the attachment
    let Some(message) = mail_parser::MessageParser::default().parse(&raw) else {
        return error_json("Failed to parse email");
    };

    // Match on the sanitized name — that's what the LLM saw in `get_email`'s
    // attachments list (see `email.rs::sanitize_external_str`). Comparing raw
    // to sanitized would otherwise 404 any attachment whose real name
    // contains stripped chars (bidi overrides, zero-width, NUL).
    let attachment = message.attachments().find(|att| {
        crate::email::sanitize_external_str(att.attachment_name().unwrap_or("")) == req.filename
    });

    // Filename in error/JSON responses: strip control/bidi so a prompt-
    // injected LLM echoing a crafted name can't round-trip the payload
    // back into its own context through our error message.
    let safe_filename = crate::email::sanitize_external_str(&req.filename);
    let Some(attachment) = attachment else {
        return error_json(&format!(
            "Attachment \"{safe_filename}\" not found in email UID {}",
            req.uid
        ));
    };

    let content_type = crate::email::format_content_type(attachment.content_type());

    let contents = attachment.contents();
    let size = contents.len();

    if size > MAX_ATTACHMENT_SIZE {
        return error_json(&format!(
            "Attachment \"{safe_filename}\" is {size} bytes — exceeds the {MAX_ATTACHMENT_SIZE}-byte cap"
        ));
    }

    // Save into the first configured attachment dir (created + mode-locked
    // by main at startup) rather than a hardcoded `/tmp/imap-mcp-rs` — the
    // hardcoded path was exploitable on multi-user hosts via a pre-created
    // symlink.
    let default_dir = crate::config::default_attachment_dir();
    let dir_str = server
        .config
        .allowed_attachment_dirs
        .first()
        .cloned()
        .unwrap_or(default_dir);
    let dir = Path::new(&dir_str);
    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        return error_json(&format!("Failed to create directory: {e}"));
    }
    // Restrict dir to user-only (0700) in case create_dir_all just created it
    // with a permissive umask default — attachments are potentially sensitive
    // (keys, contracts, private photos) and shouldn't be world-readable on
    // multi-user systems.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await;
    }

    // Per-download UUID subdirectory containing the file under its
    // ORIGINAL (filesystem-safe) name. Lets `draft_*(attachments=[saved_to])`
    // attach the file as "Lebenslauf.pdf" (recipient-friendly) instead of
    // "<UUID>.pdf" — `read_attachments` derives the MIME filename from
    // `Path::file_name()`. The UUID dir provides collision-free uniqueness
    // without leaking into the recipient view.
    let uuid = Uuid::new_v4();
    let download_dir = dir.join(uuid.to_string());
    if let Err(e) = tokio::fs::create_dir_all(&download_dir).await {
        return error_json(&format!("Failed to create download subdir: {e}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ =
            tokio::fs::set_permissions(&download_dir, std::fs::Permissions::from_mode(0o700)).await;
    }

    // Strip path separators + NUL from the LLM-supplied filename before
    // joining onto the download dir — a crafted attachment named
    // `"../../../etc/passwd"` would otherwise let `Path::join` traverse
    // outside our allowed tree. Empty / `.` / `..` collapse to "attachment".
    let fs_safe_name = filesystem_safe_filename(&req.filename);
    let save_path = download_dir.join(&fs_safe_name);
    // Write to a `.partial` sibling first, then atomically rename. If the
    // write fails mid-way (ENOSPC, quota, brief I/O error) we remove the
    // partial instead of leaving a truncated file on disk that a later
    // `draft_*(attachments=[...])` could pick up and silently send
    // corrupted to a recipient.
    let partial_path = download_dir.join(format!("{fs_safe_name}.partial"));

    if let Err(e) = tokio::fs::write(&partial_path, contents).await {
        let _ = tokio::fs::remove_file(&partial_path).await;
        return error_json(&format!("Failed to write file: {e}"));
    }
    // chmod BEFORE the rename so the final path is 0600-locked from the
    // moment it exists under its advertised name — avoids a brief window
    // where another process on the same host could open it at 0644. If the
    // chmod itself fails (ACL-hostile FS, LSM EPERM), refuse rather than
    // landing a potentially sensitive attachment at the umask default.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            tokio::fs::set_permissions(&partial_path, std::fs::Permissions::from_mode(0o600)).await
        {
            let _ = tokio::fs::remove_file(&partial_path).await;
            return error_json(&format!(
                "Failed to lock attachment permissions to 0600: {e}"
            ));
        }
    }
    if let Err(e) = tokio::fs::rename(&partial_path, &save_path).await {
        let _ = tokio::fs::remove_file(&partial_path).await;
        return error_json(&format!("Failed to finalize file: {e}"));
    }

    serde_json::to_string(&serde_json::json!({
        "account": account_name,
        "saved_to": save_path.to_string_lossy(),
        "filename": req.filename,
        "size": size,
        "content_type": content_type,
    }))
    .unwrap_or_else(|e| error_json(&e.to_string()))
}

pub async fn list_drafts(server: &ImapMcpServer, req: ListDraftsRequest) -> String {
    let (account_config, client_arc) = match server.resolve_client(req.account.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_json(&e),
    };
    let account_name = account_config.name.clone();
    // Clamp to a hard ceiling so a prompt-injected limit can't ask for 100k
    // emails and OOM the host. Users needing more should paginate via offset.
    let limit = req.limit.unwrap_or(20).clamp(1, 500);
    let offset = req.offset.unwrap_or(0);
    let mut client = client_arc.lock().await;

    let drafts_folder = match client.detect_drafts_folder().await {
        Ok(Some(f)) => f,
        Ok(None) => {
            return error_json(
                "No Drafts folder found on server. Create one manually via your webmail client.",
            );
        }
        Err(e) => return error_json(&client.check_error(e).to_string()),
    };

    match client
        .list_emails(&drafts_folder, limit, offset, false)
        .await
    {
        Ok((emails, total, _)) => serde_json::to_string(&serde_json::json!({
            "account": account_name,
            "folder": drafts_folder,
            "total": total,
            "offset": offset,
            "limit": limit,
            "drafts": emails,
        }))
        .unwrap_or_else(|e| error_json(&e.to_string())),
        Err(e) => error_json(&client.check_error(e).to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_req() -> SearchEmailsRequest {
        SearchEmailsRequest {
            account: None,
            folder: None,
            text: None,
            text_any: None,
            text_all: None,
            from: None,
            from_any: None,
            from_all: None,
            to: None,
            subject: None,
            subject_all: None,
            since: None,
            before: None,
            is_read: None,
            is_flagged: None,
            is_answered: None,
            has_attachments: None,
            min_size: None,
            max_size: None,
            limit: None,
        }
    }

    fn build(req: &SearchEmailsRequest, unicode: bool) -> (String, ClientFilter) {
        build_search_criteria(req, unicode).unwrap()
    }

    fn summary_with(subject: &str, snippet: &str, from: &str) -> EmailSummary {
        EmailSummary {
            uid: 1,
            folder: "INBOX".to_string(),
            message_id: None,
            in_reply_to: None,
            references: vec![],
            from: Some(crate::email::EmailAddress {
                name: None,
                address: from.to_string(),
            }),
            to: vec![],
            to_count: 0,
            cc_count: 0,
            subject: subject.to_string(),
            date: None,
            flags: vec![],
            has_attachments: false,
            snippet: snippet.to_string(),
            thread_message_count: None,
        }
    }

    #[test]
    fn build_search_criteria_no_criteria_errors() {
        let req = empty_req();
        let err = build_search_criteria(&req, true).unwrap_err();
        assert!(err.to_lowercase().contains("at least one"));
    }

    #[test]
    fn build_search_criteria_subject_ascii_quoted() {
        let mut req = empty_req();
        req.subject = Some("Hello".to_string());
        let (criteria, filter) = build(&req, true);
        assert_eq!(criteria, "SUBJECT \"Hello\"");
        assert!(filter.is_empty());
    }

    #[test]
    fn build_search_criteria_subject_unicode_uses_charset_and_literal() {
        let mut req = empty_req();
        req.subject = Some("Bestätigung".to_string());
        let (criteria, filter) = build(&req, true);
        assert!(criteria.starts_with("CHARSET UTF-8 SUBJECT {12+}\r\n"));
        assert!(criteria.ends_with("Bestätigung"));
        assert!(filter.is_empty());
    }

    #[test]
    fn build_search_criteria_combines_multiple_with_space() {
        let mut req = empty_req();
        req.subject = Some("Order".to_string());
        req.is_read = Some(false);
        let (criteria, _) = build(&req, true);
        assert!(criteria.contains("SUBJECT \"Order\""));
        assert!(criteria.contains("UNSEEN"));
    }

    #[test]
    fn build_search_criteria_or_combines_text_any() {
        let mut req = empty_req();
        req.text_any = Some(vec!["foo".to_string(), "bar".to_string()]);
        let (criteria, _) = build(&req, true);
        assert_eq!(criteria, "OR TEXT \"foo\" TEXT \"bar\"");
    }

    #[test]
    fn build_search_criteria_or_skips_empty_list() {
        let mut req = empty_req();
        req.text_any = Some(vec![]);
        assert!(build_search_criteria(&req, true).is_err());
    }

    #[test]
    fn build_search_criteria_dates_emit_imap_format() {
        let mut req = empty_req();
        req.since = Some("2026-01-15".to_string());
        req.before = Some("2026-12-31".to_string());
        let (criteria, _) = build(&req, true);
        assert!(criteria.contains("SINCE 15-Jan-2026"));
        assert!(criteria.contains("BEFORE 31-Dec-2026"));
    }

    #[test]
    fn build_search_criteria_invalid_date_errors() {
        let mut req = empty_req();
        req.since = Some("not-a-date".to_string());
        let err = build_search_criteria(&req, true).unwrap_err();
        assert!(err.contains("Invalid 'since' date"));
    }

    #[test]
    fn build_search_criteria_flag_filters() {
        let mut req = empty_req();
        req.is_flagged = Some(true);
        req.is_answered = Some(false);
        let (criteria, _) = build(&req, true);
        assert!(criteria.contains("FLAGGED"));
        assert!(criteria.contains("UNANSWERED"));
    }

    #[test]
    fn build_search_criteria_from_to_text() {
        let mut req = empty_req();
        req.from = Some("alice@x.com".to_string());
        req.to = Some("bob@x.com".to_string());
        req.text = Some("hello".to_string());
        let (criteria, _) = build(&req, true);
        assert!(criteria.contains("FROM \"alice@x.com\""));
        assert!(criteria.contains("TO \"bob@x.com\""));
        assert!(criteria.contains("TEXT \"hello\""));
    }

    #[test]
    fn build_search_criteria_unicode_in_or_triggers_charset() {
        let mut req = empty_req();
        req.text_any = Some(vec!["foo".to_string(), "Glückwunsch".to_string()]);
        let (criteria, _) = build(&req, true);
        assert!(criteria.starts_with("CHARSET UTF-8 "));
    }

    // ===== Outlook 365 / non-Unicode fallback =====

    #[test]
    fn fallback_diverts_unicode_subject_to_client_filter() {
        let mut req = empty_req();
        req.subject = Some("Bestätigung".to_string());
        req.since = Some("2026-01-01".to_string());
        let (criteria, filter) = build(&req, false);
        // Subject not in IMAP criteria — only date is.
        assert!(!criteria.contains("SUBJECT"));
        assert!(criteria.contains("SINCE 1-Jan-2026"));
        // Stored pre-lowercased per ClientFilter invariant.
        assert_eq!(filter.subject, vec!["bestätigung".to_string()]);
    }

    #[test]
    fn fallback_keeps_ascii_subject_server_side() {
        let mut req = empty_req();
        req.subject = Some("Order".to_string());
        let (criteria, filter) = build(&req, false);
        assert!(criteria.contains("SUBJECT \"Order\""));
        assert!(filter.is_empty());
    }

    #[test]
    fn fallback_requires_date_when_only_unicode_criteria() {
        let mut req = empty_req();
        req.subject = Some("Bestätigung".to_string());
        // No date scope → would need to fetch the entire mailbox.
        let err = build_search_criteria(&req, false).unwrap_err();
        assert!(err.contains("date filter"));
    }

    #[test]
    fn fallback_text_any_with_any_unicode_diverts_entire_group() {
        let mut req = empty_req();
        req.text_any = Some(vec!["foo".to_string(), "Glückwunsch".to_string()]);
        req.since = Some("2026-01-01".to_string());
        let (criteria, filter) = build(&req, false);
        assert!(!criteria.contains("TEXT"));
        assert_eq!(filter.text_any.len(), 1);
        assert_eq!(filter.text_any[0].len(), 2);
    }

    #[test]
    fn fallback_text_any_all_ascii_stays_server_side() {
        let mut req = empty_req();
        req.text_any = Some(vec!["foo".to_string(), "bar".to_string()]);
        let (criteria, filter) = build(&req, false);
        assert!(criteria.contains("OR TEXT \"foo\" TEXT \"bar\""));
        assert!(filter.text_any.is_empty());
    }

    #[test]
    fn client_filter_subject_substring_case_insensitive() {
        // Needles are lowercased per the ClientFilter invariant; the email
        // subject is lowercased inside matches() — so mixed-case subjects
        // still match.
        let mut filter = ClientFilter::default();
        filter.subject.push("bestätigung".to_string());
        let s = summary_with("Bestätigung Ihrer Bestellung", "", "x@y");
        assert!(filter.matches(&s));
    }

    #[test]
    fn client_filter_subject_no_match() {
        let mut filter = ClientFilter::default();
        filter.subject.push("bestätigung".to_string());
        let s = summary_with("Order shipped", "", "x@y");
        assert!(!filter.matches(&s));
    }

    #[test]
    fn client_filter_text_any_or_semantics() {
        let mut filter = ClientFilter::default();
        filter
            .text_any
            .push(vec!["glückwunsch".to_string(), "gratulation".to_string()]);
        let s_match = summary_with("Test", "Herzlichen Glückwunsch zum Geburtstag", "x@y");
        let s_no_match = summary_with("Test", "Nichts davon hier", "x@y");
        assert!(filter.matches(&s_match));
        assert!(!filter.matches(&s_no_match));
    }

    #[test]
    fn client_filter_from_matches_address_or_name() {
        let mut filter = ClientFilter::default();
        filter.from.push("alice".to_string());
        let s = summary_with("Test", "", "alice@example.com");
        assert!(filter.matches(&s));
    }

    #[test]
    fn client_filter_empty_matches_everything() {
        let filter = ClientFilter::default();
        let s = summary_with("Anything", "Whatever", "x@y");
        assert!(filter.matches(&s));
    }

    #[test]
    fn filesystem_safe_filename_keeps_normal_names() {
        assert_eq!(filesystem_safe_filename("Lebenslauf.pdf"), "Lebenslauf.pdf");
        assert_eq!(filesystem_safe_filename("photo.jpg"), "photo.jpg");
        assert_eq!(filesystem_safe_filename("ünïcödë.txt"), "ünïcödë.txt");
    }

    #[test]
    fn filesystem_safe_filename_strips_path_separators() {
        assert_eq!(
            filesystem_safe_filename("../../../etc/passwd"),
            ".._.._.._etc_passwd"
        );
        assert_eq!(filesystem_safe_filename("foo/bar.pdf"), "foo_bar.pdf");
        assert_eq!(filesystem_safe_filename("a\\b.txt"), "a_b.txt");
        // NUL is already removed by `sanitize_external_str` (control char),
        // so it never reaches the path-separator scrub.
        assert_eq!(filesystem_safe_filename("a\0b.txt"), "ab.txt");
    }

    #[test]
    fn filesystem_safe_filename_handles_traversal_only_input() {
        assert_eq!(filesystem_safe_filename(".."), "attachment");
        assert_eq!(filesystem_safe_filename("."), "attachment");
        assert_eq!(filesystem_safe_filename(""), "attachment");
        assert_eq!(filesystem_safe_filename("   "), "attachment");
    }

    #[test]
    fn filesystem_safe_filename_strips_bidi_via_sanitize_external_str() {
        // sanitize_external_str runs first, then path-separator scrub.
        assert_eq!(
            filesystem_safe_filename("invoice\u{202E}gpj.exe"),
            "invoicegpj.exe"
        );
    }

    // ===== group_summaries_by_thread =====

    fn thread_summary(
        uid: u32,
        date: &str,
        message_id: Option<&str>,
        in_reply_to: Option<&str>,
        references: &[&str],
    ) -> EmailSummary {
        let mut s = summary_with("Subject", "snippet", "a@b");
        s.uid = uid;
        s.date = Some(date.to_string());
        s.message_id = message_id.map(String::from);
        s.in_reply_to = in_reply_to.map(String::from);
        s.references = references.iter().map(|r| (*r).to_string()).collect();
        s
    }

    #[test]
    fn group_by_thread_merges_reply_chain() {
        // m1 → m2 (replies to m1) → m3 (replies to m2, references both).
        let m1 = thread_summary(1, "2026-01-01T10:00:00Z", Some("<m1>"), None, &[]);
        let m2 = thread_summary(
            2,
            "2026-01-02T10:00:00Z",
            Some("<m2>"),
            Some("<m1>"),
            &["<m1>"],
        );
        let m3 = thread_summary(
            3,
            "2026-01-03T10:00:00Z",
            Some("<m3>"),
            Some("<m2>"),
            &["<m1>", "<m2>"],
        );
        let grouped = group_summaries_by_thread(vec![m1, m2, m3]);
        assert_eq!(grouped.len(), 1, "should collapse to one thread");
        let rep = &grouped[0];
        assert_eq!(rep.uid, 3, "newest (uid 3) should be the representative");
        assert_eq!(rep.thread_message_count, Some(3));
    }

    #[test]
    fn group_by_thread_keeps_unrelated_threads_separate() {
        let a = thread_summary(1, "2026-01-01T10:00:00Z", Some("<thread-a-1>"), None, &[]);
        let b = thread_summary(2, "2026-01-02T10:00:00Z", Some("<thread-b-1>"), None, &[]);
        let grouped = group_summaries_by_thread(vec![a, b]);
        assert_eq!(grouped.len(), 2);
        assert!(grouped.iter().all(|s| s.thread_message_count == Some(1)));
    }

    #[test]
    fn group_by_thread_preserves_messages_without_message_id() {
        // Two separate message-id-less emails: each becomes its own group.
        let a = thread_summary(1, "2026-01-01T10:00:00Z", None, None, &[]);
        let b = thread_summary(2, "2026-01-02T10:00:00Z", None, None, &[]);
        let grouped = group_summaries_by_thread(vec![a, b]);
        assert_eq!(grouped.len(), 2, "synthetic keys should keep them separate");
    }

    #[test]
    fn group_by_thread_picks_newest_as_representative() {
        let old = thread_summary(10, "2026-01-01T00:00:00Z", Some("<m1>"), None, &[]);
        let new = thread_summary(
            20,
            "2026-05-01T00:00:00Z",
            Some("<m2>"),
            Some("<m1>"),
            &["<m1>"],
        );
        let grouped = group_summaries_by_thread(vec![old, new]);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].uid, 20);
    }

    #[test]
    fn group_by_thread_empty_input_empty_output() {
        let grouped = group_summaries_by_thread(vec![]);
        assert!(grouped.is_empty());
    }

    #[test]
    fn group_by_thread_single_input_annotated() {
        let s = thread_summary(1, "2026-01-01T10:00:00Z", Some("<m1>"), None, &[]);
        let grouped = group_summaries_by_thread(vec![s]);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].thread_message_count, Some(1));
    }
}
