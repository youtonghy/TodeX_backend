# Conversation runtime contract

Deploy the compatible backend before updating clients. Desktop and Web use the
shared runtime in `TodeX_protocol/src/conversationRuntime.ts`; new wire fields are
optional and canonical aliases are recomputed when old journals are replayed.

- REST pages and live frames enter one contiguous projection. A high sequence
  waits for its gap; duplicate sequences cannot append text or usage twice.
  A partial/failed replay cannot expose historical approval actions.
- Prompt commands await a correlated acknowledgement. Rejection restores text,
  skills and attachments. A sent command that loses its acknowledgement is
  unknown: recover the record before sending again; never resend automatically.
- Requested permissions and locally validated settings are distinct from values
  explicitly confirmed by a provider. Unsupported settings are rejected by the
  backend and disabled by its advertised permission matrix.
- Two minutes without protocol progress displays a waiting notice. Approval
  waiting is excluded and quiet turns have no fixed overall deadline.
- Fork and manual compact are exposed only when advertised. Codex uses native
  operations; legacy resume is not presented as retry. Retry starts a new turn
  from a validated request snapshot and does not undo prior file changes.
- Usage belongs to a turn/message and updates a stable snapshot. Unknown usage
  is not zero. TPS is omitted without a reliable generation interval. Automatic
  compaction updates its own state without ending the parent turn.
- Claude Code `result` frames close a model turn, not the process. A turn stays
  open while provider-reported background tasks remain; a zero-iteration result
  means the prompt was consumed unanswered and is resent on the open stream.
- Subagent runs are normalized to `subagent.*` events carrying a stable
  `subagentId`, `title`, `task`, `status` and optional `parentId`, `turnId`,
  `providerItemId`, `agentKind`, `agentId`, `result`, `error` and `metadata`.
  Claude Code surfaces Task/Agent tool calls plus `task_started`/
  `task_progress`/`task_notification` frames; Codex uses `subAgentActivity` and
  `collabAgentToolCall` items; Grok Build maps `subagent_*` session updates;
  Devin (once the client advertises `cognition.ai/subagentSupport` in the ACP
  `initialize` capabilities `_meta`) tracks each subagent on a pseudo
  `tool_call_update` whose `toolCallId` is the agent id and whose `_meta`
  carries `cognition.ai/subagent_started` / `subagent_completed` markers.
  Providers without native subagent signals emit no `subagent.*` events.
  Claude Code also stamps `subagentId` (the `parent_tool_use_id` of their
  frames) on every event a subagent produces — `tool.*` snapshots,
  `message.delta`/`thought.delta` chunks and `message.completed` envelopes;
  Devin does the same through each update's
  `_meta["cognition.ai/subagent_context"].parentAgentId`, and a subagent's
  `usage_update` lands on the run instead of the turn total.
  Clients fold those rows into the run's trace detail instead of letting them
  interrupt or replace the assistant stream they happen to interleave.
- Memory configuration is separate from memory content; the panel explicitly
  reports when the provider has no readable content source.
- Streaming is coalesced before it reaches the journal. Text fragments of one
  stream merge for up to 100 ms (16 KiB of text) into one `message.delta` /
  `thought.delta` whose text is the concatenation: Pi per content block; Codex
  per item block (reasoning keeps `delta` and `thought` equal); Claude Code
  `text_delta`/`thinking_delta`/`input_json_delta` (`partial_json`) per
  content-block index (signatures stay per fragment); ACP text chunks per
  message. Codex, Claude and ACP buffer in the conversation store, so any
  other event of the conversation (from any writer), a history read and daemon
  shutdown flush the open window first and sequences stay contiguous and in
  emission order. ACP and Pi `tool.updated` carry a full snapshot and
  in-progress snapshots of a call are sent at most every 500 ms; the newest
  pending one is journalled before the call's `tool.completed` (Pi) and before
  the turn ends, and terminal status is always sent. Pi stream frames keep
  their own fields (`delta`, `content`, `toolCall`, `contentIndex`, …) but not
  `partial`, Pi's snapshot of the whole message so far. A `quota.updated` or
  `usage.updated` identical to the previous one of its type from the same
  turn (or runtime scope) is not journalled again; the quota snapshot behind
  `/v2/providers/quota` is still refreshed. Clients append deltas per block
  and replace tool rows per event, so the rendered result is unchanged; live
  deltas arrive at most 100 ms later.
- A final `message.completed` may carry `block.supersedes`: the
  `assistant_progress` block ids its text was streamed under. Clients remove
  those progress entries so the answer is shown once. Pi sets it only on final
  (`stop`/`length`) messages; tool-use narration stays as progress.
  Independently, a `message.completed` whose assistant text already contains
  the joined text of earlier streamed segments supersedes them: Claude Code
  emits one envelope per message after its deltas, so steps interleaved with
  the stream must not leave each fragment beside the full text again.
- A provider stdout line that is not JSON, or is longer than 4 MiB, does not
  fail the turn. Turn loops journal up to 20 such lines per turn (per session
  for a resident Pi runtime) as `provider.event` with `{kind: "invalid_line",
  preview}` (redacted, at most 512 bytes) or `{kind: "oversized_line", bytes}`
  and continue; further lines, and lines read outside a turn loop, are only
  logged. An oversized line is discarded up to its next newline unbuffered.
- Claude Code control requests (tool permissions and `AskUserQuestion`) are
  answered by detached tasks while the turn's read loop keeps draining
  stdout, so an open prompt cannot fill the pipe and freeze the provider —
  including its async subagents. `control_response` frames are matched by
  `request_id`, and a turn that ends first resolves the prompt as cancelled.
- Payloads larger than the append budget (1 MiB − 16 KiB serialized) are
  truncated, not rejected; redaction runs first. The largest strings (over
  512 bytes) are cut on a UTF-8 boundary and end with `…[truncated N bytes]`
  (N bytes removed); an object payload gains a top-level `truncated` map
  (`_truncated` if that key is taken, none if both are) from JSON pointer to
  original string bytes. If cutting strings cannot fit, the payload becomes
  `{"truncated": true, "originalBytes": N}` plus its short top-level scalars.
  The 1 MiB limit remains a final check.
- Every started turn gets one terminal event. A panicking driver ends it with
  `turn.failed` code `PROVIDER_PANIC` (other task join errors:
  `PROVIDER_TASK_CANCELLED`). A failed terminal append is retried after
  100 ms, 500 ms and 2 s, each retry first checking the journal so the event
  is never duplicated; if every attempt fails the manifest status is forced to
  `failed` without an event and restart recovery closes the turn.
- `[agent].provider_idle_timeout_minutes` (default 60, `0` disables; env
  `TODEX_AGENTD_PROVIDER_IDLE_TIMEOUT_MINUTES`) stops a turn whose provider
  produced no stdout frame or event for that long. It does not fire while a
  permission request of the conversation is pending. The turn is cancelled
  normally, the driver is aborted if it has not finished 30 s later, and the
  turn ends with `turn.failed` code `PROVIDER_IDLE_TIMEOUT`.
- WebSocket delivery never waits on a stuck peer: a socket send over 20 s or an
  outgoing queue full for over 10 s closes the connection. Subscribe backfill
  runs beside the read loop, so replies to later commands may interleave with
  it; a subscription's own order stays backfill frames → ack → live events.

Backend control/write/cancel/compact defaults are 30/10/10/300 seconds, configured
with `TODEX_AGENTD_PROVIDER_{CONTROL,WRITE,CANCEL,COMPACT}_TIMEOUT_SECONDS`.
The first three accept 1–3600 seconds; compact accepts 1–86400 seconds.

The backend's journal remains authoritative. Its storage format is history v3
(`docs/history-encryption.md` §4): one logical sequence of files ordered by
segment number — sealed segments (`events.NNNNNN.seg` with its `.idx`, or
`events.NNNNNN.jsonl` briefly between sealing and conversion) followed by the
active `events.jsonl`, the only file appends touch. Every new line is a
short-key v3 record (`s`, `i`, `t` in µs, `y`, `r`, `p`, envelope `e`, content
`c`); v2 full-event lines and v2 compaction markers still decode, and every
read returns the same `ConversationEvent` as before, so the wire format is
unchanged. Once the active file passes 64 MiB the next append renames it to
the next segment number and starts a fresh `events.jsonl` (a crash between the
two leaves the journal ending in a sealed file, which the next append
recreates the active file for). The background maintenance task then
converts the sealed file into a `.seg`: three streams (envelope lines, summary
payloads, full payloads) cut into ~1 MiB raw-DEFLATE frames with CRC-32, and a
checksummed `.idx` holding the frame table, the segment's journal digest and
the `.seg` SHA-256. The commit renames `.idx`, then `.seg`, then deletes the
plaintext source and syncs the directory; on the first access after a restart
a `.seg` beside its surviving sources is kept only when its whole content
hashes to the index, otherwise it is removed and the conversion redone.
Sealing also slims history: a `message.delta` whose `block.id` reaches a later
`message.completed` (same turn, or listed in `block.supersedes`), a
`tool.updated` whose call reaches a later `tool.completed`/`tool.failed` or a
terminal-status `tool.updated` (ACP), and a `subagent.updated` whose run
reaches a later `subagent.completed`/`failed`/`cancelled` — all within the same
segment — become `journal.compacted` markers keeping sequence, event id and
time (payload `{"reason": "compacted", "originalType", "runStart",
"runLength"}`). Deltas without a block id (Claude Code, ACP) and
`thought.delta` are kept. Sequences stay dense and global across files.

The replay index of a conversation holds each sealed segment's range and frame
table plus per-record offsets of the plaintext files (the active file stays
under 64 MiB); indexes are LRU-bounded to 64 conversations and decompressed
frames share one 32 MiB LRU. Replay pages read plaintext spans and decode
sealed frames transparently, with the same limits (1000 records or about
8 MiB of journal, a sealed record counting the length of its v3 line). A cold
index reads only the `.idx` files and newline-scans the plaintext files,
parsing their boundary records; anything unclean falls back to the full
validating scan. In a release build a 1,000,000-event synthetic journal (1.56 GiB of v3
lines, random log-like tool output) sealed to 264 MiB in 22 s; three real
64 MiB journals sealed to 4.0, 5.85 and 5.75 MiB. On that synthetic journal a
release build measured: tail read 14 ms, cold open plus newest 200-record page
11 ms, digest 13 ms, RSS growth on open 6.5 MiB, backward paging to the first
page 0.49 / 1.71 ms p50 / p99 per page, `save_request` 7.9 / 8.8 ms p50 / p99
(local macOS measurements, not guarantees).

Conversations written before v3 are migrated lazily: no rewrite at startup;
two minutes after it, and every ten minutes, the maintenance task takes idle
conversations (not running or waiting, no write for two minutes) largest
first, hard-links their plaintext files into `journal-v2-backup/` (copies
where links fail; `backup.json` marks a complete backup), seals the active
file, and converts up to 64 MiB of consecutive files per segment, committing
each segment on its own so a killed daemon resumes. The manifest then gains
`storageVersion: 3` (new conversations carry it from the start); older
daemons cannot read a migrated conversation. Backups older than seven days
are deleted. Fork streams the source journal page by page into the new
conversation's v3 files, so a fork of any size copies in bounded memory.

No request path loads the whole history. A per-conversation journal digest
(`src/conversation/digest.rs`) holds only ids, sequences and status facts read
from each event's type and small identity fields: the first and newest
`message.created` per `clientRequestId` (with the newest `turnId`), the first
`control.requested` and newest outcome per control `requestId`, the first
native `queueAdd`/`steer` control per delivered id, turn-terminal events per
`turnId`, the newest user message, and the restart-recovery fold (status, open
turn, last runtime state, unresolved permissions). Prompt and control
idempotency, retry, terminal-event retries, follow-up delivery checks and
restart recovery query it and then point-read the one or two events whose
payload they compare. Digests of adjacent ranges merge: each sealed segment
stores its own in its `.idx`, so the digest is built once per process by
merging those (no sealed bytes are read) and paging through the plaintext
files with the replay index (validating and salvaging like replay), folded
forward by every append under the conversation lock, and rebuilt when the
journal file fingerprint changes (salvage, deletion). On a local debug fixture
of 200,000 events (55 MiB) a duplicate-prompt lookup took 1.7 s by full scan
and 0.3 ms from the warm digest (control lookup 0.5 ms); the one-off digest
build took 1.9 s and raised peak RSS by 15 MiB, versus about 240 MiB for one
full scan.

The synced journal line is the only per-append commit point. Manifests are
cached in memory: `manifest.json` and `snapshot.json` are written at once on
create, status change, metadata update, forced status, and recovery that
changes the manifest (a manifest already in step with its journal is left
untouched, sparing two fully synced writes per conversation at startup); other
fields (`lastSequence`, `updatedAt`) reach `manifest.json` through a flush
debounced to 2 s and on shutdown, and are rebuilt from the journal after a
crash. An append whose manifest is behind or ahead of the journal follows the
journal. Release append latency on macOS (200 appends) went from p50 20.02 /
p95 22.93 ms to 3.94 / 4.09 ms. Atomic JSON writes no longer create missing
parent directories, so a late flush cannot resurrect a deleted conversation.

Journal repair: a final record missing its newline is terminated during
recovery, and an append to an unterminated journal starts a new line. An
append whose write or sync fails (`ENOSPC`, `EIO`) cuts the active file back
to its length before the write, drops the cached index, digest and tail, and
returns the error; a cut that fails too is logged and left to recovery. A
failed seal (the rename of the active file, e.g. while a scanner holds it on
Windows) is only logged: the record goes to the active file and the next
append tries again. Corrupt plaintext lines with no valid record after them
are quarantined to `events.corrupt.<ts>.jsonl` and cut off. Interior
corruption is salvaged per file: the damaged files are backed up to
`events.corrupt.<ts>.jsonl`, then each is atomically rewritten in place with valid records unchanged and each
lost sequence replaced by a `journal.recordLost` event with payload
`{"reason": "corrupt", "runStart", "runLength", "backup": "<file>"}`.
Placeholders of one run share `runStart`/`runLength` and take the previous
valid event's time; their count is bounded by the corrupt bytes, and clients
cannot append that event type. A damaged `.seg` frame or `.idx` rebuilds only
that segment: frames are located through the index or, when it is lost, by
walking frame headers; records whose envelope and full content still decode
are kept (the summary stream is regenerated), the rest become
`journal.recordLost` placeholders, and the damaged files are kept as
`events.corrupt.<ts>.seg`/`.idx`. No repair collapses the journal into one
file.

End-to-end encrypted history (always on, `docs/history-encryption.md`
§3–§5, §8): every append is sealed under the
conversation's current DEK at commit time — coalesced stream text when its
window is journalled — after the payload's deduplication fields are replaced
by MACs. The journal line keeps only the envelope `e` in plaintext and stores
the ciphertext as `x`; the event the store returns, folds into the digest
and publishes on the hub is the same stored form (envelope plus `$enc`), so
live delivery carries the bytes on disk and the digest needs nothing but
envelope fields. A record's summary ciphertext is computed from the
plaintext when it is written; replays never summarize ciphertext and only
drop the ciphertext the requested detail does not use. Rotating the active
file also rotates the DEK (a seal whose rename fails keeps the file and its
DEK). A failed append, a recovery that cuts records off the journal and an
append that finds the manifest ahead of the journal retire the DEK for good,
not even leaving it as the no-recipient fallback, so a reused sequence is
never sealed twice under one key. The seal converter repacks the sealed file's
records into encrypted frames with the DEKs still in memory (decrypting,
slimming and re-framing them), then releases those DEKs; ciphertext whose
DEK is gone (the daemon restarted) is framed as it is. Sealed frames reach
clients as ciphertext read straight from the `.seg` (`frames` on HTTP pages
and on each socket replay message that needs one). With no recipient left,
new prompts and new conversations are refused with `HISTORY_KEY_REQUIRED`
before anything is written while a
running turn keeps the newest DEK in memory; without one the append fails
instead of writing plaintext. The `last-request.json` of an encrypted
conversation holds no prompt text (`conversation.retry` then needs the
client's decrypted `prompt`), titles are stored as `titleEnc`, and control
retries compare MACs (completed results are remembered in memory only). Imports and forks
are assembled encrypted: plaintext records of the draft are sealed under a
one-off key recorded in the draft's keyring, copied ciphertext keeps its
`$enc`. Plaintext history from older versions is never migrated: a one-time
startup scan (or the first write attempt) marks it `legacyPlaintext` and
the conversation becomes read-only (`HISTORY_READ_ONLY`); fully encrypted
conversations carry `historyEncryptedAt`. Deleting a conversation removes its keyring with its
directory and forgets its DEKs.

History has no size limit: every append lands, so a running turn can always
finish, and forks can carry histories of any size. The only gate is on new
prompts: when the filesystem holding the data directory has less than 1 GiB
available, saving the prompt's request snapshot fails with `STORAGE_LOW`
(HTTP 507); running turns keep appending. `JOURNAL_FULL` is no longer
returned. Replay pages
(`afterSequence`, `beforeSequence`, WebSocket subscribe backfill) stop at
`limit` events or about 8 MiB of journal, whichever comes first, but always
hold at least one event; clients keep paging while `hasMore`.

On unix every spawned provider, including the `codex.local` app-server, is
recorded in `<data_dir>/provider_processes.json` (0600, atomic rewrite from the
blocking pool, newest snapshot wins) with pid, pgid and start time, next to the
owning server's pid and start time. Start times come from the kernel
(`proc_pidinfo` on macOS, `/proc/<pid>/stat` plus the boot id on Linux) and
from `ps` elsewhere or for records an older daemon wrote. At startup the
server kills each recorded process group whose leader still has the recorded
start time, unless the owning server is still alive: then the new server
neither reaps nor tracks. Linux also sets `PR_SET_PDEATHSIG` (SIGKILL). Both
are no-ops on Windows.

Before reporting daemon readiness, startup recovers each conversation journal
once and reuses its digest to cancel stale approvals and mark resident
runtimes stopped. Whether a turn was open comes from the journal (the last
`turn.started` without a terminal event), not the manifest status: an open turn
gets `conversation.interrupted` carrying its `turnId` when known, while a
closed turn under a stale `running` manifest only has its status corrected,
with no event. Settled conversations skip this scan: provider not Pi, status
not `running` / `waiting_permission`, manifest `lastSequence` equal to the
journal tail, and either status `idle` or a journal that is empty or ends on a
turn- or conversation-terminal event. `idle` suffices because `turn.started`
persists `running` at once and only a finished turn or operation returns to
`idle`, so informational tails (imported Codex history, a provider command
catalog after a turn) no longer force a scan; `failed` / `interrupted` still
need the terminal tail, since a paused workflow is `interrupted` with its turn
open. Corruption in a skipped journal surfaces on its first read.
Recovery progress is logged every 25 conversations.
Daemon startup waits up to 120 seconds for initialization; on timeout the
spawned child is terminated and the daemon log identifies the last recovery
progress. No in-progress turn is replayed automatically.

The backend follow-up queue (`queue.json`, see API.md "后端追加队列") is held
by the supervisor for every provider. A turn task releases its active slot
before it schedules the queue, so the next item starts through the normal
prompt path under the conversation's request gate, with the item id as its
`clientRequestId`; a start replayed after a crash is therefore recognized as
already delivered. Only `turn.completed` (or a finished native compaction)
advances the queue; other terminals pause it, and so does recovery: every
queue with waiting items is paused with `daemon_restarted` at startup. A
`turn.failed` whose turn last reported an exhausted plan window (tracked per
turn in the quota store, since concurrent conversations overwrite the
provider snapshot) instead inserts one continuation item at the head and
pauses with `rate_limited` until `resumeAt`; a wall-clock timer, re-armed on
recovery, lifts that pause and drains the queue.

Regression coverage includes malformed approvals, provider wire parameters,
quiet processes and blocked writes, replay/live races, late acknowledgements,
partial replay approvals, legacy gaps, usage snapshots and status rendering.

## Provider capability boundaries

| Provider | Product permission modes | Plan mode |
| --- | --- | --- |
| Codex | ask: workspace sandbox + user review; auto: workspace sandbox + native auto_review; full-access: no sandbox + never approve | Native collaborationMode, independent of permission mode |
| Claude Code | default / auto / bypassPermissions; runtime eligibility may restrict auto | Native plan permission mode; not an OS sandbox |
| Pi | Fixed full-access; RPC has no built-in tool approval or sandbox | Unsupported |
| Grok | Fixed ask through ACP metadata and permission callbacks | Unsupported |
| Generic ACP | Fixed ask; native permission requests are forwarded | Unsupported until a profile advertises an integrated mode |

Prompt requests accept optional `permissionMode` (`ask`, `auto`, `full-access`) and
`workMode` (`implement`, `plan`). The advertised permissionConfig includes `modes`,
`defaultMode`, and `supportsPlan`. Legacy sandbox/profile/approval inputs retain
their restrictions; incompatible mixed formats are rejected, including replacing
saved read-only settings with a broader preset. Old native Codex adapter turns
also forward `approvalsReviewer` separately from approvalPolicy.

Live discovery remains authoritative. The presence of cancel alone does not
imply resume/fork/compact. Only advertised approval options can be submitted;
Codex command/file requests may explicitly offer abort-turn.

Validation on 2026-09-06 also ran one isolated real image request each through
Codex and Pi (`real_v2_provider_http_ws_roundtrip`): HTTP submission, WebSocket
stream and successful final content passed in 20.38 seconds total. This smoke
does not claim real-provider coverage for every approval/fork/compact path;
those paths additionally have captured-wire and controlled-process fixtures.

Additional measurement (same local fixture, ms): at 1k/10k events, cold first-page
latency was 13.82/124.01 versus 17.63/123.14 for full scanning. Cold first-page
latency still includes journal validation and is not materially improved at 10k.
During replay, 20 durable appends measured p50 22.01/19.97 and p95 23.12/22.03.
Allocated offset-vector capacity was 16,384/262,144 bytes; this is index capacity,
not process RSS or peak recovery memory. First-page timing measures the backend
returning 200 records, not browser rendering.
