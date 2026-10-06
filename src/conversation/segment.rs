//! Sealed journal segments (history v3, `docs/history-encryption.md` §4.1,
//! §4.3, §6).
//!
//! A sealed segment is a pair of files:
//!
//! - `events.NNNNNN.seg`: a 24-byte header (`TDXSEG1\n` plus a random
//!   16-byte segment id) followed by frames. Each frame is a 32-byte header
//!   (`TDXF`, stream, `kid` length, first sequence, record count, raw and
//!   stored length, frame ordinal, CRC-32 of the raw data), the optional
//!   `kid`, and the frame data compressed as raw DEFLATE (RFC 1951, no
//!   zlib/gzip header, so browsers can inflate it) — in e2e mode sealed by
//!   [`ContentCodec::seal_frame`] after compression. Frame headers make the
//!   file self-describing, so a lost `.idx` can be rebuilt from it.
//! - `events.NNNNNN.idx`: JSON `{"sha256", "body"}` where `sha256` covers
//!   the raw `body` text. The body holds the first/last sequence, the frame
//!   table, the segment's [`JournalDigest`], the `.seg` length, id and
//!   SHA-256, and the plaintext files the segment replaced (`sources`).
//!
//! There are three streams, each cut into frames of about
//! [`FRAME_RAW_BYTES`] raw bytes that never span two `kid`s:
//! the envelope stream (one [`super::record`] line per record without its
//! content, never encrypted), the summary content stream (a JSON array of
//! [`summarize_event`] payloads, so `detail=summary` can read it alone) and
//! the full content stream (a JSON array of payloads). Stream numbers 3
//! and 4 are the frame streams of the history crypto AAD (§2).
//!
//! Sealing also slims history (§4.3): streaming progress records whose
//! terminal record is in the same segment become `journal.compacted`
//! markers; see [`SlimPlan`] for the exact rule. Sequences stay dense.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::digest::JournalDigest;
use super::record::{
    compacted_marker, decode_journal_record, encode_envelope, parse_record, record_bytes,
    ContentCodec, JOURNAL_COMPACTED_EVENT,
};
use super::{summarize_event, ConversationEvent, ProviderKind};

const SEGMENT_MAGIC: &[u8; 8] = b"TDXSEG1\n";
const SEGMENT_ID_BYTES: usize = 16;
const SEGMENT_HEADER_BYTES: u64 = (SEGMENT_MAGIC.len() + SEGMENT_ID_BYTES) as u64;
const FRAME_MAGIC: &[u8; 4] = b"TDXF";
const FRAME_HEADER_BYTES: usize = 36;
/// Raw bytes a frame collects before it is compressed. Reading one record
/// decompresses at most this much per stream.
pub(super) const FRAME_RAW_BYTES: usize = 1024 * 1024;
/// DEFLATE level. Frames are compressed once, in the background, and read
/// many times: on real 64 MiB journals level 9 sealed to 4.0–5.9 MiB
/// against 4.1–6.5 MiB at level 6, for about 1–2 s per segment.
const DEFLATE_LEVEL: u32 = 9;
const INDEX_VERSION: u32 = 1;
/// Placeholder for sequences no readable record holds; see the store.
pub(super) const JOURNAL_RECORD_LOST_EVENT: &str = "journal.recordLost";

pub(super) const STREAM_ENVELOPE: u8 = 0;
pub(super) const STREAM_SUMMARY: u8 = 3;
pub(super) const STREAM_FULL: u8 = 4;
const STREAMS: [u8; 3] = [STREAM_ENVELOPE, STREAM_SUMMARY, STREAM_FULL];

/// `events.NNNNNN.seg` / `.idx` names of segment `number`.
pub(super) fn segment_name(number: u64) -> String {
    format!("events.{number:06}.seg")
}

pub(super) fn index_name(number: u64) -> String {
    format!("events.{number:06}.idx")
}

/// `events.NNNNNN.<extension>` → `NNNNNN`.
pub(super) fn numbered(name: &str, extension: &str) -> Option<u64> {
    let digits = name.strip_prefix("events.")?.strip_suffix(extension)?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[derive(Debug)]
pub(super) enum SegmentError {
    Io(std::io::Error),
    /// The files exist but do not hold a valid segment.
    Invalid(String),
}

impl std::fmt::Display for SegmentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Invalid(reason) => formatter.write_str(reason),
        }
    }
}

impl From<std::io::Error> for SegmentError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for SegmentError {
    fn from(error: serde_json::Error) -> Self {
        Self::Invalid(error.to_string())
    }
}

fn invalid(reason: impl Into<String>) -> SegmentError {
    SegmentError::Invalid(reason.into())
}

/// One frame of a `.seg` file, as listed in its `.idx`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(super) struct FrameEntry {
    #[serde(rename = "st")]
    pub stream: u8,
    /// Sequence of the frame's first record.
    #[serde(rename = "s")]
    pub first: u64,
    #[serde(rename = "n")]
    pub count: u32,
    /// Offset of the frame data (after its header) in the `.seg` file.
    #[serde(rename = "o")]
    pub offset: u64,
    /// Stored (compressed, and in e2e sealed) length.
    #[serde(rename = "l")]
    pub len: u32,
    /// Decompressed length.
    #[serde(rename = "u")]
    pub raw: u32,
    #[serde(rename = "k", default, skip_serializing_if = "Option::is_none")]
    pub kid: Option<String>,
    /// Frame number under `(kid, stream)`: the AEAD counter in e2e mode.
    #[serde(rename = "f")]
    pub ordinal: u32,
    /// CRC-32 of the raw (decompressed) frame; DEFLATE has no checksum of
    /// its own.
    #[serde(rename = "c")]
    pub crc: u32,
}

/// The checksummed body of an `.idx` file.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SegmentIndexBody {
    pub version: u32,
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub segment_id: String,
    pub seg_bytes: u64,
    pub seg_sha256: String,
    /// Plaintext journal files this segment replaced; recovery deletes any
    /// that a crash left behind once the segment verifies.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Records seal-time slimming replaced with markers.
    #[serde(default)]
    pub slimmed: u64,
    pub frames: Vec<FrameEntry>,
    pub digest: JournalDigest,
}

#[derive(Serialize)]
struct IndexFileOut<'a> {
    sha256: String,
    body: &'a RawValue,
}

#[derive(Deserialize)]
struct IndexFileIn<'a> {
    sha256: String,
    #[serde(borrow)]
    body: &'a RawValue,
}

/// A sealed segment as the replay index keeps it: its sequence range and
/// frame table. The digest is not kept here; it is merged into the
/// conversation digest when that is built.
#[derive(Debug)]
pub(super) struct SealedSegment {
    pub number: u64,
    pub first: u64,
    pub last: u64,
    /// Identifies the `.seg` content; frame cache keys use it.
    pub segment_id: String,
    frames: Vec<FrameEntry>,
    /// Indexes into `frames` per stream, in sequence order.
    by_stream: [Vec<usize>; 3],
}

fn stream_slot(stream: u8) -> Option<usize> {
    STREAMS.iter().position(|candidate| *candidate == stream)
}

impl SealedSegment {
    pub(super) fn from_body(number: u64, body: &SegmentIndexBody) -> Result<Self, SegmentError> {
        if body.version != INDEX_VERSION {
            return Err(invalid(format!(
                "segment index version {} is not supported",
                body.version
            )));
        }
        if body.first_sequence == 0 || body.last_sequence < body.first_sequence {
            return Err(invalid("segment index has an invalid sequence range"));
        }
        let mut by_stream: [Vec<usize>; 3] = Default::default();
        for (index, frame) in body.frames.iter().enumerate() {
            let slot = stream_slot(frame.stream)
                .ok_or_else(|| invalid(format!("unknown frame stream {}", frame.stream)))?;
            let end = frame.offset.checked_add(u64::from(frame.len));
            if frame.count == 0
                || frame.offset < SEGMENT_HEADER_BYTES + FRAME_HEADER_BYTES as u64
                || end.is_none_or(|end| end > body.seg_bytes)
            {
                return Err(invalid("segment index lists a frame outside its file"));
            }
            by_stream[slot].push(index);
        }
        for indexes in &mut by_stream {
            indexes.sort_by_key(|index| body.frames[*index].first);
            let mut expected = body.first_sequence;
            for index in indexes.iter() {
                let frame = &body.frames[*index];
                if frame.first != expected {
                    return Err(invalid("segment index frames are not contiguous"));
                }
                expected += u64::from(frame.count);
            }
            if expected != body.last_sequence + 1 {
                return Err(invalid("segment index frames do not cover the segment"));
            }
        }
        Ok(Self {
            number,
            first: body.first_sequence,
            last: body.last_sequence,
            segment_id: body.segment_id.clone(),
            frames: body.frames.clone(),
            by_stream,
        })
    }

    pub fn count(&self) -> u64 {
        self.last - self.first + 1
    }

    pub fn file_name(&self) -> String {
        segment_name(self.number)
    }

    /// The frame of `stream` holding `sequence`.
    fn frame(&self, stream: u8, sequence: u64) -> Option<&FrameEntry> {
        let indexes = &self.by_stream[stream_slot(stream)?];
        let position = indexes
            .partition_point(|index| self.frames[*index].first <= sequence)
            .checked_sub(1)?;
        let frame = &self.frames[indexes[position]];
        (sequence < frame.first + u64::from(frame.count)).then_some(frame)
    }
}

/// An `.idx` that parsed, checksummed and matched its `.seg`.
pub(super) struct LoadedIndex {
    pub segment: SealedSegment,
    pub body: SegmentIndexBody,
}

/// Read and check `events.NNNNNN.idx` against its `.seg`: the index
/// checksum, the frame table, and the `.seg` length and header id. The
/// `.seg` content itself is only hashed by [`verify_segment_file`] (crash
/// recovery) — every frame read verifies the frame's CRC-32.
pub(super) fn load_index(directory: &Path, number: u64) -> Result<LoadedIndex, SegmentError> {
    let raw = std::fs::read(directory.join(index_name(number)))?;
    let file: IndexFileIn<'_> = serde_json::from_slice(&raw)?;
    if hex(&Sha256::digest(file.body.get().as_bytes())) != file.sha256 {
        return Err(invalid("segment index checksum mismatch"));
    }
    let body: SegmentIndexBody = serde_json::from_str(file.body.get())?;
    let segment = SealedSegment::from_body(number, &body)?;
    let mut seg = std::fs::File::open(directory.join(segment_name(number)))?;
    if seg.metadata()?.len() != body.seg_bytes {
        return Err(invalid("segment file length does not match its index"));
    }
    let mut header = [0u8; SEGMENT_HEADER_BYTES as usize];
    seg.read_exact(&mut header)?;
    if &header[..SEGMENT_MAGIC.len()] != SEGMENT_MAGIC
        || hex(&header[SEGMENT_MAGIC.len()..]) != body.segment_id
    {
        return Err(invalid("segment file does not belong to its index"));
    }
    Ok(LoadedIndex { segment, body })
}

/// Hash the whole `.seg` and compare it with its index.
pub(super) fn verify_segment_file(path: &Path, body: &SegmentIndexBody) -> std::io::Result<bool> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok(total == body.seg_bytes && hex(&hasher.finalize()) == body.seg_sha256)
}

/// Raw DEFLATE (RFC 1951) of a frame.
fn deflate(raw: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = flate2::write::DeflateEncoder::new(
        Vec::with_capacity(raw.len() / 4),
        flate2::Compression::new(DEFLATE_LEVEL),
    );
    encoder.write_all(raw)?;
    encoder.finish()
}

/// Inflate a raw DEFLATE frame that must hold exactly `raw` bytes.
fn inflate(compressed: &[u8], raw: usize) -> Result<Vec<u8>, SegmentError> {
    let mut output = Vec::with_capacity(raw);
    // One byte beyond the expected length detects an overlong stream
    // without inflating an arbitrarily large one.
    flate2::read::DeflateDecoder::new(compressed)
        .take(raw as u64 + 1)
        .read_to_end(&mut output)
        .map_err(|error| invalid(format!("frame does not decompress: {error}")))?;
    if output.len() != raw {
        return Err(invalid("frame decompressed to an unexpected length"));
    }
    Ok(output)
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(bytes);
    crc.sum()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A decompressed frame with the byte range of each record in it.
pub(super) struct DecodedFrame {
    bytes: Vec<u8>,
    items: Vec<(u32, u32)>,
}

impl DecodedFrame {
    fn item(&self, index: usize) -> Option<&[u8]> {
        let (start, end) = *self.items.get(index)?;
        self.bytes.get(start as usize..end as usize)
    }

    fn cost(&self) -> usize {
        self.bytes.len() + self.items.len() * std::mem::size_of::<(u32, u32)>()
    }
}

fn decode_frame(stream: u8, bytes: Vec<u8>, count: u32) -> Result<DecodedFrame, SegmentError> {
    let mut items = Vec::with_capacity(count as usize);
    if stream == STREAM_ENVELOPE {
        let mut start = 0usize;
        for (index, byte) in bytes.iter().enumerate() {
            if *byte == b'\n' {
                items.push((start as u32, index as u32));
                start = index + 1;
            }
        }
    } else {
        let base = bytes.as_ptr() as usize;
        let array: Vec<&RawValue> = serde_json::from_slice(&bytes)?;
        for item in array {
            let start = item.get().as_ptr() as usize - base;
            items.push((start as u32, (start + item.get().len()) as u32));
        }
    }
    if items.len() != count as usize {
        return Err(invalid("frame record count does not match its index"));
    }
    Ok(DecodedFrame { bytes, items })
}

/// Read, open and decompress one frame.
fn load_frame(
    file: &mut std::fs::File,
    entry: &FrameEntry,
    codec: ContentCodec,
) -> Result<DecodedFrame, SegmentError> {
    let mut stored = vec![0u8; entry.len as usize];
    file.seek(SeekFrom::Start(entry.offset))?;
    file.read_exact(&mut stored)?;
    let compressed = codec.open_frame(entry.stream, entry.kid.as_deref(), entry.ordinal, stored)?;
    let raw = inflate(&compressed, entry.raw as usize)?;
    if crc32(&raw) != entry.crc {
        return Err(invalid("frame checksum mismatch"));
    }
    decode_frame(entry.stream, raw, entry.count)
}

/// Process-wide LRU of decompressed frames, bounded by bytes. Keys are the
/// segment id and frame offset, so a rewritten segment never hits stale
/// entries.
pub(super) struct FrameCache {
    inner: Mutex<FrameCacheInner>,
}

struct FrameCacheInner {
    frames: LruCache<(String, u64), Arc<DecodedFrame>>,
    bytes: usize,
    budget: usize,
}

impl FrameCache {
    pub fn new(budget: usize) -> Self {
        Self {
            inner: Mutex::new(FrameCacheInner {
                frames: LruCache::unbounded(),
                bytes: 0,
                budget,
            }),
        }
    }

    fn get_or_load(
        &self,
        key: (String, u64),
        load: impl FnOnce() -> Result<DecodedFrame, SegmentError>,
    ) -> Result<Arc<DecodedFrame>, SegmentError> {
        if let Some(frame) = self.lock().frames.get(&key) {
            return Ok(frame.clone());
        }
        // Decompress without the lock; a concurrent load of the same frame
        // only wastes work.
        let frame = Arc::new(load()?);
        let mut inner = self.lock();
        let cost = frame.cost();
        if cost <= inner.budget {
            if let Some(previous) = inner.frames.put(key, frame.clone()) {
                inner.bytes -= previous.cost();
            }
            inner.bytes += cost;
            while inner.bytes > inner.budget {
                let Some((_, evicted)) = inner.frames.pop_lru() else {
                    break;
                };
                inner.bytes -= evicted.cost();
            }
        }
        Ok(frame)
    }

    #[cfg(test)]
    pub fn cached_bytes(&self) -> usize {
        self.lock().bytes
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FrameCacheInner> {
        // A panic while holding the lock leaves only cache bookkeeping
        // behind; the cache stays usable.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Reads records of one sealed segment, keeping the current envelope and
/// content frame at hand so consecutive records decompress each frame once.
pub(super) struct SegmentReader<'a> {
    segment: &'a SealedSegment,
    file: std::fs::File,
    cache: &'a FrameCache,
    codec: ContentCodec,
    conversation_id: &'a str,
    current: [Option<(u64, Arc<DecodedFrame>)>; 2],
}

impl<'a> SegmentReader<'a> {
    pub fn open(
        directory: &Path,
        segment: &'a SealedSegment,
        cache: &'a FrameCache,
        codec: ContentCodec,
        conversation_id: &'a str,
    ) -> Result<Self, SegmentError> {
        Ok(Self {
            file: std::fs::File::open(directory.join(segment.file_name()))?,
            segment,
            cache,
            codec,
            conversation_id,
            current: [None, None],
        })
    }

    fn frame_item(&mut self, stream: u8, sequence: u64) -> Result<Vec<u8>, SegmentError> {
        let slot = usize::from(stream != STREAM_ENVELOPE);
        let entry = self
            .segment
            .frame(stream, sequence)
            .ok_or_else(|| invalid(format!("no frame holds sequence {sequence}")))?
            .clone();
        let current = match &self.current[slot] {
            Some((offset, frame)) if *offset == entry.offset => frame.clone(),
            _ => {
                let file = &mut self.file;
                let codec = self.codec;
                let frame = self
                    .cache
                    .get_or_load((self.segment.segment_id.clone(), entry.offset), || {
                        load_frame(file, &entry, codec)
                    })?;
                self.current[slot] = Some((entry.offset, frame.clone()));
                frame
            }
        };
        current
            .item((sequence - entry.first) as usize)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| invalid(format!("frame lacks sequence {sequence}")))
    }

    /// The record with `sequence` and its journal bytes for page budgets.
    pub fn event(&mut self, sequence: u64) -> Result<(ConversationEvent, u64), SegmentError> {
        let envelope = self.frame_item(STREAM_ENVELOPE, sequence)?;
        let content = self.frame_item(STREAM_FULL, sequence)?;
        let record = parse_record(&envelope)?;
        if record.sequence() != sequence {
            return Err(invalid(format!(
                "envelope frame holds sequence {} where {sequence} belongs",
                record.sequence()
            )));
        }
        let payload: Value = serde_json::from_slice(&content)?;
        let event = record
            .into_event(self.conversation_id, Some(payload))
            .map_err(invalid)?;
        Ok((event, record_bytes(envelope.len(), content.len())))
    }
}

/// Seal-time slimming (§4.3). A streaming progress record is replaced by a
/// `journal.compacted` marker only when a terminal record for the same
/// stream comes later *in the same segment*; matching is by id, never by
/// position or text:
///
/// - `message.delta` with a `block.id`: covered by a later
///   `message.completed` with the same `block.id` (Codex) or listing it in
///   `block.supersedes` (Pi final answers). Deltas without a block id —
///   Claude Code and ACP text, which have no completed record carrying the
///   text by id — and Pi tool-use narration (never superseded) are kept.
/// - `thought.delta`: always kept.
/// - `tool.updated`: covered by a later `tool.completed`/`tool.failed` with
///   the same `toolCallId` or `block.id`, or by a later `tool.updated` of
///   the same `toolCallId` whose `status` is `completed`/`failed` (ACP);
///   an update carrying such a terminal status is itself always kept.
/// - `subagent.updated`: covered by a later `subagent.completed`/`failed`/
///   `cancelled` with the same `subagentId`.
///
/// Message and tool ids are scoped by the record's `turnId` (payload, else
/// `block.turnId`), since fallback ids such as Codex's `current` repeat
/// across turns; subagent ids are unique and outlive turns.
#[derive(Default)]
pub(super) struct SlimPlan {
    messages: HashMap<(Option<String>, String), u64>,
    tools: HashMap<(Option<String>, String), u64>,
    subagents: HashMap<String, u64>,
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn turn_of(payload: &Value) -> Option<String> {
    text(payload, "turnId")
        .or_else(|| payload.get("block").and_then(|block| text(block, "turnId")))
        .map(str::to_owned)
}

fn terminal_tool_status(payload: &Value) -> bool {
    matches!(text(payload, "status"), Some("completed" | "failed"))
}

impl SlimPlan {
    pub fn observe(&mut self, event: &ConversationEvent) {
        let payload = &event.payload;
        let mark = |map: &mut HashMap<(Option<String>, String), u64>, id: Option<&str>| {
            if let Some(id) = id {
                let slot = map.entry((turn_of(payload), id.to_owned())).or_default();
                *slot = (*slot).max(event.sequence);
            }
        };
        let block = payload.get("block");
        let block_id = block.and_then(|block| text(block, "id"));
        match event.event_type.as_str() {
            "message.completed" => {
                mark(&mut self.messages, block_id);
                let superseded = block
                    .and_then(|block| block.get("supersedes"))
                    .and_then(Value::as_array);
                for id in superseded.into_iter().flatten() {
                    mark(&mut self.messages, id.as_str());
                }
            }
            "tool.completed" | "tool.failed" => {
                mark(&mut self.tools, block_id);
                mark(&mut self.tools, text(payload, "toolCallId"));
            }
            "tool.updated" if terminal_tool_status(payload) => {
                mark(&mut self.tools, text(payload, "toolCallId"));
            }
            "subagent.completed" | "subagent.failed" | "subagent.cancelled" => {
                if let Some(id) = text(payload, "subagentId") {
                    let slot = self.subagents.entry(id.to_owned()).or_default();
                    *slot = (*slot).max(event.sequence);
                }
            }
            _ => {}
        }
    }

    pub fn strips(&self, event: &ConversationEvent) -> bool {
        let payload = &event.payload;
        let later = |terminal: Option<&u64>| terminal.is_some_and(|seq| *seq > event.sequence);
        let scoped = |map: &HashMap<(Option<String>, String), u64>, id: Option<&str>| {
            id.is_some_and(|id| later(map.get(&(turn_of(payload), id.to_owned()))))
        };
        let block_id = payload.get("block").and_then(|block| text(block, "id"));
        match event.event_type.as_str() {
            "message.delta" => scoped(&self.messages, block_id),
            "tool.updated" => {
                !terminal_tool_status(payload)
                    && (scoped(&self.tools, text(payload, "toolCallId"))
                        || scoped(&self.tools, block_id))
            }
            "subagent.updated" => {
                text(payload, "subagentId").is_some_and(|id| later(self.subagents.get(id)))
            }
            _ => false,
        }
    }
}

/// Output of a segment build: the temporary `.seg`/`.idx` (durable) and the
/// index body they describe.
pub(super) struct PreparedSegment {
    pub number: u64,
    pub temp_seg: PathBuf,
    pub temp_idx: PathBuf,
    pub body: SegmentIndexBody,
}

impl PreparedSegment {
    pub fn discard(&self) {
        let _ = std::fs::remove_file(&self.temp_seg);
        let _ = std::fs::remove_file(&self.temp_idx);
    }
}

/// Prefix of the temporary files a segment build writes; recovery removes
/// leftovers when no build is running.
pub(super) const SEAL_TEMP_PREFIX: &str = ".seal.";

struct StreamBuffer {
    stream: u8,
    kid: Option<String>,
    first: u64,
    count: u32,
    raw: Vec<u8>,
}

/// Streams records into a new `.seg` (blocking I/O; run off the runtime).
pub(super) struct SegmentBuilder {
    number: u64,
    temp_seg: PathBuf,
    temp_idx: PathBuf,
    writer: BufWriter<std::fs::File>,
    hasher: Sha256,
    offset: u64,
    segment_id: String,
    frames: Vec<FrameEntry>,
    buffers: [StreamBuffer; 3],
    ordinals: HashMap<(Option<String>, u8), u32>,
    codec: ContentCodec,
    digest: JournalDigest,
    first: Option<u64>,
    last: u64,
    slimmed: u64,
}

impl SegmentBuilder {
    pub fn create(
        directory: &Path,
        number: u64,
        codec: ContentCodec,
    ) -> Result<Self, SegmentError> {
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let temp_seg = directory.join(format!("{SEAL_TEMP_PREFIX}{tag}.seg.tmp"));
        let temp_idx = directory.join(format!("{SEAL_TEMP_PREFIX}{tag}.idx.tmp"));
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_seg)?;
        set_owner_only_blocking(&temp_seg)?;
        let id: [u8; SEGMENT_ID_BYTES] = *uuid::Uuid::new_v4().as_bytes();
        let mut builder = Self {
            number,
            temp_seg,
            temp_idx,
            writer: BufWriter::with_capacity(256 * 1024, file),
            hasher: Sha256::new(),
            offset: 0,
            segment_id: hex(&id),
            frames: Vec::new(),
            buffers: STREAMS.map(|stream| StreamBuffer {
                stream,
                kid: None,
                first: 0,
                count: 0,
                raw: Vec::new(),
            }),
            ordinals: HashMap::new(),
            codec,
            digest: JournalDigest::default(),
            first: None,
            last: 0,
            slimmed: 0,
        };
        builder.write(SEGMENT_MAGIC)?;
        builder.write(&id)?;
        Ok(builder)
    }

    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)?;
        self.hasher.update(bytes);
        self.offset += bytes.len() as u64;
        Ok(())
    }

    /// Fold `original` into the segment digest. Called for every record in
    /// order, with the record as it was before any slimming.
    pub fn observe(&mut self, original: &ConversationEvent) {
        self.digest.apply(original);
    }

    /// Append the stored form of the next record.
    pub fn push(&mut self, stored: &ConversationEvent) -> Result<(), SegmentError> {
        if self.first.is_none() {
            self.first = Some(stored.sequence);
        } else if stored.sequence != self.last + 1 {
            return Err(invalid(format!(
                "segment record {} does not follow {}",
                stored.sequence, self.last
            )));
        }
        self.last = stored.sequence;
        if stored.event_type == JOURNAL_COMPACTED_EVENT {
            self.slimmed += 1;
        }
        let kid = self.codec.kid().map(str::to_owned);
        let mut envelope = encode_envelope(stored)?;
        envelope.push(b'\n');
        let full = serde_json::to_vec(&stored.payload)?;
        let mut summarized = stored.clone();
        let summary = if summarize_event(&mut summarized) {
            serde_json::to_vec(&summarized.payload)?
        } else {
            full.clone()
        };
        self.append(0, stored.sequence, &kid, &envelope)?;
        self.append(1, stored.sequence, &kid, &summary)?;
        self.append(2, stored.sequence, &kid, &full)?;
        Ok(())
    }

    fn append(
        &mut self,
        slot: usize,
        sequence: u64,
        kid: &Option<String>,
        item: &[u8],
    ) -> Result<(), SegmentError> {
        let buffer = &self.buffers[slot];
        if buffer.count > 0
            && (buffer.raw.len() + item.len() > FRAME_RAW_BYTES || buffer.kid != *kid)
        {
            self.flush(slot)?;
        }
        let buffer = &mut self.buffers[slot];
        if buffer.count == 0 {
            buffer.first = sequence;
            buffer.kid = kid.clone();
            if buffer.stream != STREAM_ENVELOPE {
                buffer.raw.push(b'[');
            }
        } else if buffer.stream != STREAM_ENVELOPE {
            buffer.raw.push(b',');
        }
        buffer.raw.extend_from_slice(item);
        buffer.count += 1;
        Ok(())
    }

    fn flush(&mut self, slot: usize) -> Result<(), SegmentError> {
        let mut buffer = std::mem::replace(
            &mut self.buffers[slot],
            StreamBuffer {
                stream: STREAMS[slot],
                kid: None,
                first: 0,
                count: 0,
                raw: Vec::new(),
            },
        );
        if buffer.count == 0 {
            return Ok(());
        }
        if buffer.stream != STREAM_ENVELOPE {
            buffer.raw.push(b']');
        }
        let compressed = deflate(&buffer.raw)?;
        let crc = crc32(&buffer.raw);
        let ordinal = {
            let slot = self
                .ordinals
                .entry((buffer.kid.clone(), buffer.stream))
                .or_default();
            let ordinal = *slot;
            *slot += 1;
            ordinal
        };
        let stored = self.codec.seal_frame(buffer.stream, ordinal, compressed);
        let kid_bytes = buffer
            .kid
            .as_deref()
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        if kid_bytes.len() > usize::from(u8::MAX) {
            return Err(invalid("frame key id is too long"));
        }
        let mut header = Vec::with_capacity(FRAME_HEADER_BYTES + kid_bytes.len());
        header.extend_from_slice(FRAME_MAGIC);
        header.push(buffer.stream);
        header.push(kid_bytes.len() as u8);
        header.extend_from_slice(&[0, 0]);
        header.extend_from_slice(&buffer.first.to_le_bytes());
        header.extend_from_slice(&buffer.count.to_le_bytes());
        header.extend_from_slice(&(buffer.raw.len() as u32).to_le_bytes());
        header.extend_from_slice(&(stored.len() as u32).to_le_bytes());
        header.extend_from_slice(&ordinal.to_le_bytes());
        header.extend_from_slice(&crc.to_le_bytes());
        header.extend_from_slice(&kid_bytes);
        self.write(&header)?;
        let offset = self.offset;
        self.write(&stored)?;
        self.frames.push(FrameEntry {
            stream: buffer.stream,
            first: buffer.first,
            count: buffer.count,
            offset,
            len: stored.len() as u32,
            raw: buffer.raw.len() as u32,
            kid: buffer.kid,
            ordinal,
            crc,
        });
        Ok(())
    }

    /// Flush the last frames, make the `.seg` and `.idx` temporaries
    /// durable and return what they describe.
    pub fn finish(mut self, sources: Vec<String>) -> Result<PreparedSegment, SegmentError> {
        let Some(first) = self.first else {
            self.abandon();
            return Err(invalid("a segment needs at least one record"));
        };
        for slot in 0..STREAMS.len() {
            self.flush(slot)?;
        }
        self.writer.flush()?;
        let file = self
            .writer
            .into_inner()
            .map_err(|error| SegmentError::Io(error.into_error()))?;
        file.sync_all()?;
        drop(file);
        let body = SegmentIndexBody {
            version: INDEX_VERSION,
            first_sequence: first,
            last_sequence: self.last,
            segment_id: self.segment_id,
            seg_bytes: self.offset,
            seg_sha256: hex(&self.hasher.finalize()),
            sources,
            slimmed: self.slimmed,
            frames: self.frames,
            digest: self.digest,
        };
        write_index_file(&self.temp_idx, &body)?;
        Ok(PreparedSegment {
            number: self.number,
            temp_seg: self.temp_seg,
            temp_idx: self.temp_idx,
            body,
        })
    }

    pub fn abandon(self) {
        let _ = std::fs::remove_file(&self.temp_seg);
        let _ = std::fs::remove_file(&self.temp_idx);
    }
}

fn write_index_file(path: &Path, body: &SegmentIndexBody) -> Result<(), SegmentError> {
    let body_json = serde_json::to_string(body)?;
    let raw = RawValue::from_string(body_json)?;
    let bytes = serde_json::to_vec(&IndexFileOut {
        sha256: hex(&Sha256::digest(raw.get().as_bytes())),
        body: &raw,
    })?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    set_owner_only_blocking(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

pub(super) fn set_owner_only_blocking(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Each record line of the plaintext journal files `paths`, in order,
/// decoded and checked to continue from `expected_first`.
fn for_each_plain_record(
    paths: &[PathBuf],
    conversation_id: &str,
    expected_first: u64,
    mut visit: impl FnMut(ConversationEvent) -> Result<(), SegmentError>,
) -> Result<u64, SegmentError> {
    let mut expected = expected_first;
    let mut line = Vec::new();
    for path in paths {
        let mut reader = BufReader::with_capacity(256 * 1024, std::fs::File::open(path)?);
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            let trimmed = line.trim_ascii();
            if trimmed.is_empty() {
                continue;
            }
            let event = decode_journal_record(trimmed, conversation_id).map_err(|error| {
                invalid(format!(
                    "journal record after {} is corrupt: {error}",
                    expected - 1
                ))
            })?;
            if event.sequence != expected || event.conversation_id != conversation_id {
                return Err(invalid(format!(
                    "journal record {} breaks continuity at {expected}",
                    event.sequence
                )));
            }
            expected += 1;
            visit(event)?;
        }
    }
    Ok(expected)
}

/// Build segment `number` from the sealed plaintext files `sources`
/// (consecutive, in order), slimming covered streaming records. Two passes
/// keep memory bounded: the first collects the terminal ids, the second
/// writes frames. Blocking.
pub(super) fn build_from_plain(
    directory: &Path,
    conversation_id: &str,
    number: u64,
    sources: &[String],
    expected_first: u64,
    codec: ContentCodec,
) -> Result<PreparedSegment, SegmentError> {
    let paths: Vec<PathBuf> = sources.iter().map(|name| directory.join(name)).collect();
    let mut plan = SlimPlan::default();
    for_each_plain_record(&paths, conversation_id, expected_first, |event| {
        plan.observe(&event);
        Ok(())
    })?;
    let mut builder = SegmentBuilder::create(directory, number, codec)?;
    // Stripped records of the open run: written as markers once the run's
    // length is known. Only ids and times are held.
    let mut run: Vec<ConversationEvent> = Vec::new();
    let flush_run = |builder: &mut SegmentBuilder,
                     run: &mut Vec<ConversationEvent>|
     -> Result<(), SegmentError> {
        let Some(start) = run.first().map(|event| event.sequence) else {
            return Ok(());
        };
        let length = run.len() as u64;
        for original in run.drain(..) {
            builder.push(&compacted_marker(&original, start, length))?;
        }
        Ok(())
    };
    let built = for_each_plain_record(&paths, conversation_id, expected_first, |event| {
        builder.observe(&event);
        if plan.strips(&event) {
            let mut slim = event;
            slim.payload = Value::Null;
            run.push(slim);
            return Ok(());
        }
        flush_run(&mut builder, &mut run)?;
        builder.push(&event)
    })
    .and_then(|_| flush_run(&mut builder, &mut run));
    match built {
        Ok(()) => builder.finish(sources.to_vec()),
        Err(error) => {
            builder.abandon();
            Err(error)
        }
    }
}

/// Atomically publish a prepared segment (§4.1): `.idx` then `.seg` are
/// renamed into place, then the plaintext sources are deleted and the
/// directory synced. `step` lets crash tests stop after any step. Blocking;
/// callers hold the conversation lock.
pub(super) fn commit_prepared(
    directory: &Path,
    prepared: &PreparedSegment,
    stop_after: Option<CommitStep>,
) -> Result<(), SegmentError> {
    let stop = |step: CommitStep| stop_after == Some(step);
    if stop(CommitStep::Written) {
        return Ok(());
    }
    std::fs::rename(
        &prepared.temp_idx,
        directory.join(index_name(prepared.number)),
    )?;
    if stop(CommitStep::IndexRenamed) {
        return Ok(());
    }
    std::fs::rename(
        &prepared.temp_seg,
        directory.join(segment_name(prepared.number)),
    )?;
    if stop(CommitStep::SegmentRenamed) {
        return Ok(());
    }
    for (index, source) in prepared.body.sources.iter().enumerate() {
        match std::fs::remove_file(directory.join(source)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if index == 0 && stop(CommitStep::FirstSourceRemoved) {
            return Ok(());
        }
    }
    sync_directory_blocking(directory)?;
    Ok(())
}

/// Step boundaries of [`commit_prepared`], for crash-injection tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CommitStep {
    /// Temporaries durable, nothing renamed.
    Written,
    IndexRenamed,
    SegmentRenamed,
    /// One source deleted (the others and the directory sync pending).
    FirstSourceRemoved,
}

pub(super) fn sync_directory_blocking(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// What crash recovery did to one directory.
#[derive(Debug, Default)]
pub(super) struct Reconciled {
    pub changed: bool,
}

/// Restore the §4.1 invariants after a crash. For every `.seg`/`.idx`:
/// an `.idx` without its `.seg` is removed; a pair whose plaintext sources
/// still exist is kept (and the sources removed) only when the whole `.seg`
/// hashes to its index, otherwise the pair is removed and the sources stay;
/// a `.seg` whose index is unusable is removed when a plaintext file of the
/// same number exists (the conversion is redone) and otherwise left for
/// segment salvage. Leftover build temporaries are removed unless
/// `building` says a build is writing them. Blocking.
pub(super) fn reconcile_directory(directory: &Path, building: bool) -> std::io::Result<Reconciled> {
    let mut outcome = Reconciled::default();
    let mut segs = Vec::new();
    let mut idxs = Vec::new();
    let mut plains = std::collections::HashSet::new();
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(outcome),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(number) = numbered(&name, ".seg") {
            segs.push(number);
        } else if let Some(number) = numbered(&name, ".idx") {
            idxs.push(number);
        } else if let Some(number) = numbered(&name, ".jsonl") {
            plains.insert(number);
        } else if name.starts_with(SEAL_TEMP_PREFIX) && !building {
            std::fs::remove_file(entry.path())?;
            outcome.changed = true;
        }
    }
    let remove = |name: String| -> std::io::Result<()> {
        match std::fs::remove_file(directory.join(name)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    };
    for number in &idxs {
        if !segs.contains(number) {
            remove(index_name(*number))?;
            outcome.changed = true;
        }
    }
    for number in segs {
        match load_index(directory, number) {
            Ok(loaded) => {
                let leftovers: Vec<&String> = loaded
                    .body
                    .sources
                    .iter()
                    .filter(|source| directory.join(source).exists())
                    .collect();
                if leftovers.is_empty() {
                    continue;
                }
                outcome.changed = true;
                if verify_segment_file(&directory.join(segment_name(number)), &loaded.body)? {
                    for source in leftovers {
                        remove(source.clone())?;
                    }
                } else {
                    remove(segment_name(number))?;
                    remove(index_name(number))?;
                }
            }
            Err(SegmentError::Io(error)) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error);
            }
            Err(_) => {
                if plains.contains(&number) {
                    remove(segment_name(number))?;
                    remove(index_name(number))?;
                    outcome.changed = true;
                }
            }
        }
    }
    if outcome.changed {
        sync_directory_blocking(directory)?;
    }
    Ok(outcome)
}

/// Frame table of a `.seg` recovered by walking its frame headers, for a
/// segment whose `.idx` is lost. Stops at the first damaged header.
fn walk_frames(path: &Path) -> std::io::Result<Vec<FrameEntry>> {
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let mut magic = [0u8; SEGMENT_HEADER_BYTES as usize];
    if file.read_exact(&mut magic).is_err() || &magic[..SEGMENT_MAGIC.len()] != SEGMENT_MAGIC {
        return Ok(Vec::new());
    }
    let mut offset = SEGMENT_HEADER_BYTES;
    let mut frames = Vec::new();
    let mut header = [0u8; FRAME_HEADER_BYTES];
    while offset + FRAME_HEADER_BYTES as u64 <= length {
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut header)?;
        if &header[..4] != FRAME_MAGIC {
            break;
        }
        let u32_at = |at: usize| u32::from_le_bytes(header[at..at + 4].try_into().unwrap());
        let stream = header[4];
        let kid_len = u64::from(header[5]);
        let first = u64::from_le_bytes(header[8..16].try_into().unwrap());
        let (count, raw, len, ordinal) = (u32_at(16), u32_at(20), u32_at(24), u32_at(28));
        let crc = u32_at(32);
        let data = offset + FRAME_HEADER_BYTES as u64 + kid_len;
        if stream_slot(stream).is_none() || data + u64::from(len) > length {
            break;
        }
        let kid = if kid_len > 0 {
            let mut kid = vec![0u8; kid_len as usize];
            file.read_exact(&mut kid)?;
            Some(String::from_utf8_lossy(&kid).into_owned())
        } else {
            None
        };
        frames.push(FrameEntry {
            stream,
            first,
            count,
            offset: data,
            len,
            raw,
            kid,
            ordinal,
            crc,
        });
        offset = data + u64::from(len);
    }
    Ok(frames)
}

/// A rebuilt segment and what salvage could not recover.
pub(super) struct SalvagedSegment {
    pub prepared: PreparedSegment,
    pub lost: u64,
}

/// Rebuild segment `number` from whatever of it still reads (§9 of the
/// task, per-segment salvage): frames are located through the `.idx` when
/// it is usable, else by walking frame headers. A record survives when its
/// envelope and full content decode (the summary stream is regenerated);
/// every other sequence of `first..=last` becomes a `journal.recordLost`
/// placeholder naming `backup`. `last` defaults to the newest sequence any
/// envelope frame still holds. Blocking.
pub(super) fn salvage_segment(
    directory: &Path,
    conversation_id: &str,
    number: u64,
    first: u64,
    last: Option<u64>,
    backup: &str,
    codec: ContentCodec,
) -> Result<SalvagedSegment, SegmentError> {
    let seg_path = directory.join(segment_name(number));
    let frames = match load_index(directory, number) {
        Ok(loaded) => loaded.body.frames,
        Err(_) => walk_frames(&seg_path)?,
    };
    let mut file = std::fs::File::open(&seg_path)?;
    let mut envelopes: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut contents: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut newest = 0u64;
    // Frames decode one at a time; only records of the segment's range are
    // kept, which bounds memory by the segment's raw size.
    for frame in frames.iter().filter(|frame| frame.stream != STREAM_SUMMARY) {
        let Ok(decoded) = load_frame(&mut file, frame, codec) else {
            continue;
        };
        for index in 0..frame.count as usize {
            let sequence = frame.first + index as u64;
            let Some(item) = decoded.item(index) else {
                continue;
            };
            if frame.stream == STREAM_ENVELOPE {
                newest = newest.max(sequence);
                envelopes.insert(sequence, item.to_vec());
            } else {
                contents.insert(sequence, item.to_vec());
            }
        }
    }
    let last = last.unwrap_or(newest);
    if last < first {
        return Err(invalid("nothing of the segment is readable"));
    }
    let mut builder = SegmentBuilder::create(directory, number, codec)?;
    let mut lost = 0u64;
    let mut previous: Option<(DateTime<Utc>, Option<ProviderKind>)> = None;
    let mut sequence = first;
    let result = (|| -> Result<(), SegmentError> {
        while sequence <= last {
            let recovered = envelopes
                .remove(&sequence)
                .zip(contents.remove(&sequence))
                .and_then(|(envelope, content)| {
                    let payload: Value = serde_json::from_slice(&content).ok()?;
                    let record = parse_record(&envelope).ok()?;
                    (record.sequence() == sequence)
                        .then(|| record.into_event(conversation_id, Some(payload)).ok())
                        .flatten()
                });
            if let Some(event) = recovered {
                previous = Some((event.time, event.provider));
                builder.observe(&event);
                builder.push(&event)?;
                sequence += 1;
                continue;
            }
            // A run of unreadable sequences shares one notice.
            let run_start = sequence;
            let mut run_end = sequence;
            while run_end < last
                && !(envelopes.contains_key(&(run_end + 1))
                    && contents.contains_key(&(run_end + 1)))
            {
                run_end += 1;
            }
            let run_length = run_end - run_start + 1;
            for lost_sequence in run_start..=run_end {
                let mut placeholder = ConversationEvent::new(
                    conversation_id,
                    lost_sequence,
                    JOURNAL_RECORD_LOST_EVENT,
                    serde_json::json!({
                        "reason": "corrupt",
                        "runStart": run_start,
                        "runLength": run_length,
                        "backup": backup,
                    }),
                );
                if let Some((time, provider)) = previous {
                    placeholder.time = time;
                    placeholder.provider = provider;
                }
                builder.observe(&placeholder);
                builder.push(&placeholder)?;
            }
            lost += run_length;
            sequence = run_end + 1;
        }
        Ok(())
    })();
    if let Err(error) = result {
        builder.abandon();
        return Err(error);
    }
    Ok(SalvagedSegment {
        prepared: builder.finish(Vec::new())?,
        lost,
    })
}

/// Replace a damaged segment with its salvaged rebuild: the damaged files
/// are copied to `backup` (`.seg`) and its `.idx` sibling first, then the
/// new `.seg` and `.idx` are renamed over the old ones. A crash between the
/// two renames leaves a `.seg` whose header id no longer matches its
/// `.idx`; the next open treats that index as lost and rebuilds it from
/// the frame headers. Blocking; callers hold the conversation lock.
pub(super) fn replace_with_salvaged(
    directory: &Path,
    prepared: &PreparedSegment,
    backup: &str,
) -> Result<(), SegmentError> {
    let seg_path = directory.join(segment_name(prepared.number));
    let idx_path = directory.join(index_name(prepared.number));
    std::fs::copy(&seg_path, directory.join(backup))?;
    if idx_path.exists() {
        let idx_backup = backup.strip_suffix(".seg").unwrap_or(backup);
        std::fs::copy(&idx_path, directory.join(format!("{idx_backup}.idx")))?;
    }
    sync_directory_blocking(directory)?;
    std::fs::rename(&prepared.temp_seg, &seg_path)?;
    std::fs::rename(&prepared.temp_idx, &idx_path)?;
    sync_directory_blocking(directory)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const CONVERSATION: &str = "4f1c2a4e-1b2c-4d3e-8f00-112233445566";

    fn event(sequence: u64, event_type: &str, payload: Value) -> ConversationEvent {
        ConversationEvent::new(CONVERSATION, sequence, event_type, payload)
    }

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!("todex-segment-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn slim_plan_strips_only_records_a_later_terminal_covers() {
        let block = |id: &str| json!({"category": "assistant_final", "id": id, "turnId": "t"});
        let history = [
            // Codex: deltas and completed share block.id.
            event(
                1,
                "message.delta",
                json!({"turnId": "t", "block": block("a")}),
            ),
            event(
                2,
                "message.completed",
                json!({"turnId": "t", "block": block("a")}),
            ),
            // Same id in another turn is not covered by turn t.
            event(
                3,
                "message.delta",
                json!({"turnId": "u", "block": block("a")}),
            ),
            // Pi final answer supersedes progress block p.
            event(
                4,
                "message.delta",
                json!({"turnId": "t", "block": block("p")}),
            ),
            event(
                5,
                "message.completed",
                json!({"turnId": "t",
                "block": {"id": "final", "supersedes": ["p"]}}),
            ),
            // Claude: no block id, kept.
            event(
                6,
                "message.delta",
                json!({"turnId": "t", "delta": {"text": "x"}}),
            ),
            event(
                7,
                "thought.delta",
                json!({"turnId": "t", "block": block("a")}),
            ),
            // ACP tool snapshots: the terminal-status update covers the
            // earlier ones and is kept itself.
            event(
                8,
                "tool.updated",
                json!({"turnId": "t", "toolCallId": "c1", "status": "in_progress"}),
            ),
            event(
                9,
                "tool.updated",
                json!({"turnId": "t", "toolCallId": "c1", "status": "completed"}),
            ),
            // Claude/Pi tools: tool.completed with the same id.
            event(
                10,
                "tool.updated",
                json!({"turnId": "t", "toolCallId": "c2"}),
            ),
            event(
                11,
                "tool.completed",
                json!({"turnId": "t", "toolCallId": "c2"}),
            ),
            event(
                12,
                "tool.updated",
                json!({"turnId": "t", "toolCallId": "c3"}),
            ),
            event(
                13,
                "subagent.updated",
                json!({"subagentId": "s1", "turnId": "t"}),
            ),
            event(
                14,
                "subagent.completed",
                json!({"subagentId": "s1", "turnId": "t2"}),
            ),
            event(15, "subagent.updated", json!({"subagentId": "s2"})),
            // A delta after its completed record is not covered by it.
            event(
                16,
                "message.delta",
                json!({"turnId": "t", "block": block("a")}),
            ),
        ];
        let mut plan = SlimPlan::default();
        history.iter().for_each(|event| plan.observe(event));
        let stripped: Vec<u64> = history
            .iter()
            .filter(|event| plan.strips(event))
            .map(|event| event.sequence)
            .collect();
        assert_eq!(stripped, vec![1, 4, 8, 10, 13]);
    }

    fn write_plain(directory: &Path, name: &str, events: &[ConversationEvent]) {
        let mut bytes = Vec::new();
        for event in events {
            bytes.extend(super::super::record::encode_record(event, ContentCodec::Plain).unwrap());
            bytes.push(b'\n');
        }
        std::fs::write(directory.join(name), bytes).unwrap();
    }

    fn read_all(directory: &Path, segment: &SealedSegment) -> Vec<ConversationEvent> {
        let cache = FrameCache::new(4 * 1024 * 1024);
        let mut reader = SegmentReader::open(
            directory,
            segment,
            &cache,
            ContentCodec::Plain,
            CONVERSATION,
        )
        .unwrap();
        (segment.first..=segment.last)
            .map(|sequence| reader.event(sequence).unwrap().0)
            .collect()
    }

    fn history(count: u64) -> Vec<ConversationEvent> {
        (1..=count)
            .map(|sequence| {
                event(
                    sequence,
                    "provider.event",
                    json!({"turnId": "t", "text": format!("payload {sequence} ").repeat(40)}),
                )
            })
            .collect()
    }

    #[test]
    fn built_segments_read_back_identically_across_many_frames() {
        let directory = temp_dir();
        let events = history(6_000);
        write_plain(&directory, "events.000001.jsonl", &events[..3_000]);
        write_plain(&directory, "events.000002.jsonl", &events[3_000..]);
        let prepared = build_from_plain(
            &directory,
            CONVERSATION,
            1,
            &[
                "events.000001.jsonl".to_owned(),
                "events.000002.jsonl".to_owned(),
            ],
            1,
            ContentCodec::Plain,
        )
        .unwrap();
        commit_prepared(&directory, &prepared, None).unwrap();
        assert!(!directory.join("events.000002.jsonl").exists());
        let loaded = load_index(&directory, 1).unwrap();
        assert!(loaded.body.frames.len() > 6, "{}", loaded.body.frames.len());
        assert_eq!(loaded.body.digest, JournalDigest::from_events(&events));
        assert!(verify_segment_file(&directory.join("events.000001.seg"), &loaded.body).unwrap());
        let read = read_all(&directory, &loaded.segment);
        assert_eq!(
            serde_json::to_value(&read).unwrap(),
            serde_json::to_value(&events).unwrap()
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn damaged_frames_salvage_into_placeholders_and_a_lost_index_is_rebuilt() {
        let directory = temp_dir();
        let events = history(3_000);
        write_plain(&directory, "events.000001.jsonl", &events);
        let prepared = build_from_plain(
            &directory,
            CONVERSATION,
            1,
            &["events.000001.jsonl".to_owned()],
            1,
            ContentCodec::Plain,
        )
        .unwrap();
        commit_prepared(&directory, &prepared, None).unwrap();
        let loaded = load_index(&directory, 1).unwrap();
        // Flip a byte inside the second full-content frame.
        let damaged = loaded
            .body
            .frames
            .iter()
            .filter(|frame| frame.stream == STREAM_FULL)
            .nth(1)
            .unwrap()
            .clone();
        let seg_path = directory.join("events.000001.seg");
        let mut bytes = std::fs::read(&seg_path).unwrap();
        bytes[(damaged.offset + u64::from(damaged.len) / 2) as usize] ^= 0xff;
        std::fs::write(&seg_path, &bytes).unwrap();
        let cache = FrameCache::new(1 << 20);
        let mut reader = SegmentReader::open(
            &directory,
            &loaded.segment,
            &cache,
            ContentCodec::Plain,
            CONVERSATION,
        )
        .unwrap();
        assert!(reader.event(damaged.first).is_err());
        // Losing the index too: salvage walks the frame headers.
        std::fs::remove_file(directory.join("events.000001.idx")).unwrap();
        let salvaged = salvage_segment(
            &directory,
            CONVERSATION,
            1,
            1,
            Some(3_000),
            "events.corrupt.test.seg",
            ContentCodec::Plain,
        )
        .unwrap();
        assert_eq!(salvaged.lost, u64::from(damaged.count));
        replace_with_salvaged(&directory, &salvaged.prepared, "events.corrupt.test.seg").unwrap();
        let reloaded = load_index(&directory, 1).unwrap();
        let read = read_all(&directory, &reloaded.segment);
        assert_eq!(read.len(), 3_000);
        for (event, original) in read.iter().zip(&events) {
            let lost = (damaged.first..damaged.first + u64::from(damaged.count))
                .contains(&original.sequence);
            if lost {
                assert_eq!(event.event_type, JOURNAL_RECORD_LOST_EVENT);
                assert_eq!(event.payload["runStart"], damaged.first);
                assert_eq!(event.payload["runLength"], damaged.count);
            } else {
                assert_eq!(event.payload, original.payload);
            }
        }
        assert!(directory.join("events.corrupt.test.seg").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn frame_cache_stays_within_its_byte_budget() {
        let cache = FrameCache::new(10_000);
        for offset in 0..20u64 {
            cache
                .get_or_load(("seg".to_owned(), offset), || {
                    Ok(DecodedFrame {
                        bytes: vec![0; 3_000],
                        items: vec![(0, 3_000)],
                    })
                })
                .unwrap();
            assert!(cache.cached_bytes() <= 10_000);
        }
    }
}
