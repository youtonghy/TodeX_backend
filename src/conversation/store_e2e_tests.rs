//! End-to-end encrypted history through the store
//! (`docs/history-encryption.md`): what reaches the disk, what replays and
//! live events carry, sealing, restarts and migration. Kept beside
//! `store.rs` so they reach its private items. Decryption uses the test
//! recipient seeds, standing in for a client. Legacy plaintext history is
//! written by a keyless store, as older versions wrote it.

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
/// `encrypted: false` starts with a keyless store writing plaintext, as
/// versions before mandatory encryption did; [`Self::restart`] always comes
/// back with keys.
pub(super) struct E2e {
    pub root: PathBuf,
    pub keys: HistoryKeys,
    pub store: ConversationStore,
}

impl E2e {
    pub async fn new(prefix: &str, encrypted: bool) -> Self {
        let root = temp_dir(prefix);
        let keys = HistoryKeys::load(&root, None).unwrap();
        keys.recipients()
            .register_device("dev_a", &recipient(1))
            .unwrap();
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let store = if encrypted {
            store.with_history_keys(keys.clone())
        } else {
            store
        };
        Self { root, keys, store }
    }

    /// A restarted daemon: same files, no DEK in memory.
    pub async fn restart(&self) -> Self {
        let keys = HistoryKeys::load(&self.root, None).unwrap();
        let store = ConversationStore::open(self.root.clone(), keys.clone())
            .await
            .unwrap();
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

/// A legacy conversation: plaintext title, records and request snapshot,
/// with one plaintext segment sealed.
async fn legacy_conversation(e2e: &E2e) -> String {
    let id = e2e.create(Some(SECRETS[4])).await.id;
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
            &json!({"turnId": "turn_0", "request": {"text": SECRETS[0]}, "files": []}),
        )
        .await
        .unwrap();
    assert!(e2e.store.seal_next(&id).await.unwrap());
    id
}

/// Every file of a conversation except its manifest and snapshot, which
/// the legacy scan may rewrite.
fn journal_bytes(directory: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut files = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file())
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            name != MANIFEST_FILE && name != SNAPSHOT_FILE
        })
        .map(|path| {
            (
                path.file_name().unwrap().to_string_lossy().to_string(),
                fs::read(&path).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

#[tokio::test]
async fn legacy_plaintext_history_is_marked_once_and_left_untouched() {
    let legacy_writer = E2e::new("todex-e2e-legacy", false).await;
    let legacy = legacy_conversation(&legacy_writer).await;
    // Plaintext only in a title or only in the request snapshot.
    let titled = legacy_writer.create(Some("plain title")).await.id;
    let requested = legacy_writer.create(None).await.id;
    legacy_writer
        .store
        .save_request(&requested, &json!({"request": {"text": "plain"}}))
        .await
        .unwrap();
    // Never used: nothing plaintext, so it is not legacy.
    let empty = legacy_writer.create(None).await.id;
    // Plaintext followed by encrypted records (encryption switched on
    // without the retired migration).
    let mixed = legacy_writer.create(None).await.id;
    legacy_writer
        .store
        .append(&mixed, "message.created", json!({"role": "user", "content": SECRETS[1]}))
        .await
        .unwrap();
    let directory = legacy_writer.store.directory(&legacy).unwrap();
    let legacy_files = journal_bytes(&directory);
    let before = legacy_writer.store.complete_history(&legacy).await.unwrap();

    let daemon = legacy_writer.restart().await;
    daemon
        .store
        .append(&mixed, "message.completed", json!({"role": "assistant", "content": "x"}))
        .await
        .unwrap();
    // Created encrypted: decided from the start.
    let encrypted = daemon.create(Some("secret title")).await.id;
    daemon
        .store
        .append(&encrypted, "message.created", json!({"role": "user", "content": "hi"}))
        .await
        .unwrap();
    assert!(daemon
        .store
        .get(&encrypted)
        .await
        .unwrap()
        .history_encrypted_at
        .is_some());
    // An interrupted earlier pass already decided one conversation.
    assert_eq!(
        daemon.store.ensure_not_legacy(&titled).await.unwrap_err().code(),
        "HISTORY_READ_ONLY"
    );

    let scan = daemon.store.scan_legacy(&daemon.root).await.unwrap();
    assert_eq!(
        scan,
        crate::conversation::legacy::LegacyScan {
            conversations: 6,
            already_decided: 2,
            marked_legacy: 3,
            marked_encrypted: 1,
            failed: 0,
            skipped: false,
        }
    );
    for (id, expected) in [
        (&legacy, true),
        (&titled, true),
        (&requested, true),
        (&mixed, true),
        (&empty, false),
        (&encrypted, false),
    ] {
        let manifest = daemon.store.get(id).await.unwrap();
        assert_eq!(manifest.legacy_plaintext, expected, "{id}");
        assert_eq!(manifest.history_encrypted_at.is_some(), !expected, "{id}");
        let wire = serde_json::to_value(&manifest).unwrap();
        assert_eq!(wire.get("legacyPlaintext").is_some(), expected, "{id}");
    }
    // Nothing of the legacy conversation was rewritten and it reads as
    // before.
    assert_eq!(journal_bytes(&directory), legacy_files);
    let after = daemon.store.complete_history(&legacy).await.unwrap();
    assert_eq!(after.len(), before.len());
    for (after, before) in after.iter().zip(&before) {
        assert_eq!(after.payload, before.payload);
    }
    // The marker makes later passes skip; a restart keeps the flags.
    assert!(daemon.root.join(crate::conversation::legacy::LEGACY_SCAN_MARKER).exists());
    let restarted = daemon.restart().await;
    assert!(restarted.store.scan_legacy(&restarted.root).await.unwrap().skipped);
    assert!(restarted.store.get(&legacy).await.unwrap().legacy_plaintext);
    restarted.cleanup();
}

#[tokio::test]
async fn legacy_conversations_are_read_only() {
    let legacy_writer = E2e::new("todex-e2e-read-only", false).await;
    let id = legacy_conversation(&legacy_writer).await;
    let daemon = legacy_writer.restart().await;
    let store = &daemon.store;
    let last_sequence = store.get(&id).await.unwrap().last_sequence;
    // Decided on the first write attempt, before any scan ran.
    let read_only = |result: Result<(), AppError>| {
        assert_eq!(result.unwrap_err().code(), "HISTORY_READ_ONLY");
    };
    read_only(store.ensure_history_writable(&id).await);
    read_only(
        store
            .save_request(&id, &json!({"request": {"text": "new"}}))
            .await,
    );
    read_only(store.append(&id, "turn.started", json!({"turnId": "t"})).await.map(|_| ()));
    read_only(
        store
            .update_metadata(&id, Some(Some("renamed".to_owned())), None)
            .await
            .map(|_| ()),
    );
    assert_eq!(store.get(&id).await.unwrap().last_sequence, last_sequence);
    // Reading, archiving and deleting stay allowed.
    let page = store.replay(&id, 0, 10).await.unwrap();
    assert_eq!(page.events[0].payload["text"], SECRETS[0]);
    let archived = store.update_metadata(&id, None, Some(true)).await.unwrap();
    assert!(archived.archived_at.is_some() && archived.legacy_plaintext);
    assert_eq!(archived.title.as_deref(), Some(SECRETS[4]));
    let restored = store.update_metadata(&id, None, Some(false)).await.unwrap();
    assert!(restored.archived_at.is_none());
    store.delete(&id).await.unwrap();
    daemon.cleanup();
}

#[tokio::test]
async fn imports_and_fork_trailers_are_encrypted() {
    let e2e = E2e::new("todex-e2e-import", true).await;
    let manifest = ConversationManifest::new(
        ProviderKind::Codex,
        e2e.root.clone(),
        Some(SECRETS[4].to_owned()),
        None,
    );
    let id = manifest.id.clone();
    let events = (1..=3)
        .map(|sequence| {
            ConversationEvent::new(
                &id,
                sequence,
                "message.created",
                json!({"role": "user", "content": format!("{}-{sequence}", SECRETS[0])}),
            )
        })
        .collect::<Vec<_>>();
    let imported = e2e
        .store
        .create_with_history(manifest, events.clone(), None, None)
        .await
        .unwrap();
    assert!(imported.history_encrypted_at.is_some() && !imported.legacy_plaintext);
    assert!(imported.title.is_none() && imported.title_enc.is_some());
    let directory = e2e.store.directory(&id).unwrap();
    assert!(!contains(&all_bytes(&directory), SECRETS[0]));
    assert!(!contains(&all_bytes(&directory), SECRETS[4]));
    let keys = e2e.client_keys(&id).await;
    let (stored, frames) = history(&e2e.store, &id, ReplayDetail::Full).await;
    let view = client_view(&keys, &stored, &frames, false);
    assert_eq!(view.len(), 3);
    for (seen, original) in view.iter().zip(&events) {
        assert_eq!(seen.payload["content"], original.payload["content"]);
    }
    // Writable: new appends continue the conversation.
    e2e.store.ensure_history_writable(&id).await.unwrap();
    e2e.store
        .append(&id, "turn.started", json!({"turnId": "t"}))
        .await
        .unwrap();

    // A fork copies the ciphertext and encrypts its own trailer.
    let fork = ConversationManifest::new(ProviderKind::Codex, e2e.root.clone(), None, None);
    let fork_id = fork.id.clone();
    let copy_id = fork_id.clone();
    let trailer_id = fork_id.clone();
    let fork = e2e
        .store
        .create_from_journal(
            &id,
            fork,
            None,
            None,
            move |event, sequence| {
                ConversationEvent::new(&copy_id, sequence, event.event_type, event.payload)
            },
            move |copied| {
                vec![ConversationEvent::new(
                    &trailer_id,
                    copied + 1,
                    "conversation.forked",
                    json!({"sourceConversationId": SECRETS[2]}),
                )]
            },
        )
        .await
        .unwrap();
    assert!(fork.history_encrypted_at.is_some() && !fork.legacy_plaintext);
    assert!(!contains(
        &all_bytes(&e2e.store.directory(&fork_id).unwrap()),
        SECRETS[2]
    ));
    let fork_keys = e2e.client_keys(&fork_id).await;
    let (forked, frames) = history(&e2e.store, &fork_id, ReplayDetail::Full).await;
    assert!(forked
        .iter()
        .all(|event| encrypted_content(&event.payload).is_some()));
    let view = client_view(&fork_keys, &forked, &frames, false);
    assert_eq!(view.last().unwrap().payload["sourceConversationId"], SECRETS[2]);
    assert_eq!(view[0].payload["content"], events[0].payload["content"]);
    // The startup scan agrees: nothing here is legacy.
    let scan = e2e.store.scan_legacy(&e2e.root).await.unwrap();
    assert_eq!((scan.already_decided, scan.marked_legacy), (2, 0));
    e2e.cleanup();
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
    assert_eq!(refused.code(), "HISTORY_KEY_REQUIRED");
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
    assert_eq!(error.code(), "HISTORY_KEY_REQUIRED");
    // Nor can a new conversation be created.
    assert_eq!(
        restarted
            .store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                restarted.root.clone(),
                None,
                None,
            ))
            .await
            .unwrap_err()
            .code(),
        "HISTORY_KEY_REQUIRED"
    );
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
