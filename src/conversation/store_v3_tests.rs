//! History v3 storage tests: sealing, crash recovery, segment salvage,
//! reading v2 journals and the storage floor. Kept beside `store.rs` so they reach its
//! private items.

use std::fs;

use serde_json::json;

use super::*;
use crate::conversation::ProviderKind;

fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
    fs::create_dir_all(&path).unwrap();
    path
}

/// A representative mix: user prompts, Codex-style deltas with their
/// completed record, Claude-style deltas without ids, tool snapshots with
/// terminals, thoughts and turn boundaries.
pub(super) fn mixed_payload(index: u64) -> (&'static str, Value) {
    let turn = format!("turn_{}", index / 40);
    let block = format!("item_{}", index / 8);
    match index % 40 {
        0 => (
            "message.created",
            json!({"turnId": turn, "role": "user", "clientRequestId": format!("req_{index}"),
                   "text": format!("Please look at module {index} and fix the failing test.")}),
        ),
        1 => ("turn.started", json!({"turnId": turn})),
        39 => (
            "turn.completed",
            json!({"turnId": turn, "stopReason": "end_turn"}),
        ),
        n if n % 8 == 7 => (
            "message.completed",
            json!({"turnId": turn, "role": "assistant",
                   "block": {"category": "assistant_final", "id": block, "turnId": turn, "phase": "completed"},
                   "message": {"text": format!("Answer {index}: ").repeat(12)}}),
        ),
        n if n % 5 == 0 => (
            "tool.updated",
            json!({"turnId": turn, "toolCallId": format!("call_{}", index / 10),
                   "status": "in_progress", "result": "line of build output\n".repeat(6)}),
        ),
        n if n % 10 == 9 => (
            "tool.completed",
            json!({"turnId": turn, "toolCallId": format!("call_{}", index / 10),
                   "result": "final output\n".repeat(10), "isError": false}),
        ),
        n if n % 7 == 3 => (
            "thought.delta",
            json!({"turnId": turn, "delta": {"text": "considering the options "}}),
        ),
        n if n % 2 == 0 => (
            "message.delta",
            json!({"turnId": turn, "role": "assistant",
                   "block": {"category": "assistant_final", "id": block, "turnId": turn, "phase": "delta"},
                   "delta": "streamed words "}),
        ),
        _ => (
            "message.delta",
            json!({"turnId": turn, "role": "assistant", "delta": {"type": "text_delta", "text": "claude text "}}),
        ),
    }
}

async fn store_with(prefix: &str, count: u64) -> (PathBuf, ConversationStore, String) {
    let root = temp_dir(prefix);
    let store = ConversationStore::new(root.clone()).await.unwrap();
    let manifest = store
        .create(ConversationManifest::new(
            ProviderKind::Codex,
            root.clone(),
            None,
            None,
        ))
        .await
        .unwrap();
    for index in 0..count {
        let (event_type, payload) = mixed_payload(index);
        store
            .append(&manifest.id, event_type, payload)
            .await
            .unwrap();
    }
    (root, store, manifest.id)
}

async fn seal_all(store: &ConversationStore, id: &str) -> usize {
    let mut sealed = 0;
    while store.seal_next(id).await.unwrap() {
        sealed += 1;
    }
    sealed
}

fn as_json(events: &[ConversationEvent]) -> Vec<Value> {
    events
        .iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect()
}

/// `history` with every record seal-time slimming may strip replaced by
/// what the reader must show: either the original or its marker.
fn assert_same_or_marker(sealed: &[ConversationEvent], original: &[ConversationEvent]) {
    assert_eq!(sealed.len(), original.len());
    for (after, before) in sealed.iter().zip(original) {
        assert_eq!(after.sequence, before.sequence);
        assert_eq!(after.event_id, before.event_id);
        assert_eq!(after.time, before.time);
        if after.event_type == JOURNAL_COMPACTED_EVENT {
            assert_eq!(after.payload["originalType"], before.event_type.as_str());
            assert!(matches!(
                before.event_type.as_str(),
                "message.delta" | "tool.updated" | "subagent.updated"
            ));
        } else {
            assert_eq!(
                serde_json::to_value(after).unwrap(),
                serde_json::to_value(before).unwrap()
            );
        }
    }
}

fn dir_names(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn sealed_segments_replay_like_plaintext_and_keep_the_digest() {
    let (root, store, id) = store_with("todex-v3-seal", 1_500).await;
    let directory = store.directory(&id).unwrap();
    let before = store.complete_history(&id).await.unwrap();
    let digest_before = store.digest(&id, Clone::clone).await.unwrap();
    assert!(dir_names(&directory)
        .iter()
        .any(|name| name.ends_with(".jsonl") && name != EVENTS_FILE));
    let sealed = seal_all(&store, &id).await;
    assert!(sealed >= 2, "fixture must span several segments");
    let names = dir_names(&directory);
    assert!(names.iter().any(|name| name.ends_with(".seg")));
    assert!(names.iter().any(|name| name.ends_with(".idx")));
    assert!(
        !names
            .iter()
            .any(|name| name.ends_with(".jsonl") && name != EVENTS_FILE),
        "{names:?}"
    );
    // Warm (index updated in place) and cold (fresh store) reads agree.
    let warm = store.complete_history(&id).await.unwrap();
    assert_same_or_marker(&warm, &before);
    assert!(warm
        .iter()
        .any(|event| event.event_type == JOURNAL_COMPACTED_EVENT));
    // Thoughts and id-less deltas survive slimming.
    for (after, original) in warm.iter().zip(&before) {
        let id_less_delta =
            original.event_type == "message.delta" && original.payload.get("block").is_none();
        if original.event_type == "thought.delta" || id_less_delta {
            assert_eq!(after.event_type, original.event_type);
        }
    }
    let cold = ConversationStore::new(root.clone()).await.unwrap();
    let cold_history = cold.complete_history(&id).await.unwrap();
    assert_eq!(as_json(&cold_history), as_json(&warm));
    assert_eq!(cold.digest(&id, Clone::clone).await.unwrap(), digest_before);
    // Backward paging crosses segment boundaries with the same pages.
    let mut cursor = u64::MAX;
    let mut backward = Vec::new();
    loop {
        let page = cold.replay_before(&id, cursor, 97).await.unwrap();
        backward.splice(0..0, page.events.iter().cloned());
        if !page.has_more {
            break;
        }
        cursor = page.from_sequence;
    }
    assert_eq!(as_json(&backward), as_json(&warm));
    for sequence in [1, 333, 900, 1_500] {
        let event = cold.event_at(&id, sequence).await.unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            serde_json::to_value(&warm[sequence as usize - 1]).unwrap()
        );
    }
    // Appends continue after the sealed history.
    let appended = cold
        .append(&id, "turn.started", json!({"turnId": "x"}))
        .await
        .unwrap();
    assert_eq!(appended.sequence, 1_501);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn replay_pages_over_sealed_records_keep_the_byte_budget() {
    let root = temp_dir("todex-v3-budget");
    let store = ConversationStore::new(root.clone()).await.unwrap();
    let manifest = store
        .create(ConversationManifest::new(
            ProviderKind::Codex,
            root.clone(),
            None,
            None,
        ))
        .await
        .unwrap();
    // ~200 KiB records: a 8 MiB page holds about 40 of them.
    for index in 0..60 {
        store
            .append(
                &manifest.id,
                "provider.event",
                json!({"index": index, "blob": "x".repeat(200 * 1024)}),
            )
            .await
            .unwrap();
    }
    let plain_forward = store.replay(&manifest.id, 0, 1000).await.unwrap();
    let plain_backward = store
        .replay_before(&manifest.id, u64::MAX, 1000)
        .await
        .unwrap();
    seal_all(&store, &manifest.id).await;
    let cold = ConversationStore::new(root.clone()).await.unwrap();
    let forward = cold.replay(&manifest.id, 0, 1000).await.unwrap();
    let backward = cold
        .replay_before(&manifest.id, u64::MAX, 1000)
        .await
        .unwrap();
    assert!(forward.has_more && forward.events.len() < 60);
    assert_eq!(as_json(&forward.events), as_json(&plain_forward.events));
    assert_eq!(as_json(&backward.events), as_json(&plain_backward.events));
    assert_eq!(backward.from_sequence, plain_backward.from_sequence);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn every_commit_step_recovers_to_the_same_history() {
    for step in [
        CommitStep::Written,
        CommitStep::IndexRenamed,
        CommitStep::SegmentRenamed,
        CommitStep::FirstSourceRemoved,
    ] {
        let (root, store, id) = store_with("todex-v3-crash", 900).await;
        // A migration-style group of several plaintext files exercises the
        // multi-source deletion step too.
        let directory = store.directory(&id).unwrap();
        let before = store.complete_history(&id).await.unwrap();
        store.set_commit_stop(Some(step));
        assert!(store.seal_next(&id).await.is_err());
        store.set_commit_stop(None);
        // "Restart": a fresh store reconciles the directory.
        let restarted = ConversationStore::new(root.clone()).await.unwrap();
        let after = restarted.complete_history(&id).await.unwrap();
        assert_same_or_marker(&after, &before);
        let names = dir_names(&directory);
        assert!(
            !names
                .iter()
                .any(|name| name.starts_with(segment::SEAL_TEMP_PREFIX)),
            "{step:?}: {names:?}"
        );
        // Each sequence lives in exactly one file: either the segment was
        // rolled back or its sources are gone.
        let first_seg = names.iter().any(|name| name == "events.000001.seg");
        let first_plain = names.iter().any(|name| name == "events.000001.jsonl");
        assert!(first_seg != first_plain, "{step:?}: {names:?}");
        // The conversion converges when retried.
        seal_all(&restarted, &id).await;
        let sealed = restarted.complete_history(&id).await.unwrap();
        assert_same_or_marker(&sealed, &before);
        fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn a_segment_left_beside_its_damaged_sources_is_rolled_back() {
    let (root, store, id) = store_with("todex-v3-crash-damaged", 600).await;
    let directory = store.directory(&id).unwrap();
    let before = store.complete_history(&id).await.unwrap();
    store.set_commit_stop(Some(CommitStep::SegmentRenamed));
    assert!(store.seal_next(&id).await.is_err());
    // A torn `.seg` (crash before its data reached the disk) does not hash.
    let seg = directory.join("events.000001.seg");
    let length = fs::metadata(&seg).unwrap().len();
    fs::OpenOptions::new()
        .write(true)
        .open(&seg)
        .unwrap()
        .set_len(length / 2)
        .unwrap();
    let restarted = ConversationStore::new(root.clone()).await.unwrap();
    assert_eq!(
        as_json(&restarted.complete_history(&id).await.unwrap()),
        as_json(&before)
    );
    assert!(!seg.exists());
    assert!(directory.join("events.000001.jsonl").exists());
    fs::remove_dir_all(root).unwrap();
}

fn damage_frame(directory: &Path, number: u64, stream: u8) -> (u64, u64) {
    let loaded = segment::load_index(directory, number).unwrap();
    let frame = loaded
        .body
        .frames
        .iter()
        .find(|frame| frame.stream == stream)
        .unwrap()
        .clone();
    let path = directory.join(segment::segment_name(number));
    let mut bytes = fs::read(&path).unwrap();
    for offset in 0..8 {
        bytes[(frame.offset + u64::from(frame.len) / 2) as usize + offset] ^= 0x5a;
    }
    fs::write(&path, bytes).unwrap();
    (frame.first, u64::from(frame.count))
}

#[tokio::test]
async fn a_damaged_frame_salvages_only_its_records() {
    let (root, store, id) = store_with("todex-v3-frame", 1_200).await;
    seal_all(&store, &id).await;
    let before = store.complete_history(&id).await.unwrap();
    let directory = store.directory(&id).unwrap();
    let (first, count) = damage_frame(&directory, 1, segment::STREAM_FULL);
    let restarted = ConversationStore::new(root.clone()).await.unwrap();
    let after = restarted.complete_history(&id).await.unwrap();
    assert_eq!(after.len(), before.len());
    for (event, original) in after.iter().zip(&before) {
        if (first..first + count).contains(&event.sequence) {
            assert_eq!(event.event_type, JOURNAL_RECORD_LOST_EVENT);
            assert_eq!(event.payload["runStart"], first);
            assert_eq!(event.payload["runLength"], count);
        } else {
            assert_eq!(
                serde_json::to_value(event).unwrap(),
                serde_json::to_value(original).unwrap()
            );
        }
    }
    // The rebuilt segment is a normal one; the damaged bytes are kept.
    assert!(segment::load_index(&directory, 1).is_ok());
    assert!(dir_names(&directory)
        .iter()
        .any(|name| name.starts_with("events.corrupt.") && name.ends_with(".seg")));
    // Only the damaged segment was rewritten: no other file is affected.
    let again = ConversationStore::new(root.clone()).await.unwrap();
    assert_eq!(
        as_json(&again.complete_history(&id).await.unwrap()),
        as_json(&after)
    );
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn a_damaged_or_lost_index_is_rebuilt_from_the_segment() {
    for damage in ["garbage", "missing"] {
        let (root, store, id) = store_with("todex-v3-idx", 1_200).await;
        seal_all(&store, &id).await;
        let before = store.complete_history(&id).await.unwrap();
        let directory = store.directory(&id).unwrap();
        let idx = directory.join(segment::index_name(1));
        if damage == "missing" {
            fs::remove_file(&idx).unwrap();
        } else {
            let mut bytes = fs::read(&idx).unwrap();
            let middle = bytes.len() / 2;
            bytes[middle] ^= 0x01;
            fs::write(&idx, bytes).unwrap();
        }
        let restarted = ConversationStore::new(root.clone()).await.unwrap();
        let after = restarted.complete_history(&id).await.unwrap();
        assert_eq!(as_json(&after), as_json(&before), "{damage}");
        assert!(segment::load_index(&directory, 1).is_ok());
        assert_eq!(
            restarted.digest(&id, Clone::clone).await.unwrap(),
            store.digest(&id, Clone::clone).await.unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn cold_open_and_digest_read_no_sealed_bytes() {
    let (root, store, id) = store_with("todex-v3-cold", 1_200).await;
    seal_all(&store, &id).await;
    let digest = store.digest(&id, Clone::clone).await.unwrap();
    let tail = store
        .append(&id, "turn.started", json!({"turnId": "tail"}))
        .await
        .unwrap();
    let directory = store.directory(&id).unwrap();
    // Scramble every frame payload of the sealed segments while keeping
    // their length: any read of sealed content would now fail.
    for name in dir_names(&directory) {
        if let Some(number) = segment::numbered(&name, ".seg") {
            let loaded = segment::load_index(&directory, number).unwrap();
            let path = directory.join(&name);
            let mut bytes = fs::read(&path).unwrap();
            for frame in &loaded.body.frames {
                for byte in &mut bytes
                    [frame.offset as usize..(frame.offset + u64::from(frame.len)) as usize]
                {
                    *byte = !*byte;
                }
            }
            fs::write(&path, bytes).unwrap();
        }
    }
    let cold = ConversationStore::new(root.clone()).await.unwrap();
    let mut expected = digest;
    expected.apply(&tail);
    assert_eq!(cold.digest(&id, Clone::clone).await.unwrap(), expected);
    let page = cold.replay_before(&id, u64::MAX, 1).await.unwrap();
    assert_eq!(page.events[0].sequence, tail.sequence);
    let recovered = cold.recover(&id).await.unwrap();
    assert_eq!(recovered.last_sequence, tail.sequence);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn a_journal_ending_in_a_sealed_segment_reads_its_tail_from_frames() {
    let (root, store, id) = store_with("todex-v3-tail", 700).await;
    let last = store.complete_history(&id).await.unwrap().pop().unwrap();
    {
        let _guard = store.lock(&id).await;
        store.seal_active_locked(&id).await.unwrap();
    }
    seal_all(&store, &id).await;
    let directory = store.directory(&id).unwrap();
    assert_eq!(fs::metadata(directory.join(EVENTS_FILE)).unwrap().len(), 0);
    let cold = ConversationStore::new(root.clone()).await.unwrap();
    let (tail, terminated) = cold.journal_tail(&id).await.unwrap();
    assert!(terminated);
    assert_eq!(tail.unwrap().event_id, last.event_id);
    let next = cold
        .append(&id, "turn.started", json!({"turnId": "n"}))
        .await
        .unwrap();
    assert_eq!(next.sequence, last.sequence + 1);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn prompts_are_refused_only_when_the_disk_is_nearly_full() {
    let (root, store, id) = store_with("todex-v3-storage", 3).await;
    store.save_request(&id, &json!({"ok": 1})).await.unwrap();
    store.set_free_space_for_tests(Some(STORAGE_LOW_BYTES - 1));
    let error = store
        .save_request(&id, &json!({"ok": 2}))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "STORAGE_LOW");
    // Running turns keep appending regardless.
    store
        .append(&id, "turn.completed", json!({}))
        .await
        .unwrap();
    store.set_free_space_for_tests(Some(STORAGE_LOW_BYTES));
    store.save_request(&id, &json!({"ok": 3})).await.unwrap();
    assert!(super::super::maintenance::available_space(&root).unwrap() > 0);
    fs::remove_dir_all(root).unwrap();
}

/// A v2 journal as older builds left it: full-event lines split across
/// several sealed files plus the active file.
async fn write_v2_conversation(root: &Path, count: u64, per_file: u64) -> String {
    let store = ConversationStore::new(root.to_owned()).await.unwrap();
    let mut manifest =
        ConversationManifest::new(ProviderKind::ClaudeCode, root.to_owned(), None, None);
    manifest.updated_at = Utc::now() - chrono::TimeDelta::hours(1);
    let id = manifest.id.clone();
    let directory = root.join("conversations").join(&id);
    fs::create_dir_all(&directory).unwrap();
    let mut file_index = 0u64;
    let mut lines = Vec::new();
    for sequence in 1..=count {
        let (event_type, payload) = mixed_payload(sequence - 1);
        let mut event = ConversationEvent::new(&id, sequence, event_type, payload);
        event.provider = Some(ProviderKind::ClaudeCode);
        event.time =
            DateTime::from_timestamp(1_700_000_000 + sequence as i64, 123_456_789).unwrap();
        lines.extend(serde_json::to_vec(&event).unwrap());
        lines.push(b'\n');
        if sequence % per_file == 0 && sequence < count {
            file_index += 1;
            fs::write(
                directory.join(format!("events.{file_index:06}.jsonl")),
                &lines,
            )
            .unwrap();
            lines.clear();
        }
    }
    fs::write(directory.join(EVENTS_FILE), &lines).unwrap();
    manifest.last_sequence = count;
    write_atomic_json(&directory.join(MANIFEST_FILE), &manifest)
        .await
        .unwrap();
    write_atomic_json(
        &directory.join(PROVIDER_STATE_FILE),
        &ProviderState::new(manifest.provider),
    )
    .await
    .unwrap();
    drop(store);
    id
}

#[tokio::test]
async fn v2_journals_stay_readable_and_are_never_rewritten() {
    let root = temp_dir("todex-v3-v2-legacy");
    let id = write_v2_conversation(&root, 1_000, 150).await;
    let directory = root.join("conversations").join(&id);
    let originals = |directory: &Path| {
        let mut files = dir_names(directory)
            .into_iter()
            .filter(|name| name.ends_with(".jsonl"))
            .map(|name| {
                let bytes = fs::read(directory.join(&name)).unwrap();
                (name, bytes)
            })
            .collect::<Vec<_>>();
        files.sort();
        files
    };
    let before = originals(&directory);
    let keys = crate::history_keys::HistoryKeys::load(&root, None).unwrap();
    keys.recipients()
        .register_device("local", &crate::history_keys::test_support::recipient(1))
        .unwrap();
    let store = ConversationStore::open(root.clone(), keys).await.unwrap();
    let history = store.complete_history(&id).await.unwrap();
    assert_eq!(history.len(), 1_000);
    assert_eq!(history[999].sequence, 1_000);
    let scan = store.scan_legacy(&root).await.unwrap();
    assert_eq!(scan.marked_legacy, 1);
    assert!(store.get(&id).await.unwrap().legacy_plaintext);
    // Maintenance has nothing to do with it: no conversion was queued, and
    // the read path never rewrote the v2 files.
    assert_eq!(originals(&directory), before);
    assert!(!directory.join("journal-v2-backup").exists());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn forks_stream_sealed_history_into_new_segments() {
    let (root, store, id) = store_with("todex-v3-fork", 1_100).await;
    seal_all(&store, &id).await;
    let source = store.complete_history(&id).await.unwrap();
    let fork = ConversationManifest::new(ProviderKind::Codex, root.clone(), None, None);
    let fork_id = fork.id.clone();
    let created = store
        .create_from_journal(
            &id,
            fork,
            None,
            None,
            |event, sequence| {
                let mut next =
                    ConversationEvent::new(&fork_id, sequence, event.event_type, event.payload);
                next.time = event.time;
                next
            },
            |copied| {
                vec![ConversationEvent::new(
                    &fork_id,
                    copied + 1,
                    "conversation.forked",
                    json!({"sourceSequence": copied}),
                )]
            },
        )
        .await
        .unwrap();
    assert_eq!(created.last_sequence, source.len() as u64 + 1);
    assert_eq!(
        created.storage_version,
        Some(super::super::model::STORAGE_VERSION)
    );
    let copied = store.complete_history(&fork_id).await.unwrap();
    assert_eq!(copied.len(), source.len() + 1);
    for (copy, original) in copied.iter().zip(&source) {
        assert_eq!(copy.event_type, original.event_type);
        assert_eq!(copy.payload, original.payload);
    }
    // The copy rotated plaintext files that maintenance converts.
    assert!(seal_all(&store, &fork_id).await > 0);
    assert_eq!(
        as_json(&store.complete_history(&fork_id).await.unwrap())[..10],
        as_json(&copied)[..10]
    );
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn tail_and_digest_caches_are_bounded_and_rebuild_on_demand() {
    let root = temp_dir("todex-v3-tail-lru");
    let store = ConversationStore::new(root.clone()).await.unwrap();
    let mut ids = Vec::new();
    for _ in 0..TAIL_CACHE_ENTRIES.max(DIGEST_CACHE_ENTRIES) + 3 {
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        store
            .append(&manifest.id, "turn.started", json!({"turnId": "t"}))
            .await
            .unwrap();
        store
            .digest(&manifest.id, |digest| digest.last_sequence())
            .await
            .unwrap();
        ids.push(manifest.id);
    }
    assert_eq!(store.tails.len(), TAIL_CACHE_ENTRIES);
    assert_eq!(store.digests.len(), DIGEST_CACHE_ENTRIES);
    assert!(store.tails.with(&ids[0], |_| ()).is_none());
    assert!(store.digests.with(&ids[0], |_| ()).is_none());
    // An evicted conversation re-reads its tail and rebuilds its digest.
    let appended = store
        .append(&ids[0], "turn.completed", json!({"turnId": "t"}))
        .await
        .unwrap();
    assert_eq!(appended.sequence, 2);
    assert_eq!(
        store
            .digest(&ids[0], |digest| digest.last_sequence())
            .await
            .unwrap(),
        2
    );
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn evicted_indexes_rebuild_on_demand() {
    let root = temp_dir("todex-v3-lru");
    let store = ConversationStore::new(root.clone()).await.unwrap();
    let mut ids = Vec::new();
    for _ in 0..INDEX_CACHE_ENTRIES + 3 {
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        store
            .append(&manifest.id, "turn.started", json!({"turnId": "t"}))
            .await
            .unwrap();
        store.replay(&manifest.id, 0, 10).await.unwrap();
        ids.push(manifest.id);
    }
    assert_eq!(store.index_cache().len(), INDEX_CACHE_ENTRIES);
    assert!(store.index_get(&ids[0], |_| ()).is_none());
    let page = store.replay(&ids[0], 0, 10).await.unwrap();
    assert_eq!(page.events.len(), 1);
    // An append to an evicted conversation needs no index.
    store
        .append(&ids[1], "turn.completed", json!({"turnId": "t"}))
        .await
        .unwrap();
    assert_eq!(store.replay(&ids[1], 0, 10).await.unwrap().events.len(), 2);
    fs::remove_dir_all(root).unwrap();
}
