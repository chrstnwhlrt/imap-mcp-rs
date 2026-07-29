# Changelog

Notable changes per release. Versions follow [semantic versioning](https://semver.org):
the MCP tool surface and the config format are the public API.

## 1.4.1

### Changed

- Integration coverage now includes every public client method. `move_emails`,
  `delete_emails`, `fetch_raw` and `get_folder_names` had none — the two most
  destructive operations in the tool among them. A move is COPY plus `\Deleted`
  plus UID EXPUNGE rather than one atomic step, and the failure that matters
  leaves the message in both folders or in neither; the new test asserts both
  ends. Deletion is checked in both modes: the default must leave the message
  recoverable in Trash, and a permanent delete must not quietly route through
  it. `fetch_raw` must return the message verbatim with headers, and report a
  stale UID as absent rather than as an error, so a caller can tell "gone"
  from "broken". 15 integration tests, up from 11.
- README test counts corrected alongside.

## 1.4.0

### Fixed

- **The server identified itself as the SDK.** `ServerInfo::new` fills
  `serverInfo` from `Implementation::from_build_env()`, which resolves
  `CARGO_CRATE_NAME` inside `rmcp` — so every handshake announced `rmcp` and
  the SDK's version instead of this server and its own. A client could not
  tell which server it was talking to, nor which release. Now set explicitly
  from this crate's `CARGO_PKG_NAME` / `CARGO_PKG_VERSION`, so the two cannot
  drift apart. Present since the first release; found by running an actual
  handshake rather than trusting the build.

### Changed

- **`rmcp` 1.5 → 3.0**, across two major versions, with no source change
  required. Verified beyond compilation: a real `initialize` / `tools/list`
  exchange returns all 19 tools with their full parameter sets, and the
  instructions still lead with the untrusted-content warning.
- **`base64` 0.22 → 0.23**, likewise without source changes.
- Fourteen compatible dependency updates, including `tokio` 1.53, `rustls`
  0.23.42, `aws-lc-rs` 1.17, `async-imap` 0.11.3 and `mail-parser` 0.11.5.
- Flake inputs refreshed (`nixpkgs`, `crane`, `rust-overlay`).

All six CI gates pass, plus the 11 integration tests against a real IMAP
server — which is what actually exercises the updated TLS, IMAP and MIME
crates.

## 1.3.1

### Changed

- Test fixtures use neutral placeholder names for folders and accounts
  throughout.
- Documentation gaps from 1.3.0: the README did not mention that `list_drafts`
  gained `compact`, and the `search_emails` description did not list the
  `offset` and `limit` it returns.

## 1.3.0

A full pass over every tool's parameters and return shape, prompted by the
question whether 1.2.0 had actually left the interface consistent. It had not.

### Added

- **`offset` on `search_emails`.** 1.2.0 started reporting that a result was
  capped — without offering any way to reach the rest, so a caller could only
  narrow the criteria and risk double-counting the overlap. Paging happens on
  the UID list before the fetch, so skipping costs nothing. Requires a single
  `folder`: across folders it would skip `offset` messages in *each* one,
  dropping matches instead of paging past them, so that combination is
  refused rather than silently mishandled. A code comment had promised this
  parameter for some time; it did not exist.
- **`compact` on `list_drafts`**, which renders the same rows as `list_emails`
  and was simply missed in 1.2.0. It also reports `returned` now.
- **`prefix` and `unread_only` on `list_folders`.** A mailbox with a hundred
  folders was returned in full for every question about a handful of them.
  `prefix` matches the decoded name too, so `Entwü` finds `Entw&APw-rfe`, and
  `total` still reports the unfiltered count so a narrowed listing can't be
  mistaken for the whole mailbox.

## 1.2.1

### Changed

- The three behaviours added in 1.2.0 were verified by hand against a live
  server but had no regression test: the thread cap, the compact/full row
  switch and the bidi re-check on decoded folder names all lived inline in
  functions that need a network client. Pulled out as `cap_threads`,
  `summary_rows` and `safe_display_name` — the same treatment `note_replacement`
  already had — and covered by six tests, including the crafted
  `INBOX/&IC4-evil` folder whose ASCII name passes the raw filter while its
  decoded form carries a right-to-left override. No behaviour change.

## 1.2.0

Fixes found by using the server for a real three-week mailbox catch-up:
scanning ~350 messages across 18 folders surfaced problems no unit test had.

### Fixed

- **`search_emails` reported the delivered count as `matched`.** The number was
  taken after `truncate(limit)`, so `matched: 120` at `limit: 120` was
  indistinguishable from "there are exactly 120" — a caller asking for
  everything since a date would silently miss the remainder. `matched` is now
  the server-side match count and a separate `returned` says how many rows came
  back; `returned < matched` means there is more. (`list_emails` was already
  correct — this was an inconsistency between the two.)
- **`list_emails(group_by_thread: true)` cut the thread list without saying
  so.** Collapsing happens after `matched` is fixed, so a caller saw a message
  count beside a thread list with no hint that rows were dropped. Now reports
  `threads_truncated_from` when the cap applied.

### Added

- **`compact: true` on `list_emails` and `search_emails`** — drops the snippet,
  Message-ID, References chain and recipient preview, keeping identity, sender,
  subject, date and flags. Roughly 80% smaller. A 120-row window of full rows
  is ~82 KB, enough to exhaust a client's response budget and force paging that
  the data itself does not warrant.
- **`display_name` on `list_folders`** — decodes modified UTF-7 (RFC 3501
  §5.1.3), so `Entw&APw-rfe` is also shown as `Entwürfe`. Display only; `name`
  stays the value every other tool takes. The decoded form is re-checked
  against the control/bidi filter: the encoding hides non-ASCII from the check
  the raw name passes, so a crafted folder could otherwise smuggle a
  right-to-left override into the listing.

## 1.1.1

### Changed

- The server instructions now mention that every `draft_*` call returns the
  new draft's `uid`. Without it a client still learned the revision flow but
  not where the UID comes from, and would call `list_drafts` for it — the
  round-trip 1.1.0 set out to remove.

## 1.1.0

### Added

- `draft_reply`, `draft_forward` and `draft_email` return the saved draft's
  `uid`. Revising a draft previously meant calling `list_drafts` in between
  just to learn which UID to pass as `replaces_uid` — a round-trip that
  fetched headers for every draft in the folder to answer a question about
  the one just written. `APPEND` does not surface the UID through the IMAP
  client library, so it is resolved by `Message-ID` immediately after saving;
  when that lookup finds nothing the field is omitted and the draft is still
  saved.

### Changed

- `move_email` and `delete_email` now state that the UIDs they return are
  valid in the *source* folder only. IMAP assigns UIDs per folder, so reusing
  them against the target could address an unrelated message that happens to
  hold the same number. Documentation only — no behaviour change.

## 1.0.0

First stable release. The tool surface has been unchanged for months, tokens
survive provider rotation, and the security model is documented rather than
implied.

### Added

- **19 MCP tools**: discovery (`list_accounts`, `list_folders`), reading
  (`list_emails`, `search_emails`, `get_email`, `get_thread`), organizing
  (`mark_as_read`, `mark_as_unread`, `flag_email`, `unflag_email`,
  `move_email`, `delete_email`), composing (`draft_reply`, `draft_forward`,
  `draft_email`, `list_drafts`, `delete_draft`), `download_attachment` and
  `account_health` for diagnosis. There is no send tool and no SMTP code —
  drafts are written to the Drafts folder for a human to send.
- **`reauth` subcommand** — browser-based OAuth2 authorization on a loopback
  listener, secured with PKCE (RFC 7636, S256), verifying the result with a
  real IMAP login before reporting success.
- **Refresh tokens as state** — kept in `$XDG_STATE_HOME/imap-mcp-rs/tokens.toml`
  (file `0600`, directory `0700`, lock-protected atomic replace) rather than in
  the config. Providers that rotate refresh tokens (Microsoft Entra) get the
  rotation persisted, so an account no longer dies 90 days after its first
  authorization.
- **Prompt-injection defenses** — untrusted-content notice first in the server
  instructions and repeated inline with every message body, attachment
  directory whitelist, per-account permissions, and `body_parts_diverge` for
  messages whose plain-text part carries text the HTML reader never sees.
- **Multi-account support** with per-account `read_only`, `allow_move`,
  `allow_delete` and `allowed_folders`.
- `--help` / `--version`.

### Security

- All LLM-visible strings are stripped of control, bidirectional-override and
  zero-width characters; outgoing header values likewise, preventing CRLF
  injection.
- TLS via rustls with an explicitly pinned provider; UIDPLUS-gated EXPUNGE;
  resource caps on bodies, folders, threads, drafts and attachments.
