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
        total >= 3,
        "test-server.sh seeds 3 emails, got total={total}"
    );
    assert!(!emails.is_empty());
    // Seeded subjects: "Project Update Q2", "Re: Project Update Q2", "Team Meeting Tomorrow"
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
    let summaries = client
        .search_emails("INBOX", "FROM \"bob@example.com\"", 10)
        .await
        .expect("search_emails failed");
    assert!(
        !summaries.is_empty(),
        "expected at least one email from bob@example.com"
    );
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
