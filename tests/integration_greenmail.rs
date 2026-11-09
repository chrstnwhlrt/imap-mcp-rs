//! Integration tests against a local `GreenMail` IMAP server.
//!
//! Run the test server first:
//! ```bash
//! ./test-server.sh
//! cargo test --test integration_greenmail -- --ignored --nocapture
//! ```
//!
//! These tests are `#[ignore]` by default so `cargo test` stays green in
//! environments without `GreenMail` (CI, dev boxes). The `./test-server.sh`
//! script spins up a `GreenMail` container on 127.0.0.1:3993 with user
//! `test` / `password`, creates Drafts, Sent, Trash and `B&APw-cher` (Bücher,
//! encoded on the wire) and seeds INBOX with four emails: the "Project Update
//! Q2" thread (original + reply), a second "Project Update Q2" without
//! threading headers (the subject-collision case) and a standalone meeting
//! invite.
//!
//! Each test creates its own `ImapClient` so they can run in any order,
//! but keep in mind they share the `GreenMail` mailbox — tests that mutate
//! state must clean up after themselves, and they run one at a time (see
//! [`serial`]).
//!
//! The test's point is coverage of the wire-protocol / MIME path that
//! pure-Rust unit tests can't exercise.

use imap_mcp_rs::config::{AccountConfig, AuthMethod};
use imap_mcp_rs::imap_client::{BodyTextFilter, ImapClient, PostFetchFilter};

fn greenmail_config() -> AccountConfig {
    AccountConfig {
        name: "Greenmail".to_string(),
        host: "127.0.0.1".to_string(),
        port: 3993,
        username: "test".to_string(),
        email: Some("test@localhost".to_string()),
        display_name: None,
        signature_html: None,
        signature_text: None,
        message_id_domain: None,
        locale: None,
        read_only: false,
        allow_delete: true,
        allow_move: true,
        allow_flag_change: true,
        allow_unsafe_expunge: false,
        accept_invalid_certs: true, // `GreenMail` self-signed cert
        allowed_folders: None,
        auth_method: AuthMethod::Password,
        password: Some("password".to_string()),
        oauth2: None,
    }
}

/// One test at a time against the shared mailbox. `GreenMail` does not keep
/// a per-session view of a folder the way RFC 3501 requires: a FETCH that
/// races another session's EXPUNGE fails on the server ("No such message
/// with uid …") and comes back short, so parallel tests failed at random.
/// Every test holds this guard for its whole run, which keeps the suite
/// serial however the harness schedules it.
async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    static MAILBOX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    MAILBOX.lock().await
}

/// Skip helper: if the test server isn't reachable, produce a clear
/// `#[ignore]`-appropriate message rather than a cryptic connect error.
async fn client_or_skip() -> Option<ImapClient> {
    let mut client = ImapClient::new(greenmail_config());
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), client.connect()).await;
    if matches!(result, Ok(Ok(()))) {
        Some(client)
    } else {
        eprintln!("GreenMail not reachable at 127.0.0.1:3993 — run ./test-server.sh first");
        None
    }
}

#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn connect_and_disconnect() {
    let _serial = serial().await;
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    client.disconnect().await;
}

#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn list_folders_contains_inbox_and_drafts() {
    let _serial = serial().await;
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let folders = client.list_folders().await.expect("list_folders failed");
    let names: Vec<&str> = folders.iter().map(|f| f.name.as_str()).collect();
    assert!(names.contains(&"INBOX"), "INBOX missing: {names:?}");
    assert!(
        names.iter().any(|n| n.eq_ignore_ascii_case("Drafts")),
        "Drafts missing: {names:?}"
    );
    // Role detection: `GreenMail`'s "Drafts" / "Sent" / "Trash" should tag.
    let drafts = folders
        .iter()
        .find(|f| f.name.eq_ignore_ascii_case("Drafts"))
        .expect("Drafts folder");
    assert_eq!(drafts.role, Some("drafts"), "drafts role not detected");
    client.disconnect().await;
}

#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn list_emails_inbox_returns_seeded_messages() {
    let _serial = serial().await;
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let (emails, total, _matched) = client
        .list_emails("INBOX", 50, 0, false)
        .await
        .expect("list_emails failed");
    assert!(
        total >= 4,
        "test-server.sh seeds 4 emails, got total={total}"
    );
    assert!(!emails.is_empty());
    // Seeded subjects: "Project Update Q2" (×2), "Re: Project Update Q2",
    //                  "Team Meeting Tomorrow"
    let subjects: Vec<&str> = emails.iter().map(|e| e.subject.as_str()).collect();
    assert!(
        subjects.iter().any(|s| s.contains("Project Update Q2")),
        "expected Q2 subject in {subjects:?}"
    );
    // Without a sub-day bound, `internal_date` stays absent — next to
    // `date` it would be near-redundant noise in every listing row.
    assert!(
        emails.iter().all(|e| e.internal_date.is_none()),
        "internal_date must only appear under a sub-day bound"
    );
    // `\Recent` is session-scoped server bookkeeping that reads like "new
    // for me" — it must never surface in flags. GreenMail does set it on a
    // fresh session, which makes this the one place to prove the filter.
    assert!(
        emails
            .iter()
            .all(|e| !e.flags.iter().any(|f| f == "\\Recent")),
        "\\Recent leaked into flags: {:?}",
        emails.iter().map(|e| &e.flags).collect::<Vec<_>>()
    );
    // Dates are kept UTC-normalized for sorting (responses show them in the
    // machine's local time): every value ends in Z, none carries a sender
    // offset.
    assert!(
        emails
            .iter()
            .filter_map(|e| e.date.as_deref())
            .all(|d| d.ends_with('Z')),
        "non-UTC date leaked: {:?}",
        emails
            .iter()
            .filter_map(|e| e.date.as_deref())
            .collect::<Vec<_>>()
    );
    client.disconnect().await;
}

#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn search_time_bounds_cut_on_internaldate_over_the_wire() {
    let _serial = serial().await;
    // The sub-day part of since/before cuts on INTERNALDATE, resolved on a
    // lightweight (UID INTERNALDATE) round before counting and paging. The
    // seeds arrived when the container started, i.e. within the last hour
    // of running this test — so "since an hour ago" must keep them all and
    // "before an hour ago" must drop them all, with `matched` reporting
    // the count after the time cut (rows a caller can actually get).
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        .cast_signed();

    let recent = PostFetchFilter {
        body: BodyTextFilter::EMPTY,
        internal_since_unix: Some(now - 3600),
        internal_before_unix: None,
    };
    let (rows, matched) = client
        .search_emails("INBOX", "ALL", 50, 0, &recent)
        .await
        .expect("search failed");
    assert!(rows.len() >= 4, "seeds arrived within the hour: {rows:?}");
    assert!(matched >= 4);
    // Under a sub-day bound every row carries the arrival time the cut ran
    // on, kept in UTC like `date` (responses show it in local time), so the
    // caller can verify the decision.
    assert!(
        rows.iter()
            .all(|r| r.internal_date.as_deref().is_some_and(|d| d.ends_with('Z'))),
        "rows under a sub-day bound must carry internal_date: {rows:?}"
    );

    let ancient = PostFetchFilter {
        body: BodyTextFilter::EMPTY,
        internal_since_unix: None,
        internal_before_unix: Some(now - 3600),
    };
    let (rows, matched) = client
        .search_emails("INBOX", "ALL", 50, 0, &ancient)
        .await
        .expect("search failed");
    assert!(
        rows.is_empty(),
        "nothing arrived more than an hour ago: {rows:?}"
    );
    assert_eq!(matched, 0, "the time cut runs before the count");
    client.disconnect().await;
}

#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn get_email_full_content_with_body_text() {
    let _serial = serial().await;
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let (emails, _, _) = client
        .list_emails("INBOX", 50, 0, false)
        .await
        .expect("list_emails failed");
    let meeting = emails
        .iter()
        .find(|e| e.subject.contains("Team Meeting"))
        .expect("Team Meeting email seeded by test-server.sh");
    let full = client
        .get_email("INBOX", meeting.uid)
        .await
        .expect("get_email failed")
        .expect("email present");
    assert!(
        full.body_text.contains("room 4B"),
        "body_text missing expected content: {}",
        full.body_text
    );
    client.disconnect().await;
}

#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn search_emails_from_bob() {
    let _serial = serial().await;
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    // `GreenMail` seeds a reply from bob@example.com.
    let (summaries, matched) = client
        .search_emails(
            "INBOX",
            "FROM \"bob@example.com\"",
            10,
            0,
            &PostFetchFilter::EMPTY,
        )
        .await
        .expect("search_emails failed");
    assert!(
        !summaries.is_empty(),
        "expected at least one email from bob@example.com"
    );
    assert_eq!(
        matched as usize,
        summaries.len(),
        "below the limit, the match count must equal what was returned"
    );

    // The contract that matters: a capped search still reports how many
    // messages actually matched, so a caller can tell a partial answer from a
    // complete one. Reporting the delivered count here would make "the newest
    // one of several" indistinguishable from "the only one".
    let (capped, capped_matched) = client
        .search_emails("INBOX", "ALL", 1, 0, &PostFetchFilter::EMPTY)
        .await
        .expect("capped search failed");
    assert_eq!(capped.len(), 1, "limit must cap what is delivered");
    assert!(
        capped_matched > 1,
        "seeded INBOX has more than one message, so matched ({capped_matched}) must exceed the cap"
    );

    // Paging is what makes the match count actionable: knowing more exist is
    // useless if the rest cannot be reached. Page two must continue where page
    // one stopped, not repeat it.
    let (page_two, page_two_matched) = client
        .search_emails("INBOX", "ALL", 1, 1, &PostFetchFilter::EMPTY)
        .await
        .expect("offset search failed");
    assert_eq!(page_two.len(), 1, "second page must deliver its row");
    assert_eq!(
        page_two_matched, capped_matched,
        "the match count describes the whole result, not the page"
    );
    assert_ne!(
        page_two[0].uid, capped[0].uid,
        "offset must advance past the first page, not repeat it"
    );
    client.disconnect().await;
}

#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn search_emails_applies_the_body_filter_to_full_bodies() {
    let _serial = serial().await;
    // The client-side fallback path (Outlook 365 diverts non-ASCII terms
    // there) must filter on each message's FULL body over the wire — the
    // unit tests cover it only with synthetic strings. "room 4B" sits in the
    // seeded meeting mail's body text and in no other message.
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let filter = PostFetchFilter {
        body: BodyTextFilter {
            all: vec!["room 4b".to_string()],
            any: vec![],
        },
        internal_since_unix: None,
        internal_before_unix: None,
    };
    let (rows, matched) = client
        .search_emails("INBOX", "ALL", 50, 0, &filter)
        .await
        .expect("filtered search failed");
    assert_eq!(rows.len(), 1, "exactly the meeting mail should pass");
    assert!(
        rows[0].subject.contains("Team Meeting"),
        "wrong mail passed the body filter: {}",
        rows[0].subject
    );
    // `matched` stays the server-side count (the documented upper bound) —
    // the client-side filter must not shrink it to the delivered rows.
    assert!(
        matched >= 4,
        "matched must report what the server saw, got {matched}"
    );

    // A term no seeded body carries filters everything out, while `matched`
    // still says the server had candidates.
    let none = PostFetchFilter {
        body: BodyTextFilter {
            all: vec!["definitely-not-in-any-seeded-body".to_string()],
            any: vec![],
        },
        internal_since_unix: None,
        internal_before_unix: None,
    };
    let (rows, matched) = client
        .search_emails("INBOX", "ALL", 50, 0, &none)
        .await
        .expect("filtered search failed");
    assert!(rows.is_empty(), "no body carries the term: {rows:?}");
    assert!(matched >= 4);
    client.disconnect().await;
}

#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn get_thread_strict_follows_references_only() {
    let _serial = serial().await;
    // `test-server.sh` seeds the Q2 thread as msg1 (alice, "Project Update Q2"
    // with Message-ID) + msg2 (bob, "Re: Project Update Q2" with In-Reply-To
    // + References → msg1), plus a *separate* msg4 (charlie) sharing the
    // exact subject "Project Update Q2" but without References. strict=true
    // (the default) must NOT merge msg4 into msg1's thread.
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let (emails, _, _) = client
        .list_emails("INBOX", 50, 0, false)
        .await
        .expect("list_emails failed");
    // Pick the alice mail as the thread anchor — it's the one with the
    // References chain. There are now two "Project Update Q2" subjects
    // (alice + charlie); disambiguate by sender.
    let alice = emails
        .iter()
        .find(|e| {
            e.subject == "Project Update Q2"
                && e.from.as_ref().is_some_and(|a| a.address.contains("alice"))
        })
        .expect("alice's Q2 mail seeded by test-server.sh");

    let thread = client
        .get_thread("INBOX", alice.uid, true)
        .await
        .expect("get_thread(strict=true) failed");

    assert_eq!(
        thread.len(),
        2,
        "strict=true must return exactly the References chain (msg1 + msg2), \
         got {} messages: {:?}",
        thread.len(),
        thread.iter().map(|e| &e.subject).collect::<Vec<_>>()
    );
    // charlie's collision-subject mail must NOT be in the thread.
    assert!(
        !thread.iter().any(|e| e
            .from
            .as_ref()
            .is_some_and(|a| a.address.contains("charlie"))),
        "strict=true must not merge subject-kernel collisions — \
         charlie's mail leaked in"
    );
    // msg1 is older than msg2 → chronological sort puts it first.
    assert!(
        thread[0]
            .from
            .as_ref()
            .is_some_and(|a| a.address.contains("alice")),
        "chronological sort: alice's original should come first"
    );
    client.disconnect().await;
}

#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn get_thread_non_strict_merges_subject_collisions() {
    let _serial = serial().await;
    // Opposite of the strict test: strict=false enables the subject-kernel
    // fallback, so charlie's mail (same subject, no References) DOES get
    // pulled in. This is the Lotus-Notes-friendly mode.
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let (emails, _, _) = client
        .list_emails("INBOX", 50, 0, false)
        .await
        .expect("list_emails failed");
    let alice = emails
        .iter()
        .find(|e| {
            e.subject == "Project Update Q2"
                && e.from.as_ref().is_some_and(|a| a.address.contains("alice"))
        })
        .expect("alice's Q2 mail seeded by test-server.sh");

    let thread = client
        .get_thread("INBOX", alice.uid, false)
        .await
        .expect("get_thread(strict=false) failed");

    assert!(
        thread.len() >= 3,
        "strict=false should pull in charlie's subject-collision mail \
         (expected >= 3, got {}): {:?}",
        thread.len(),
        thread.iter().map(|e| &e.subject).collect::<Vec<_>>()
    );
    assert!(
        thread.iter().any(|e| e
            .from
            .as_ref()
            .is_some_and(|a| a.address.contains("charlie"))),
        "strict=false must include charlie's subject-collision mail"
    );
    client.disconnect().await;
}

#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn get_thread_standalone_returns_single_message() {
    let _serial = serial().await;
    // The Team Meeting mail has no References and a unique subject — even
    // strict=false's subject-fallback shouldn't find anyone to merge.
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let (emails, _, _) = client
        .list_emails("INBOX", 50, 0, false)
        .await
        .expect("list_emails failed");
    let meeting = emails
        .iter()
        .find(|e| e.subject.contains("Team Meeting"))
        .expect("Team Meeting mail seeded by test-server.sh");

    let thread = client
        .get_thread("INBOX", meeting.uid, true)
        .await
        .expect("get_thread failed");

    assert_eq!(
        thread.len(),
        1,
        "standalone mail should return single-element thread, got {}",
        thread.len()
    );
    assert_eq!(thread[0].uid, meeting.uid);
    client.disconnect().await;
}

#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn mark_flags_intersects_against_input() {
    let _serial = serial().await;
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let (emails, _, _) = client
        .list_emails("INBOX", 1, 0, false)
        .await
        .expect("list_emails failed");
    let Some(first) = emails.first() else {
        eprintln!("no emails to test mark_flags against");
        return;
    };
    // Pass a real UID mixed with a bogus one — only the real one should come back.
    let succeeded = client
        .mark_flags("INBOX", &[first.uid, 99_999_999], "\\Seen", true)
        .await
        .expect("mark_flags failed");
    assert_eq!(
        succeeded,
        vec![first.uid],
        "mark_flags should only echo server-acknowledged UIDs"
    );
    // Restore state.
    let _ = client
        .mark_flags("INBOX", &[first.uid], "\\Seen", false)
        .await;
    client.disconnect().await;
}

#[tokio::test]
#[ignore = "requires a running GreenMail server (./test-server.sh)"]
async fn get_folder_names_lists_every_mailbox_and_serves_repeats_from_cache() {
    let _serial = serial().await;
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let first = client
        .get_folder_names()
        .await
        .expect("listing folder names failed");
    for expected in ["INBOX", "Drafts", "Sent", "Trash"] {
        assert!(
            first.iter().any(|n| n == expected),
            "{expected} missing from {first:?}"
        );
    }

    // The result is cached for the session (IMAP LIST runs once). A cache that
    // returned something different on the second call would make folder
    // resolution depend on how often it was asked.
    let second = client.get_folder_names().await.expect("second call failed");
    assert_eq!(first, second, "cached listing must match the first");
    client.disconnect().await;
}

#[tokio::test]
#[ignore = "requires a running GreenMail server (./test-server.sh)"]
async fn move_emails_lands_the_message_in_the_target_and_clears_the_source() {
    let _serial = serial().await;
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let drafts = client
        .detect_drafts_folder()
        .await
        .expect("drafts detection failed")
        .expect("no Drafts folder on the test server");

    // A move is COPY + \Deleted + UID EXPUNGE, not an atomic operation. The
    // failure that matters is a half-done one: present in both folders (a
    // duplicate) or in neither (data loss). Assert both ends explicitly.
    let subject = "Move probe";
    client
        .save_draft(
            format!("From: test@localhost\r\nTo: bob@localhost\r\nMessage-ID: <move@probe>\r\nSubject: {subject}\r\n\r\nbody\r\n")
                .as_bytes(),
        )
        .await
        .expect("saving the probe failed");
    let (before, _, _) = client
        .list_emails(&drafts, 50, 0, false)
        .await
        .expect("listing failed");
    let uid = before
        .iter()
        .find(|e| e.subject == subject)
        .expect("probe not in Drafts")
        .uid;

    // Pre-flag the probe \Deleted, as a parallel client (phone, webmail)
    // would: the move's own +FLAGS STORE is then a no-op for it, and RFC
    // 7162 permits omitting the FETCH acknowledgement for no-ops — which is
    // why `succeeded` is existence-verified up front rather than derived
    // from STORE responses. A pre-flagged message that moves must be
    // reported as moved; an under-report here invites a retry, and a
    // retried move duplicates the message.
    client
        .mark_flags(&drafts, &[uid], "\\Deleted", true)
        .await
        .expect("pre-flagging failed");

    // A stale UID rides along: it does not exist, and the response must not
    // echo it as moved — succeeded is what existed when the operation ran.
    let moved = client
        .move_emails(&drafts, &[uid, 99_999_999], "Trash")
        .await
        .expect("move failed");
    assert_eq!(
        moved,
        vec![uid],
        "the pre-flagged message must be reported, the stale UID must not"
    );

    let (source_after, _, _) = client
        .list_emails(&drafts, 50, 0, false)
        .await
        .expect("listing source failed");
    assert!(
        !source_after.iter().any(|e| e.subject == subject),
        "message still in the source folder — a move that copies is a duplicate"
    );
    let (target_after, _, _) = client
        .list_emails("Trash", 50, 0, false)
        .await
        .expect("listing target failed");
    let landed = target_after
        .iter()
        .find(|e| e.subject == subject)
        .expect("message reached neither folder — that is data loss");
    // UIDs are per-folder; the target assigns its own.
    let _ = landed.uid;

    let _ = client.delete_emails("Trash", &[landed.uid], true).await;
    client.disconnect().await;
}

#[tokio::test]
#[ignore = "requires a running GreenMail server (./test-server.sh)"]
async fn delete_emails_moves_to_trash_by_default_and_expunges_when_asked() {
    let _serial = serial().await;
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let drafts = client
        .detect_drafts_folder()
        .await
        .expect("drafts detection failed")
        .expect("no Drafts folder on the test server");

    let make = |id: &str, subject: &str| {
        format!("From: test@localhost\r\nTo: bob@localhost\r\nMessage-ID: <{id}@del>\r\nSubject: {subject}\r\n\r\nbody\r\n")
            .into_bytes()
    };
    let uid_of = |rows: &[imap_mcp_rs::email::EmailSummary], s: &str| {
        rows.iter().find(|e| e.subject == s).map(|e| e.uid)
    };

    client
        .save_draft(&make("soft", "Delete probe soft"))
        .await
        .expect("save failed");
    client
        .save_draft(&make("hard", "Delete probe hard"))
        .await
        .expect("save failed");
    let (rows, _, _) = client
        .list_emails(&drafts, 50, 0, false)
        .await
        .expect("listing failed");
    let soft = uid_of(&rows, "Delete probe soft").expect("soft probe missing");
    let hard = uid_of(&rows, "Delete probe hard").expect("hard probe missing");

    // Default delete is recoverable: the message must be findable in Trash,
    // otherwise "moves to Trash" is a promise the tool does not keep.
    client
        .delete_emails(&drafts, &[soft], false)
        .await
        .expect("soft delete failed");
    let (trash, _, _) = client
        .list_emails("Trash", 50, 0, false)
        .await
        .expect("listing trash failed");
    let recovered = uid_of(&trash, "Delete probe soft")
        .expect("non-permanent delete must leave the message recoverable in Trash");

    // Permanent delete expunges in place — gone from the source, and not
    // quietly relocated to Trash either.
    let deleted = client
        .delete_emails(&drafts, &[hard, 99_999_999], true)
        .await
        .expect("permanent delete failed");
    assert_eq!(
        deleted,
        vec![hard],
        "a stale UID must not be echoed as deleted"
    );
    let (after, _, _) = client
        .list_emails(&drafts, 50, 0, false)
        .await
        .expect("listing failed");
    assert!(
        uid_of(&after, "Delete probe hard").is_none(),
        "permanently deleted message still in the folder"
    );
    let (trash2, _, _) = client
        .list_emails("Trash", 50, 0, false)
        .await
        .expect("listing trash failed");
    assert!(
        uid_of(&trash2, "Delete probe hard").is_none(),
        "permanent delete must not silently route through Trash"
    );

    let _ = client.delete_emails("Trash", &[recovered], true).await;
    client.disconnect().await;
}

#[tokio::test]
#[ignore = "requires a running GreenMail server (./test-server.sh)"]
async fn fetch_raw_returns_the_verbatim_message_and_none_for_unknown_uids() {
    let _serial = serial().await;
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let (rows, _, _) = client
        .list_emails("INBOX", 5, 0, false)
        .await
        .expect("listing failed");
    let first = rows.first().expect("seeded INBOX is empty");

    // The attachment path decodes MIME from these bytes, so they have to be
    // the message as the server stores it — headers included, not a rendering.
    let raw = client
        .fetch_raw("INBOX", first.uid)
        .await
        .expect("fetch_raw failed")
        .expect("no body for an existing UID");
    let text = String::from_utf8_lossy(&raw);
    assert!(text.contains("Subject:"), "raw message must carry headers");
    assert!(
        text.contains(&first.subject),
        "raw message must be the one that was asked for"
    );

    // A stale UID must be reported as absent rather than as an error, so a
    // caller can tell "gone" from "broken".
    let missing = client
        .fetch_raw("INBOX", 999_999)
        .await
        .expect("a missing UID is not an error");
    assert!(missing.is_none(), "unknown UID must yield None");
    client.disconnect().await;
}

#[tokio::test]
#[ignore = "requires a running GreenMail server (./test-server.sh)"]
async fn save_draft_reports_the_uid_it_landed_on() {
    let _serial = serial().await;
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let drafts = client
        .detect_drafts_folder()
        .await
        .expect("drafts detection failed")
        .expect("no Drafts folder on the test server");

    // APPEND does not hand the UID back through async-imap, so it is looked
    // up by Message-ID. This proves the lookup finds the message we just
    // wrote — and the right one, with a near-identical draft alongside it.
    let with_id = |id: &str, subject: &str| {
        format!(
            "From: test@localhost\r\nTo: bob@localhost\r\nMessage-ID: <{id}@uidtest>\r\nSubject: {subject}\r\n\r\nbody\r\n"
        )
        .into_bytes()
    };

    let decoy = client
        .save_draft(&with_id("decoy", "UID probe decoy"))
        .await
        .expect("saving the decoy failed");
    let uid = client
        .save_draft(&with_id("target", "UID probe target"))
        .await
        .expect("saving the draft failed")
        .expect("save_draft reported no UID");

    let fetched = client
        .get_email(&drafts, uid)
        .await
        .expect("fetching the reported UID failed")
        .expect("the reported UID does not resolve to a message");
    assert_eq!(
        fetched.subject, "UID probe target",
        "reported UID points at the wrong message"
    );

    // A message without a Message-ID cannot be located — that must degrade to
    // "saved, UID unknown" rather than failing the save.
    let anonymous = client
        .save_draft(b"From: test@localhost\r\nTo: bob@localhost\r\nSubject: UID probe anonymous\r\n\r\nbody\r\n")
        .await
        .expect("saving a draft without Message-ID must still succeed");
    assert!(
        anonymous.is_none(),
        "expected no UID for a message without Message-ID, got {anonymous:?}"
    );

    // Clean up — the mailbox is shared between tests.
    let (all, _, _) = client
        .list_emails(&drafts, 50, 0, false)
        .await
        .expect("listing drafts failed");
    let ours: Vec<u32> = all
        .iter()
        .filter(|e| e.subject.starts_with("UID probe "))
        .map(|e| e.uid)
        .collect();
    let _ = decoy;
    let _ = client.delete_draft(&ours).await;
    client.disconnect().await;
}

/// IMAP has no update-in-place: `replaces_uid` appends the new version and
/// only then expunges the old one. The ordering is the whole point — a test
/// against a real server is the only way to show that the new draft survives
/// and the old UID is really gone.
#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn draft_replacement_keeps_new_version_and_drops_the_old() {
    let _serial = serial().await;
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let drafts = client
        .detect_drafts_folder()
        .await
        .expect("drafts detection failed")
        .expect("no Drafts folder on the test server");

    let message = |subject: &str| {
        format!("From: test@localhost\r\nTo: bob@localhost\r\nSubject: {subject}\r\n\r\nbody\r\n")
            .into_bytes()
    };

    // First version.
    client
        .save_draft(&message("Replacement test v1"))
        .await
        .expect("saving the first draft failed");
    let (before, _, _) = client
        .list_emails(&drafts, 50, 0, false)
        .await
        .expect("listing drafts failed");
    let old_uid = before
        .iter()
        .find(|e| e.subject == "Replacement test v1")
        .expect("first draft not found in Drafts")
        .uid;

    // Second version, then remove the first — the order `replaces_uid` uses.
    client
        .save_draft(&message("Replacement test v2"))
        .await
        .expect("saving the replacement failed");
    let removed = client
        .delete_draft(&[old_uid])
        .await
        .expect("removing the replaced draft failed");
    assert_eq!(
        removed,
        vec![old_uid],
        "server did not report the old UID gone"
    );

    // Deleting it again (or any unknown UID) must come back empty — this is
    // what turns note_replacement's "already gone" case into a warning
    // instead of a false replaced_uid.
    let again = client
        .delete_draft(&[old_uid])
        .await
        .expect("re-deleting an absent draft is not an error");
    assert!(
        again.is_empty(),
        "absent draft reported as deleted: {again:?}"
    );

    let (after, _, _) = client
        .list_emails(&drafts, 50, 0, false)
        .await
        .expect("listing drafts failed");
    let subjects: Vec<&str> = after.iter().map(|e| e.subject.as_str()).collect();
    assert!(
        subjects.contains(&"Replacement test v2"),
        "replacement draft missing: {subjects:?}"
    );
    assert!(
        !subjects.contains(&"Replacement test v1"),
        "replaced draft still present: {subjects:?}"
    );

    // Clean up after ourselves — the mailbox is shared between tests.
    if let Some(new_uid) = after
        .iter()
        .find(|e| e.subject == "Replacement test v2")
        .map(|e| e.uid)
    {
        let _ = client.delete_draft(&[new_uid]).await;
    }
    client.disconnect().await;
}

/// A folder stays selected between calls, and the server reports mail
/// arriving there only as an untagged EXISTS alongside a later command —
/// which async-imap queues where nothing reads it. `list_emails` used to page
/// by the count of its original SELECT and never showed what arrived after
/// it, for as long as the folder stayed selected.
///
/// Only the listing is asserted, not `total`: other tests append to and
/// expunge from the same Drafts folder while this one runs, so an exact
/// before/after count would race them.
#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn list_emails_sees_mail_that_arrived_after_the_folder_was_selected() {
    let _serial = serial().await;
    let Some(mut reader) = client_or_skip().await else {
        return;
    };
    let Some(mut writer) = client_or_skip().await else {
        return;
    };
    let drafts = reader
        .detect_drafts_folder()
        .await
        .expect("drafts detection failed")
        .expect("no Drafts folder on the test server");

    // Selects Drafts in the reader's session, which then stays selected.
    reader
        .list_emails(&drafts, 50, 0, false)
        .await
        .expect("first listing failed");

    // Arrives through ANOTHER session while the reader keeps Drafts selected.
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let subject = format!("Arrival probe {tag}");
    let uid = writer
        .save_draft(
            format!(
                "From: test@localhost\r\nTo: bob@localhost\r\nMessage-ID: <{tag}@arrival>\r\n\
                 Subject: {subject}\r\n\r\nbody\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("saving the probe failed");

    let (rows, _, _) = reader
        .list_emails(&drafts, 50, 0, false)
        .await
        .expect("second listing failed");
    assert!(
        rows.iter().any(|e| e.subject == subject),
        "mail that arrived after the SELECT is missing from the listing"
    );

    if let Some(uid) = uid {
        let _ = writer.delete_draft(&[uid]).await;
    }
    reader.disconnect().await;
    writer.disconnect().await;
}

/// The "full thread" must include sibling branches — replies to an ancestor
/// rather than to the message `get_thread` was called on. Those reference
/// neither that message nor anything it references except the root.
#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn get_thread_includes_sibling_branches() {
    let _serial = serial().await;
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let drafts = client
        .detect_drafts_folder()
        .await
        .expect("drafts detection failed")
        .expect("no Drafts folder on the test server");

    let tag = uuid::Uuid::new_v4().simple().to_string();
    let root = format!("<root-{tag}@thread>");
    let message = |id: &str, subject: &str, parent: Option<&str>| {
        let threading = parent.map_or_else(String::new, |p| {
            format!("In-Reply-To: {p}\r\nReferences: {p}\r\n")
        });
        format!(
            "From: test@localhost\r\nTo: bob@localhost\r\nMessage-ID: {id}\r\n{threading}\
             Subject: {subject}\r\n\r\nbody\r\n"
        )
        .into_bytes()
    };
    let mut uids = Vec::new();
    for (id, subject, parent) in [
        (root.clone(), format!("Branch root {tag}"), None),
        (
            format!("<a-{tag}@thread>"),
            format!("Re: Branch A {tag}"),
            Some(root.as_str()),
        ),
        (
            format!("<b-{tag}@thread>"),
            format!("Re: Branch B {tag}"),
            Some(root.as_str()),
        ),
    ] {
        let uid = client
            .save_draft(&message(&id, &subject, parent))
            .await
            .expect("saving a thread message failed")
            .expect("no UID for a message with a Message-ID");
        uids.push(uid);
    }

    // Called on branch A: branch B is its sibling.
    let thread = client
        .get_thread(&drafts, uids[1], true)
        .await
        .expect("get_thread failed");
    let subjects: Vec<&str> = thread.iter().map(|e| e.subject.as_str()).collect();
    assert_eq!(
        thread.len(),
        3,
        "root + both branches expected: {subjects:?}"
    );
    assert!(
        subjects.iter().any(|s| s.contains("Branch B")),
        "sibling branch missing: {subjects:?}"
    );

    let _ = client.delete_draft(&uids).await;
    client.disconnect().await;
}

/// Moving a folder into itself only re-creates its messages under new UIDs,
/// and a non-permanent delete from the Trash would leave them where they are
/// while reporting them deleted — both are refused before anything runs.
#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn moves_that_cannot_move_anything_are_refused() {
    let _serial = serial().await;
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let (rows, _, _) = client
        .list_emails("INBOX", 1, 0, false)
        .await
        .expect("listing failed");
    let uid = rows.first().expect("seeded INBOX is empty").uid;

    let err = client
        .move_emails("INBOX", &[uid], "INBOX")
        .await
        .expect_err("a move into the source folder must be refused");
    assert!(err.to_string().contains("same"), "{err}");

    let err = client
        .delete_emails("Trash", &[uid], false)
        .await
        .expect_err("a soft delete from the Trash must be refused");
    assert!(err.to_string().contains("permanent"), "{err}");

    // Nothing moved: the message is still where it was, under its UID.
    let (after, _, _) = client
        .list_emails("INBOX", 1, 0, false)
        .await
        .expect("listing failed");
    assert_eq!(after.first().map(|e| e.uid), Some(uid));
    client.disconnect().await;
}

/// A folder that does not exist is refused with the names of those that do,
/// for reading as for a move target, and the move refuses before anything
/// is copied.
#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn missing_folders_are_refused_with_the_existing_names() {
    let _serial = serial().await;
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    let err = client
        .list_emails("Papierkorb", 10, 0, false)
        .await
        .expect_err("a missing folder must be refused")
        .to_string();
    assert!(err.contains("\"Papierkorb\" does not exist"), "{err}");
    assert!(
        err.contains("Folders: INBOX, B&APw-cher (Bücher), Drafts"),
        "{err}"
    );

    let (rows, _, _) = client
        .list_emails("INBOX", 1, 0, false)
        .await
        .expect("listing failed");
    let uid = rows.first().expect("seeded INBOX is empty").uid;
    let err = client
        .move_emails("INBOX", &[uid], "Archiv")
        .await
        .expect_err("a missing target must be refused")
        .to_string();
    assert!(err.contains("\"Archiv\" does not exist"), "{err}");
    let (after, _, _) = client
        .list_emails("INBOX", 1, 0, false)
        .await
        .expect("listing failed");
    assert_eq!(
        after.first().map(|e| e.uid),
        Some(uid),
        "nothing may have moved"
    );
    client.disconnect().await;
}

/// Folder names as models pass them resolve to the wire name: another case,
/// the readable form of an encoded name, a role word. A name nothing fits
/// passes through when it is ASCII (the server's refusal follows) and is
/// refused with the folder list otherwise.
#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn folder_names_resolve_as_people_say_them() {
    let _serial = serial().await;
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    for (requested, wire) in [
        ("inbox", "INBOX"),
        ("trash", "Trash"),
        ("Bücher", "B&APw-cher"),
        ("BÜCHER", "B&APw-cher"),
        ("Papierkorb", "Trash"),
        ("Entwürfe", "Drafts"),
        ("Gesendete Elemente", "Sent"),
    ] {
        let resolved = client
            .resolve_folder(requested)
            .await
            .unwrap_or_else(|e| panic!("{requested}: {e}"));
        assert_eq!(resolved, wire, "{requested}");
    }
    let passed = client
        .resolve_folder("NoSuchFolder")
        .await
        .expect("an ASCII name passes through");
    assert_eq!(passed, "NoSuchFolder");
    let err = client
        .resolve_folder("Größe")
        .await
        .expect_err("a name outside ASCII that fits nothing is refused")
        .to_string();
    assert!(err.contains("\"Größe\" does not exist"), "{err}");
    assert!(err.contains("Folders: INBOX"), "{err}");

    // The resolved name selects: the encoded folder reads like any other.
    let books = client.resolve_folder("Bücher").await.expect("resolved");
    let (rows, total, _) = client
        .list_emails(&books, 5, 0, false)
        .await
        .expect("listing the encoded folder failed");
    assert!(rows.is_empty() && total == 0, "{rows:?}");
    client.disconnect().await;
}
