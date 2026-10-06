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

The backend's JSONL journal remains authoritative. It is one logical sequence
of files: sealed `events.NNNNNN.jsonl` segments in numeric order followed by
the active `events.jsonl`, the only file appends touch. Once the active file
passes 8 MiB the next append seals it under the next segment number and starts
a fresh `events.jsonl` — a crash between the rename and the create leaves the
journal ending in a sealed segment, which the next append recreates the active
file for. Sequence `N` is still global journal line `N` across all files, so
replay cursors, the cold index and `journal.*` placeholder semantics are
unchanged; a legacy single `events.jsonl` is simply a journal of one active
segment. Salvage rewrites collapse the journal back into one `events.jsonl`
and unlink the sealed files; compaction instead rewrites each segment in
place and merges adjacent sealed segments that fit one target size, keeping
the file count bounded. Its in-memory sequence/offset index is rebuildable
and records `(segment, start, end)` per record. A local
debug fixture with 200 events per page measured
1,000 events at 66.09 ms for repeated full parsing versus 22.12 ms cold / 8.49 ms
warm indexing; 10,000 events measured 6084.97 ms versus 199.97 / 78.05 ms. These
are local measurements, not production latency guarantees.
A cold index (for example after fork or migration) is built by a newline scan
that parses only the first and last records of each segment; a journal without
its final newline, or whose last sequence differs from its line count, gets
the full validating scan with tail repair. Every page still validates its records, and a
page that reaches a damaged record runs the full scan, which salvages or reports
the damage. A local release fixture measured the cold 50-event tail page at
7.8 → 0.5 ms for 10,000 events and 77 → 5 ms for 100,000 events (53 MB).

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
recovery, and an append to an unterminated journal starts a new line. Corrupt
lines with no valid record after them are still quarantined to
`events.corrupt.<ts>.jsonl` and cut off. Interior corruption is salvaged: the
whole journal is backed up to `events.corrupt.<ts>.jsonl`, then atomically
rewritten with valid records unchanged and each lost sequence replaced by a
`journal.recordLost` event with payload `{"reason": "corrupt", "runStart",
"runLength", "backup": "<file>"}`. Placeholders of one run share
`runStart`/`runLength` and take the previous valid event's time; their count
is bounded by the corrupt bytes, and clients cannot append that event type.
A corrupt tail may span several trailing segments: wholly corrupt sealed files
are deleted, the file containing the last valid record is truncated, and an
empty `events.jsonl` is kept so the journal still ends in its active file.

The journal has no total capacity limit: every append lands, so a running
turn can always finish, and forks can carry histories of any size. The only
gate is on new prompts. Once an append or a prompt would push the journal
past 56 MiB, a compaction pass runs segment by segment: older streaming
progress records that a terminal record already covers (`message.delta`,
`thought.delta`, `tool.updated`, `subagent.updated` — everything but the
newest 16 MiB) are replaced by `journal.compacted` markers keeping the
original sequence, event id and time with payload
`{"reason": "compacted", "originalType", "runStart", "runLength"}` where one
run shares `runStart`/`runLength`. On disk a marker is a compact line
(`{"sequence", "compacted": {"eventId", "timeUs", "originalType", "runStart",
"runLength"}}`, ~150 bytes instead of ~350 for a full event, since a full
envelope is as large as the small delta it replaces); every read expands it
back to the full event, and a pass re-encodes full-event markers written by
older builds. Each sequence still occupies its line, so the cold index and
replay cursors stay valid; clients classify the marker as an unknown type
and render nothing. Any remaining oversized payload strings
are then truncated largest-first, oldest files first, until the journal fits
48 MiB or nothing more can shrink. Only files that actually change are
rewritten, in place and atomically; adjacent sealed segments that fit one
8 MiB target are concatenated so the file count stays bounded, and a
sealed segment processed in full is remembered by its `(name, length,
mtime)` fingerprint so later passes skip it. A journal still above 56 MiB
after compaction refuses new prompts with `JOURNAL_FULL` (HTTP 507) — start
a new conversation — while appends keep landing regardless. Replay pages
(`afterSequence`, `beforeSequence`, WebSocket subscribe backfill) stop at
`limit` events or about 8 MiB of journal, whichever comes first, but always
hold at least one event; clients keep paging while `hasMore`.

On unix every spawned provider is recorded in
`<data_dir>/provider_processes.json` (0600, atomic rewrite) with pid, pgid and
start time, next to the owning server's pid and start time. At startup the
server kills each recorded process group whose leader still has the recorded
start time, unless the owning server is still alive: then the new server
neither reaps nor tracks. Linux also sets `PR_SET_PDEATHSIG` (SIGKILL). Both
are no-ops on Windows.

Before reporting daemon readiness, startup recovers each conversation journal
once and reuses that history to cancel stale approvals and mark resident
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
queue with waiting items is paused with `daemon_restarted` at startup.

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
