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
  `collabAgentToolCall` items; Grok Build maps `subagent_*` session updates.
  Providers without native subagent signals emit no `subagent.*` events.
- Memory configuration is separate from memory content; the panel explicitly
  reports when the provider has no readable content source.
- Streaming is coalesced before it reaches the journal. Pi thinking/text
  fragments of one block merge for up to 100 ms (2 KiB) into one delta; ACP
  `tool.updated` carries a full snapshot and in-progress snapshots of a call are
  sent at most every 500 ms, while terminal status is always sent. Clients
  append deltas per block and replace tool rows per event, so the rendered
  result is unchanged.

Backend control/write/cancel/compact defaults are 30/10/10/300 seconds, configured
with `TODEX_AGENTD_PROVIDER_{CONTROL,WRITE,CANCEL,COMPACT}_TIMEOUT_SECONDS`.
The first three accept 1–3600 seconds; compact accepts 1–86400 seconds.

The backend's JSONL journal remains authoritative. Its in-memory sequence/offset
index is rebuildable. A local debug fixture with 200 events per page measured
1,000 events at 66.09 ms for repeated full parsing versus 22.12 ms cold / 8.49 ms
warm indexing; 10,000 events measured 6084.97 ms versus 199.97 / 78.05 ms. These
are local measurements, not production latency guarantees.

Before reporting daemon readiness, startup validates each conversation journal
once and reuses that recovered history to cancel stale approvals and mark
resident runtimes stopped. Recovery progress is logged every 25 conversations.
Daemon startup waits up to 120 seconds for initialization; on timeout the
spawned child is terminated and the daemon log identifies the last recovery
progress. No in-progress turn is replayed automatically.

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
