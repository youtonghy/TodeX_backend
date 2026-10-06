//! Sealed journal segments (history v3, `docs/history-encryption.md` §4.1,
//! §4.3, §6).
//!
//! A sealed segment is a pair of files:
//!
//! - `events.NNNNNN.seg`: a 24-byte header (`TDXSEG1\n` plus a random
//!   16-byte segment id) followed by frames. Each frame is a 36-byte header
//!   (`TDXF`, stream, `kid` length, flags, first sequence, record count, raw
//!   and stored length, frame ordinal, CRC-32), the optional `kid`, and the
//!   frame data compressed as raw DEFLATE (RFC 1951, no zlib/gzip header,
//!   so browsers can inflate it). Frame headers make the file
//!   self-describing, so a lost `.idx` can be rebuilt from it.
//! - `events.NNNNNN.idx`: JSON `{"sha256", "body"}` where `sha256` covers
//!   the raw `body` text. The body holds the first/last sequence, the frame
//!   table, the segment's [`JournalDigest`], the `.seg` length, id and
//!   SHA-256, and the plaintext files the segment replaced (`sources`).
//!
//! There are three streams, each cut into frames of about
//! [`FRAME_RAW_BYTES`] raw bytes: the envelope stream (one
//! [`super::record`] line per record without its content, never
//! encrypted), the summary content stream (a JSON array of
//! [`summarize_event`] payloads, so `detail=summary` can read it alone) and
//! the full content stream (a JSON array of payloads).
//!
//! A content frame belongs to one [`FrameGroup`] and never mixes two:
//!
//! - `Plain`: plaintext payloads (history encryption off, or not migrated
//!   yet). The CRC covers the raw frame.
//! - `Sealed(kid)`: payloads sealed under that DEK after compression
//!   (history crypto stream 3/4, counter [`frame_counter`]). The daemon
//!   only does this while the DEK is still in memory (sealing right after
//!   the records were written, or migrating plaintext under a fresh key)
//!   and never decrypts such a frame again: replays hand the ciphertext to
//!   clients (`$enc.fr` plus the page's `frames`). The summary and full
//!   frames of a sealed run cover the same records, so one index `i`
//!   addresses both. The CRC covers the stored ciphertext.
//! - `Passthrough`: event-level `$enc` objects kept as ciphertext because
//!   their DEK was gone when the segment was sealed (the daemon restarted)
//!   or they are a fork's copies. Replays serve them like active-file
//!   records.
//!
//! Sealing also slims history (§4.3): streaming progress records whose
//! terminal record is in the same segment become `journal.compacted`
//! markers; see [`SlimPlan`] for the exact rule. Sequences stay dense.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};

use super::digest::JournalDigest;
use super::record::{
    add_history_macs, compacted_marker, decode_journal_record, encode_envelope, encrypted_content,
    open_event, parse_record, record_bytes, ENCRYPTED_FIELD, JOURNAL_COMPACTED_EVENT,
};
use super::{summarize_event, ConversationEvent, ProviderKind};
use crate::history_crypto::{self, ContentStream, SegmentKey};
use crate::history_keys::FingerprintKey;

const SEGMENT_MAGIC: &[u8; 8] = b"TDXSEG1\n";
const SEGMENT_ID_BYTES: usize = 16;
const SEGMENT_HEADER_BYTES: u64 = (SEGMENT_MAGIC.len() + SEGMENT_ID_BYTES) as u64;
const FRAME_MAGIC: &[u8; 4] = b"TDXF";
const FRAME_HEADER_BYTES: usize = 36;
/// Frame header flag: the items are event-level `$enc` objects.
const FLAG_PASSTHROUGH: u8 = 1;
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

/// The AEAD counter of sealed frame `ordinal` in segment `number`:
/// `number << 32 | ordinal`. Ordinals restart in every segment, and a key's
/// records normally all live in one segment (the active file rotates its
/// DEK when it is sealed); the segment number in the high bits keeps the
/// nonce unique even when a key's frames end up in several segments.
pub(super) fn frame_counter(number: u64, ordinal: u32) -> u64 {
    (number << 32) | u64::from(ordinal)
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

fn is_false(value: &bool) -> bool {
    !*value
}

/// What the items of a content frame are; see the module docs.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum FrameGroup {
    Plain,
    Passthrough,
    Sealed(String),
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
    /// Stored (compressed, and when sealed encrypted) length.
    #[serde(rename = "l")]
    pub len: u32,
    /// Decompressed length.
    #[serde(rename = "u")]
    pub raw: u32,
    /// The DEK a sealed frame is encrypted under.
    #[serde(rename = "k", default, skip_serializing_if = "Option::is_none")]
    pub kid: Option<String>,
    /// The items are event-level `$enc` objects ([`FrameGroup::Passthrough`]).
    #[serde(rename = "x", default, skip_serializing_if = "is_false")]
    pub passthrough: bool,
    /// Frame number under `(group, stream)` within this segment; the AEAD
    /// counter of a sealed frame is [`frame_counter`] of it.
    #[serde(rename = "f")]
    pub ordinal: u32,
    /// CRC-32 of the raw (decompressed) frame — DEFLATE has no checksum of
    /// its own — or, for a sealed frame, of the stored ciphertext the
    /// daemon cannot open.
    #[serde(rename = "c")]
    pub crc: u32,
}

impl FrameEntry {
    pub fn group(&self) -> FrameGroup {
        match (&self.kid, self.passthrough) {
            (Some(kid), _) => FrameGroup::Sealed(kid.clone()),
            (None, true) => FrameGroup::Passthrough,
            (None, false) => FrameGroup::Plain,
        }
    }

    fn contains(&self, sequence: u64) -> bool {
        sequence >= self.first && sequence < self.first + u64::from(self.count)
    }
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

impl SegmentIndexBody {
    /// Whether any content is stored as plaintext: what end-to-end
    /// migration still has to re-encrypt.
    pub fn has_plain_content(&self) -> bool {
        self.frames
            .iter()
            .any(|frame| frame.stream != STREAM_ENVELOPE && frame.group() == FrameGroup::Plain)
    }
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
    /// Identifies the `.seg` content; frame cache keys and wire frame ids
    /// use it.
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
                || (frame.stream == STREAM_ENVELOPE && frame.group() != FrameGroup::Plain)
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
        let segment = Self {
            number,
            first: body.first_sequence,
            last: body.last_sequence,
            segment_id: body.segment_id.clone(),
            frames: body.frames.clone(),
            by_stream,
        };
        // A sealed run's summary and full frames must line up: `$enc.fr`
        // carries one index for both.
        for frame in segment.frames.iter().filter(|frame| frame.kid.is_some()) {
            let other = if frame.stream == STREAM_FULL {
                STREAM_SUMMARY
            } else {
                STREAM_FULL
            };
            let paired = segment.frame(other, frame.first).is_some_and(|pair| {
                pair.first == frame.first && pair.count == frame.count && pair.kid == frame.kid
            });
            if !paired {
                return Err(invalid("sealed segment frames are not aligned"));
            }
        }
        Ok(segment)
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
        frame.contains(sequence).then_some(frame)
    }

    /// The wire id of `frame` (§5.3 `frames` keys): unique per segment
    /// build, so a client's cached frame never names other ciphertext.
    pub fn frame_id(&self, frame: &FrameEntry) -> String {
        format!("{}-{:x}", self.segment_id, frame.offset)
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

/// The stored bytes of one frame.
fn read_stored(file: &mut std::fs::File, entry: &FrameEntry) -> Result<Vec<u8>, SegmentError> {
    let mut stored = vec![0u8; entry.len as usize];
    file.seek(SeekFrom::Start(entry.offset))?;
    file.read_exact(&mut stored)?;
    Ok(stored)
}

/// The stored ciphertext of a sealed frame, checked against its CRC.
fn read_sealed(file: &mut std::fs::File, entry: &FrameEntry) -> Result<Vec<u8>, SegmentError> {
    let stored = read_stored(file, entry)?;
    if crc32(&stored) != entry.crc {
        return Err(invalid("sealed frame checksum mismatch"));
    }
    Ok(stored)
}

/// Read and decompress one plaintext or passthrough frame. Sealed frames
/// are never opened by the daemon.
fn load_frame(file: &mut std::fs::File, entry: &FrameEntry) -> Result<DecodedFrame, SegmentError> {
    if entry.kid.is_some() {
        return Err(invalid("a sealed frame cannot be read by the daemon"));
    }
    let stored = read_stored(file, entry)?;
    let raw = inflate(&stored, entry.raw as usize)?;
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

/// A sealed frame one replayed record refers to (`$enc.fr`).
#[derive(Clone, Debug)]
pub(super) struct FrameRef {
    pub id: String,
    pub entry: FrameEntry,
}

/// One record read back from a segment.
pub(super) struct SegmentRecord {
    pub event: ConversationEvent,
    /// Journal bytes for page budgets; a sealed record counts its envelope
    /// only, the caller adds each referenced frame once.
    pub bytes: u64,
    /// The sealed frame the record's content is in, for the requested
    /// detail.
    pub frame: Option<FrameRef>,
}

/// Reads records of one sealed segment, keeping the current envelope and
/// content frame at hand so consecutive records decompress each frame once.
pub(super) struct SegmentReader<'a> {
    segment: &'a SealedSegment,
    file: std::fs::File,
    cache: &'a FrameCache,
    conversation_id: &'a str,
    current: [Option<(u64, Arc<DecodedFrame>)>; 3],
}

impl<'a> SegmentReader<'a> {
    pub fn open(
        directory: &Path,
        segment: &'a SealedSegment,
        cache: &'a FrameCache,
        conversation_id: &'a str,
    ) -> Result<Self, SegmentError> {
        Ok(Self {
            file: std::fs::File::open(directory.join(segment.file_name()))?,
            segment,
            cache,
            conversation_id,
            current: [None, None, None],
        })
    }

    fn entry(&self, stream: u8, sequence: u64) -> Result<FrameEntry, SegmentError> {
        self.segment
            .frame(stream, sequence)
            .cloned()
            .ok_or_else(|| invalid(format!("no frame holds sequence {sequence}")))
    }

    fn frame_item(&mut self, entry: &FrameEntry, sequence: u64) -> Result<Vec<u8>, SegmentError> {
        let slot = stream_slot(entry.stream).unwrap_or_default();
        let current = match &self.current[slot] {
            Some((offset, frame)) if *offset == entry.offset => frame.clone(),
            _ => {
                let file = &mut self.file;
                let frame = self
                    .cache
                    .get_or_load((self.segment.segment_id.clone(), entry.offset), || {
                        load_frame(file, entry)
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

    /// The record with `sequence`. Plaintext content is always the full
    /// payload (replays summarize it); ciphertext comes in the detail asked
    /// for: a passthrough record's summary item, or the summary frame of a
    /// sealed run.
    pub fn record(&mut self, sequence: u64, summary: bool) -> Result<SegmentRecord, SegmentError> {
        let envelope_entry = self.entry(STREAM_ENVELOPE, sequence)?;
        let envelope = self.frame_item(&envelope_entry, sequence)?;
        let record = parse_record(&envelope)?;
        if record.sequence() != sequence {
            return Err(invalid(format!(
                "envelope frame holds sequence {} where {sequence} belongs",
                record.sequence()
            )));
        }
        let full = self.entry(STREAM_FULL, sequence)?;
        let (payload, content_bytes, frame) = match full.group() {
            FrameGroup::Plain => {
                let content = self.frame_item(&full, sequence)?;
                (serde_json::from_slice(&content)?, content.len(), None)
            }
            FrameGroup::Passthrough => {
                let entry = if summary {
                    self.entry(STREAM_SUMMARY, sequence)?
                } else {
                    full
                };
                let content = self.frame_item(&entry, sequence)?;
                let encrypted: Map<String, Value> = serde_json::from_slice(&content)?;
                let mut payload = record.envelope().clone();
                payload.insert(ENCRYPTED_FIELD.to_owned(), Value::Object(encrypted));
                (Value::Object(payload), content.len(), None)
            }
            FrameGroup::Sealed(kid) => {
                let summary_entry = self.entry(STREAM_SUMMARY, sequence)?;
                let index = sequence - full.first;
                let reference = serde_json::json!({
                    "s": self.segment.frame_id(&summary_entry),
                    "f": self.segment.frame_id(&full),
                    "i": index,
                });
                let mut encrypted = Map::new();
                encrypted.insert("v".to_owned(), Value::from(1u64));
                encrypted.insert("kid".to_owned(), Value::String(kid));
                encrypted.insert(
                    "c".to_owned(),
                    Value::String(self.conversation_id.to_owned()),
                );
                encrypted.insert("n".to_owned(), Value::from(sequence));
                encrypted.insert("fr".to_owned(), reference);
                let mut payload = record.envelope().clone();
                payload.insert(ENCRYPTED_FIELD.to_owned(), Value::Object(encrypted));
                let entry = if summary { summary_entry } else { full };
                let frame = FrameRef {
                    id: self.segment.frame_id(&entry),
                    entry,
                };
                (Value::Object(payload), 0, Some(frame))
            }
        };
        let event = record
            .into_event(self.conversation_id, Some(payload))
            .map_err(invalid)?;
        Ok(SegmentRecord {
            event,
            bytes: record_bytes(envelope.len(), content_bytes),
            frame,
        })
    }

    /// The record with `sequence` and its journal bytes, in full detail.
    pub fn event(&mut self, sequence: u64) -> Result<(ConversationEvent, u64), SegmentError> {
        let record = self.record(sequence, false)?;
        Ok((record.event, record.bytes))
    }

    /// The wire form of a sealed frame (§5.3 `frames` value): `{kid, stream,
    /// counter, c, ct}`, the ciphertext read straight from the `.seg`.
    pub fn wire_frame(&mut self, entry: &FrameEntry) -> Result<Value, SegmentError> {
        let kid = entry
            .kid
            .clone()
            .ok_or_else(|| invalid("only sealed frames are sent as frames"))?;
        let stored = read_sealed(&mut self.file, entry)?;
        let stream = match entry.stream {
            STREAM_SUMMARY => ContentStream::FrameSummary,
            _ => ContentStream::FrameFull,
        };
        Ok(serde_json::json!({
            "kid": kid,
            "stream": stream.as_u32(),
            "counter": frame_counter(self.segment.number, entry.ordinal),
            "c": self.conversation_id,
            "ct": URL_SAFE_NO_PAD.encode(stored),
        }))
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
/// across turns; subagent ids are unique and outlive turns. Encrypted
/// records are judged by their plaintext while their DEK is in memory;
/// passthrough ciphertext only contributes its envelope as a terminal and
/// is never stripped.
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
        if payload.get(ENCRYPTED_FIELD).is_some() {
            return false;
        }
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

/// A fresh DEK end-to-end migration seals plaintext records under, with the
/// fingerprint key their deduplication fields are MAC'd with first.
pub(super) struct MigrationKey {
    pub kid: String,
    pub key: Arc<SegmentKey>,
    pub fingerprint: Arc<FingerprintKey>,
}

/// The keys a segment build may encrypt with. Default: none (plaintext and
/// ciphertext are framed as they are).
#[derive(Default)]
pub(super) struct SealPolicy {
    /// DEKs still in memory, by kid: event ciphertext under them is opened
    /// and repacked into sealed frames.
    pub keys: HashMap<String, Arc<SegmentKey>>,
    /// History encryption is on: plaintext records are sealed under this
    /// key.
    pub migrate: Option<MigrationKey>,
}

/// Content of one record handed to [`SegmentBuilder::push`].
pub(super) enum Item {
    /// Plaintext payload, framed as plaintext.
    Plain(Value),
    /// Plaintext payload to seal under the builder's key `kid`.
    Sealed { kid: String, payload: Value },
    /// An event-level `$enc` object kept as ciphertext.
    Passthrough(Map<String, Value>),
    /// The record's content is in sealed frames copied with
    /// [`SegmentBuilder::push_verbatim`].
    Verbatim,
}

/// One record as a build will store it.
struct Classified {
    /// What the envelope stream describes and the digest folds in.
    stored: ConversationEvent,
    /// The plaintext, for slimming; `None` for passthrough ciphertext.
    view: Option<ConversationEvent>,
    /// The group its content (or its marker's) goes to.
    group: FrameGroup,
}

impl SealPolicy {
    /// Decide how `event` (as read from a plaintext journal file) is
    /// stored; see [`FrameGroup`].
    fn classify(&self, event: ConversationEvent, conversation_id: &str) -> Classified {
        if let Some(encrypted) = encrypted_content(&event.payload) {
            let opened = encrypted
                .get("kid")
                .and_then(Value::as_str)
                .and_then(|kid| Some((kid, self.keys.get(kid)?)))
                .and_then(|(kid, key)| {
                    let (full, _) = open_event(encrypted, conversation_id, event.sequence, key)?;
                    Some((kid.to_owned(), full))
                });
            return match opened {
                Some((kid, full)) => {
                    let mut view = event.clone();
                    view.payload = full;
                    Classified {
                        stored: event,
                        view: Some(view),
                        group: FrameGroup::Sealed(kid),
                    }
                }
                None => Classified {
                    stored: event,
                    view: None,
                    group: FrameGroup::Passthrough,
                },
            };
        }
        match &self.migrate {
            Some(migrate) => {
                let mut stored = event;
                add_history_macs(
                    &stored.event_type,
                    &mut stored.payload,
                    &migrate.fingerprint,
                );
                Classified {
                    view: Some(stored.clone()),
                    stored,
                    group: FrameGroup::Sealed(migrate.kid.clone()),
                }
            }
            None => Classified {
                view: Some(event.clone()),
                stored: event,
                group: FrameGroup::Plain,
            },
        }
    }

    /// The content item of `classified` (or of a marker in its place).
    fn item(group: &FrameGroup, payload: Value) -> Item {
        match group {
            FrameGroup::Plain => Item::Plain(payload),
            FrameGroup::Sealed(kid) => Item::Sealed {
                kid: kid.clone(),
                payload,
            },
            FrameGroup::Passthrough => match payload {
                Value::Object(mut map) => match map.remove(ENCRYPTED_FIELD) {
                    Some(Value::Object(encrypted)) => Item::Passthrough(encrypted),
                    _ => Item::Plain(Value::Object(map)),
                },
                other => Item::Plain(other),
            },
        }
    }

    /// Every key a frame may be sealed under.
    fn sealing_keys(&self) -> HashMap<String, Arc<SegmentKey>> {
        let mut keys = self.keys.clone();
        if let Some(migrate) = &self.migrate {
            keys.insert(migrate.kid.clone(), migrate.key.clone());
        }
        keys
    }
}

/// Output of a segment build: the temporary `.seg`/`.idx` (durable) and the
/// index body they describe.
pub(super) struct PreparedSegment {
    pub number: u64,
    pub temp_seg: PathBuf,
    pub temp_idx: PathBuf,
    pub body: SegmentIndexBody,
    /// DEKs whose event ciphertext was opened and repacked: released once
    /// the segment is committed.
    pub repacked: Vec<String>,
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
    group: FrameGroup,
    first: u64,
    count: u32,
    raw: Vec<u8>,
}

impl StreamBuffer {
    fn new(stream: u8) -> Self {
        Self {
            stream,
            group: FrameGroup::Plain,
            first: 0,
            count: 0,
            raw: Vec::new(),
        }
    }
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
    ordinals: HashMap<(FrameGroup, u8), u32>,
    /// Ordinals taken per `(kid, stream)`, so a copied frame and a new one
    /// can never share an AEAD counter.
    sealed_ordinals: HashSet<(String, u8, u32)>,
    /// Next sequence each content stream expects (summary, full), once
    /// the first record or copied frame set it.
    content_next: [u64; 2],
    content_started: bool,
    conversation_id: String,
    keys: HashMap<String, Arc<SegmentKey>>,
    repacked: BTreeSet<String>,
    digest: JournalDigest,
    first: Option<u64>,
    last: u64,
    slimmed: u64,
}

impl SegmentBuilder {
    pub fn create(
        directory: &Path,
        conversation_id: &str,
        number: u64,
        policy: &SealPolicy,
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
            buffers: STREAMS.map(StreamBuffer::new),
            ordinals: HashMap::new(),
            sealed_ordinals: HashSet::new(),
            content_next: [0; 2],
            content_started: false,
            conversation_id: conversation_id.to_owned(),
            keys: policy.sealing_keys(),
            repacked: BTreeSet::new(),
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

    /// Note that event ciphertext under `kid` was opened for this segment.
    pub fn note_repacked(&mut self, kid: &str) {
        if !self.repacked.contains(kid) {
            self.repacked.insert(kid.to_owned());
        }
    }

    /// Append the next record: `stored` describes its envelope, `item` its
    /// content.
    pub fn push(&mut self, stored: &ConversationEvent, item: Item) -> Result<(), SegmentError> {
        let sequence = stored.sequence;
        if self.first.is_none() {
            self.first = Some(sequence);
            if !self.content_started {
                self.content_next = [sequence; 2];
                self.content_started = true;
            }
        } else if sequence != self.last + 1 {
            return Err(invalid(format!(
                "segment record {sequence} does not follow {}",
                self.last
            )));
        }
        self.last = sequence;
        if stored.event_type == JOURNAL_COMPACTED_EVENT {
            self.slimmed += 1;
        }
        let mut envelope = encode_envelope(stored)?;
        envelope.push(b'\n');
        self.append(0, sequence, &FrameGroup::Plain, &envelope)?;
        let (group, payload) = match item {
            Item::Verbatim => {
                if self.content_next.iter().any(|next| *next <= sequence) {
                    return Err(invalid(format!(
                        "record {sequence} has no copied frame holding its content"
                    )));
                }
                return Ok(());
            }
            Item::Plain(payload) => (FrameGroup::Plain, payload),
            Item::Sealed { kid, payload } => {
                if !self.keys.contains_key(&kid) {
                    return Err(invalid("no key to seal the frame under"));
                }
                (FrameGroup::Sealed(kid), payload)
            }
            Item::Passthrough(encrypted) => {
                // The full item keeps both ciphertexts (so the summary item
                // can be derived again); the summary item drops `f` when a
                // separate summary exists.
                let mut summary = encrypted.clone();
                if summary.contains_key("s") {
                    summary.remove("f");
                }
                let full = serde_json::to_vec(&encrypted)?;
                let summary = serde_json::to_vec(&summary)?;
                return self.append_content(sequence, FrameGroup::Passthrough, &summary, &full);
            }
        };
        let full = serde_json::to_vec(&payload)?;
        let mut summarized = stored.clone();
        summarized.payload = payload;
        let summary = if summarize_event(&mut summarized) {
            serde_json::to_vec(&summarized.payload)?
        } else {
            full.clone()
        };
        self.append_content(sequence, group, &summary, &full)
    }

    /// Copy a sealed run's summary and full frames as they are (salvage,
    /// re-encryption of a segment holding them). The records they hold are
    /// then pushed with [`Item::Verbatim`].
    pub fn push_verbatim(
        &mut self,
        summary: (FrameEntry, Vec<u8>),
        full: (FrameEntry, Vec<u8>),
    ) -> Result<(), SegmentError> {
        let first = full.0.first;
        if !self.content_started {
            self.content_next = [first; 2];
            self.content_started = true;
        }
        for (slot, entry) in [(1usize, &summary.0), (2usize, &full.0)] {
            if entry.kid.is_none()
                || entry.first != first
                || entry.count != full.0.count
                || entry.stream != STREAMS[slot]
                || entry.kid != full.0.kid
                || self.content_next[slot - 1] != first
            {
                return Err(invalid("copied frames do not line up"));
            }
            self.flush(slot)?;
        }
        for (slot, (entry, stored)) in [(1usize, summary), (2usize, full)] {
            let kid = entry.kid.clone().unwrap_or_default();
            if !self
                .sealed_ordinals
                .insert((kid, entry.stream, entry.ordinal))
            {
                return Err(invalid("copied frame reuses a frame counter"));
            }
            self.content_next[slot - 1] = entry.first + u64::from(entry.count);
            self.write_frame(entry, &stored)?;
        }
        Ok(())
    }

    fn append_content(
        &mut self,
        sequence: u64,
        group: FrameGroup,
        summary: &[u8],
        full: &[u8],
    ) -> Result<(), SegmentError> {
        if self.content_next != [sequence; 2] {
            return Err(invalid(format!(
                "segment content for {sequence} is out of order"
            )));
        }
        let needs_flush = |buffer: &StreamBuffer, item: &[u8]| {
            buffer.count > 0
                && (buffer.raw.len() + item.len() > FRAME_RAW_BYTES || buffer.group != group)
        };
        let summary_full = needs_flush(&self.buffers[1], summary);
        let full_full = needs_flush(&self.buffers[2], full);
        if matches!(group, FrameGroup::Sealed(_)) {
            // A sealed run's summary and full frames cover the same records.
            if summary_full || full_full {
                self.flush(1)?;
                self.flush(2)?;
            }
        } else {
            if summary_full {
                self.flush(1)?;
            }
            if full_full {
                self.flush(2)?;
            }
        }
        self.append(1, sequence, &group, summary)?;
        self.append(2, sequence, &group, full)?;
        self.content_next = [sequence + 1; 2];
        Ok(())
    }

    fn append(
        &mut self,
        slot: usize,
        sequence: u64,
        group: &FrameGroup,
        item: &[u8],
    ) -> Result<(), SegmentError> {
        let buffer = &self.buffers[slot];
        if slot == 0
            && buffer.count > 0
            && (buffer.raw.len() + item.len() > FRAME_RAW_BYTES || buffer.group != *group)
        {
            self.flush(slot)?;
        }
        let buffer = &mut self.buffers[slot];
        if buffer.count == 0 {
            buffer.first = sequence;
            buffer.group = group.clone();
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
        let mut buffer =
            std::mem::replace(&mut self.buffers[slot], StreamBuffer::new(STREAMS[slot]));
        if buffer.count == 0 {
            return Ok(());
        }
        if buffer.stream != STREAM_ENVELOPE {
            buffer.raw.push(b']');
        }
        let compressed = deflate(&buffer.raw)?;
        let mut ordinal = {
            let slot = self
                .ordinals
                .entry((buffer.group.clone(), buffer.stream))
                .or_default();
            let ordinal = *slot;
            *slot += 1;
            ordinal
        };
        let (stored, crc, kid) = match &buffer.group {
            FrameGroup::Sealed(kid) => {
                while !self
                    .sealed_ordinals
                    .insert((kid.clone(), buffer.stream, ordinal))
                {
                    ordinal = ordinal
                        .checked_add(1)
                        .ok_or_else(|| invalid("frame ordinals exhausted"))?;
                    self.ordinals
                        .insert((buffer.group.clone(), buffer.stream), ordinal + 1);
                }
                let key = self
                    .keys
                    .get(kid)
                    .ok_or_else(|| invalid("no key to seal the frame under"))?;
                let stream = if buffer.stream == STREAM_SUMMARY {
                    ContentStream::FrameSummary
                } else {
                    ContentStream::FrameFull
                };
                let sealed = history_crypto::seal(
                    key,
                    &self.conversation_id,
                    stream,
                    frame_counter(self.number, ordinal),
                    &compressed,
                )
                .map_err(|error| invalid(error.to_string()))?;
                let crc = crc32(&sealed);
                (sealed, crc, Some(kid.clone()))
            }
            _ => (compressed, crc32(&buffer.raw), None),
        };
        let entry = FrameEntry {
            stream: buffer.stream,
            first: buffer.first,
            count: buffer.count,
            offset: 0,
            len: stored.len() as u32,
            raw: buffer.raw.len() as u32,
            kid,
            passthrough: buffer.group == FrameGroup::Passthrough,
            ordinal,
            crc,
        };
        self.write_frame(entry, &stored)
    }

    /// Write a frame header and data; `entry.offset` is filled in.
    fn write_frame(&mut self, mut entry: FrameEntry, stored: &[u8]) -> Result<(), SegmentError> {
        let kid_bytes = entry.kid.as_deref().unwrap_or_default().as_bytes().to_vec();
        if kid_bytes.len() > usize::from(u8::MAX) {
            return Err(invalid("frame key id is too long"));
        }
        let flags = if entry.passthrough {
            FLAG_PASSTHROUGH
        } else {
            0
        };
        let mut header = Vec::with_capacity(FRAME_HEADER_BYTES + kid_bytes.len());
        header.extend_from_slice(FRAME_MAGIC);
        header.push(entry.stream);
        header.push(kid_bytes.len() as u8);
        header.extend_from_slice(&[flags, 0]);
        header.extend_from_slice(&entry.first.to_le_bytes());
        header.extend_from_slice(&entry.count.to_le_bytes());
        header.extend_from_slice(&entry.raw.to_le_bytes());
        header.extend_from_slice(&(stored.len() as u32).to_le_bytes());
        header.extend_from_slice(&entry.ordinal.to_le_bytes());
        header.extend_from_slice(&entry.crc.to_le_bytes());
        header.extend_from_slice(&kid_bytes);
        self.write(&header)?;
        entry.offset = self.offset;
        entry.len = stored.len() as u32;
        self.write(stored)?;
        self.frames.push(entry);
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
        if self.content_next != [self.last + 1; 2] {
            self.abandon();
            return Err(invalid("segment content does not cover its records"));
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
            repacked: self.repacked.into_iter().collect(),
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

/// Push one classified record (see [`SealPolicy::classify`]).
fn push_classified(
    builder: &mut SegmentBuilder,
    classified: Classified,
) -> Result<(), SegmentError> {
    let Classified {
        stored,
        view,
        group,
    } = classified;
    let item = match (&group, view) {
        (FrameGroup::Sealed(kid), Some(view)) => {
            if encrypted_content(&stored.payload).is_some() {
                builder.note_repacked(kid);
            }
            Item::Sealed {
                kid: kid.clone(),
                payload: view.payload,
            }
        }
        _ => SealPolicy::item(&group, stored.payload.clone()),
    };
    builder.push(&stored, item)
}

/// Build segment `number` from the sealed plaintext files `sources`
/// (consecutive, in order), slimming covered streaming records and
/// encrypting what `policy` holds keys for. Two passes keep memory
/// bounded: the first collects the terminal ids, the second writes frames.
/// Blocking.
pub(super) fn build_from_plain(
    directory: &Path,
    conversation_id: &str,
    number: u64,
    sources: &[String],
    expected_first: u64,
    policy: &SealPolicy,
) -> Result<PreparedSegment, SegmentError> {
    let paths: Vec<PathBuf> = sources.iter().map(|name| directory.join(name)).collect();
    let mut plan = SlimPlan::default();
    for_each_plain_record(&paths, conversation_id, expected_first, |event| {
        let classified = policy.classify(event, conversation_id);
        plan.observe(classified.view.as_ref().unwrap_or(&classified.stored));
        Ok(())
    })?;
    let mut builder = SegmentBuilder::create(directory, conversation_id, number, policy)?;
    // Stripped records of the open run: written as markers once the run's
    // length is known. Only ids, times and their frame group are held.
    let mut run: Vec<(ConversationEvent, FrameGroup)> = Vec::new();
    let flush_run = |builder: &mut SegmentBuilder,
                     run: &mut Vec<(ConversationEvent, FrameGroup)>|
     -> Result<(), SegmentError> {
        let Some(start) = run.first().map(|(event, _)| event.sequence) else {
            return Ok(());
        };
        let length = run.len() as u64;
        for (original, group) in run.drain(..) {
            let marker = compacted_marker(&original, start, length);
            let item = SealPolicy::item(&group, marker.payload.clone());
            builder.push(&marker, item)?;
        }
        Ok(())
    };
    let built = for_each_plain_record(&paths, conversation_id, expected_first, |event| {
        let classified = policy.classify(event, conversation_id);
        builder.observe(&classified.stored);
        let stripped = classified
            .view
            .as_ref()
            .is_some_and(|view| plan.strips(view));
        if stripped {
            if let (FrameGroup::Sealed(kid), true) = (
                &classified.group,
                encrypted_content(&classified.stored.payload).is_some(),
            ) {
                builder.note_repacked(kid);
            }
            let mut slim = classified.stored;
            slim.payload = Value::Null;
            run.push((slim, classified.group));
            return Ok(());
        }
        flush_run(&mut builder, &mut run)?;
        push_classified(&mut builder, classified)
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

/// The envelope-only event an envelope line describes (digest input of a
/// record whose content stays in a copied sealed frame).
fn envelope_event(line: &[u8], conversation_id: &str) -> Result<ConversationEvent, SegmentError> {
    let record = parse_record(line)?;
    let payload = Value::Object(record.envelope().clone());
    record
        .into_event(conversation_id, Some(payload))
        .map_err(invalid)
}

/// Rewrite intact segment `number` with `policy` (end-to-end migration):
/// plaintext content is sealed under the migration key, ciphertext whose
/// DEK is in memory is repacked, sealed runs are copied as they are. No
/// slimming happens again. Blocking.
pub(super) fn rebuild_segment(
    directory: &Path,
    conversation_id: &str,
    number: u64,
    policy: &SealPolicy,
) -> Result<PreparedSegment, SegmentError> {
    let loaded = load_index(directory, number)?;
    let segment = loaded.segment;
    let mut file = std::fs::File::open(directory.join(segment_name(number)))?;
    let mut builder = SegmentBuilder::create(directory, conversation_id, number, policy)?;
    let result = (|| -> Result<(), SegmentError> {
        let mut decoded: [Option<(u64, DecodedFrame)>; 2] = [None, None];
        let mut item = |file: &mut std::fs::File,
                        slot: usize,
                        entry: &FrameEntry,
                        sequence: u64|
         -> Result<Vec<u8>, SegmentError> {
            if decoded[slot]
                .as_ref()
                .is_none_or(|(offset, _)| *offset != entry.offset)
            {
                decoded[slot] = Some((entry.offset, load_frame(file, entry)?));
            }
            let (_, frame) = decoded[slot].as_ref().expect("frame was just loaded");
            frame
                .item((sequence - entry.first) as usize)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| invalid(format!("frame lacks sequence {sequence}")))
        };
        let mut sequence = segment.first;
        while sequence <= segment.last {
            let envelope_entry = segment
                .frame(STREAM_ENVELOPE, sequence)
                .cloned()
                .ok_or_else(|| invalid(format!("no envelope holds sequence {sequence}")))?;
            let full = segment
                .frame(STREAM_FULL, sequence)
                .cloned()
                .ok_or_else(|| invalid(format!("no content holds sequence {sequence}")))?;
            if full.kid.is_some() {
                let summary = segment
                    .frame(STREAM_SUMMARY, sequence)
                    .cloned()
                    .ok_or_else(|| invalid("sealed run lacks its summary frame"))?;
                let summary_bytes = read_sealed(&mut file, &summary)?;
                let full_bytes = read_sealed(&mut file, &full)?;
                let end = full.first + u64::from(full.count);
                builder.push_verbatim((summary, summary_bytes), (full, full_bytes))?;
                while sequence < end {
                    let entry = segment
                        .frame(STREAM_ENVELOPE, sequence)
                        .cloned()
                        .ok_or_else(|| invalid("no envelope holds a sealed record"))?;
                    let line = item(&mut file, 0, &entry, sequence)?;
                    let event = envelope_event(&line, conversation_id)?;
                    builder.observe(&event);
                    builder.push(&event, Item::Verbatim)?;
                    sequence += 1;
                }
                continue;
            }
            let line = item(&mut file, 0, &envelope_entry, sequence)?;
            let content = item(&mut file, 1, &full, sequence)?;
            let record = parse_record(&line)?;
            let payload = if full.passthrough {
                let encrypted: Map<String, Value> = serde_json::from_slice(&content)?;
                let mut payload = record.envelope().clone();
                payload.insert(ENCRYPTED_FIELD.to_owned(), Value::Object(encrypted));
                Value::Object(payload)
            } else {
                serde_json::from_slice(&content)?
            };
            let event = record
                .into_event(conversation_id, Some(payload))
                .map_err(invalid)?;
            if event.sequence != sequence {
                return Err(invalid(format!(
                    "envelope frame holds sequence {} where {sequence} belongs",
                    event.sequence
                )));
            }
            let classified = policy.classify(event, conversation_id);
            builder.observe(&classified.stored);
            push_classified(&mut builder, classified)?;
            sequence += 1;
        }
        Ok(())
    })();
    match result {
        Ok(()) => builder.finish(Vec::new()),
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

/// Suffix of a replacement segment's files while [`commit_replacement`]
/// swaps them in.
const REPLACEMENT_SUFFIX: &str = ".next";

/// Replace existing segment `number` with a rebuild of it (end-to-end
/// migration), crash-safely: both new files are first renamed to
/// `events.NNNNNN.idx.next` / `.seg.next` and synced, then `.idx` and
/// `.seg` are replaced in that order. [`reconcile_directory`] finishes or
/// rolls back an interrupted swap: while `.idx.next` exists the old pair is
/// intact and the `.next` files are dropped; once it is gone the new index
/// is in place and `.seg.next` is renamed after it. Blocking; callers hold
/// the conversation lock.
pub(super) fn commit_replacement(
    directory: &Path,
    prepared: &PreparedSegment,
    stop_after: Option<CommitStep>,
) -> Result<(), SegmentError> {
    let stop = |step: CommitStep| stop_after == Some(step);
    let number = prepared.number;
    let next = |name: String| directory.join(format!("{name}{REPLACEMENT_SUFFIX}"));
    std::fs::rename(&prepared.temp_idx, next(index_name(number)))?;
    std::fs::rename(&prepared.temp_seg, next(segment_name(number)))?;
    sync_directory_blocking(directory)?;
    if stop(CommitStep::Written) {
        return Ok(());
    }
    std::fs::rename(next(index_name(number)), directory.join(index_name(number)))?;
    if stop(CommitStep::IndexRenamed) {
        return Ok(());
    }
    std::fs::rename(
        next(segment_name(number)),
        directory.join(segment_name(number)),
    )?;
    sync_directory_blocking(directory)?;
    Ok(())
}

/// Step boundaries of [`commit_prepared`] and [`commit_replacement`], for
/// crash-injection tests.
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

/// Restore the §4.1 invariants after a crash. An interrupted
/// [`commit_replacement`] is finished or rolled back first. Then for every
/// `.seg`/`.idx`: an `.idx` without its `.seg` is removed; a pair whose
/// plaintext sources still exist is kept (and the sources removed) only
/// when the whole `.seg` hashes to its index, otherwise the pair is
/// removed and the sources stay; a `.seg` whose index is unusable is
/// removed when a plaintext file of the same number exists (the conversion
/// is redone) and otherwise left for segment salvage. Leftover build
/// temporaries are removed unless `building` says a build is writing them.
/// Blocking.
pub(super) fn reconcile_directory(directory: &Path, building: bool) -> std::io::Result<Reconciled> {
    let mut outcome = Reconciled::default();
    let mut segs = Vec::new();
    let mut idxs = Vec::new();
    let mut plains = std::collections::HashSet::new();
    let mut replacements = BTreeSet::new();
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(outcome),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(base) = name.strip_suffix(REPLACEMENT_SUFFIX) {
            if let Some(number) = numbered(base, ".seg").or_else(|| numbered(base, ".idx")) {
                replacements.insert(number);
            }
        } else if let Some(number) = numbered(&name, ".seg") {
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
    for number in replacements {
        outcome.changed = true;
        let next_idx = format!("{}{REPLACEMENT_SUFFIX}", index_name(number));
        let next_seg = format!("{}{REPLACEMENT_SUFFIX}", segment_name(number));
        if directory.join(&next_idx).exists() || !directory.join(&next_seg).exists() {
            // The swap had not started: the old pair is intact.
            remove(next_idx)?;
            remove(next_seg)?;
            continue;
        }
        // The new index is in place; its segment follows it.
        std::fs::rename(
            directory.join(&next_seg),
            directory.join(segment_name(number)),
        )?;
        if !segs.contains(&number) {
            segs.push(number);
        }
    }
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
        let flags = header[6];
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
            passthrough: flags & FLAG_PASSTHROUGH != 0,
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

/// Rebuild segment `number` from whatever of it still reads (per-segment
/// salvage): frames are located through the `.idx` when it is usable, else
/// by walking frame headers. A plaintext or passthrough record survives
/// when its envelope and full content decode (the summary is regenerated);
/// a sealed run survives as a whole — both frames copied as they are —
/// when both pass their checksum and every envelope of the run decodes.
/// Every other sequence of `first..=last` becomes a `journal.recordLost`
/// placeholder naming `backup`. `last` defaults to the newest sequence any
/// envelope frame still holds. Never encrypts anything. Blocking.
pub(super) fn salvage_segment(
    directory: &Path,
    conversation_id: &str,
    number: u64,
    first: u64,
    last: Option<u64>,
    backup: &str,
) -> Result<SalvagedSegment, SegmentError> {
    let seg_path = directory.join(segment_name(number));
    let frames = match load_index(directory, number) {
        Ok(loaded) => loaded.body.frames,
        Err(_) => walk_frames(&seg_path)?,
    };
    let mut file = std::fs::File::open(&seg_path)?;
    let mut envelopes: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut contents: HashMap<u64, (bool, Vec<u8>)> = HashMap::new();
    // Intact sealed frames by (stream, first sequence).
    let mut sealed: HashMap<(u8, u64), (FrameEntry, Vec<u8>)> = HashMap::new();
    let mut newest = 0u64;
    // Frames decode one at a time; only records of the segment's range are
    // kept, which bounds memory by the segment's raw size.
    for frame in &frames {
        if frame.kid.is_some() {
            if let Ok(stored) = read_sealed(&mut file, frame) {
                sealed.insert((frame.stream, frame.first), (frame.clone(), stored));
            }
            continue;
        }
        if frame.stream == STREAM_SUMMARY {
            continue;
        }
        let Ok(decoded) = load_frame(&mut file, frame) else {
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
                contents.insert(sequence, (frame.passthrough, item.to_vec()));
            }
        }
    }
    let last = last.unwrap_or(newest);
    if last < first {
        return Err(invalid("nothing of the segment is readable"));
    }
    // Sealed runs whose two frames and every envelope survived.
    let mut runs: HashMap<u64, ((FrameEntry, Vec<u8>), (FrameEntry, Vec<u8>))> = HashMap::new();
    let full_starts: Vec<u64> = sealed
        .keys()
        .filter(|(stream, _)| *stream == STREAM_FULL)
        .map(|(_, start)| *start)
        .collect();
    for start in full_starts {
        let Some(full) = sealed.remove(&(STREAM_FULL, start)) else {
            continue;
        };
        let Some(summary) = sealed.remove(&(STREAM_SUMMARY, start)) else {
            continue;
        };
        let end = start + u64::from(full.0.count);
        let complete = summary.0.count == full.0.count
            && summary.0.kid == full.0.kid
            && start >= first
            && end <= last + 1
            && (start..end).all(|sequence| {
                envelopes.get(&sequence).is_some_and(|line| {
                    parse_record(line).is_ok_and(|record| record.sequence() == sequence)
                })
            });
        if complete {
            runs.insert(start, (summary, full));
        }
    }
    let readable = |sequence: u64,
                    envelopes: &HashMap<u64, Vec<u8>>,
                    contents: &HashMap<u64, (bool, Vec<u8>)>,
                    runs: &HashMap<u64, _>| {
        runs.contains_key(&sequence)
            || (envelopes.contains_key(&sequence) && contents.contains_key(&sequence))
    };
    let policy = SealPolicy::default();
    let mut builder = SegmentBuilder::create(directory, conversation_id, number, &policy)?;
    let mut lost = 0u64;
    let mut previous: Option<(DateTime<Utc>, Option<ProviderKind>)> = None;
    let mut sequence = first;
    let result = (|| -> Result<(), SegmentError> {
        while sequence <= last {
            if let Some((summary, full)) = runs.remove(&sequence) {
                let end = full.0.first + u64::from(full.0.count);
                builder.push_verbatim(summary, full)?;
                while sequence < end {
                    let line = envelopes.remove(&sequence).unwrap_or_default();
                    let event = envelope_event(&line, conversation_id)?;
                    previous = Some((event.time, event.provider));
                    builder.observe(&event);
                    builder.push(&event, Item::Verbatim)?;
                    sequence += 1;
                }
                continue;
            }
            let recovered = envelopes
                .remove(&sequence)
                .zip(contents.remove(&sequence))
                .and_then(|(envelope, (passthrough, content))| {
                    let record = parse_record(&envelope).ok()?;
                    if record.sequence() != sequence {
                        return None;
                    }
                    if passthrough {
                        let encrypted: Map<String, Value> =
                            serde_json::from_slice(&content).ok()?;
                        let mut payload = record.envelope().clone();
                        payload
                            .insert(ENCRYPTED_FIELD.to_owned(), Value::Object(encrypted.clone()));
                        let event = record
                            .into_event(conversation_id, Some(Value::Object(payload)))
                            .ok()?;
                        Some((event, Item::Passthrough(encrypted)))
                    } else {
                        let payload: Value = serde_json::from_slice(&content).ok()?;
                        let event = record
                            .into_event(conversation_id, Some(payload.clone()))
                            .ok()?;
                        Some((event, Item::Plain(payload)))
                    }
                });
            if let Some((event, item)) = recovered {
                previous = Some((event.time, event.provider));
                builder.observe(&event);
                builder.push(&event, item)?;
                sequence += 1;
                continue;
            }
            // A run of unreadable sequences shares one notice.
            let run_start = sequence;
            let mut run_end = sequence;
            while run_end < last && !readable(run_end + 1, &envelopes, &contents, &runs) {
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
                let payload = placeholder.payload.clone();
                builder.push(&placeholder, Item::Plain(payload))?;
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
            bytes.extend(super::super::record::encode_record(event).unwrap());
            bytes.push(b'\n');
        }
        std::fs::write(directory.join(name), bytes).unwrap();
    }

    fn read_all(directory: &Path, segment: &SealedSegment) -> Vec<ConversationEvent> {
        let cache = FrameCache::new(4 * 1024 * 1024);
        let mut reader = SegmentReader::open(directory, segment, &cache, CONVERSATION).unwrap();
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
            &SealPolicy::default(),
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
            &SealPolicy::default(),
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
        let mut reader =
            SegmentReader::open(&directory, &loaded.segment, &cache, CONVERSATION).unwrap();
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
