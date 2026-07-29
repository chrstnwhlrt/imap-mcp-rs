//! Persistent store for rotated `OAuth2` refresh tokens.
//!
//! The config file is operator-owned and declarative — the server never
//! writes it. Providers with refresh-token rotation (Entra) hand out a new
//! refresh token on every grant, so the *current* token is server-state,
//! not configuration. It lives here: `$XDG_STATE_HOME/imap-mcp-rs/tokens.toml`
//! (0600, dir 0700), keyed by `username|host|client_id` so it survives
//! account renames and is shared by configs reaching the same mailbox
//! through the same app registration (see [`account_key`]).
//!
//! Concurrency: several server processes may refresh independently (each
//! MCP client spawns its own). Entra keeps superseded tokens valid until
//! their own expiry, so last-writer-wins is correct; writes still go
//! through an exclusive lock on a sidecar lock file + write-temp-then-rename
//! so the file is never observed half-written. The lock file (not the data
//! file) is locked because rename replaces the data file's inode — a lock
//! held on the old inode would not exclude a writer that opened the new one.
//!
//! Precedence is deliberately trivial: **a stored token always wins.** The
//! config's `refresh_token` is a bootstrap value only — it is used when no
//! entry exists yet (fresh install, or a config written before `reauth`
//! existed) and is superseded the moment the first grant lands here. There
//! is no second source of truth and no rule to remember: obtaining a token
//! is `imap-mcp-rs reauth`, and replacing one means deleting the entry.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::AccountConfig;

const STATE_FILE: &str = "tokens.toml";
const LOCK_FILE: &str = "tokens.lock";
/// Prefix of the write-temp files; also what the crash sweep looks for.
const TMP_PREFIX: &str = "tokens.toml.tmp-";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub refresh_token: String,
    /// Unix seconds at write time — diagnostics only ("how old is this?").
    pub updated_at_unix: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
    #[serde(default)]
    tokens: BTreeMap<String, StoredToken>,
}

/// Stable identity of a grant, independent of the user-chosen account name:
/// `username|host|client_id`, lowercased. The client id is part of the key
/// because refresh tokens are bound to the app registration that issued
/// them — two configs pointing at the same mailbox through different apps
/// must not overwrite each other's tokens.
pub fn account_key(config: &AccountConfig) -> String {
    let client_id = config
        .oauth2
        .as_ref()
        .and_then(|o| o.client_id.as_deref())
        .unwrap_or("-");
    format!(
        "{}|{}|{}",
        config.username.to_lowercase(),
        config.host.to_lowercase(),
        client_id.to_lowercase()
    )
}

fn state_dir() -> Result<PathBuf> {
    // Linux: ~/.local/state (XDG_STATE_HOME). macOS has no state dir in
    // `dirs` — fall back to the local data dir, same privacy properties.
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|d| d.join("imap-mcp-rs"))
        .context("Cannot determine a state directory (no XDG state/data dir)")
}

pub fn state_file_path() -> Result<PathBuf> {
    Ok(state_dir()?.join(STATE_FILE))
}

/// Refuse symlinks at `path` — mirrors the attachment-dir startup check:
/// on a shared host a pre-planted symlink could redirect our token write
/// into an attacker-readable location.
fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            bail!("{} is a symlink — refusing to use", path.display())
        }
        _ => Ok(()),
    }
}

/// Read the stored token for `key`. Never fails hard: a missing or corrupt
/// state file degrades to `None` — the caller then falls back to whatever
/// the config bootstrapped with, and a broken sidecar cannot take mail down.
pub fn load(key: &str) -> Option<StoredToken> {
    load_from(&state_dir().ok()?, key)
}

/// `load` against an explicit directory — the whole file path is exercised
/// by the tests this way, without depending on the caller's XDG layout.
fn load_from(dir: &Path, key: &str) -> Option<StoredToken> {
    let path = dir.join(STATE_FILE);
    if reject_symlink(&path).is_err() {
        tracing::warn!(path = %path.display(), "Token state file is a symlink — ignoring");
        return None;
    }
    let content = fs::read_to_string(&path).ok()?;
    match toml::from_str::<StateFile>(&content) {
        Ok(state) => state.tokens.get(key).cloned(),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Token state file unparseable — ignoring this entry"
            );
            None
        }
    }
}

/// Persist `refresh_token` for `key`. Exclusive-locked read-merge-write so
/// concurrent server processes (each MCP client runs its own) never lose
/// each other's entries; the data file itself is replaced atomically.
///
/// Deliberately synchronous I/O despite async callers: this runs at most
/// once per access-token expiry (~1 h) on a tiny file, and the lock is held
/// for microseconds — `spawn_blocking` machinery would outweigh the work.
/// Same precedent as the synchronous `config::load_config` in async `main`.
pub fn store(key: &str, refresh_token: &str) -> Result<()> {
    store_in(&state_dir()?, key, refresh_token)
}

/// `store` against an explicit directory — see [`load_from`].
fn store_in(dir: &Path, key: &str, refresh_token: &str) -> Result<()> {
    reject_symlink(dir)?;
    fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create state dir {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }

    let lock_path = dir.join(LOCK_FILE);
    reject_symlink(&lock_path)?;
    let lock_file = {
        let mut opts = fs::OpenOptions::new();
        opts.create(true).truncate(false).write(true);
        #[cfg(unix)]
        {
            // The lock file stays empty, so this guards nothing by itself —
            // it keeps every file we create here at one permission, so a
            // loosened state dir can't leave a world-writable straggler
            // through which the lock could be held open against us.
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(&lock_path)
            .with_context(|| format!("Failed to open lock file {}", lock_path.display()))?
    };
    lock_file
        .lock()
        .context("Failed to acquire token state lock")?;
    // Released on drop at end of scope (and by the OS if we die holding it).
    // Each call opens its own file description, so this excludes concurrent
    // threads of one process just as it does separate processes.

    // Sweep temp files abandoned by a crash mid-write: they carry tokens and
    // nothing else would ever reap them. Best-effort, under the lock.
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with(TMP_PREFIX) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    let data_path = dir.join(STATE_FILE);
    reject_symlink(&data_path)?;
    let mut state: StateFile = fs::read_to_string(&data_path).map_or_else(
        |_| StateFile::default(),
        |content| {
            toml::from_str(&content).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "Token state file unparseable — rewriting it");
                StateFile::default()
            })
        },
    );

    let updated_at_unix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    state.tokens.insert(
        key.to_string(),
        StoredToken {
            refresh_token: refresh_token.to_string(),
            updated_at_unix,
        },
    );

    let serialized = toml::to_string_pretty(&state).context("Failed to serialize token state")?;
    let tmp_path = dir.join(format!("{TMP_PREFIX}{}", std::process::id()));
    {
        let mut opts = fs::OpenOptions::new();
        opts.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut tmp = opts
            .open(&tmp_path)
            .with_context(|| format!("Failed to create {}", tmp_path.display()))?;
        tmp.write_all(serialized.as_bytes())?;
        tmp.sync_all()?;
    }
    fs::rename(&tmp_path, &data_path)
        .with_context(|| format!("Failed to replace {}", data_path.display()))?;
    // Log the mailbox part of the key but not its trailing client id: the
    // config's Debug impl redacts that, and the README promises to keep it
    // out of logs. `username|host` is already in the startup lines.
    let mailbox = key.rsplit_once('|').map_or(key, |(mailbox, _)| mailbox);
    tracing::info!(mailbox = %mailbox, "Persisted OAuth2 refresh token");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scratch directory that removes itself, so the file-level tests can
    /// exercise `store_in`/`load_from` (merge, atomic replace, modes, sweep)
    /// without touching the developer's real state directory.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "imap-mcp-rs-test-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_account() -> AccountConfig {
        // Deserialize a minimal TOML instead of struct literal so this test
        // doesn't break when optional fields are added to AccountConfig.
        toml::from_str(
            r#"
            name = "T"
            host = "h"
            username = "u@h"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn store_then_load_roundtrip() {
        let dir = TempDir::new();
        store_in(dir.path(), "k1", "rt-1").unwrap();
        let got = load_from(dir.path(), "k1").unwrap();
        assert_eq!(got.refresh_token, "rt-1");
        assert!(got.updated_at_unix > 1_700_000_000, "timestamp looks unset");
        assert!(load_from(dir.path(), "other-key").is_none());
    }

    #[test]
    fn store_creates_missing_directory() {
        let parent = TempDir::new();
        let nested = parent.path().join("does/not/exist/yet");
        store_in(&nested, "k", "rt").unwrap();
        assert_eq!(load_from(&nested, "k").unwrap().refresh_token, "rt");
    }

    #[test]
    fn store_merges_without_dropping_other_accounts() {
        let dir = TempDir::new();
        store_in(dir.path(), "account-a", "rt-a").unwrap();
        store_in(dir.path(), "account-b", "rt-b").unwrap();
        // The second write must not drop the first account's entry — that
        // would silently log out every other mailbox on the next start.
        assert_eq!(
            load_from(dir.path(), "account-a").unwrap().refresh_token,
            "rt-a"
        );
        assert_eq!(
            load_from(dir.path(), "account-b").unwrap().refresh_token,
            "rt-b"
        );
    }

    #[test]
    fn store_overwrites_existing_key() {
        let dir = TempDir::new();
        store_in(dir.path(), "k", "rt-old").unwrap();
        store_in(dir.path(), "k", "rt-new").unwrap();
        assert_eq!(load_from(dir.path(), "k").unwrap().refresh_token, "rt-new");
    }

    #[test]
    fn store_sweeps_abandoned_temp_files() {
        let dir = TempDir::new();
        // Simulate a crash between create and rename by an earlier run.
        let orphan = dir.path().join(format!("{TMP_PREFIX}999999"));
        fs::write(&orphan, "leftover with a token in it").unwrap();
        store_in(dir.path(), "k", "rt").unwrap();
        assert!(!orphan.exists(), "abandoned temp file was not swept");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(TMP_PREFIX))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn store_creates_private_file_and_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        store_in(dir.path(), "k", "rt").unwrap();
        let file_mode = fs::metadata(dir.path().join(STATE_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let dir_mode = fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        let lock_mode = fs::metadata(dir.path().join(LOCK_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            file_mode, 0o600,
            "token file must not be group/world readable"
        );
        assert_eq!(
            dir_mode, 0o700,
            "state dir must not be group/world readable"
        );
        assert_eq!(
            lock_mode, 0o600,
            "lock file must match the rest of the state dir"
        );
    }

    #[test]
    fn store_rewrites_corrupt_state_file() {
        let dir = TempDir::new();
        fs::write(dir.path().join(STATE_FILE), "this is not = valid toml [[[").unwrap();
        assert!(load_from(dir.path(), "k").is_none());
        // A broken sidecar must not block persisting a fresh token.
        store_in(dir.path(), "k", "rt").unwrap();
        assert_eq!(load_from(dir.path(), "k").unwrap().refresh_token, "rt");
    }

    #[test]
    fn load_from_missing_file_is_none() {
        let dir = TempDir::new();
        assert!(load_from(dir.path(), "k").is_none());
    }

    #[test]
    fn store_stays_consistent_across_threads() {
        // Locking is per open file description, so this also covers the
        // "two threads in one process" case that a naive lock would miss.
        let dir = TempDir::new();
        let path = dir.path().to_path_buf();
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for round in 0..5 {
                        store_in(&path, &format!("key-{i}"), &format!("rt-{i}-{round}")).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // Every writer's final value survived — no lost update, no corruption.
        for i in 0..4 {
            assert_eq!(
                load_from(dir.path(), &format!("key-{i}"))
                    .unwrap()
                    .refresh_token,
                format!("rt-{i}-4")
            );
        }
    }

    #[test]
    fn account_key_is_lowercased_identity_with_client_id() {
        let mut cfg = test_account();
        cfg.username = "User@Example.COM".into();
        cfg.host = "Outlook.Office365.com".into();
        assert_eq!(
            account_key(&cfg),
            "user@example.com|outlook.office365.com|-"
        );
        cfg.oauth2 = Some(
            toml::from_str(
                r#"
                provider = "outlook365"
                client_id = "CID-123"
                "#,
            )
            .unwrap(),
        );
        assert_eq!(
            account_key(&cfg),
            "user@example.com|outlook.office365.com|cid-123"
        );
    }

    #[test]
    fn state_file_roundtrip_via_toml() {
        let mut state = StateFile::default();
        state.tokens.insert(
            "user|host|cid".into(),
            StoredToken {
                refresh_token: "rt-state".into(),
                updated_at_unix: 42,
            },
        );
        let serialized = toml::to_string_pretty(&state).unwrap();
        let parsed: StateFile = toml::from_str(&serialized).unwrap();
        let e = &parsed.tokens["user|host|cid"];
        assert_eq!(e.refresh_token, "rt-state");
        assert_eq!(e.updated_at_unix, 42);
    }

    #[test]
    fn state_file_tolerates_unknown_entry_fields() {
        // A file written by a newer version must stay readable by this one:
        // the token is what matters, extra bookkeeping fields are not our
        // business. Without this the whole store would fail to parse and
        // every account would silently fall back to re-authorization.
        let parsed: StateFile = toml::from_str(
            r#"
            [tokens."u|h|c"]
            refresh_token = "rt"
            updated_at_unix = 7
            some_field_a_future_version_added = "value"
            "#,
        )
        .unwrap();
        assert_eq!(parsed.tokens["u|h|c"].refresh_token, "rt");
    }

    #[test]
    fn state_file_parses_empty_input_to_default() {
        let parsed: Result<StateFile, _> = toml::from_str("tokens = 3");
        assert!(parsed.is_err());
        let empty: StateFile = toml::from_str("").unwrap();
        assert!(empty.tokens.is_empty());
    }
}
