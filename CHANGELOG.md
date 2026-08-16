# Changelog

Notable changes per release. Versions follow [semantic versioning](https://semver.org):
the MCP tool surface and the config format are the public API.

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
