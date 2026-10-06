//! Opt-in history v3 measurements. Run in release, for example:
//!
//! ```text
//! TODEX_BENCH_DIR=/tmp/v3-bench cargo test --release --locked -- \
//!   --ignored measure_v3_build_synthetic --nocapture
//! TODEX_BENCH_DIR=/tmp/v3-bench cargo test --release --locked -- \
//!   --ignored measure_v3_open_synthetic --nocapture
//! TODEX_REAL_DATA=/tmp/copy-of-data cargo test --release --locked -- \
//!   --ignored measure_v3_real_journal_copies --nocapture
//! ```
//!
//! The open measurement runs in its own process so its RSS growth is not
//! hidden by the fixture build. `TODEX_REAL_DATA` must be a *copy* of a
//! data directory: its journals are migrated in place.

use std::fs;
use std::io::Write as _;
use std::time::Instant;

use serde_json::json;

use super::v3_tests::mixed_payload;
use super::*;
use crate::conversation::ProviderKind;

/// Current resident set size, from `ps` (macOS and Linux).
fn rss_bytes() -> u64 {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
        * 1024
}

fn bench_dir() -> PathBuf {
    PathBuf::from(std::env::var("TODEX_BENCH_DIR").expect("set TODEX_BENCH_DIR"))
}

/// Deterministic, log-like text: realistic enough that compression ratios
/// are not flattered by repetition.
struct TextGenerator(u64);

impl TextGenerator {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn lines(&mut self, bytes: usize) -> String {
        const WORDS: [&str; 24] = [
            "error",
            "warning",
            "src/conversation/store.rs",
            "fn",
            "let",
            "compiling",
            "test",
            "ok",
            "passed",
            "failed",
            "assert_eq!",
            "Result<",
            "Value",
            "await",
            "tokio",
            "serde_json",
            "journal",
            "segment",
            "-->",
            "|",
            "note:",
            "help:",
            "expected",
            "found",
        ];
        let mut text = String::with_capacity(bytes + 64);
        while text.len() < bytes {
            let words = 4 + self.next() % 9;
            for _ in 0..words {
                text.push_str(WORDS[(self.next() % WORDS.len() as u64) as usize]);
                text.push(' ');
                if self.next().is_multiple_of(4) {
                    text.push_str(&(self.next() % 100_000).to_string());
                    text.push(' ');
                }
            }
            text.push('\n');
        }
        text
    }
}

/// The synthetic event at `index` (0-based) of a ~1 KiB/event mixed
/// history: tool output carries the bulk.
fn synthetic_event(id: &str, index: u64, text: &mut TextGenerator) -> ConversationEvent {
    let (event_type, mut payload) = mixed_payload(index);
    if matches!(event_type, "tool.completed" | "tool.updated") {
        let size = if event_type == "tool.completed" {
            9_000
        } else {
            2_500
        };
        let size = size + (text.next() % 2_000) as usize;
        payload["result"] = json!(text.lines(size));
    }
    let mut event = ConversationEvent::new(id, index + 1, event_type, payload);
    event.provider = Some(ProviderKind::Codex);
    event.time =
        DateTime::from_timestamp_micros(1_700_000_000_000_000 + index as i64 * 1_000).unwrap();
    event
}

const SYNTHETIC_ID: &str = "5b0c51a4-3f43-4a7e-9a55-6f0e1d2c3b4a";

#[tokio::test]
#[ignore = "opt-in: builds a ~1 GiB synthetic journal and seals it"]
async fn measure_v3_build_synthetic() {
    let root = bench_dir();
    let _ = fs::remove_dir_all(&root);
    let count: u64 = std::env::var("TODEX_BENCH_EVENTS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1_000_000);
    let store = ConversationStore::new(root.clone()).await.unwrap();
    let directory = store.directory(SYNTHETIC_ID).unwrap();
    fs::create_dir_all(&directory).unwrap();
    // Plaintext files exactly as 64 MiB rotations leave them.
    let mut text = TextGenerator(7);
    let mut raw_total = 0u64;
    let mut file_number = 0u64;
    let mut writer: Option<std::io::BufWriter<fs::File>> = None;
    let mut written = 0u64;
    let started = Instant::now();
    let mut last = None;
    for index in 0..count {
        if writer.is_none() || written >= 64 * 1024 * 1024 {
            file_number += 1;
            let file =
                fs::File::create(directory.join(format!("events.{file_number:06}.jsonl"))).unwrap();
            writer = Some(std::io::BufWriter::with_capacity(1 << 20, file));
            written = 0;
        }
        let event = synthetic_event(SYNTHETIC_ID, index, &mut text);
        let mut line = encode_record(&event, ContentCodec::Plain).unwrap();
        line.push(b'\n');
        writer.as_mut().unwrap().write_all(&line).unwrap();
        written += line.len() as u64;
        raw_total += line.len() as u64;
        last = Some(event);
    }
    writer.unwrap().flush().unwrap();
    fs::write(directory.join(EVENTS_FILE), b"").unwrap();
    let mut manifest = ConversationManifest::new(ProviderKind::Codex, root.clone(), None, None);
    manifest.id = SYNTHETIC_ID.to_owned();
    manifest.last_sequence = count;
    manifest.updated_at = last.unwrap().time;
    manifest.storage_version = Some(super::super::model::STORAGE_VERSION);
    write_atomic_json(&directory.join(MANIFEST_FILE), &manifest)
        .await
        .unwrap();
    write_atomic_json(
        &directory.join(PROVIDER_STATE_FILE),
        &ProviderState::new(ProviderKind::Codex),
    )
    .await
    .unwrap();
    let generated = started.elapsed();
    let started = Instant::now();
    let mut segments = 0;
    while store.seal_next(SYNTHETIC_ID).await.unwrap() {
        segments += 1;
    }
    let sealing = started.elapsed();
    let sealed_bytes: u64 = journal_files(&directory)
        .await
        .unwrap()
        .iter()
        .map(|file| file.bytes)
        .sum::<u64>()
        + fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".idx"))
            .map(|entry| entry.metadata().unwrap().len())
            .sum::<u64>();
    eprintln!(
        "v3_build events={count} raw_bytes={raw_total} raw_mib={:.1} segments={segments} \
         sealed_bytes={sealed_bytes} sealed_mib={:.1} ratio={:.2} generate_s={:.1} seal_s={:.1} \
         seal_mib_per_s={:.1}",
        raw_total as f64 / 1048576.,
        sealed_bytes as f64 / 1048576.,
        raw_total as f64 / sealed_bytes as f64,
        generated.as_secs_f64(),
        sealing.as_secs_f64(),
        raw_total as f64 / 1048576. / sealing.as_secs_f64(),
    );
}

#[tokio::test]
#[ignore = "opt-in: opens the sealed synthetic journal built by measure_v3_build_synthetic"]
async fn measure_v3_open_synthetic() {
    let root = bench_dir();
    let rss_before = rss_bytes();
    let store = ConversationStore::new(root.clone()).await.unwrap();
    // Startup recovery of a settled conversation reads the tail only.
    let started = Instant::now();
    let (tail, _) = store.journal_tail(SYNTHETIC_ID).await.unwrap();
    let tail_ms = started.elapsed().as_secs_f64() * 1000.;
    let total = tail.unwrap().sequence;
    let started = Instant::now();
    let latest = store
        .replay_before(SYNTHETIC_ID, u64::MAX, 200)
        .await
        .unwrap();
    let open_ms = started.elapsed().as_secs_f64() * 1000.;
    assert_eq!(latest.events.last().unwrap().sequence, total);
    let rss_open = rss_bytes();
    let started = Instant::now();
    let digest_last = store
        .digest(SYNTHETIC_ID, |digest| digest.last_sequence())
        .await
        .unwrap();
    let digest_ms = started.elapsed().as_secs_f64() * 1000.;
    assert_eq!(digest_last, total);
    // Backward paging to page 1 (200-record pages, like clients).
    let mut cursor = latest.from_sequence;
    let mut page_ms = Vec::new();
    let mut pages = 1u64;
    let paging_started = Instant::now();
    while cursor > 0 {
        let started = Instant::now();
        let page = store
            .replay_before(SYNTHETIC_ID, cursor, 200)
            .await
            .unwrap();
        page_ms.push(started.elapsed().as_secs_f64() * 1000.);
        cursor = page.from_sequence;
        pages += 1;
    }
    let paging_s = paging_started.elapsed().as_secs_f64();
    page_ms.sort_by(f64::total_cmp);
    let rss_paged = rss_bytes();
    // Prompt saves (free-space probe plus atomic snapshot write).
    let mut save_ms = Vec::new();
    for index in 0..200 {
        let started = Instant::now();
        store
            .save_request(
                SYNTHETIC_ID,
                &json!({"request": {"prompt": "hi"}, "index": index}),
            )
            .await
            .unwrap();
        save_ms.push(started.elapsed().as_secs_f64() * 1000.);
    }
    save_ms.sort_by(f64::total_cmp);
    let percentile = |values: &[f64], p: f64| values[((values.len() - 1) as f64 * p) as usize];
    eprintln!(
        "v3_open events={total} tail_read_ms={tail_ms:.2} open_latest_page_ms={open_ms:.2} \
         digest_ms={digest_ms:.2} rss_growth_open_mib={:.1} pages={pages} paging_total_s={paging_s:.1} \
         page_p50_ms={:.2} page_p99_ms={:.2} page_max_ms={:.2} rss_growth_after_paging_mib={:.1} \
         save_request_p50_ms={:.2} save_request_p99_ms={:.2}",
        (rss_open.saturating_sub(rss_before)) as f64 / 1048576.,
        percentile(&page_ms, 0.5),
        percentile(&page_ms, 0.99),
        page_ms.last().unwrap(),
        (rss_paged.saturating_sub(rss_before)) as f64 / 1048576.,
        percentile(&save_ms, 0.5),
        percentile(&save_ms, 0.99),
    );
}

#[tokio::test]
#[ignore = "opt-in: migrates a COPY of a real data directory in place"]
async fn measure_v3_real_journal_copies() {
    let data = PathBuf::from(std::env::var("TODEX_REAL_DATA").expect("set TODEX_REAL_DATA"));
    let store = ConversationStore::new(data.clone()).await.unwrap();
    for manifest in store.list().await.unwrap() {
        let id = manifest.id.clone();
        let directory = store.directory(&id).unwrap();
        let before_bytes: u64 = journal_files(&directory)
            .await
            .unwrap()
            .iter()
            .map(|file| file.bytes)
            .sum();
        let reference = store.complete_history(&id).await.unwrap();
        let digest = store.digest(&id, Clone::clone).await.unwrap();
        // Migration requires a quiet conversation; these are copies.
        {
            let _guard = store.lock(&id).await;
            let mut quiet = store.get_unlocked(&id).await.unwrap();
            quiet.updated_at = Utc::now() - chrono::TimeDelta::hours(1);
            quiet.status = super::super::ConversationStatus::Idle;
            store.persist_manifest_locked(&quiet).await.unwrap();
        }
        let started = Instant::now();
        let migrated = store.migrate_conversation(&id).await.unwrap();
        let migrate_s = started.elapsed().as_secs_f64();
        let after_files = journal_files(&directory).await.unwrap();
        let idx_bytes: u64 = fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".idx"))
            .map(|entry| entry.metadata().unwrap().len())
            .sum();
        let after_bytes: u64 = after_files.iter().map(|file| file.bytes).sum::<u64>() + idx_bytes;
        let cold = ConversationStore::new(data.clone()).await.unwrap();
        let started = Instant::now();
        let latest = cold.replay_before(&id, u64::MAX, 200).await.unwrap();
        let open_ms = started.elapsed().as_secs_f64() * 1000.;
        let history = cold.complete_history(&id).await.unwrap();
        assert_eq!(history.len(), reference.len());
        let mut slimmed = 0usize;
        for (after, before) in history.iter().zip(&reference) {
            assert_eq!(after.event_id, before.event_id);
            if after.event_type == JOURNAL_COMPACTED_EVENT
                && before.event_type != JOURNAL_COMPACTED_EVENT
            {
                slimmed += 1;
            } else {
                assert_eq!(
                    serde_json::to_value(after).unwrap(),
                    serde_json::to_value(before).unwrap()
                );
            }
        }
        assert_eq!(cold.digest(&id, Clone::clone).await.unwrap(), digest);
        eprintln!(
            "v3_real conversation={id} provider={} events={} migrated={migrated} before_mib={:.1} \
             after_mib={:.2} ratio={:.1} slimmed={slimmed} migrate_s={migrate_s:.1} \
             cold_latest_page_ms={open_ms:.2} latest_page_events={}",
            manifest.provider.as_str(),
            reference.len(),
            before_bytes as f64 / 1048576.,
            after_bytes as f64 / 1048576.,
            before_bytes as f64 / after_bytes as f64,
            latest.events.len(),
        );
    }
}
