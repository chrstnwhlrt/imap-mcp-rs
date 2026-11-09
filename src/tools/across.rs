//! Reads over several accounts.
//!
//! With several accounts configured, `list_folders`, `list_emails`,
//! `search_emails` and `list_drafts` read all of them when a call names
//! none: "how many unread emails do I have?" means every account, and a
//! model asked it should not have to know there are several. The accounts
//! are read at once, each over its own connection. The answer says before
//! its rows what it covers, since a small model often answers from the start
//! of a result: the totals, the counts per account, the accounts that failed
//! (with `retryable`) and the ones without the folder asked for. The rows
//! name their account, which a call on one of them passes back.

use std::future::Future;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use super::{Target, error_fields};
use crate::config::AccountConfig;
use crate::imap_client::{ImapClient, folder_names_shown};

/// How one account answered a read over several.
pub enum Answer<T> {
    Done(T),
    /// The account has no such folder; its folder names, for the error when
    /// no account has it.
    NoFolder(Vec<String>),
    /// The raw error.
    Failed(String),
}

/// The answers of all accounts, in the order of the configuration.
pub struct Answers<T> {
    /// The accounts that answered, with their name.
    pub done: Vec<(String, T)>,
    failed: Vec<(String, String)>,
    missing: Vec<(String, Vec<String>)>,
}

/// Run `read` on every account at once and sort the answers.
pub async fn read_each<'t, T, F, Fut>(targets: &[Target<'t>], read: F) -> Answers<T>
where
    F: Fn(&'t AccountConfig, Arc<Mutex<ImapClient>>) -> Fut,
    Fut: Future<Output = Answer<T>>,
{
    let answers = futures_util::future::join_all(
        targets
            .iter()
            .map(|(account, client)| read(account, Arc::clone(client))),
    )
    .await;
    let mut sorted = Answers {
        done: Vec::new(),
        failed: Vec::new(),
        missing: Vec::new(),
    };
    for ((account, _), answer) in targets.iter().zip(answers) {
        let name = account.name.clone();
        match answer {
            Answer::Done(value) => sorted.done.push((name, value)),
            Answer::NoFolder(names) => sorted.missing.push((name, names)),
            Answer::Failed(error) => sorted.failed.push((name, error)),
        }
    }
    sorted
}

impl<T> Answers<T> {
    /// Answers put together by hand, for tests of what is built from them.
    #[cfg(test)]
    pub const fn from_parts(
        done: Vec<(String, T)>,
        failed: Vec<(String, String)>,
        missing: Vec<(String, Vec<String>)>,
    ) -> Self {
        Self {
            done,
            failed,
            missing,
        }
    }

    /// The error of a read that covered one account and failed there, as
    /// the tool returns it: the account's own error, without its name.
    pub fn failure(&self) -> Option<String> {
        self.failed.first().map(|(_, raw)| super::error_json(raw))
    }

    /// `failed` and `folder_missing_in`, when there are any, into the
    /// payload being built: after the counts, before the rows.
    pub fn report(&self, payload: &mut Map<String, Value>) {
        if !self.failed.is_empty() {
            let failed: Vec<Value> = self
                .failed
                .iter()
                .map(|(account, raw)| {
                    let (error, retryable) = error_fields(raw);
                    json!({"account": account, "error": error, "retryable": retryable})
                })
                .collect();
            payload.insert("failed".into(), Value::Array(failed));
        }
        if !self.missing.is_empty() {
            let names: Vec<&str> = self.missing.iter().map(|(a, _)| a.as_str()).collect();
            payload.insert("folder_missing_in".into(), json!(names));
        }
    }

    /// The error when no account answered: what failed where, and for a
    /// folder no account has, the folders each one has. `folder` names what
    /// the call asked for, as the accounts were asked for it. `retryable`
    /// when one of the failures is.
    pub fn nothing_read(&self, folder: &str) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut retryable = false;
        for (account, raw) in &self.failed {
            let (error, again) = error_fields(raw);
            retryable |= again;
            parts.push(format!("{account}: {error}"));
        }
        let folders = |names: &[String]| folder_names_shown(names);
        if self.failed.is_empty() {
            let each: Vec<String> = self
                .missing
                .iter()
                .map(|(account, names)| format!("Folders of {account}: {}.", folders(names)))
                .collect();
            parts.push(format!(
                "No account has the folder \"{folder}\". {}",
                each.join(" ")
            ));
        } else {
            for (account, names) in &self.missing {
                parts.push(format!(
                    "{account} has no folder \"{folder}\" (its folders: {})",
                    folders(names)
                ));
            }
        }
        let error = crate::email::sanitize_external_str(&parts.join("; "));
        serde_json::to_string(&json!({"error": error, "retryable": retryable})).unwrap_or_default()
    }
}

/// A row with `account` as its first field.
pub fn with_account(account: &str, row: Value) -> Value {
    match row {
        Value::Object(mut fields) => {
            fields.shift_insert(0, "account".into(), account.into());
            Value::Object(fields)
        }
        other => other,
    }
}

/// The error for `offset` in a read over several accounts: paging the
/// merged rows would skip `offset` rows in each account.
pub fn offset_needs_account(names: &[String]) -> String {
    super::error_json(&format!(
        "offset pages through one account: pass account as well. Available: {}",
        names.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers(done: &[&str], failed: &[(&str, &str)], missing: &[&str]) -> Answers<u32> {
        Answers {
            done: done.iter().map(|a| ((*a).to_string(), 1)).collect(),
            failed: failed
                .iter()
                .map(|(a, e)| ((*a).to_string(), (*e).to_string()))
                .collect(),
            missing: missing
                .iter()
                .map(|a| ((*a).to_string(), vec!["INBOX".into(), "Archiv".into()]))
                .collect(),
        }
    }

    #[test]
    fn the_report_names_failed_accounts_and_missing_folders() {
        let mut payload = Map::new();
        payload.insert("total".into(), 3.into());
        answers(
            &["Privat"],
            &[("Arbeit", "Connection refused")],
            &["Verein"],
        )
        .report(&mut payload);
        payload.insert("emails".into(), json!([]));
        let keys: Vec<&String> = payload.keys().collect();
        assert_eq!(keys, ["total", "failed", "folder_missing_in", "emails"]);
        assert_eq!(
            payload["failed"],
            json!([{"account": "Arbeit", "error": "Connection refused", "retryable": true}])
        );
        assert_eq!(payload["folder_missing_in"], json!(["Verein"]));

        let mut quiet = Map::new();
        answers(&["Privat"], &[], &[]).report(&mut quiet);
        assert!(quiet.is_empty(), "{quiet:?}");
    }

    #[test]
    fn nothing_read_lists_the_folders_or_the_failures() {
        let v: Value =
            serde_json::from_str(&answers(&[], &[], &["Privat", "Arbeit"]).nothing_read("X"))
                .unwrap();
        assert_eq!(
            v["error"],
            "No account has the folder \"X\". Folders of Privat: INBOX, Archiv. Folders of \
             Arbeit: INBOX, Archiv."
        );
        assert_eq!(v["retryable"], false);

        let v: Value = serde_json::from_str(
            &answers(&[], &[("Arbeit", "Connection refused")], &["Privat"]).nothing_read("X"),
        )
        .unwrap();
        assert_eq!(
            v["error"],
            "Arbeit: Connection refused; Privat has no folder \"X\" (its folders: INBOX, Archiv)"
        );
        assert_eq!(v["retryable"], true);
    }

    #[test]
    fn rows_get_their_account_first() {
        let row = with_account("Privat", json!({"folder": "INBOX", "uid": 3}));
        let keys: Vec<&String> = row.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["account", "folder", "uid"]);
    }
}
