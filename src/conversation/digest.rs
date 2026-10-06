//! Journal digest: the handful of facts hot paths need from a conversation's
//! whole history, kept so that no request has to load that history.
//!
//! Every fact is derived from envelope-level fields only — the event type
//! and sequence plus small identity and status fields (`turnId`,
//! `clientRequestId`, a control's `requestId`, `permissionId`, runtime id
//! and state, a message's `role`). Answers that need payload *content*
//! (a prompt fingerprint, a control's text or result) are left to a point
//! read of the one event the digest names by sequence.
//!
//! A digest describes a contiguous range of the journal and is mergeable:
//! `digest(A ++ B) == digest(A).merge(digest(B))`. The store keeps one per
//! conversation for the whole journal and folds each appended event in;
//! the same shape can later be persisted per sealed segment.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{status_after_conversation_event, ConversationEvent, ConversationStatus};

/// Events that end the turn they name (or, without a turnId, any open turn).
pub const TURN_TERMINAL_EVENTS: [&str; 4] = [
    "turn.completed",
    "turn.failed",
    "turn.cancelled",
    "turn.interrupted",
];
/// Conversation-level events that end whatever turn was open.
pub const CONVERSATION_TERMINAL_EVENTS: [&str; 2] =
    ["conversation.interrupted", "conversation.failed"];

/// Every status, in the slot order of [`StatusTransform`].
const STATUSES: [ConversationStatus; 5] = [
    ConversationStatus::Idle,
    ConversationStatus::Running,
    ConversationStatus::WaitingPermission,
    ConversationStatus::Interrupted,
    ConversationStatus::Failed,
];

fn status_slot(status: ConversationStatus) -> usize {
    match status {
        ConversationStatus::Idle => 0,
        ConversationStatus::Running => 1,
        ConversationStatus::WaitingPermission => 2,
        ConversationStatus::Interrupted => 3,
        ConversationStatus::Failed => 4,
    }
}

/// How a control request ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlOutcome {
    Completed,
    Rejected,
    Unknown,
}

impl ControlOutcome {
    fn from_event_type(event_type: &str) -> Option<Self> {
        match event_type {
            "control.completed" => Some(Self::Completed),
            "control.rejected" => Some(Self::Rejected),
            "control.unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// `message.created` records carrying one `clientRequestId`.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientRequestFacts {
    /// Sequence of the first such record.
    pub first: u64,
    /// Sequence of the newest such record and its `turnId`.
    pub last: u64,
    pub last_turn_id: Option<String>,
}

/// One control `requestId`: its first `control.requested` and the newest
/// outcome recorded under the id (which may exist without a request).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ControlFacts {
    pub requested: Option<u64>,
    pub outcome: Option<(ControlOutcome, u64)>,
}

/// What restart recovery needs to close a cancelled permission dialog.
/// `scope` and `runtimeId` are short routing values echoed back on the
/// synthetic `permission.resolved`.
#[derive(Clone, Debug, PartialEq)]
pub struct PermissionContext {
    pub scope: Option<Value>,
    pub runtime_id: Option<Value>,
}

/// Inputs restart recovery folds out of a journal; see
/// [`JournalDigest::recovery`].
#[derive(Clone, Debug, PartialEq)]
pub struct RecoveryFacts {
    /// Status the journal folds to from `Idle`.
    pub status: ConversationStatus,
    pub open_turn: Option<String>,
    /// Runtimes whose last `provider.runtime` record says `ready`, by id.
    pub resident_runtimes: Vec<String>,
    /// Permission dialogs requested and never resolved, by id.
    pub pending_permissions: Vec<(String, PermissionContext)>,
}

/// The status fold as a function of the status the range starts from, so
/// two ranges compose without replaying either.
#[derive(Clone, Copy, Debug, PartialEq)]
struct StatusTransform([ConversationStatus; 5]);

impl Default for StatusTransform {
    fn default() -> Self {
        Self(STATUSES)
    }
}

impl StatusTransform {
    fn apply(&mut self, event: &ConversationEvent) {
        for status in &mut self.0 {
            *status = status_after_conversation_event(*status, event);
        }
    }

    fn then(self, later: Self) -> Self {
        Self(self.0.map(|status| later.0[status_slot(status)]))
    }

    fn from(self, start: ConversationStatus) -> ConversationStatus {
        self.0[status_slot(start)]
    }
}

/// The turn a crash left open: the last `turn.started` with no later
/// terminal event for it. A range without a `turn.started` cannot know
/// which turn is open, so it records what it would close instead.
#[derive(Clone, Debug, Default, PartialEq)]
struct OpenTurnFold {
    /// `Some` once the range holds a `turn.started`: the turn still open at
    /// the end of the range (`Some(None)` when it was closed again).
    started: Option<Option<String>>,
    /// Before the range's first `turn.started`: whether a conversation
    /// terminal or an id-less turn terminal closed whatever was open…
    closes_any: bool,
    /// …and the turn ids turn terminals closed.
    closed: HashSet<String>,
}

impl OpenTurnFold {
    fn apply(&mut self, event_type: &str, turn_id: Option<&str>) {
        if event_type == "turn.started" {
            *self = Self {
                started: Some(turn_id.map(str::to_owned)),
                ..Self::default()
            };
            return;
        }
        let conversation_terminal = CONVERSATION_TERMINAL_EVENTS.contains(&event_type);
        if !conversation_terminal && !TURN_TERMINAL_EVENTS.contains(&event_type) {
            return;
        }
        match &mut self.started {
            Some(open) => {
                if conversation_terminal || turn_id.is_none() || turn_id == open.as_deref() {
                    *open = None;
                }
            }
            None => match turn_id.filter(|_| !conversation_terminal) {
                Some(turn_id) => {
                    self.closed.insert(turn_id.to_owned());
                }
                None => self.closes_any = true,
            },
        }
    }

    fn merge(&mut self, later: Self) {
        if later.started.is_some() {
            *self = later;
            return;
        }
        match &mut self.started {
            Some(open) => {
                if later.closes_any
                    || open
                        .as_deref()
                        .is_some_and(|turn_id| later.closed.contains(turn_id))
                {
                    *open = None;
                }
            }
            None => {
                self.closes_any |= later.closes_any;
                self.closed.extend(later.closed);
            }
        }
    }

    /// The open turn of a range that starts the journal.
    fn open_turn(&self) -> Option<&str> {
        self.started.as_ref().and_then(Option::as_deref)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct JournalDigest {
    last_sequence: u64,
    last_time: Option<DateTime<Utc>>,
    /// Newest `message.created` with `role: "user"`.
    last_user_message: Option<u64>,
    client_requests: HashMap<String, ClientRequestFacts>,
    controls: HashMap<String, ControlFacts>,
    /// First `control.requested` delivering prompt text natively — a
    /// `queueAdd` keyed by its `itemId` or a `steer` keyed by its
    /// `requestId` — whose control carries a `text` string.
    text_controls: HashMap<String, u64>,
    /// Bit `i` set: the journal holds `TURN_TERMINAL_EVENTS[i]` for the turn.
    turn_terminals: HashMap<String, u8>,
    status: StatusTransform,
    open_turn: OpenTurnFold,
    /// Last `provider.runtime` state per runtime id: `true` when `ready`.
    runtimes: BTreeMap<String, bool>,
    /// Last state per permission id: `Some` requested, `None` resolved.
    /// Resolved ids stay as tombstones so a later range can be merged in.
    permissions: BTreeMap<String, Option<PermissionContext>>,
}

impl JournalDigest {
    #[cfg(test)]
    pub fn from_events<'a>(events: impl IntoIterator<Item = &'a ConversationEvent>) -> Self {
        let mut digest = Self::default();
        for event in events {
            digest.apply(event);
        }
        digest
    }

    /// Fold the next journal event in.
    pub fn apply(&mut self, event: &ConversationEvent) {
        let payload = &event.payload;
        let text = |key: &str| payload.get(key).and_then(Value::as_str);
        let event_type = event.event_type.as_str();
        let turn_id = text("turnId");
        self.last_sequence = event.sequence;
        self.last_time = Some(event.time);
        self.status.apply(event);
        self.open_turn.apply(event_type, turn_id);
        match event_type {
            "message.created" => {
                if text("role") == Some("user") {
                    self.last_user_message = Some(event.sequence);
                }
                if let Some(request_id) = text("clientRequestId") {
                    let facts = self.client_requests.entry(request_id.to_owned()).or_insert(
                        ClientRequestFacts {
                            first: event.sequence,
                            last: event.sequence,
                            last_turn_id: None,
                        },
                    );
                    facts.last = event.sequence;
                    facts.last_turn_id = turn_id.map(str::to_owned);
                }
            }
            "control.requested" => {
                if let Some(request_id) = text("requestId") {
                    self.controls
                        .entry(request_id.to_owned())
                        .or_default()
                        .requested
                        .get_or_insert(event.sequence);
                }
                let control = payload.get("control");
                let control_text = |key: &str| {
                    control
                        .and_then(|control| control.get(key))
                        .and_then(Value::as_str)
                };
                let delivered = match control_text("action") {
                    Some("queueAdd") => control_text("itemId"),
                    Some("steer") => text("requestId"),
                    _ => None,
                };
                if let Some(id) = delivered.filter(|_| control_text("text").is_some()) {
                    self.text_controls
                        .entry(id.to_owned())
                        .or_insert(event.sequence);
                }
            }
            "provider.runtime" => {
                if let Some(runtime_id) = text("runtimeId") {
                    match text("status") {
                        Some("ready") => {
                            self.runtimes.insert(runtime_id.to_owned(), true);
                        }
                        Some("stopped") => {
                            self.runtimes.insert(runtime_id.to_owned(), false);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        if let Some(outcome) = ControlOutcome::from_event_type(event_type) {
            if let Some(request_id) = text("requestId") {
                self.controls
                    .entry(request_id.to_owned())
                    .or_default()
                    .outcome = Some((outcome, event.sequence));
            }
        }
        if let Some(bit) = TURN_TERMINAL_EVENTS
            .iter()
            .position(|terminal| *terminal == event_type)
        {
            if let Some(turn_id) = turn_id {
                *self.turn_terminals.entry(turn_id.to_owned()).or_default() |= 1 << bit;
            }
        }
        if let Some(permission_id) = text("permissionId") {
            match event_type {
                "permission.requested" | "tool.awaitingApproval" => {
                    self.permissions.insert(
                        permission_id.to_owned(),
                        Some(PermissionContext {
                            scope: payload.get("scope").cloned(),
                            runtime_id: payload.get("runtimeId").cloned(),
                        }),
                    );
                }
                "permission.resolved" => {
                    self.permissions.insert(permission_id.to_owned(), None);
                }
                _ => {}
            }
        }
    }

    /// Append the digest of the range directly after this one.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn merge(&mut self, later: Self) {
        if later.last_sequence == 0 {
            return;
        }
        self.last_sequence = later.last_sequence;
        self.last_time = later.last_time;
        self.last_user_message = later.last_user_message.or(self.last_user_message);
        for (request_id, facts) in later.client_requests {
            self.client_requests
                .entry(request_id)
                .and_modify(|earlier| {
                    earlier.last = facts.last;
                    earlier.last_turn_id = facts.last_turn_id.clone();
                })
                .or_insert(facts);
        }
        for (request_id, facts) in later.controls {
            let earlier = self.controls.entry(request_id).or_default();
            earlier.requested = earlier.requested.or(facts.requested);
            earlier.outcome = facts.outcome.or(earlier.outcome);
        }
        for (id, sequence) in later.text_controls {
            self.text_controls.entry(id).or_insert(sequence);
        }
        for (turn_id, mask) in later.turn_terminals {
            *self.turn_terminals.entry(turn_id).or_default() |= mask;
        }
        self.status = self.status.then(later.status);
        self.open_turn.merge(later.open_turn);
        self.runtimes.extend(later.runtimes);
        self.permissions.extend(later.permissions);
    }

    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    /// Time of the newest event; `None` for an empty journal.
    pub fn last_time(&self) -> Option<DateTime<Utc>> {
        self.last_time
    }

    /// Sequence of the newest user `message.created`.
    pub fn last_user_message(&self) -> Option<u64> {
        self.last_user_message
    }

    pub fn client_request(&self, client_request_id: &str) -> Option<&ClientRequestFacts> {
        self.client_requests.get(client_request_id)
    }

    pub fn control(&self, request_id: &str) -> Option<&ControlFacts> {
        self.controls.get(request_id)
    }

    /// Sequence of the first native queue/steer control that delivered
    /// prompt text under `id`.
    pub fn text_control(&self, id: &str) -> Option<u64> {
        self.text_controls.get(id).copied()
    }

    /// Whether the journal holds `event_type` for `turn_id`; `None` when
    /// `event_type` is not one of [`TURN_TERMINAL_EVENTS`], which are the
    /// only events tracked.
    pub fn has_turn_terminal(&self, turn_id: &str, event_type: &str) -> Option<bool> {
        let bit = TURN_TERMINAL_EVENTS
            .iter()
            .position(|terminal| *terminal == event_type)?;
        Some(
            self.turn_terminals
                .get(turn_id)
                .is_some_and(|mask| mask & (1 << bit) != 0),
        )
    }

    /// Status the whole journal folds to from `start`.
    pub fn status_from(&self, start: ConversationStatus) -> ConversationStatus {
        self.status.from(start)
    }

    pub fn open_turn(&self) -> Option<&str> {
        self.open_turn.open_turn()
    }

    pub fn recovery(&self) -> RecoveryFacts {
        RecoveryFacts {
            status: self.status_from(ConversationStatus::Idle),
            open_turn: self.open_turn().map(str::to_owned),
            resident_runtimes: self
                .runtimes
                .iter()
                .filter(|(_, ready)| **ready)
                .map(|(id, _)| id.clone())
                .collect(),
            pending_permissions: self
                .permissions
                .iter()
                .filter_map(|(id, context)| Some((id.clone(), context.clone()?)))
                .collect(),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;

    use super::*;

    // Reference answers: the full-history scans the digest replaced, kept
    // verbatim so equivalence is checked against the original semantics.

    fn reference_open_turn(history: &[ConversationEvent]) -> Option<String> {
        let mut open = None;
        for event in history {
            let turn_id = event.payload.get("turnId").and_then(Value::as_str);
            let event_type = event.event_type.as_str();
            if event_type == "turn.started" {
                open = turn_id.map(str::to_owned);
            } else if CONVERSATION_TERMINAL_EVENTS.contains(&event_type)
                || (TURN_TERMINAL_EVENTS.contains(&event_type)
                    && (turn_id.is_none() || turn_id == open.as_deref()))
            {
                open = None;
            }
        }
        open
    }

    /// `(resident runtimes, expired permissions)` as `recover_conversation`
    /// collected them.
    fn reference_recovery(
        history: &[ConversationEvent],
    ) -> (BTreeMap<String, Value>, std::collections::BTreeSet<String>) {
        let mut expired = BTreeMap::new();
        let mut resident_runtimes = std::collections::BTreeSet::new();
        for event in history {
            if event.event_type == "provider.runtime" {
                if let Some(id) = event.payload.get("runtimeId").and_then(Value::as_str) {
                    match event.payload.get("status").and_then(Value::as_str) {
                        Some("ready") => {
                            resident_runtimes.insert(id.to_owned());
                        }
                        Some("stopped") => {
                            resident_runtimes.remove(id);
                        }
                        _ => {}
                    }
                }
            }
            if let Some(id) = event.payload.get("permissionId").and_then(Value::as_str) {
                match event.event_type.as_str() {
                    "permission.requested" | "tool.awaitingApproval" => {
                        expired.insert(id.to_owned(), json!({"scope":event.payload.get("scope"),"runtimeId":event.payload.get("runtimeId")}));
                    }
                    "permission.resolved" => {
                        expired.remove(id);
                    }
                    _ => {}
                }
            }
        }
        (expired, resident_runtimes)
    }

    fn reference_prompt_previous(history: &[ConversationEvent], id: &str) -> Option<u64> {
        history
            .iter()
            .find(|event| {
                event.event_type == "message.created"
                    && event.payload.get("clientRequestId").and_then(Value::as_str) == Some(id)
            })
            .map(|event| event.sequence)
    }

    fn reference_control_text(history: &[ConversationEvent], request_id: &str) -> Option<String> {
        history.iter().find_map(|event| {
            if event.event_type != "control.requested" {
                return None;
            }
            let control = event.payload.get("control")?;
            let matches_id = match control.get("action").and_then(Value::as_str) {
                Some("queueAdd") => {
                    control.get("itemId").and_then(Value::as_str) == Some(request_id)
                }
                Some("steer") => {
                    event.payload.get("requestId").and_then(Value::as_str) == Some(request_id)
                }
                _ => false,
            };
            matches_id
                .then(|| control.get("text").and_then(Value::as_str))
                .flatten()
                .map(str::to_owned)
        })
    }

    fn reference_control(
        history: &[ConversationEvent],
        request_id: &str,
    ) -> (Option<u64>, Option<(String, u64)>) {
        let prior = history.iter().find(|event| {
            event.event_type == "control.requested"
                && event.payload.get("requestId").and_then(Value::as_str) == Some(request_id)
        });
        let done = history.iter().rev().find(|event| {
            matches!(
                event.event_type.as_str(),
                "control.completed" | "control.rejected" | "control.unknown"
            ) && event.payload.get("requestId").and_then(Value::as_str) == Some(request_id)
        });
        (
            prior.map(|event| event.sequence),
            done.map(|event| (event.event_type.clone(), event.sequence)),
        )
    }

    fn reference_delivered_turn(history: &[ConversationEvent], id: &str) -> Option<String> {
        history
            .iter()
            .rev()
            .find(|event| {
                event.event_type == "message.created"
                    && event.payload.get("clientRequestId").and_then(Value::as_str) == Some(id)
            })
            .and_then(|event| event.payload.get("turnId").and_then(Value::as_str))
            .map(str::to_owned)
    }

    fn reference_has_turn_event(history: &[ConversationEvent], turn: &str, kind: &str) -> bool {
        history.iter().rev().any(|event| {
            event.event_type == kind
                && event.payload.get("turnId").and_then(Value::as_str) == Some(turn)
        })
    }

    fn reference_last_user_message(history: &[ConversationEvent]) -> Option<u64> {
        history
            .iter()
            .rev()
            .find(|event| {
                event.event_type == "message.created"
                    && event.payload.get("role").and_then(Value::as_str) == Some("user")
            })
            .map(|event| event.sequence)
    }

    fn outcome_type(outcome: ControlOutcome) -> &'static str {
        match outcome {
            ControlOutcome::Completed => "control.completed",
            ControlOutcome::Rejected => "control.rejected",
            ControlOutcome::Unknown => "control.unknown",
        }
    }

    const IDS: [&str; 4] = ["a", "b", "c", "d"];

    /// Asserts every digest answer equals the reference scan of `history`.
    pub(crate) fn assert_matches_reference(digest: &JournalDigest, history: &[ConversationEvent]) {
        let at = |sequence: u64| &history[sequence as usize - 1];
        assert_eq!(
            digest.last_sequence(),
            history.last().map_or(0, |event| event.sequence)
        );
        assert_eq!(digest.last_time(), history.last().map(|event| event.time));
        assert_eq!(
            digest.last_user_message(),
            reference_last_user_message(history)
        );
        for start in STATUSES {
            assert_eq!(
                digest.status_from(start),
                history.iter().fold(start, status_after_conversation_event)
            );
        }
        assert_eq!(digest.open_turn(), reference_open_turn(history).as_deref());
        let recovery = digest.recovery();
        let (expired, resident) = reference_recovery(history);
        assert_eq!(
            recovery.resident_runtimes,
            resident.into_iter().collect::<Vec<_>>()
        );
        let pending: BTreeMap<String, Value> = recovery
            .pending_permissions
            .into_iter()
            .map(|(id, context)| {
                (
                    id,
                    json!({"scope": context.scope, "runtimeId": context.runtime_id}),
                )
            })
            .collect();
        assert_eq!(pending, expired);
        for id in IDS {
            let request = digest.client_request(id);
            assert_eq!(
                request.map(|facts| facts.first),
                reference_prompt_previous(history, id)
            );
            assert_eq!(
                request.and_then(|facts| facts.last_turn_id.clone()),
                reference_delivered_turn(history, id)
            );
            if let Some(facts) = request {
                assert_eq!(at(facts.last).payload["clientRequestId"], id);
            }
            assert_eq!(
                digest
                    .text_control(id)
                    .map(|sequence| at(sequence).payload["control"]["text"]
                        .as_str()
                        .unwrap()
                        .to_owned()),
                reference_control_text(history, id)
            );
            let (prior, done) = reference_control(history, id);
            let control = digest.control(id).cloned().unwrap_or_default();
            assert_eq!(control.requested, prior);
            assert_eq!(
                control
                    .outcome
                    .map(|(outcome, sequence)| (outcome_type(outcome).to_owned(), sequence)),
                done
            );
            for kind in TURN_TERMINAL_EVENTS {
                assert_eq!(
                    digest.has_turn_terminal(id, kind),
                    Some(reference_has_turn_event(history, id, kind))
                );
            }
        }
        assert_eq!(digest.has_turn_terminal("a", "turn.started"), None);
    }

    /// Deterministic xorshift so failures reproduce from the seed.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn pick<'a>(&mut self, values: &[&'a str]) -> &'a str {
            values[(self.next() % values.len() as u64) as usize]
        }

        fn maybe<'a>(&mut self, values: &[&'a str]) -> Option<&'a str> {
            (!self.next().is_multiple_of(4)).then(|| self.pick(values))
        }
    }

    /// A journal mixing every event shape the digest reads, with ids drawn
    /// from a tiny pool so duplicates, retries and repeats are common.
    pub(crate) fn varied_payloads(seed: u64, count: usize) -> Vec<(String, Value)> {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut events = Vec::with_capacity(count);
        for _ in 0..count {
            let turn = rng.maybe(&IDS);
            let id = rng.maybe(&IDS);
            let (event_type, payload) = match rng.next() % 22 {
                0 | 1 => (
                    "message.created",
                    json!({ "role": rng.pick(&["user", "assistant"]), "turnId": turn,
                        "clientRequestId": id, "requestFingerprint": rng.maybe(&["f1", "f2"]),
                        "content": "hello" }),
                ),
                2 => ("turn.started", json!({ "turnId": turn })),
                3 => (rng.pick(&TURN_TERMINAL_EVENTS), json!({ "turnId": turn })),
                4 => (rng.pick(&CONVERSATION_TERMINAL_EVENTS), json!({})),
                5 => (
                    "control.requested",
                    json!({ "turnId": turn, "requestId": id, "status": "pending",
                        "control": { "action": rng.pick(&["setModel", "queueAdd", "steer"]),
                            "itemId": rng.maybe(&IDS), "text": rng.maybe(&["x", "y"]) } }),
                ),
                6 => (
                    rng.pick(&["control.completed", "control.rejected", "control.unknown"]),
                    json!({ "turnId": turn, "requestId": id, "result": {}, "message": "m" }),
                ),
                7 | 8 => (
                    rng.pick(&[
                        "permission.requested",
                        "tool.awaitingApproval",
                        "permission.resolved",
                    ]),
                    json!({ "permissionId": id, "scope": rng.maybe(&["session", "turn"]),
                        "runtimeId": rng.maybe(&["r1", "r2"]) }),
                ),
                9 => (
                    "provider.runtime",
                    json!({ "runtimeId": rng.maybe(&["r1", "r2", "r3"]),
                        "status": rng.pick(&["ready", "stopped", "starting"]) }),
                ),
                10 => (
                    rng.pick(&[
                        "compaction.started",
                        "compaction.completed",
                        "compaction.failed",
                    ]),
                    json!({ "operationId": rng.maybe(&["op"]) }),
                ),
                11 => (
                    rng.pick(&[
                        "workflow.started",
                        "workflow.paused",
                        "workflow.completed",
                        "conversation.forked",
                    ]),
                    json!({}),
                ),
                // A permissionId on an unrelated event must not count.
                12 => (
                    "tool.started",
                    json!({ "permissionId": id, "turnId": turn }),
                ),
                _ => (
                    rng.pick(&["message.delta", "tool.updated", "thought.delta"]),
                    json!({ "turnId": turn, "delta": "…" }),
                ),
            };
            events.push((event_type.to_owned(), payload));
        }
        events
    }

    fn varied_journal(seed: u64, count: usize) -> Vec<ConversationEvent> {
        varied_payloads(seed, count)
            .into_iter()
            .enumerate()
            .map(|(index, (event_type, payload))| {
                ConversationEvent::new("conversation", index as u64 + 1, event_type, payload)
            })
            .collect()
    }

    #[test]
    fn digest_answers_match_the_full_history_scans() {
        for seed in 1..=200 {
            let history = varied_journal(seed, (seed as usize * 7) % 160);
            let mut digest = JournalDigest::default();
            for (index, event) in history.iter().enumerate() {
                digest.apply(event);
                // Incremental maintenance: the digest is right after every append.
                if index % 17 == 0 {
                    assert_matches_reference(&digest, &history[..=index]);
                }
            }
            assert_matches_reference(&digest, &history);
        }
    }

    #[test]
    fn merged_range_digests_equal_the_whole_journal_digest() {
        for seed in 1..=60 {
            let history = varied_journal(seed, 90);
            let whole = JournalDigest::from_events(&history);
            for split in [0, 1, 13, 45, 89, 90] {
                let mut merged = JournalDigest::from_events(&history[..split]);
                merged.merge(JournalDigest::from_events(&history[split..]));
                assert_eq!(
                    merged.recovery(),
                    whole.recovery(),
                    "seed {seed} split {split}"
                );
                assert_matches_reference(&merged, &history);
            }
            let mut three = JournalDigest::from_events(&history[..20]);
            three.merge(JournalDigest::from_events(&history[20..21]));
            three.merge(JournalDigest::from_events(&history[21..]));
            assert_matches_reference(&three, &history);
        }
    }

    #[test]
    fn open_turn_is_the_last_started_turn_without_its_own_terminal_event() {
        let event = |event_type: &str, payload: Value| {
            ConversationEvent::new("conversation", 1, event_type, payload)
        };
        let open = |events: &[ConversationEvent]| {
            JournalDigest::from_events(events)
                .open_turn()
                .map(str::to_owned)
        };
        assert_eq!(open(&[]), None);
        assert_eq!(
            open(&[
                event("turn.started", json!({ "turnId": "a" })),
                event("turn.completed", json!({ "turnId": "a" })),
                event("turn.started", json!({ "turnId": "b" })),
                event("turn.failed", json!({ "turnId": "a" })),
            ]),
            Some("b".to_owned())
        );
        for terminal in [
            event("turn.cancelled", json!({ "turnId": "b" })),
            event("turn.interrupted", json!({})),
            event(
                "conversation.interrupted",
                json!({ "reason": "daemon_restarted" }),
            ),
            event("conversation.failed", json!({})),
        ] {
            assert_eq!(
                open(&[event("turn.started", json!({ "turnId": "b" })), terminal]),
                None
            );
        }
    }
}
