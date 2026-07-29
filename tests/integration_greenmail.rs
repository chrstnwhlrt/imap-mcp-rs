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
//! `test` / `password` and seeds INBOX with three emails (a Q2 Report
//! thread + a standalone meeting invite).
//!
//! Each test creates its own `ImapClient` so they can run in any order,
//! but keep in mind they share the `GreenMail` mailbox — tests that mutate
//! state must clean up after themselves.
//!
//! The test's point is coverage of the wire-protocol / MIME path that
//! pure-Rust unit tests can't exercise.

use imap_mcp_rs::config::{AccountConfig, AuthMethod};
use imap_mcp_rs::imap_client::ImapClient;

fn greenmail_config() -> AccountConfig {
    AccountConfig {
        name: "Greenmail".to_string(),
        host: "127.0.0.1".to_string(),
        port: 3993,
        username: "test".to_string(),
        email: Some("test@localhost".to_string()),
        display_name: None,
        signature_html: None,
        locale: None,
        read_only: false,
        allow_delete: true,
        allow_move: true,
        allow_unsafe_expunge: false,
        accept_invalid_certs: true, // `GreenMail` self-signed cert
        allowed_folders: None,
        auth_method: AuthMethod::Password,
        password: Some("password".to_string()),
        oauth2: None,
    }
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
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    client.disconnect().await;
}

#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn list_folders_contains_inbox_and_drafts() {
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
    client.disconnect().await;
}

#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn get_email_full_content_with_body_text() {
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
    let Some(mut client) = client_or_skip().await else {
        return;
    };
    // `GreenMail` seeds a reply from bob@example.com.
    let (summaries, matched) = client
        .search_emails("INBOX", "FROM \"bob@example.com\"", 10, 0)
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
        .search_emails("INBOX", "ALL", 1, 0)
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
        .search_emails("INBOX", "ALL", 1, 1)
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
async fn get_thread_strict_follows_references_only() {
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

/// Draft replacement is the one new IMAP-interactive path, and IMAP has no
/// update-in-place: `replaces_uid` appends the new version and only then
/// expunges the old one. The ordering is the whole point — a test against a
/// real server is the only way to show that the new draft survives and the
/// old UID is really gone.
#[tokio::test]
#[ignore = "requires a running GreenMail server (./test-server.sh)"]
async fn get_folder_names_lists_every_mailbox_and_serves_repeats_from_cache() {
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

    let moved = client
        .move_emails(&drafts, &[uid], "Trash")
        .await
        .expect("move failed");
    assert_eq!(moved, vec![uid], "server must confirm the moved UID");

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
    client
        .delete_emails(&drafts, &[hard], true)
        .await
        .expect("permanent delete failed");
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

#[tokio::test]
#[ignore = "requires GreenMail via ./test-server.sh"]
async fn draft_replacement_keeps_new_version_and_drops_the_old() {
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
