# Changelog

Notable changes per release. Versions follow [semantic versioning](https://semver.org):
the MCP tool surface and the config format are the public API.

## 3.2.0

Fixes from a review of the whole code. The tool surface is unchanged:
`initialize` and `tools/list` are byte for byte those of 3.1.1.

### Changed

- **An unknown key in an account block stops the server at startup.** It
  used to be ignored, so `read_onyl = true` or `allow-delete = false` left
  the account writable or deletable, the opposite of what the line says.
  The error names the key. The same holds for `[accounts.oauth2]`; unknown
  keys at the top level (such as a leftover `tool_set`) still load. A
  config that only loaded because a misspelled key went unnoticed needs
  that key fixed.
- **The first attachment directory must belong to the user the server
  runs as.** The mode check let a directory through that another user owns
  and keeps private for the moment; its owner could open it again at any
  time and read or replace every download. `config.sample.toml` and the
  README promised this check already.

### Security

- **A broken line of the config or the token state reached the log in
  full.** toml's error rendering quotes the line it stopped at: a typo
  next to a password printed the password at startup, and a damaged
  `tokens.toml` logged a refresh token, both to the stderr that MCP
  clients keep in their logs. The errors now give line, column and
  message only.

### Fixed

- **A forward left the original's attachments behind.** `draft_forward`
  quoted the text alone, so forwarding an invoice sent it without the
  invoice, unlike every mail client. The draft now carries every
  attachment `get_email` lists, decoded as it arrived and ahead of the
  files the call adds; inline images come along as ordinary attachments.
  The original is fetched once for both.
- **A reply ignored `Reply-To`.** `draft_reply` addressed the original's
  `From` even when the mail named another address for replies, as shops,
  ticket systems and mailing lists do, so the draft went to a `noreply@`
  sender instead of the service desk. It now goes to the `Reply-To`
  addresses when there are any, as every mail client reads them. The
  response shows them in `to`; `get_email` stays as it was.
- **A reply to a reply without `References` lost the start of the
  thread.** The new draft's `References` now falls back to the parent's
  `In-Reply-To`, as RFC 5322 prescribes.
- **`body_parts_diverge` missed mirrored text in mail from Outlook.** The
  HTML part was read with its `<style>` block, whose CSS words outnumbered
  the message's own, so the plain part never covered the HTML and a
  paragraph hidden from the reader went unflagged. The content of `<head>`,
  `<style>`, `<script>` and `<template>` no longer counts as text, as in
  mail-parser's own conversion, and a `<` inside a script no longer hides
  the rest of the mail.
- **A signature copied out of Outlook put its CSS into the text part of
  every draft.** The text derived from `signature_html` leaves out the same
  elements.
- **`search_emails` with `to` and a term outside ASCII found nobody on
  Outlook 365.** Such terms are matched there after the fetch, and that
  match looked only at the recipients' addresses, while a term like
  "Müller" sits in the name. It now checks names and addresses, as the
  parameter says and as `from` always did.
- **`list_folders` and a search across all folders went by a stale
  folder list.** The names were cached per session, so a folder created
  in another client since was missing until the next reconnect, and one
  deleted there was still listed. Both now list afresh; resolving a single
  folder name keeps the cache and renews it on a miss, as before.
- **A STATUS that timed out in `list_folders` could hand its counts to
  the next folder.** The command was given up after 10 seconds while its
  answer was still on the way, and the session read that answer as the
  one to the next STATUS. The session is now dropped there, and the next
  folder reconnects first.
- **An empty `IMAP_MCP_CONFIG` stopped the server with "Failed to read
  config file: " and no path,** and a path in it that is not UTF-8 was
  skipped, so the server started on the default config instead. An empty
  value now counts as unset, and the path is read as it is.
- **The `Host` header for a custom token endpoint lacked a port other
  than 443**, which RFC 9110 requires.
- **The GreenMail integration tests could not fail for a missing
  server.** Each returned early when nothing answered and counted as
  passed. Run with `--ignored`, they now fail and say to start
  `./test-server.sh`. Two unit tests asserted less than their names say:
  the client-side `from` match was never tried by name, and
  `unread_only: false` passed with `UNSEEN` as well as with `SEEN`.

364 unit tests, 23 GreenMail integration tests and 75 checks in the MCP
end-to-end script.

## 3.1.1

All dependencies and the toolchain at their newest releases. Nothing a
client sees changes: `initialize` and `tools/list` are byte for byte those
of 3.1.0, compared for nine configs (the three of the Gemma kit and six
variants with fewer permissions, no signature or a signature on only one of
two accounts) and three protocol versions.

### Changed

- jiff 0.2.42 and uuid 1.29.2, and further down the tree syn 3.0.10, itoa
  1.0.19 and crc32fast 1.5.5. No major version is outstanding anywhere in
  the tree.
- The flake's rust-overlay input moved to its state of 10 November 2026,
  nixpkgs and crane were already current. The toolchain is Rust 1.99.1, the
  newest stable release. `rust-version` stays at 1.98, the floor set by
  mail-parser 1.0, and the tests pass with that version.
- cargo audit reports no vulnerability, only that pin-utils is unmaintained
  (RUSTSEC-2026-0441). It comes in through async-imap 0.12.0, the newest
  release, so only a new async-imap can remove it.

350 unit tests, 23 GreenMail integration tests and 73 checks in the MCP
end-to-end script.

## 3.1.0

Answers that leave a small model less to infer, found in the tool calls
recorded while measuring 3.0.1 with Gemma 4 E4B. No tool, schema or
instruction changes.

### Added

- **The newest emails after an empty search carry the start of their
  text** (`snippet`). Asked "Was braucht die Steuerberaterin noch von
  mir?", Gemma found Selin Kaya among them but did not open her email
  (0 of 3). With the text in the row it answered from there (3 of 3).
- **Every draft response says that nothing was sent**, right after its
  `status`: `"saved": "as a draft in Drafts; nothing was sent, the user
  sends it"`. Told only `"status": "ok"`, Gemma reported a forward as
  "weitergeleitet" and a reply as sent. `status` stays as it was.
- **`list_emails` with `unread_only` gives its count as `unread`** as well,
  ahead of `total`, at the top and per account. Asked how many unread
  emails there are, Gemma once named `total`, every email of both
  inboxes.
- **A search for an account's name points to the account.** When a search
  for words finds nothing and one of them is the name of an account, the
  hint begins with how to search that account ("\"arbeit\" is an account:
  to search it, pass account \"Arbeit\"."). Asked which unread email was
  from work, Gemma had searched `from: "Arbeit"` and answered "none".

Measured against 3.0.1 with Gemma behind Otomo: the catalog went from 86
to 94 of 99 requests (three runs each), the read-only set from 24 to 26 of
28. The held-out requests (25 of 28 against 26), the fresh set (13 of 14
against 14) and the fresh read-only set (8 of 12 against 10) moved by a
request or two, with no link to these changes in the recorded answers.
Repeated six times, the read-only requests to forward or delete came to 11
of 12 against 10, and "Markier die Rechnung von den Stadtwerken als
wichtig", where 3.0.1 twice flagged both invoices, to 6 of 6 against 4.
Claude Sonnet 5.5 handled every request of all five sets. A sentence that
told the model the first row is the one meant brought nothing measurable
and stayed out.

350 unit tests, 23 GreenMail integration tests and 73 checks in the MCP
end-to-end script.

## 3.0.1

Fixes from a review of 3.0.0 and of the tool calls recorded while measuring
it with Gemma 4 E4B.

### Fixed

- **`list_emails` with `unread_only` listed the oldest unread email
  first.** The server picked the newest unread messages but passed them on
  in the order the IMAP server answered the FETCH, ascending on Dovecot and
  GreenMail, so "what is new?" began with the oldest of them. The recorded
  calls had it 14 times. The rows come newest first again, as the
  description says.
- **A revised draft had to repeat its recipients and subject.**
  `draft_email` and `draft_forward` with `replaces_uid` but without `to`
  (or `subject`) failed, and in the recorded calls Gemma needed three
  attempts to revise a draft. A revision now keeps what it leaves out of
  `to`, `cc` and `subject` from the draft it replaces; until now a left-out
  `cc` was dropped silently. `bcc` and attachments are not carried over.
  Without `replaces_uid` the error names everything missing at once. The
  schemas are unchanged.
- **Where only some accounts sign, a draft could end in two closings.**
  The draft texts mention the signature only where every account that can
  draft has one. With a signature on one account alone, a model was told
  nothing, signed the draft itself and got the signature below its own
  greeting. The draft texts now name the accounts that sign ("The server
  adds the user's signature to drafts from Office: write those without a
  closing greeting or name.").
- `account: ""` counts as no account: a read covers every account, and a
  call on one email asks for the account by name instead of reporting an
  account "" that does not exist. `folder: ""` in `search_emails` searches
  every folder.
- An account name passed as the folder of a read over several accounts
  ("Arbeit") is answered with how to read that account's inbox, before the
  folder lists.

349 unit tests, 23 GreenMail integration tests and 68 checks in the MCP
end-to-end script.

## 3.0.0

One tool surface for every client, written for the smallest model it was
measured with, Gemma 4 E4B behind a voice assistant, and fitted to what the
configured accounts allow. With several accounts the reading tools read all
of them at once. Besides that a dependency update and a review pass over
code and docs; most fixes below concern behaviour that predates the update.
Major version because the tools offered now follow the permissions and a
read without `account` answers for every account.

### Breaking

- **Only the tools the accounts may use are offered.** A tool that no
  configured account may use is left out of `tools/list`: with every
  account `read_only` the eight reading tools and `download_attachment`
  remain, and a call to a tool left out fails as unknown instead of with
  the permission error. The instructions follow the same permissions. They
  promise no drafts where none can be saved, and they say what cannot be
  done in the words a user asks with: "Reading only: to reply, write,
  forward, move, mark or delete, tell the user that you can only read
  their mail here." Told instead that nothing "can be drafted", Gemma
  answered "Antworte Mama ..." with "Ich antworte ihr ..." and wrote a
  reply out as if it had gone; with the user's words it declined 16 of 16
  such requests, against 12 of 16. Adding "forward", which users ask for
  as well, took a wider set of such requests from 25 to 27 of 28.
- **With several accounts, `list_folders`, `list_emails`, `search_emails`
  and `list_drafts` read all of them when `account` is left out.** That was
  an error. The answer gives the totals and the counts per account first
  (`accounts`), then the accounts that failed (`failed`, each with `error`
  and `retryable`) and those without the folder asked for
  (`folder_missing_in`), then the rows of all accounts merged newest first.
  "How many unread emails do I have?" is one call, and a model need not
  know there are several accounts. `offset` pages through one account and
  needs `account` then. A call that names an account answers as before.
- **Rows name their account** (`account`, first in the row) whenever
  several accounts are configured: it is what a call on that email passes
  back. The schemas of the tools that work on one email then mark `account`
  as required, as the server already did. With one account `account` is
  not offered at all, and a call that sends it still works.
- **Descriptions, parameter texts and instructions are the measured texts**
  (see Added), the instructions at most 1000 characters. A client that
  matched on the old wording needs to look again. `unread_only` of
  `search_emails`, an alias of `is_read`, is no longer offered and still
  accepted.

### Security

- **A `.env` file could choose the config.** Before resolving the config
  path the server loaded `.env` from its working directory and every parent
  directory. Such a file could set `IMAP_MCP_CONFIG`, or the XDG variables
  that place the token state and the attachment directory, and so inject the
  very config the working directory is deliberately not searched for.
  Nothing ever read a credential from the environment, so `dotenvy` is gone,
  along with the sample config's hint at a `.env` file.
- **The startup prune could delete the user's own files.** It removed every
  entry older than seven days in the first `allowed_attachment_dirs` entry,
  subdirectories recursively, and the sample config suggests a Documents
  folder for that list. It now removes only the per-download UUID
  directories that `download_attachment` creates.
- **The attachment directory is verified after it is prepared.** A directory
  another user created beforehand (possible with the `/tmp` fallback) was
  accepted although the chmod to 0700 failed silently, so that user could
  read every download or swap a file before a draft attached it. The check
  now runs after creation and chmod, at startup and before every download,
  and refuses a symlink or anything still open to other users.
- **Unknown command-line arguments are refused.** A misspelled `--config`
  started the server on the default config, which may grant permissions the
  intended one withholds. `--config` without a path did the same.
- `reauth` strips control characters from the provider error it prints. Any
  web page open during the flow can send a request to the loopback listener,
  and terminal escape sequences in its `error` parameter reached the
  terminal unfiltered. The error code of a token-endpoint response is now
  stripped as well, as its description already was.

### Fixed

- **A session the server had ended returned empty results instead of an
  error.** Gmail and Outlook close IMAP sessions after a spell without
  traffic, and the server noticed only with the next command. async-imap
  took the closed connection for the end of that command's answer, so the
  command "succeeded" with nothing: `list_folders` showed no folders, a
  search found nothing, a flag change reported nothing changed, and the
  empty folder list stayed cached, so Drafts and Trash were gone until a
  restart. A closed connection is now an error. Reads reconnect and repeat,
  and a write, which is never repeated, checks the session with NOOP first.
  `mcp-tool-test/gemma/kick.py` checks this against Dovecot (`doveadm
  kick`): 1 of 9 checks passed before, 9 of 9 now.
- **A failed connect named no cause and was not `retryable`.** The errors
  of the connect, the TLS handshake, the greeting and the login carried
  only their own words ("Failed to connect to imap.example.com:993", "IMAP
  login failed"), so `account_health` could not say why, and a refused or
  timed-out connection came back `retryable: false`, which tells a caller
  to change the call rather than try again. They now name the cause
  ("...: Connection refused", "IMAP login failed: Invalid credentials").
- **Gmail OAuth2 did not work.** Google's token endpoint answers HTTP/1.1
  with `Transfer-Encoding: chunked`, and the minimal HTTPS client parsed the
  raw body, chunk sizes included, as JSON. Every token refresh and every
  `reauth` code exchange failed, and an `invalid_grant` was never recognized
  as one. The client now decodes chunked bodies and honours
  `Content-Length`. Outlook 365 was unaffected because Microsoft sends a
  `Content-Length`.
- **Failed tool calls are flagged as errors.** Every error came back as an
  ordinary tool result with `isError: false`, the `{"error", "retryable"}`
  object only in its text. The MCP specification reports a failed tool
  execution with `isError: true`, so a client going by the flag saw every
  refusal and every IMAP failure as a success. The flag is set now, the text
  is unchanged.
- **`list_emails` missed mail that arrived while its folder stayed
  selected.** The page window came from the message count of the folder's
  last SELECT. A server reports new mail in the selected folder only as an
  untagged `EXISTS` next to a later command, which async-imap queues where
  nothing reads it. As long as nothing else selected a folder in between,
  as with an assistant watching a single inbox, new mail stayed out of the
  listing and `total` kept the old count. Listings now select afresh.
- **`get_thread` missed sibling branches.** It looked for replies to the
  requested message and for its ancestors, so another reply to one of those
  ancestors was left out of the "full thread". It now also collects
  everything that descends from the thread's root.
- **A search across all folders turned a lost connection into "no
  matches".** Every per-folder error was logged and skipped, so with the
  connection gone each remaining folder made its own reconnect attempt and
  the call returned a partial result, down to `matched: 0`. Transient
  errors now fail the call with `retryable: true`. A folder that refuses
  every search (a `\Noselect` parent such as `[Gmail]`) is still skipped,
  unless no folder could be searched at all. That case also came back as an
  empty result, for example from a server refusing the criteria everywhere.
- **Developer notes reached every client in the draft tools' schemas.**
  schemars turns doc comments into schema descriptions, so 1.3 KB of notes
  on serde and schemars stood in `tools/list` three times, in the
  `attachments` entries of the three draft tools. The entry type now has a
  one-line description of its own, and `tools/list` is 3.8 KB smaller.
- **Text after an inline image was missing from `body_text`**, which held
  only the first text part. Phone clients put a photo between two text
  parts. All text parts are now joined in reading order.
- **Text-only mail reported an HTML body it does not have.** mail-parser
  stands the text part in for a missing HTML alternative and converts it on
  the fly. `get_email(include_html: true)` returned that conversion, replies
  quoted it instead of taking the plain-text path built for text-only
  originals, and the divergence check compared the conversion with itself.
  `body_html` now carries only an HTML part the sender wrote.
- **Moving messages into the folder they are in is refused**, and so is a
  non-permanent `delete_email` from the Trash folder, the `dry_run` preview
  included. Both copied the messages onto themselves and expunged the
  originals, so nothing moved while every UID changed. The delete from
  Trash reported success with the messages still in place.
- **Draft recipients are validated.** mail-builder writes any string between
  `<` and `>`, so `Alice <alice@example.com>` became
  `<Alice <alice@example.com>>`, a comma-separated list turned into one
  broken mailbox and an entry that sanitized to nothing became `<>`.
  Recipients now take a bare address or the `Name <address>` form, anything
  else is refused with the entry named, and `draft_forward` and
  `draft_email` require at least one `to`, as their schema always said.
  Reply-all no longer lists an address twice, and an original recipient
  without an address no longer turns into an empty `<>`.
- **XOAUTH2 answers the server's error challenge with an empty response**,
  as the mechanism defines it. Sending the credentials a second time left
  the outcome of a failed login to however the server treats that protocol
  violation.
- **TCP keepalive gives up after three unanswered probes.** The README
  promised about 60 seconds to notice a dead connection, and the Linux
  default of nine probes made it two minutes.
- **`body_parts_diverge` misread German HTML.** The HTML side of the
  comparison decoded only six named entities, so `Gr&uuml;&szlig;e` fell
  apart into fragments and every word with an umlaut counted as text the
  HTML part lacked. All named entities are decoded now (through
  mail-parser's table), and text that is no entity stays as written. Before,
  a long run after a lone `&` gained a second `&`, `&;` vanished, and a lone
  `&` kept the entity after it from being decoded (`Müller & S&ouml;hne`).
- **The text signature derived from `signature_html` showed raw entities.**
  It decoded six of them, so a signature with `Gr&uuml;&szlig;e` in its
  markup put exactly that into the text part of every draft. It now uses
  the full entity table.
- `draft_email` caps the subject at 2 KiB, as the comment on the body cap
  always claimed it did.
- `since` and `before` refuse dates that do not exist. `2026-02-30` went to
  the server as `30-Feb-2026`, and the caller got whatever the server's
  parser made of it.
- An `expires_in` from the token endpoint is capped at a day. The value
  came off the network unchecked, and an absurd one overflowed the
  computation of the token's deadline, which panics.
- `reauth` refuses a repeated `--config` or `--port`. The last value used
  to win silently.
- `allowed_attachment_dirs` refuses entries starting with `~`. Nothing
  expands it, so `"~/Downloads"` created a literal `~` directory wherever
  the MCP client started the server, and downloads landed in it. Other
  relative paths still load, with a warning.
- `RUST_LOG` sets the log level. INFO was added on top of it and overwrote
  any global level, so `RUST_LOG=debug` and `RUST_LOG=warn` both logged at
  INFO.
- `is_retryable_error` matched the bare word "temporarily", so a folder
  named "Temporarily Archived" made "Unknown Mailbox" look retryable.
- The UIDVALIDITY warning compared folder names case-insensitively, so two
  distinct folders differing only in case set it off. Only `INBOX` is
  case-insensitive.
- `download_attachment` releases the account while it decodes and writes
  the file, instead of blocking every other call on that account until the
  write finished.
- The `reauth` hint for `AADSTS50011` recommended the portal's "Mobile and
  desktop applications" platform, which rejects the loopback URI. It now
  points at the manifest entry the README describes.
- Downloaded attachment names longer than 200 bytes are shortened, keeping
  the extension. Longer names hit the file system's 255-byte limit and the
  download failed.
- An unknown account name now lists the configured accounts, like the error
  for a missing one already did.
- Error texts from Dovecot no longer end in its command timing
  (`(0.001 + 0.041 + 0.040 secs)`), which says nothing about the error.
- Server error texts keep brackets that belong to them. Stripping the
  leading `[CODE]` also cut at any later `] `, so
  `Unknown Mailbox: [Kunden] Projekte` came out as `Projekte`.
- `get_email` and `download_attachment` take the server's answer for the
  requested UID. They took the first FETCH response, and an unsolicited
  flag update for another message, which servers send when a parallel
  client changes one, made the call fail with "no body" or "not found".
- `account_health` names a startup connect that timed out. The timeout was
  only logged, so the account showed as offline without a reason.
- `account_health` leaves out the token fields for password accounts and
  `last_error` when there is none, as documented. Both came as `null`.
- Snippets keep the lines of a mail apart. Line breaks went out with the
  other control characters, so a listing read "Liebe GrüßeMama"; they now
  become single spaces.
- A `replaces_uid` that equals the UID the new draft was saved under
  deletes nothing. It named no older draft, and the deletion threw away the
  draft just written.
- An extra `cc` on a reply that names someone the reply already addresses
  is not listed a second time.
- The MCP end-to-end script removes its working directory when it exits;
  every run used to leave one behind.

### Added

- **Tool annotations.** Every tool carries the MCP hints, so a client that
  asks before risky calls knows where to ask. The eight reading tools are
  read-only, and only `delete_email` and `delete_draft` are destructive.
  Without annotations MCP treats every tool as destructive, and such a
  client asked before each `list_emails`. The flag tools are marked
  idempotent, and `list_accounts` and `account_health`, which answer from
  the configuration, as closed-world.
- **One tool surface for every client, measured with a small model.** The
  texts were measured with Gemma 4 E4B behind the Otomo voice assistant
  (kit in `mcp-tool-test/gemma`), and what works there works for larger
  models as well. Such an assistant shows the model only a tool's name,
  description and input schema, decides from the first 140 characters of a
  description whether a request needs the tool, and keeps 1000 characters
  of the instructions. The descriptions therefore begin with what a tool
  does for the user, every parameter has one short line in the user's
  terms (`dry_run`: "true: only shows what would happen, changes
  nothing."), and the instructions stay within the cut. Schema detail that
  changes nothing for the caller is left out, and with the shorter texts
  `tools/list` shrinks from 48 to 20 KB (one account, every permission; 9 KB
  with read-only accounts). Where every account that can
  draft has a signature, the draft texts say that the server adds it, since
  the model signed the message itself. A search for words that finds
  nothing lists the ten newest INBOX emails, with the sender's name, to
  pick from. During development a `tool_set` option chose between this
  surface and the old one; it never shipped in a release, and a leftover
  `tool_set` key is ignored.

  Measured against the built binary, Gemma 4 E4B in Otomo handled 86 of 99
  spoken requests (33 requests, three runs each), where the texts of 2.0.0
  managed 34 of 62 (31 requests, two runs), and 26 of 28 held-out requests
  that were never used for tuning, against 11 of 28. A first version of
  these texts that left out the parameters a spoken request rarely needs
  reached 84 of 99 and 25 of 28, so offering every parameter costs nothing
  measurable. With two read-only accounts, as Otomo runs them, Gemma
  handled 24 of 28 requests of a set written for that case (two runs each,
  which vary by a hit or two). It declined all six requests to reply,
  delete or mark as read, while a follow-up such as "Schreib ihr zurück"
  still drew a claimed reply about one time in four. The prompt per model
  call fell from 15.6k to 10.1k tokens (median), 6.8k with read-only
  accounts. Claude Sonnet 5.5 on the same surface, measured before the
  read-only sentence took its final wording, handled 28 of 28 requests (27
  with the texts of 2.0.0), 12 of 12 held-out ones and 11 of 11 with
  read-only accounts. The texts carry the gain: the server-side help below
  (folder names, the folder list in errors, local dates, the recipient
  error) made no measurable difference for Gemma once these texts say what
  to do, and it stays because it is right for every client. Details,
  including what was tried and dropped, are in
  `mcp-tool-test/gemma/EMPFEHLUNG.md`.
- **Reads over several accounts** (see Breaking), at once, each account
  over its own connection.
- **Measurement kit** in `mcp-tool-test/gemma`: a Dovecot container with an
  invented mailbox, reset before every conversation, a catalog of spoken
  requests checked against the resulting mailbox, runners for Gemma behind
  Otomo and for Claude, and the text variants with their ablation. It never
  talks to a real mailbox.
- **Folder names as people say them.** Every tool that takes `folder` or
  `target_folder` accepts the readable name from `list_folders`
  (`Entwürfe` for `Entw&APw-rfe`), the name in another case when only one
  folder fits, and role words such as `Papierkorb`, `Trash`, `Gesendet` or
  `Posteingang` for the folder with that role. Models pass the name they
  showed the user or the one the user said, and the server refused both:
  Dovecot rejects a readable name as invalid modified UTF-7. Responses name
  the folder as the server knows it, and a name outside ASCII that fits no
  folder is refused with the folder list before it reaches the server. The
  `dry_run` previews keep the name as given, since they make no IMAP call.
  With the measured texts, which say which name to pass, Gemma did as well
  without it (8 of 12 requests either way).

### Changed

- **All dependencies updated to their latest releases.** mail-parser 1.0 is
  a rewrite with a new API and requires Rust 1.98, so `rust-version` moves
  from 1.91 to 1.98. mail-builder 2.0, dirs 7 and rmcp 3.9 needed no code
  changes beyond a renamed type. async-imap 0.12 words errors from SELECT,
  STATUS and SEARCH differently from every other command, and the cleanup
  that reduces them to the server's own text handles both forms.
  mail-builder no longer pulls in `gethostname`, since every draft sets its
  own Message-ID; the fallback that once put the machine's name there now
  writes "localhost". The direct `schemars` dependency is gone as well,
  since the code only ever used rmcp's re-export, which keeps the two in
  lockstep. Flake inputs refreshed, which brings Rust 1.99.1.
- **Dates are shown in the machine's local time**, with its offset
  (`2026-11-09T00:16:00+01:00`), in `date` and `internal_date`. They were
  UTC (`...Z`), and a model telling the user when a mail came read the hour
  off that: "23:19" for a mail that arrived at 00:16. The server still sorts
  on UTC internally, and any ISO 8601 parser reads both forms.
  `date_original` now appears where the sender's offset differs from the
  machine's, no longer wherever it differs from UTC. Gemma got the hour
  right with UTC dates too (4 of 4), because it took `date_original`, which
  carried the local offset of the test mails. Mail from another zone or with
  a UTC `Date` header has no such fallback.
- A folder that does not exist is refused with the names of the folders
  there are, for reading and as a move target, so the next call can take the
  right one. The move refuses before anything is copied. With the measured
  texts it made no measurable difference for Gemma (9 of 12 either way); it
  and the folder names stand in for each other, and with either of them a
  move "to the Papierkorb" worked 4 of 4 times.
- A search for words that finds nothing says what to try instead (`hint`).
  Searches by date or flags only get none, since there an empty result is
  the answer.
- A recipient given as a bare name ("Ben") is refused with the way to its
  address (search the sender) instead of the address format alone. Once
  the measured texts say to look the address up, Gemma no longer needed it.
- `delete_email` on an account without a trash folder under a known name
  refuses and says why. It used to move to a guessed folder named "Trash".
- `text` and `subject` in `search_emails` take several words, all of
  which must occur. Before, the whole value had to occur as written, so
  "Stadtwerke Rechnung" found nothing and a model reported the mail
  missing. "Quoted words" stay one phrase, and filler words (der, vom, the,
  Mail and a few dozen more) are left out. A word joined by hyphens counts
  as its parts when each has at least three letters, so "Telekom-Rechnung"
  also finds "Rechnung der Telekom" ("E-Mail" stays whole), and a hyphen at
  the edge of a word ("Telekom- und Stromrechnung") is dropped. Only ever
  adds matches: every mail the old phrase found contains all its words. The
  hyphen rule came from a held-out request of the Gemma measurement ("Leg
  die Telekom-Rechnung zurück in den Posteingang"), which went from 0 of 2
  to 4 of 4 with it. It changes how the server searches, not what a text
  says; two new requests with hyphenated words went 8 of 8 with and without
  it, since Gemma did not search for the whole word there.
- Non-ASCII attachment names in drafts are written as RFC 2231 parameters
  (`filename*=UTF-8''...`), the form RFC 2183 prescribes, instead of RFC 2047
  encoded words.
- **Keys come in the order the server writes them** (`serde_json` with
  `preserve_order`), no longer alphabetically: counts and `has_more` before
  the rows, `content_warning` before the message it is about, a row
  starting with `account`, `folder`, `uid`, `date`, `from`, `subject` and
  ending with the threading headers, the saved draft's `uid` right after
  `account`, and the parameters of a schema in the order a model should
  fill them. JSON parsers see no difference; a small model reads the raw
  text and often answers from its start.
- A message without a usable `Date` header gets its arrival time
  (INTERNALDATE) as `date` instead of `null`, in listings, searches,
  `get_email` and `get_thread`. Without it such a message had no time to
  tell the user and none to be sorted by among others.
- The error for a missing folder points at `delete_email` only when the
  account may delete.
- A client speaking protocol `2026-07-28` no longer sends `initialize`, and
  rmcp 3.9 answers an `initialize` naming that version with `2025-11-25`,
  the newest version that has the handshake. The handshake-free `tools/list`
  still carries `ttlMs` and `cacheScope`.
- The MCP layer dispatches through the tool router built at startup. The
  macro default it relied on rebuilt the router, all 19 tool schemas
  included, on every `tools/list` and `tools/call`; the stored router it
  was meant to use sat unread behind an `allow(dead_code)`.
- Cargo.toml declares every tokio feature the code uses (`time`, `net`,
  `sync`, `io-util` were only enabled through other crates before).
- Both TLS clients hand rustls its crypto provider (aws-lc-rs) explicitly.
  `ClientConfig::builder()` resolved it from the enabled crate features and
  would panic at runtime once a second provider entered the dependency
  tree.
- `nix flake check` runs the same clippy gate as CI (all targets, pedantic
  and nursery, warnings denied). It used to skip the tests and pass with
  warnings.
- `test-server.sh` uses GreenMail 2.1.15 under its fully qualified image
  name, which podman needs when no registries are configured.
- The GreenMail integration tests run one at a time. They share one
  mailbox, and GreenMail fails a FETCH that races another session's
  EXPUNGE, so parallel runs failed at random.

346 unit tests (up from 283), 22 GreenMail integration tests (up from 17)
and 61 checks in the MCP end-to-end script (up from 35). `test-server.sh`
also creates `B&APw-cher` (Bücher), a folder whose name is encoded on the
wire, and a second user, `zweit`, for the script's rounds with two
accounts and with two read-only accounts.

## 2.0.0

Ergonomics release, driven by a field report from an LLM assistant using the
server unattended (twelve folders, every 15 minutes, nobody watching): in
that mode a misleading description becomes a wrong result and a convenient
default a silent misfire. Three of the report's findings were verifiable
bugs; the rest reshaped defaults and result fields. Major version because
two behaviours and one result field change incompatibly.

### Breaking

- **`account` is required when several accounts are configured.** The old
  silent first-account fallback made every call quietly work in whichever
  account was listed first — for `draft_*` that composed from the wrong
  mailbox under the wrong sender. With one account the parameter stays
  optional (the default is unambiguous); with several, omitting it errors
  and lists the names, so recovery is one retry.
- **`date` fields are UTC-normalized.** Sender offsets were passed through,
  so one response mixed `+02:00`, `-07:00` and `Z` rows: unreadable
  (`11:02-07:00` skims as older than `13:20Z` while being five hours
  younger) and — worse — the cross-folder sort and the thread-representative
  pick order these strings lexicographically, which is simply wrong for
  mixed offsets. `date` is now always `…Z` (sort and compare directly);
  `date_original` carries the sender's rendition when its offset differs.
  Draft quote headers render the time in the reader's local zone, the way
  desktop clients do.
- **`move_email`, `delete_email`, `delete_draft` return `failed` alongside
  `succeeded`** — the input minus what existed. A caller checking only "no
  error came back" read a partial success as a full one; an empty `failed`
  is a statement, a gap in `succeeded` was not. (`mark_*` keeps the
  two-field shape: there the difference means "already in the target state
  or unknown", and calling that `failed` would be the next misdirection.)

### Fixed

- **The documented `get_email` → `download_attachment` flow dead-ended on
  nameless attachments.** `get_email` renders missing names as the
  placeholder `"attachment"`, but `download_attachment` compared against a
  DIFFERENT default (`""`) — the shown name could never match, and with two
  nameless parts even a fixed name is ambiguous. Attachments now carry an
  `index` (the unambiguous handle, accepted by `download_attachment`),
  filename matching uses the same placeholder default as the display, and
  an ambiguous name errors with the candidate indices instead of silently
  picking the first.
- **A client-side-only search no longer blames "Non-ASCII search".**
  `search_emails(has_attachments: true)` without a date failed with
  "Non-ASCII search on this server requires a date filter" — no non-ASCII
  anywhere in the request, on every provider. The message now names the
  actual criteria (`has_attachments`, diverted non-ASCII terms) and the
  actual requirement (a server-side scope such as since/before).
- **`\Recent` no longer appears in `flags`.** It is session-scoped server
  bookkeeping (removed from IMAP4rev2 by RFC 9051) that reads like "new
  for me" and invited exactly that misread.

### Added

- **Sub-day `since`/`before`.** Both accept a time of day
  (`2026-08-15T12:20`, local; or with `Z`/`±HH:MM`) on top of the historic
  day form. IMAP's own operators are day-granular, so the server window is
  widened by a day and the exact cut runs client-side against INTERNALDATE
  — the arrival time, deliberately not the sender-controlled Date header.
  The cut is resolved on a lightweight `(UID INTERNALDATE)` round before
  counting and paging, so `matched`, `offset` and `has_more` operate on
  rows a caller can actually get — no discarded full-body transfers, no
  empty pages on the way to the first hit. Result rows echo the arrival
  time as `internal_date` while the bound is active: `date` is the
  sender's header and may legitimately sit outside the requested window,
  which would otherwise read as a filter bug with no way to verify it.
  Previously "everything since 12:20" meant fetching all unread mail and
  hand-filtering, and a capped page of the newest N rows silently dropped
  the rest.
- **`group_by_thread` on `search_emails`** — same collapsing as
  `list_emails`, so "unread since 12:20, grouped by conversation" is one
  call instead of a capability puzzle across two tools. `unread_only` is
  accepted as an alias for `is_read` (the two tools named the same filter
  differently); contradictions are refused.
- **`has_more` on every listing tool, `returned` on `list_emails`,
  `limit_capped` when a requested limit was cut** — the three tools shipped
  three different count-field combinations, each requiring different
  arithmetic to answer "is there more". `has_more` answers it directly.
- **`retryable` on every error** — the one bit the error text cannot
  convey: whether repeating the call later can help. A field report showed
  an unattended run treating "Server Unavailable" as permanent and skipping
  a folder for a day. Transient states (unavailable, in use, connection
  class) are `true`; facts (no such folder, unknown UID, permission denied)
  are `false`.
- **`allow_flag_change` per account plus `dry_run` on all four flag
  tools.** The protections sat inversely to the danger: `move`/`delete` had
  preview and per-account gates while `mark_as_read` — which erases a
  human's work queue with no trash and no record of which messages it hit —
  had neither. The existing pattern, applied consistently.
- **`folder_display` in result rows** — the modified-UTF-7-decoded folder
  name (`Gel&APY-schte Elemente` → `Gelöschte Elemente`), previously only
  in `list_folders`; a cross-folder hit from the trash is now recognizable
  in place.

### Changed

- **The server instructions were cut to fit the client's truncation
  budget.** Field reports (and this repo's own development sessions) showed
  MCP clients truncating the ~3.4 KB instructions mid-word — from the
  permissions section on, with the reader unaware anything was missing.
  Now ~1.6 KB, ordered security → permissions → workflow, with everything
  tool-specific moved into that tool's description; a test pins the budget.
- `list_emails` and `search_emails` describe their return shape as a field
  list instead of one-line pseudo-JSON, and document honestly that
  `has_attachments` counts every non-text part (signature images and
  S/MIME signatures included) — read it as "has parts", not "has a
  document".

283 unit tests (up from 261) and 17 GreenMail integration tests (up from
16 — the new one proves the sub-day cut runs on INTERNALDATE over the
wire), plus `tests/e2e_mcp_all_tools.py`: an MCP-layer end-to-end round
that exercises every one of the 19 tools as real JSON-RPC calls against
the built binary — 35 checks covering the account requirement,
`retryable`, grouped sub-day search with the `internal_date` echo, the
index-based attachment round trip, `dry_run` previews and the `failed`
field.

## 1.6.1

### Fixed

- **The inline-image `multipart/related` container lacked its mandatory
  `type` parameter.** RFC 2387 — the very RFC the hand-built tree cites —
  requires `type="multipart/alternative"` so a client knows the root part
  before walking the children. Tolerant clients guess; strict ones may treat
  the container as malformed and show the body as a detached attachment,
  the exact failure the hand-built tree exists to avoid.
- **Inline `Content-ID`s are now globally unique msg-ids.** The marker id
  was written verbatim as `Content-ID: <shot>` — not the `local@domain`
  shape RFC 2045 requires, identical in every draft that names its image
  `shot`, and a fingerprint of this tool. Clients and gateways that cache or
  deduplicate inline parts by Content-ID could show another mail's image; a
  `cid:` reference inside a quoted original could collide outright. The wire
  id is now `<id.random@domain>` (domain as for `message_id_domain`) while
  the marker id stays the user-facing handle. One shared `is_valid_cid`
  rule (alphabet, 128-byte cap, RFC 5322 dot-atom placement: no leading,
  trailing or doubled dots) now governs the body scanner, explicit cids and
  the derived fallback alike — previously `derive_cid` had no length cap,
  so a long file-name stem minted an id the scanner rejected by
  construction (a dead end with a misleading error), and `shot..png`
  produced `<shot..uuid@domain>`, which is not a valid msg-id local part.
  An empty `message_id_domain = ""` in the config now counts as unset
  instead of minting `<uuid@>` ids.
- **The marker scanner is linear now — including validation and rendering.**
  A body crafted as thousands of `![` sharing one distant `]` made the scan
  quadratic — minutes of CPU within the 10 MiB body cap, on the async
  worker. Bracket, parenthesis and newline lookups go through a
  forward-only cache. Review of the fix found two more quadratics hiding
  beside it: the stray-fragment check re-tested every `](cid:` occurrence
  against the whole marker list (O(k²) on the SUCCESS path — empirically
  ~7 minutes at 10 MiB of repeated valid markers) and id dedup walked a
  `Vec` (O(u²) in distinct ids, reachable with zero attachments). Both
  lists are position-sorted, so the fragment check is now a two-pointer
  walk and dedup a `HashSet`; validation runs off ONE shared scan, the
  render passes look refs up in a map, and regression tests pin all three
  shapes.
- **A stray `![` in prose can no longer swallow paragraphs.** The alt text
  of a marker must stay on one line (and within 300 bytes); previously a
  `![` followed pages later by `](cid:x)` moved everything in between into
  an invisible `alt` attribute of the rendered HTML.
- **Marker validation and HTML rendering see the same markers.** Rendering
  used to re-scan the HTML-escaped body, so an id the raw-body validation
  rejected (e.g. containing `"`) could be accepted after escaping — saved as
  visible marker source with no warning. The HTML pass now scans the raw
  body and escapes around the markers; ids are additionally restricted to
  the attachment-cid alphabet (letters, digits, `.`, `_`, `-`, max 128), so
  the scanner can never accept a reference no attachment could carry.
- **Malformed marker attempts are refused, not silently kept as text.** A
  `](cid:` fragment the scanner rejects — an id with spaces (the shape every
  downloaded `Bildschirmfoto 2026-… .png` produces), an alt spanning lines —
  now fails the draft with the offending line and the marker grammar,
  instead of saving a draft that shows raw marker source. Scoped to inline
  context: with no inline attachments and no valid markers the fragment is
  most likely prose *about* the syntax, so the draft saves with a warning
  naming what was seen — explaining the feature in a mail must not make it
  unsendable.
- **Reading back an own draft no longer trips `body_parts_diverge`.** The
  plain part's `[alt]` placeholders sit in the HTML only inside `alt`
  attributes, which the text extraction discarded — the exact
  hidden-extra-text signature the divergence heuristic flags. `strip_html`
  now keeps `<img>` alt texts as text (they are what a reader sees with
  images blocked), which also makes the heuristic fairer to incoming mail
  with meaningful alt texts. The extraction is quote-aware: a `>` inside a
  quoted attribute value (`alt="Umsatz > Vorjahr"` — legal HTML) no longer
  truncates the tag, losing the alt and leaking the attribute tail into
  the body text, and an `alt=` lookalike inside another attribute's quoted
  value (`data-caption="… alt='x' …"`) is no longer mistaken for the real
  alt. Documented trade-off: counting alt text as visible gives a sender
  one more place to mirror plain-part text where an HTML reader will not
  see it (an alt behind an always-loading inline image) — one of several
  evasions the heuristic already cannot catch, now listed in the README's
  prompt-injection section.
- **Attachment objects with unknown fields are rejected by name.** The
  untagged deserialization ignored unknown fields, so a typo like
  `"inlin": true` silently degraded the entry to a regular attachment; a
  wrong type produced only "data did not match any variant". Both now fail
  with the field name respectively the accepted shapes, and the parameter's
  JSON schema is kept `$ref`-free for strict client-side validators.
- **Inline type and size checks run before the file is read.** A PDF marked
  inline, or an oversized file, was read fully into memory (up to 50 MiB —
  or unbounded for the oversize check itself) only to be rejected; the
  extension-based type check and a metadata size precheck now fire first,
  with the post-read check kept authoritative.
- **`move_email`, `delete_email` and `delete_draft` report only UIDs that
  actually existed.** All three echoed the input list as `succeeded` — but
  IMAP UID commands silently ignore nonexistent UIDs, so a stale UID
  (rotated UIDVALIDITY, externally expunged, typo) was reported as moved or
  deleted when nothing happened. The confirmation source is a `UID SEARCH`
  up front, not the STORE acknowledgements an intermediate version used:
  RFC 3501 only SHOULDs the untagged FETCH and RFC 7162 explicitly allows
  omitting it for a no-op change (a message another client already flagged
  `\Deleted`), so acknowledgement-based reporting could under-report a
  fully processed message — and an under-reported move invites the retry
  that duplicates it. Existence-before-action cannot under-report;
  `mark_as_read` keeps its acknowledgement-based reply, whose "actually
  updated" meaning is documented and where a retry is harmless. Side
  effect: replacing an already-gone draft via `replaces_uid` now correctly
  returns `replace_warning` instead of a false `replaced_uid`. An
  integration test pins the parallel-client shape: a pre-flagged message
  that moves is reported, a stale UID is not.
- **The Outlook 365 non-ASCII `text` fallback matches the full body.** The
  diverted terms were checked against the 200-character snippet, silently
  dropping every mail whose term appeared later — undocumented false
  negatives in exactly the searches the fallback exists for. Full-text
  criteria now travel into the IMAP client and run against each fetched
  message's subject, addressing headers and complete `body_text` before it
  is cut down to the snippet — RFC 3501's `TEXT` matches header OR body, so
  a body-only fallback would still have dropped mails carrying the term
  only in their subject.
  `matched` stays the server-side count and therefore an upper bound, as
  documented. (Known remaining gap, deliberate: a non-ASCII `to` fallback
  still sees only the 3-recipient summary preview — IDN mailbox names are
  rare enough that the extra plumbing isn't warranted.)
- **The all-folder search docs described a skip that never existed.** README
  and tool description claimed Gmail's `[Gmail]/All Mail` mirror "is
  skipped to avoid duplicates"; the code searches every folder and
  deduplicates by Message-ID afterwards. The code is the correct side: an
  actual skip would lose archived mail, which exists *only* in All Mail.
  The docs now say what happens (and why `matched` counts per folder).
- `draft_email` and `draft_forward` echo the sanitized recipients and
  subject — the values actually written to the headers — instead of the
  raw input: a request smuggling `\r\nBcc:` into an address had the
  injection stripped from the saved draft but reflected verbatim in the
  response. `draft_email` also routes its recipients through the same
  `clean_recipients` helper as the other flows, and the attachments list
  is capped at 100 entries (the byte caps alone allowed thousands of
  one-byte files).
- **`rust-version = "1.91"` declared in Cargo.toml.** The crate uses APIs
  stabilized in recent releases (`File::lock`, `Duration::from_mins`,
  `str::floor_char_boundary`); an older toolchain previously failed with
  bare E0658 compiler errors instead of cargo's one-line version message.
  Determined empirically: 1.91 compiles, 1.90 does not. The flake pins
  latest stable and is unaffected.
- The `draft_*` tool descriptions now document the object attachment form,
  the `inline_warning` field, and the revision caveat that `get_email`
  returns `[alt]` placeholders — markers and inline attachments must be
  re-supplied when revising via `replaces_uid`, otherwise images degrade to
  regular attachments.

- **Claude Code refused every tool of this server** with
  `Failed to fetch tools: Invalid result for tools/list` naming two missing
  fields, `ttlMs` and `cacheScope`. The server itself was healthy — it
  started, connected both accounts and answered `initialize` — but none of
  its tools were registered, while every other MCP server on the same client
  kept working.

  Cause: the client offers protocol version `2026-07-28`, and rmcp 3.0.0
  accepts it. Under that version `tools/list` switches to the new discovery
  shape (it does set `resultType`), but rmcp 3.0.0 omits the `ttlMs` and
  `cacheScope` fields that shape requires, so the client rejects the whole
  response. Servers negotiating an older version — everything else in this
  setup — are validated against the old shape and stay unaffected, which is
  why the fault looked server-specific.

  Fixed by moving to rmcp 3.1.2, which emits both fields. Verified by
  replaying the client's handshake: with `2026-07-28` the response now
  carries `ttlMs: 0` and `cacheScope: "public"` alongside `resultType` and
  `tools`.

### Changed

- All 24 dependencies updated to their latest compatible releases, among them
  tokio 1.53.1, rustls 0.23.43, mail-parser 0.11.6 and schemars 1.2.2. No
  major version is outstanding.

261 unit tests (up from 240), the 16 GreenMail integration tests — now also
proving over the wire that a stale UID mixed into a move or delete is absent
from `succeeded`, that re-deleting a gone draft returns empty, and that the
body filter matches full bodies on a real server — and a live
`initialize` + `tools/list` + `draft_email` exchange under protocol
`2026-07-28` — including a stored-draft MIME inspection — verified alongside.
The handshake replay is also what caught the `$ref` reappearing in the
*served* schema after a unit test on the bare type had passed.

## 1.6.0

### Added

- **Inline images in drafts.** An `attachments` entry may now be an object
  instead of a path: `{"path": "…", "inline": true, "cid": "shot"}`. Reference
  it from the body as `![alt](cid:shot)` and the image renders at that exact
  spot instead of dangling at the end of the mail. `cid` is optional and
  defaults to a slug of the file name — extension dropped, characters outside
  letters/digits/`.`/`_`/`-` collapsed to `-` (`Rollen und Rechte.png` →
  `Rollen-und-Rechte`); setting it implies `inline`. Bare path strings keep
  their meaning, and both spellings mix in one array, so existing callers are
  unaffected.

  The MIME tree follows RFC 2387: inline parts go into a `multipart/related`
  beside the HTML that references them, regular attachments stay outside it in
  a `multipart/mixed`. `mail-builder`'s own `.inline()` helper places the part
  next to the `multipart/alternative` instead, which some clients render as a
  detached attachment — so the tree is assembled by hand when inline images are
  present, and left untouched when they are not.

  The plaintext part receives a readable `[alt]` placeholder at the same
  position, so a text-only reader still learns that an image belongs there.

### Fixed

- Mismatches between body markers and attachments are caught before the draft
  is saved. A marker with no matching attachment is an error that lists the
  available ids; an inline attachment that no marker references is saved but
  reported back as `inline_warning`, because the recipient's client would
  otherwise place it arbitrarily.
- Only raster images (`image/*` except SVG) can be inlined. A marker always
  renders an `<img>` tag, so a PDF marked inline would arrive as a broken
  picture. SVG is refused because it can carry script and inline files often
  originate from a received message via `download_attachment`.
- `draft_email` reports the rendered body in `body_preview`, the way
  `draft_reply` and `draft_forward` already did. It previously echoed the raw
  input, so with inline images the preview showed `![alt](cid:…)` markers that
  no longer exist in the saved message.

## 1.5.1

### Fixed

- The text-part signature derived from `signature_html` carried raw newlines
  from the HTML *source* into the rendered text, breaking lines mid-sentence
  (`Telefon:` / number split apart) and inserting a stray blank line between
  every signature line. Source whitespace now collapses the way a browser
  renders it; line structure comes from tags alone. Found by diffing a live
  1.5.0 draft against the hand-written client reference.
- A signature-separator line renders as `-- ` (with the trailing space RFC
  3676 defines and clients write) instead of a bare `--`.

## 1.5.0

Draft fidelity release: a saved draft is now indistinguishable from one
composed in the mail client itself. Found by diffing a hand-written client
draft against a generated one — same HTML, but four tells in the headers and
the text part.

### Added

- **`message_id_domain` config option (per account).** Without an explicit
  Message-ID the MIME builder generated one at write time using the
  **machine's hostname** as the domain — leaking the local machine name into
  every draft and marking it as machine-built. Drafts now always carry an
  explicit Message-ID; the domain defaults to the sender address's domain, so
  the fix needs no configuration.
- **`signature_text` config option (per account).** The signature previously
  existed only in the HTML part; a client whose user reads the text part saw
  none, and the text/HTML divergence is exactly what this server flags on
  *incoming* mail. When unset, a text rendering is derived from
  `signature_html` automatically.

### Changed

- **Reply and forward quotes keep the original's formatting.** The quoted
  HTML is now the original's own HTML run through a sanitizer (ammonia):
  bold, links, tables and inline styles survive; scripts, event handlers and
  `javascript:` URLs do not. Previously the quote was the escaped plaintext
  body — safe, but visibly not what a mail client produces. Text-only
  originals keep the escaped-plaintext path, wrapped in the same structure
  desktop clients use for them.
- **The plaintext part now mirrors the client format**: signature included,
  original quoted below a locale-aware `From/Sent/To/Subject` header block —
  replacing the `> `-prefixed quote with an "On … wrote:" intro line, which
  no desktop client writes.
- **`Date` header in the local timezone** (via jiff) instead of the MIME
  builder's UTC fallback.
- **Drafts are appended with `\Seen`** alongside `\Draft` — clients save
  their own drafts as read; an unseen draft renders bold in the Drafts
  folder and marks it as externally injected.
- **Reply To/Cc headers keep display names** (`Name <addr>`) from the
  original instead of bare addresses.

216 unit tests (up from 206), 15 integration tests against a real IMAP
server, clippy pedantic clean.

## 1.4.3

### Changed

- The parsers are now fuzzed. `decode_modified_utf7` and `extract_message_id`
  read data that arrives from the server or the sender; `percent_decode` and
  `parse_query` read a browser redirect that the authorization URL can
  influence. Each is now driven through several hundred adversarial inputs —
  truncations at every byte boundary, pairwise concatenations, lone `%` and
  `&`, invalid base64, bare surrogates and byte sequences that are not valid
  UTF-8 — asserting only that nothing panics. An earlier `percent_decode`
  sliced a multi-byte character in half and did exactly that, so the class of
  bug is not hypothetical. 206 unit tests, up from 204.

## 1.4.2

### Changed

- The permission gates now have tests. `read_only`, `allow_move`,
  `allow_delete`, `dry_run` and the 1000-UID cap were implemented and
  documented but never verified — the switches a user sets to make an account
  observable-but-untouchable, checked by nothing. Covered now: `read_only`
  refuses all six mutating tools, the finer switches refuse on their own and
  *stop* refusing once enabled, oversized UID lists are rejected rather than
  truncated (truncating would act on a different set than asked for), and
  `dry_run` previews without reaching the network while still respecting the
  gates — previewing a forbidden action would suggest it is available.
- The tests point the account at a closed port, so a gate that stopped firing
  before the IMAP call would attempt a connection and fail the test instead of
  passing quietly. They complete in microseconds, which is itself the evidence
  that nothing went out. 204 unit tests, up from 199.

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
