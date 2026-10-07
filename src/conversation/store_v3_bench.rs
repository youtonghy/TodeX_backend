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
        let mut line = encode_record(&event).unwrap();
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

fn percentiles(mut samples: Vec<f64>) -> (f64, f64, f64) {
    samples.sort_by(f64::total_cmp);
    let at = |fraction: f64| samples[((samples.len() - 1) as f64 * fraction) as usize];
    (at(0.5), at(0.95), at(1.0))
}

/// Append and replay hot paths of an encrypted store, run on the
/// single-threaded test runtime so CPU work left on the async thread shows
/// up as latency of the other conversation:
///
/// - `append`: per-append latency of a mixed history with ~2-11 KiB tool
///   output (seal, encode, write, fsync, rotations every 128 KiB);
/// - `replay`: full-history paging of a second conversation (1000-record,
///   8 MiB pages over sealed plaintext files awaiting conversion);
/// - `cross`: append latency of the first conversation while the second
///   is replayed in a loop.
///
/// ```text
/// cargo test --release --locked -- --ignored measure_store_hot_paths --nocapture
/// ```
#[tokio::test]
#[ignore = "opt-in append/replay hot path measurement"]
async fn measure_store_hot_paths() {
    let e2e = super::e2e_tests::E2e::new("todex-hot-paths", true).await;
    let writer = e2e.create(None).await.id;
    let reader = e2e.create(None).await.id;
    let mut text = TextGenerator(7);
    let mut append_ms = Vec::with_capacity(2_000);
    let start = Instant::now();
    for index in 0..2_000 {
        let event = synthetic_event(&writer, index, &mut text);
        let begin = Instant::now();
        e2e.store
            .append(&writer, event.event_type, event.payload)
            .await
            .unwrap();
        append_ms.push(begin.elapsed().as_secs_f64() * 1000.);
    }
    let append_total = start.elapsed();
    let (p50, p95, max) = percentiles(append_ms);
    eprintln!(
        "append samples=2000 total_ms={:.1} p50_ms={p50:.3} p95_ms={p95:.3} max_ms={max:.3}",
        append_total.as_secs_f64() * 1000.
    );
    for index in 0..4_000 {
        let event = synthetic_event(&reader, index, &mut text);
        e2e.store
            .append(&reader, event.event_type, event.payload)
            .await
            .unwrap();
    }
    let replay_all = |store: ConversationStore, id: String| async move {
        let mut after = 0;
        let mut pages = 0;
        loop {
            let page = store.replay(&id, after, 1_000).await.unwrap();
            after = page.next_sequence;
            pages += 1;
            if !page.has_more {
                return pages;
            }
        }
    };
    // Warm the index first; the cold build is measured elsewhere.
    replay_all(e2e.store.clone(), reader.clone()).await;
    let start = Instant::now();
    let mut pages = 0;
    for _ in 0..5 {
        pages += replay_all(e2e.store.clone(), reader.clone()).await;
    }
    eprintln!(
        "replay events=4000 rounds=5 pages={pages} total_ms={:.1}",
        start.elapsed().as_secs_f64() * 1000.
    );
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let replayer = {
        let done = done.clone();
        let store = e2e.store.clone();
        let reader = reader.clone();
        tokio::spawn(async move {
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                replay_all(store.clone(), reader.clone()).await;
            }
        })
    };
    let mut cross_ms = Vec::with_capacity(200);
    for index in 2_000..2_200 {
        let event = synthetic_event(&writer, index, &mut text);
        let begin = Instant::now();
        e2e.store
            .append(&writer, event.event_type, event.payload)
            .await
            .unwrap();
        cross_ms.push(begin.elapsed().as_secs_f64() * 1000.);
        tokio::task::yield_now().await;
    }
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    replayer.await.unwrap();
    let (p50, p95, max) = percentiles(cross_ms);
    eprintln!("cross samples=200 p50_ms={p50:.3} p95_ms={p95:.3} max_ms={max:.3}");
    e2e.cleanup();
}
