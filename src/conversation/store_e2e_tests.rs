//! End-to-end encrypted history through the store
//! (`docs/history-encryption.md`): what reaches the disk, what replays and
//! live events carry, sealing, restarts and migration. Kept beside
//! `store.rs` so they reach its private items. Decryption uses the test
//! recipient seeds, standing in for a client.

use std::collections::{BTreeSet, HashMap};
use std::fs;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Map};

use super::v3_tests::mixed_payload;
use super::*;
use crate::conversation::e2e_support::{all_bytes, client_keys, client_view, contains};
use crate::conversation::{present_event, summarize_event, ProviderKind};
use crate::history_crypto::open;
use crate::history_keys::test_support::recipient;
use crate::history_keys::HistoryKeys;

pub(super) fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
    fs::create_dir_all(&path).unwrap();
    path
}

/// A data directory with one recipient device (seed 1) and its store.
pub(super) struct E2e {
    pub root: PathBuf,
    pub keys: HistoryKeys,
    pub store: ConversationStore,
}

impl E2e {
    pub async fn new(prefix: &str, encrypted: bool) -> Self {
        let root = temp_dir(prefix);
        let keys = HistoryKeys::load(&root, HistoryEncryption::Off, None).unwrap();
        keys.recipients()
            .register_device("dev_a", &recipient(1))
            .unwrap();
        if encrypted {
            keys.recipients().set_mode(HistoryEncryption::E2e).unwrap();
        }
        let store = ConversationStore::new(root.clone())
            .await
            .unwrap()
            .with_history_keys(keys.clone());
        Self { root, keys, store }
    }

    /// A restarted daemon: same files, no DEK in memory.
    pub async fn restart(&self) -> Self {
        let keys = HistoryKeys::load(&self.root, HistoryEncryption::Off, None).unwrap();
        let store = ConversationStore::new(self.root.clone())
            .await
            .unwrap()
            .with_history_keys(keys.clone());
        Self {
            root: self.root.clone(),
            keys,
            store,
        }
    }

    pub async fn create(&self, title: Option<&str>) -> ConversationManifest {
        self.store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                self.root.clone(),
                title.map(str::to_owned),
                None,
            ))
            .await
            .unwrap()
    }

    /// Every DEK of `keyring_owner`'s keyring, unwrapped with seed 1.
    pub async fn client_keys(&self, keyring_owner: &str) -> HashMap<String, SegmentKey> {
        client_keys(&self.keys, keyring_owner, 1).await
    }

    pub fn cleanup(self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Whole history in `detail`, page by page, with every page's frames.
async fn history(
    store: &ConversationStore,
    id: &str,
    detail: ReplayDetail,
) -> (Vec<ConversationEvent>, Map<String, Value>) {
    let mut events = Vec::new();
    let mut frames = Map::new();
    let mut after = 0;
    loop {
        let page = store.replay_detail(id, after, 97, detail).await.unwrap();
        after = page.next_sequence;
        frames.extend(page.frames);
        events.extend(page.events);
        if !page.has_more {
            return (events, frames);
        }
    }
}

fn summarized(event: &ConversationEvent) -> ConversationEvent {
    let mut event = event.clone();
    summarize_event(&mut event);
    event
}

fn assert_payloads(actual: &[ConversationEvent], expected: &[ConversationEvent]) {
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(actual.sequence, expected.sequence);
        if actual.event_type == JOURNAL_COMPACTED_EVENT {
            assert_eq!(actual.payload["originalType"], expected.event_type.as_str());
            continue;
        }
        assert_eq!(actual.event_type, expected.event_type);
        assert_eq!(
            actual.payload, expected.payload,
            "sequence {}",
            actual.sequence
        );
    }
}

const SECRETS: [&str; 5] = [
    "PROMPT-SECRET-1",
    "REPLY-SECRET-2",
    "TOOL-OUTPUT-SECRET-3",
    "STEER-SECRET-4",
    "TITLE-SECRET-5",
];

#[tokio::test]
async fn e2e_appends_keep_plaintext_off_disk_and_replay_the_written_ciphertext() {
    let e2e = E2e::new("todex-e2e-append", true).await;
    let store = &e2e.store;
    let manifest = e2e.create(Some(SECRETS[4])).await;
    let id = manifest.id.clone();
    // The title is sealed for the recipients; no plaintext title remains.
    assert!(manifest.title.is_none());
    let title = manifest.title_enc.clone().unwrap();
    assert_eq!(store.get(&id).await.unwrap().title_enc, Some(title.clone()));
    let keys = e2e.client_keys(&id).await;
    let sealed = URL_SAFE_NO_PAD.decode(&title.ct).unwrap();
    assert_eq!(
        open(&keys[&title.kid], &id, ContentStream::EventFull, 0, &sealed).unwrap(),
        SECRETS[4].as_bytes()
    );
    assert!(store.get(&id).await.unwrap().history_encrypted_at.is_some());

    let hub = ConversationEventHub::default();
    let mut live = hub.subscribe(&id);
    let originals = vec![
        (
            "message.created",
            json!({"turnId": "t1", "clientRequestId": "r1", "requestFingerprint": "abc123",
                   "role": "user", "content": SECRETS[0]}),
        ),
        ("turn.started", json!({"turnId": "t1", "status": "running"})),
        (
            "tool.updated",
            json!({"turnId": "t1", "toolCallId": "call_1",
                   "block": {"category": "tool", "id": "call_1", "turnId": "t1", "phase": "delta"},
                   "output": format!("{} ", SECRETS[2]).repeat(20)}),
        ),
        (
            "control.requested",
            json!({"turnId": "t1", "requestId": "ctl_1",
                   "control": {"action": "steer", "text": SECRETS[3]}, "status": "pending"}),
        ),
    ];
    for (event_type, payload) in &originals {
        store
            .append_and_publish(&id, *event_type, payload.clone(), &hub)
            .await
            .unwrap();
    }
    // Coalesced stream text is sealed when its window commits.
    for fragment in ["REPLY-", "SECRET-2", " done"] {
        let payload = json!({"turnId": "t1", "role": "assistant", "delta": fragment});
        let delta =
            crate::conversation::DeltaFragment::from_payload(&payload, &["/delta"], None).unwrap();
        store
            .append_delta_and_publish(&id, "message.delta", payload, delta, &hub)
            .await
            .unwrap();
    }
    store
        .append_and_publish(
            &id,
            "turn.completed",
            json!({"turnId": "t1", "status": "completed"}),
            &hub,
        )
        .await
        .unwrap();

    // (a) Nothing readable reached the disk.
    let directory = store.directory(&id).unwrap();
    let disk = all_bytes(&directory);
    for secret in SECRETS {
        assert!(!contains(&disk, secret), "{secret} is on disk");
    }
    assert!(
        !contains(&disk, "abc123"),
        "the plain fingerprint is on disk"
    );
    let lines: Vec<Value> = fs::read_to_string(directory.join(EVENTS_FILE))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 6);
    assert!(lines.iter().all(|line| line.get("c").is_none()));

    // (c) Live events carry exactly the stored ciphertext.
    for line in &lines {
        let event = live.recv().await.unwrap();
        assert_eq!(event.sequence, line["s"].as_u64().unwrap());
        assert_eq!(event.payload["$enc"], line["x"]);
        assert_eq!(
            serde_json::to_vec(&event.payload["$enc"]).unwrap(),
            serde_json::to_vec(&line["x"]).unwrap()
        );
    }

    // Envelope fields stay plaintext, deduplication fields as MACs.
    let fingerprint = e2e.keys.fingerprint().unwrap();
    assert_eq!(
        lines[0]["e"]["requestFingerprint"],
        fingerprint.mac("abc123")
    );
    assert_eq!(lines[0]["e"]["role"], "user");
    assert_eq!(
        lines[3]["e"]["control"],
        json!({"action": "steer", "textMac": fingerprint.mac(SECRETS[3])})
    );
    assert_eq!(
        lines[3]["e"]["requestFingerprint"],
        fingerprint.mac(&json!({"action": "steer", "text": SECRETS[3]}).to_string())
    );
    // The summarizable tool record has a separate summary; others do not.
    assert!(lines[2]["x"].get("s").is_some());
    assert!(lines[0]["x"].get("s").is_none());

    // (b) Replays decrypt to the full payloads and their summaries.
    let mut expected = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let (event_type, mut payload) = match originals.get(index) {
            Some((event_type, payload)) if index < 4 => (event_type.to_string(), payload.clone()),
            _ if index == 4 => (
                "message.delta".to_owned(),
                json!({"turnId": "t1", "role": "assistant", "delta": "REPLY-SECRET-2 done"}),
            ),
            _ => (
                "turn.completed".to_owned(),
                json!({"turnId": "t1", "status": "completed"}),
            ),
        };
        add_history_macs(&event_type, &mut payload, &fingerprint);
        let mut event =
            ConversationEvent::new(&id, line["s"].as_u64().unwrap(), event_type, payload);
        event.provider = Some(ProviderKind::Codex);
        expected.push(event);
    }
    let keys = e2e.client_keys(&id).await;
    let (full, frames) = history(store, &id, ReplayDetail::Full).await;
    assert!(frames.is_empty());
    assert_payloads(&client_view(&keys, &full, &frames, false), &expected);
    let (summary, frames) = history(store, &id, ReplayDetail::Summary).await;
    let summary_view = client_view(&keys, &summary, &frames, true);
    let expected_summary: Vec<_> = expected.iter().map(summarized).collect();
    assert_payloads(&summary_view, &expected_summary);
    assert_ne!(summary_view[2].payload, expected[2].payload);
    // Summary replays never run summarize_event on ciphertext: the wire
    // payload is the envelope plus `$enc` with only the summary ciphertext.
    let mut wire = summary[2].clone();
    present_event(&mut wire, true);
    assert!(wire.payload["$enc"].get("f").is_none());
    assert!(wire.payload["$enc"].get("s").is_some());

    // The digest folds the same envelope facts warm and cold.
    let warm = store.digest(&id, Clone::clone).await.unwrap();
    let cold = e2e.restart().await;
    assert_eq!(cold.store.digest(&id, Clone::clone).await.unwrap(), warm);
    assert!(warm.text_control("ctl_1").is_some());
    e2e.cleanup();
}

#[tokio::test]
async fn sealing_repacks_e2e_records_into_frames_and_releases_their_keys() {
    let e2e = E2e::new("todex-e2e-seal", true).await;
    let store = &e2e.store;
    let id = e2e.create(None).await.id;
    let mut originals = Vec::new();
    for index in 0..1_200 {
        let (event_type, payload) = mixed_payload(index);
        let event = store
            .append(&id, event_type, payload.clone())
            .await
            .unwrap();
        let mut original = ConversationEvent::new(&id, event.sequence, event_type, payload);
        original.provider = Some(ProviderKind::Codex);
        originals.push(original);
    }
    let directory = store.directory(&id).unwrap();
    // The kids of the sealed (rotated) files are still in memory.
    let mut sealed_kids = BTreeSet::new();
    for file in journal_files(&directory).await.unwrap() {
        if file.name == EVENTS_FILE {
            continue;
        }
        for line in fs::read_to_string(directory.join(&file.name))
            .unwrap()
            .lines()
        {
            let record: Value = serde_json::from_str(line).unwrap();
            sealed_kids.insert(record["x"]["kid"].as_str().unwrap().to_owned());
        }
    }
    assert!(!sealed_kids.is_empty());
    for kid in &sealed_kids {
        assert!(e2e.keys.deks().key_for(&id, kid).await.is_some());
    }
    let mut sealed = 0;
    while store.seal_next(&id).await.unwrap() {
        sealed += 1;
    }
    assert!(sealed >= 2, "fixture must span several segments");
    // (d) DEKs of sealed segments are released.
    for kid in &sealed_kids {
        assert!(e2e.keys.deks().key_for(&id, kid).await.is_none());
    }
    // Frame counters never repeat under one key.
    let mut counters = BTreeSet::new();
    let mut sealed_frames = 0;
    for file in journal_files(&directory).await.unwrap() {
        let Some(number) = segment::numbered(&file.name, ".seg") else {
            continue;
        };
        let loaded = segment::load_index(&directory, number).unwrap();
        assert!(!loaded.body.has_plain_content());
        for frame in &loaded.body.frames {
            if let Some(kid) = &frame.kid {
                sealed_frames += 1;
                assert!(counters.insert((
                    kid.clone(),
                    frame.stream,
                    segment::frame_counter(number, frame.ordinal)
                )));
            }
        }
    }
    assert!(sealed_frames >= 4);
    let disk = all_bytes(&directory);
    assert!(!contains(&disk, "Please look at module"));
    assert!(!contains(&disk, "line of build output"));

    let keys = e2e.client_keys(&id).await;
    let (full, frames) = history(store, &id, ReplayDetail::Full).await;
    assert!(full
        .iter()
        .any(|event| event.payload["$enc"].get("fr").is_some()));
    assert!(frames.values().all(|frame| frame["stream"] == 4));
    assert_payloads(&client_view(&keys, &full, &frames, false), &originals);
    let (summary, frames) = history(store, &id, ReplayDetail::Summary).await;
    assert!(frames.values().all(|frame| frame["stream"] == 3));
    let expected_summary: Vec<_> = originals.iter().map(summarized).collect();
    assert_payloads(
        &client_view(&keys, &summary, &frames, true),
        &expected_summary,
    );
    // A cold daemon serves the same frames without any DEK.
    let cold = e2e.restart().await;
    let (cold_full, cold_frames) = history(&cold.store, &id, ReplayDetail::Full).await;
    assert_eq!(
        serde_json::to_value(&cold_full).unwrap(),
        serde_json::to_value(&full).unwrap()
    );
    assert_eq!(cold_frames.len(), {
        let (_, frames) = history(store, &id, ReplayDetail::Full).await;
        frames.len()
    });
    e2e.cleanup();
}

#[tokio::test]
async fn a_restart_before_sealing_keeps_event_ciphertext_readable() {
    let e2e = E2e::new("todex-e2e-restart", true).await;
    let id = e2e.create(None).await.id;
    let mut originals = Vec::new();
    for index in 0..900 {
        let (event_type, payload) = mixed_payload(index);
        let event = e2e
            .store
            .append(&id, event_type, payload.clone())
            .await
            .unwrap();
        let mut original = ConversationEvent::new(&id, event.sequence, event_type, payload);
        original.provider = Some(ProviderKind::Codex);
        originals.push(original);
    }
    // (e) The daemon restarts before the sealed files are converted: their
    // DEKs are gone, so the ciphertext is framed as it is.
    let restarted = e2e.restart().await;
    let store = &restarted.store;
    let mut sealed = 0;
    while store.seal_next(&id).await.unwrap() {
        sealed += 1;
    }
    assert!(sealed >= 1);
    let directory = store.directory(&id).unwrap();
    for file in journal_files(&directory).await.unwrap() {
        let Some(number) = segment::numbered(&file.name, ".seg") else {
            continue;
        };
        let loaded = segment::load_index(&directory, number).unwrap();
        assert!(loaded
            .body
            .frames
            .iter()
            .filter(|frame| frame.stream != segment::STREAM_ENVELOPE)
            .all(|frame| frame.passthrough && frame.kid.is_none()));
        // Nothing is slimmed without the plaintext.
        assert_eq!(loaded.body.slimmed, 0);
    }
    let keys = restarted.client_keys(&id).await;
    let (full, frames) = history(store, &id, ReplayDetail::Full).await;
    assert!(frames.is_empty());
    assert!(full
        .iter()
        .all(|event| event.payload["$enc"].get("f").is_some()));
    let full_view = client_view(&keys, &full, &frames, false);
    for (actual, expected) in full_view.iter().zip(&originals) {
        assert_eq!(actual.payload, expected.payload);
    }
    let (summary, frames) = history(store, &id, ReplayDetail::Summary).await;
    let summary_view = client_view(&keys, &summary, &frames, true);
    for (actual, expected) in summary_view.iter().zip(&originals) {
        assert_eq!(actual.payload, summarized(expected).payload);
    }
    // Appends go on under a new key.
    let event = store
        .append(&id, "turn.started", json!({"turnId": "after"}))
        .await
        .unwrap();
    assert!(encrypted_content(&event.payload).is_some());
    restarted.cleanup();
}

#[tokio::test]
async fn enabling_encryption_migrates_plaintext_history_through_crashes() {
    let e2e = E2e::new("todex-e2e-migrate", false).await;
    let id = e2e.create(Some(SECRETS[4])).await.id;
    // Ends with a completed turn: migration only touches idle conversations.
    for index in 0..880 {
        let (event_type, mut payload) = mixed_payload(index);
        if index == 0 {
            payload["text"] = json!(SECRETS[0]);
        }
        e2e.store.append(&id, event_type, payload).await.unwrap();
    }
    e2e.store
        .save_request(
            &id,
            &json!({"turnId": "turn_0", "request": {"text": SECRETS[0], "content": [
                {"type": "text", "text": SECRETS[1]},
                {"type": "file", "path": "/tmp/notes.md"}]}, "files": []}),
        )
        .await
        .unwrap();
    // One plaintext segment is sealed while encryption is off.
    assert!(e2e.store.seal_next(&id).await.unwrap());
    let before = e2e.store.complete_history(&id).await.unwrap();
    let directory = e2e.store.directory(&id).unwrap();
    assert!(contains(&all_bytes(&directory), SECRETS[0]));

    e2e.keys
        .recipients()
        .set_mode(HistoryEncryption::E2e)
        .unwrap();
    let age = |store: &ConversationStore| {
        let store = store.clone();
        let id = id.clone();
        async move {
            let _guard = store.lock(&id).await;
            let mut manifest = store.get_unlocked(&id).await.unwrap();
            manifest.updated_at = Utc::now() - chrono::TimeDelta::hours(1);
            store.persist_manifest_locked(&manifest).await.unwrap();
        }
    };
    // (f) A crash in every step of re-encrypting the plaintext segment,
    // then one while converting the sealed active file.
    let mut daemon = e2e;
    for step in [CommitStep::Written, CommitStep::IndexRenamed] {
        daemon.store.set_commit_stop(Some(step));
        assert!(daemon.store.reencrypt_segment(&id, 1).await.is_err());
        daemon.store.set_commit_stop(None);
        let restarted = daemon.restart().await;
        let keys = restarted.client_keys(&id).await;
        let (events, frames) = history(&restarted.store, &id, ReplayDetail::Full).await;
        assert_same_or_marker_payloads(&client_view(&keys, &events, &frames, false), &before);
        daemon = restarted;
    }
    age(&daemon.store).await;
    daemon
        .store
        .set_commit_stop(Some(CommitStep::SegmentRenamed));
    assert!(daemon.store.encrypt_conversation(&id).await.is_err());
    daemon.store.set_commit_stop(None);
    let daemon = daemon.restart().await;
    age(&daemon.store).await;
    assert!(daemon.store.encrypt_conversation(&id).await.unwrap());

    let manifest = daemon.store.get(&id).await.unwrap();
    assert!(manifest.history_encrypted_at.is_some());
    assert!(manifest.title.is_none() && manifest.title_enc.is_some());
    let disk = all_bytes(&directory);
    for secret in [SECRETS[0], SECRETS[1], SECRETS[4]] {
        assert!(!contains(&disk, secret), "{secret} is still on disk");
    }
    let saved = daemon.store.last_request(&id).await.unwrap().unwrap();
    assert_eq!(saved["request"]["text"], "");
    assert_eq!(
        saved["request"]["content"],
        json!([{"type": "file", "path": "/tmp/notes.md"}])
    );
    let fingerprint = daemon.keys.fingerprint().unwrap();
    assert_eq!(saved["request"]["textMac"], fingerprint.mac(SECRETS[0]));
    for file in journal_files(&directory).await.unwrap() {
        if let Some(number) = segment::numbered(&file.name, ".seg") {
            let loaded = segment::load_index(&directory, number).unwrap();
            assert!(!loaded.body.has_plain_content(), "{}", file.name);
        }
    }
    let keys = daemon.client_keys(&id).await;
    let (full, frames) = history(&daemon.store, &id, ReplayDetail::Full).await;
    assert!(full
        .iter()
        .all(|event| encrypted_content(&event.payload).is_some()));
    assert_same_or_marker_payloads(&client_view(&keys, &full, &frames, false), &before);
    // Migration is idempotent.
    assert!(daemon.store.encrypt_conversation(&id).await.unwrap());
    daemon.cleanup();
}

fn assert_same_or_marker_payloads(after: &[ConversationEvent], before: &[ConversationEvent]) {
    assert_eq!(after.len(), before.len());
    for (after, before) in after.iter().zip(before) {
        assert_eq!(after.sequence, before.sequence);
        assert_eq!(after.event_id, before.event_id);
        if after.event_type == JOURNAL_COMPACTED_EVENT
            || before.event_type == JOURNAL_COMPACTED_EVENT
        {
            continue;
        }
        assert_eq!(after.payload, before.payload, "sequence {}", after.sequence);
    }
}

#[tokio::test]
async fn running_turns_keep_their_key_when_recipients_disappear() {
    let e2e = E2e::new("todex-e2e-revoked", true).await;
    let id = e2e.create(None).await.id;
    let first = e2e
        .store
        .append(&id, "turn.started", json!({"turnId": "t"}))
        .await
        .unwrap();
    let kid = first.payload["$enc"]["kid"].clone();
    let rid = crate::history_keys::encode_id(&recipient(1).rid());
    e2e.keys.recipients().revoke(&rid, "local").unwrap();
    // New prompts are refused before anything is written…
    let refused = e2e
        .store
        .save_request(&id, &json!({"turnId": "t2"}))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), "CONFLICT");
    // …while the running turn keeps encrypting under its last key.
    let next = e2e
        .store
        .append(&id, "turn.completed", json!({"turnId": "t"}))
        .await
        .unwrap();
    assert_eq!(next.payload["$enc"]["kid"], kid);
    // Without any key in memory the append fails instead of writing
    // plaintext.
    let restarted = e2e.restart().await;
    let error = restarted
        .store
        .append(&id, "turn.started", json!({"turnId": "t3"}))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "CONFLICT");
    let directory = restarted.store.directory(&id).unwrap();
    let lines = fs::read_to_string(directory.join(EVENTS_FILE)).unwrap();
    assert_eq!(lines.lines().count(), 2);
    for line in lines.lines() {
        let record: Value = serde_json::from_str(line).unwrap();
        assert!(record.get("c").is_none() && record.get("x").is_some());
    }
    restarted.cleanup();
}

#[tokio::test]
async fn forks_copy_ciphertext_frames_and_keys() {
    let e2e = E2e::new("todex-e2e-fork", true).await;
    let source = e2e.create(None).await.id;
    for index in 0..900 {
        let (event_type, payload) = mixed_payload(index);
        e2e.store
            .append(&source, event_type, payload)
            .await
            .unwrap();
    }
    while e2e.store.seal_next(&source).await.unwrap() {}
    let keys = e2e.client_keys(&source).await;
    let (source_events, source_frames) = history(&e2e.store, &source, ReplayDetail::Full).await;
    let source_view = client_view(&keys, &source_events, &source_frames, false);

    let fork = ConversationManifest::new(ProviderKind::Codex, e2e.root.clone(), None, None);
    let fork_id = fork.id.clone();
    let copy_id = fork_id.clone();
    let fork = e2e
        .store
        .create_from_journal(
            &source,
            fork,
            None,
            None,
            move |event, sequence| {
                let mut next =
                    ConversationEvent::new(&copy_id, sequence, event.event_type, event.payload);
                next.time = event.time;
                next
            },
            |_| Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(fork.last_sequence, source_events.len() as u64);
    // The fork reads its copies through its own keyring.
    let fork_keys = e2e.client_keys(&fork_id).await;
    assert_eq!(fork_keys.len(), keys.len());
    let (fork_events, fork_frames) = history(&e2e.store, &fork_id, ReplayDetail::Full).await;
    assert!(fork_events
        .iter()
        .any(|event| event.payload["$enc"].get("fr").is_some()));
    for (copy, original) in fork_events.iter().zip(&source_events) {
        // `$enc.c/n` keep naming the source record.
        assert_eq!(copy.payload["$enc"]["c"], original.payload["$enc"]["c"]);
        assert_eq!(copy.payload["$enc"]["n"], original.payload["$enc"]["n"]);
    }
    let fork_view = client_view(&fork_keys, &fork_events, &fork_frames, false);
    for (copy, original) in fork_view.iter().zip(&source_view) {
        assert_eq!(copy.payload, original.payload);
    }
    // Sealing the fork keeps the copies readable.
    while e2e.store.seal_next(&fork_id).await.unwrap() {}
    let (sealed_fork, sealed_frames) = history(&e2e.store, &fork_id, ReplayDetail::Full).await;
    let sealed_view = client_view(&fork_keys, &sealed_fork, &sealed_frames, false);
    for (copy, original) in sealed_view.iter().zip(&source_view) {
        assert_eq!(copy.payload, original.payload);
    }
    // Deleting the fork removes its keyring and forgets its keys.
    e2e.store.delete(&fork_id).await.unwrap();
    assert!(e2e.keys.keyrings().keys(&fork_id).await.unwrap().is_empty());
    e2e.store.delete(&source).await.unwrap();
    assert!(e2e.keys.deks().fallback_key(&source).await.is_none());
    e2e.cleanup();
}

#[tokio::test]
async fn salvage_keeps_intact_sealed_runs_of_an_e2e_segment() {
    let e2e = E2e::new("todex-e2e-salvage", true).await;
    let id = e2e.create(None).await.id;
    for index in 0..700 {
        let (event_type, payload) = mixed_payload(index);
        e2e.store.append(&id, event_type, payload).await.unwrap();
    }
    while e2e.store.seal_next(&id).await.unwrap() {}
    let keys = e2e.client_keys(&id).await;
    let (before, frames) = history(&e2e.store, &id, ReplayDetail::Full).await;
    let view = client_view(&keys, &before, &frames, false);
    // Losing the index: the frames are intact, the segment is rebuilt with
    // its sealed runs copied as they are.
    let directory = e2e.store.directory(&id).unwrap();
    fs::remove_file(directory.join(segment::index_name(1))).unwrap();
    let restarted = e2e.restart().await;
    let (after, frames) = history(&restarted.store, &id, ReplayDetail::Full).await;
    let after_view = client_view(&keys, &after, &frames, false);
    assert_eq!(after_view.len(), view.len());
    for (after, before) in after_view.iter().zip(&view) {
        assert_eq!(after.payload, before.payload);
    }
    restarted.cleanup();
}

#[tokio::test]
async fn a_provider_payload_cannot_pose_as_ciphertext() {
    let e2e = E2e::new("todex-e2e-reserved", false).await;
    let id = e2e.create(None).await.id;
    let forged = json!({"turnId": "t", "$enc": {"v": 1, "kid": "k", "c": "c", "n": 1, "f": "AA"}});
    e2e.store
        .append(&id, "provider.event", forged.clone())
        .await
        .unwrap();
    let replayed = e2e.store.event_at(&id, 1).await.unwrap().unwrap();
    assert!(encrypted_content(&replayed.payload).is_none());
    assert_eq!(replayed.payload["_$enc"], forged["$enc"]);
    e2e.cleanup();
}

#[tokio::test]
async fn large_payloads_sealed_on_the_blocking_pool_keep_journal_order() {
    let e2e = E2e::new("todex-e2e-large", true).await;
    let id = e2e.create(None).await.id;
    let hub = ConversationEventHub::default();
    let mut live = hub.subscribe(&id);
    // Small and large payloads interleave: both encode paths share one
    // order, and every record decrypts to what was appended.
    let payloads: Vec<Value> = (0..6)
        .map(|index| {
            let size = if index % 2 == 0 { 16 } else { 200 * 1024 };
            json!({"turnId": "t1", "index": index, "output": "x".repeat(size)})
        })
        .collect();
    for payload in &payloads {
        e2e.store
            .append_and_publish(&id, "tool.updated", payload.clone(), &hub)
            .await
            .unwrap();
    }
    let keys = e2e.client_keys(&id).await;
    let (events, frames) = history(&e2e.store, &id, ReplayDetail::Full).await;
    assert_eq!(events.len(), payloads.len());
    for (index, (event, payload)) in events.iter().zip(&payloads).enumerate() {
        assert_eq!(event.sequence, index as u64 + 1);
        let plain = crate::conversation::e2e_support::decrypt(&keys, event, &frames)
            .expect("record is sealed");
        assert_eq!(plain["index"], payload["index"]);
        assert_eq!(plain["output"], payload["output"]);
        let published = live.try_recv().unwrap();
        assert_eq!(published.sequence, event.sequence);
        assert_eq!(published.payload, event.payload);
    }
    e2e.cleanup();
}
