//! One camera session: samples its camera every
//! [`engine::SAMPLE_INTERVAL_MS`], keeps the frames the [`engine::Pacer`]
//! picks as `frame_<n>_t<MM>m<SS>s<mmm>.jpg` files, and writes
//! `manifest.json` when it ends.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::json;
use tokio::sync::watch;
use uuid::Uuid;

use super::{
    encode_jpeg,
    engine::{self, ActivityLog, CaptureSettings, ChangeMeter, KeepReason, Pacer},
    overview::{self, Collector, OverviewImage},
    CameraDevice, Frame, StreamUse, FRAME_MAX_WIDTH, FRAME_QUALITY,
};
use crate::{provider::ConversationSupervisor, secure_fs};

/// No new frame for this long: the camera is gone (unplugged, taken).
const STALL_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const MANIFEST_FILE: &str = "manifest.json";

/// Why a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionEnd {
    /// `camera_stop`.
    Agent,
    TimeLimit,
    FrameLimit,
    /// The conversation's turn ended (completed, failed or cancelled).
    TurnEnded,
    /// Taken back: settings, a revoke, the conversation deleted.
    Revoked,
    /// The stop button.
    User,
    /// The camera stopped delivering frames.
    CameraLost,
    /// Frames could not be saved.
    Failed,
}

impl SessionEnd {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::TimeLimit => "time_limit",
            Self::FrameLimit => "frame_limit",
            Self::TurnEnded => "turn_ended",
            Self::Revoked => "revoked",
            Self::User => "user",
            Self::CameraLost => "camera_lost",
            Self::Failed => "failed",
        }
    }

    /// For the agent.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::Agent => "stopped by camera_stop",
            Self::TimeLimit => "reached its time limit",
            Self::FrameLimit => "reached its frame limit",
            Self::TurnEnded => "ended with the turn",
            Self::Revoked => "camera access was taken back",
            Self::User => "stopped by the user",
            Self::CameraLost => "the camera stopped delivering pictures",
            Self::Failed => "frames could not be saved",
        }
    }
}

/// How far a running session got.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Progress {
    pub frames: usize,
    pub elapsed_ms: u64,
    /// Change bitrate of the latest sample.
    pub change_kbps: f64,
    /// Capture rate in effect.
    pub rate_fps: f64,
}

/// One kept frame, as `manifest.json` lists it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FrameRecord {
    pub file: String,
    /// Since the session started.
    pub offset_ms: u64,
    /// Wall-clock time (RFC 3339).
    pub at: String,
    pub change_kbps: f64,
    pub rate_fps: f64,
    pub reason: KeepReason,
}

/// An ended session.
#[derive(Debug)]
pub(crate) struct Summary {
    pub reason: SessionEnd,
    pub frames: Vec<FrameRecord>,
    pub duration_ms: u64,
    pub manifest: PathBuf,
    pub activity: Vec<engine::Activity>,
    pub threshold_kbps: f64,
    /// What `camera_stop` shows inline (see [`overview`]).
    pub overview: Vec<OverviewImage>,
    pub error: Option<String>,
    pub ended: Instant,
}

pub(crate) struct Session {
    pub id: String,
    pub conversation_id: String,
    pub device: CameraDevice,
    pub capture: CaptureSettings,
    pub max_seconds: u32,
    pub max_frames: u32,
    /// Where the frames go; readable by the agent.
    pub dir: PathBuf,
    pub started: Instant,
    pub started_at: jiff::Timestamp,
    stop: watch::Sender<Option<SessionEnd>>,
    progress: Mutex<Progress>,
    done: watch::Sender<Option<Arc<Summary>>>,
}

impl Session {
    pub(crate) fn new(
        conversation_id: &str,
        device: CameraDevice,
        capture: CaptureSettings,
        max_seconds: u32,
        max_frames: u32,
        dir: PathBuf,
    ) -> Self {
        Self {
            id: format!("cam_{}", Uuid::new_v4().simple()),
            conversation_id: conversation_id.to_owned(),
            device,
            capture,
            max_seconds,
            max_frames,
            dir,
            started: Instant::now(),
            started_at: jiff::Timestamp::now(),
            stop: watch::channel(None).0,
            progress: Mutex::new(Progress::default()),
            done: watch::channel(None).0,
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.done.borrow().is_none()
    }

    /// Asks the session to end (the first reason wins).
    pub(crate) fn stop(&self, reason: SessionEnd) {
        self.stop.send_if_modified(|current| {
            if current.is_none() {
                *current = Some(reason);
                true
            } else {
                false
            }
        });
    }

    /// Waits for the end (frames and manifest written).
    pub(crate) async fn ended(&self) -> Arc<Summary> {
        let mut done = self.done.subscribe();
        loop {
            if let Some(summary) = done.borrow_and_update().clone() {
                return summary;
            }
            if done.changed().await.is_err() {
                // The sender lives as long as the session.
                std::future::pending::<()>().await;
            }
        }
    }

    /// How long ago it ended.
    pub(crate) fn ended_for(&self) -> Option<Duration> {
        self.done
            .borrow()
            .as_ref()
            .map(|summary| summary.ended.elapsed())
    }

    pub(crate) fn progress(&self) -> Progress {
        let mut progress = self
            .progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if self.is_running() {
            progress.elapsed_ms = self.started.elapsed().as_millis() as u64;
        }
        progress
    }

    fn set_progress(&self, progress: Progress) {
        *self
            .progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = progress;
    }

    fn timestamp(&self, offset_ms: u64) -> String {
        self.started_at
            .checked_add(jiff::SignedDuration::from_millis(offset_ms as i64))
            .unwrap_or(self.started_at)
            .to_string()
    }
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// A sample not kept yet: the session's last frame if nothing newer comes.
struct Newest {
    at_ms: u64,
    frame: Frame,
    change_kbps: f64,
    rate_fps: f64,
}

/// Records until stopped, a limit, or the camera goes away.
pub(super) async fn record(
    session: Arc<Session>,
    camera: StreamUse,
    conversations: ConversationSupervisor,
) {
    let mut stop = session.stop.subscribe();
    let mut interval = tokio::time::interval(Duration::from_millis(engine::SAMPLE_INTERVAL_MS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut meter = ChangeMeter::default();
    let mut pacer = Pacer::new(session.capture);
    let mut activity = ActivityLog::default();
    let mut frames: Vec<FrameRecord> = Vec::new();
    let mut collector = Collector::default();
    let mut newest: Option<Newest> = None;
    let mut seq = 0u64;
    let mut last_new = Instant::now();
    let mut error = None;
    let limit_ms = u64::from(session.max_seconds) * 1000;
    let reason = loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = stop.changed() => {}
        }
        if let Some(reason) = *stop.borrow() {
            break reason;
        }
        let at_ms = session.started.elapsed().as_millis() as u64;
        if at_ms >= limit_ms {
            break SessionEnd::TimeLimit;
        }
        let frame = match camera.stream.latest(seq) {
            Ok(Some((next, frame))) => {
                seq = next;
                last_new = Instant::now();
                frame
            }
            Ok(None) => {
                if last_new.elapsed() >= STALL_TIMEOUT {
                    break SessionEnd::CameraLost;
                }
                continue;
            }
            Err(failure) => {
                error = Some(failure.to_string());
                break SessionEnd::CameraLost;
            }
        };
        let (frame, thumb) = match tokio::task::spawn_blocking(move || {
            let thumb = engine::thumbnail(&frame.bgra, frame.width, frame.height);
            (frame, thumb)
        })
        .await
        {
            Ok(measured) => measured,
            Err(failure) => {
                error = Some(failure.to_string());
                break SessionEnd::Failed;
            }
        };
        let change_kbps = meter.sample(at_ms, thumb);
        activity.sample(at_ms, change_kbps, pacer.threshold());
        let decision = pacer.decide(at_ms, change_kbps);
        session.set_progress(Progress {
            frames: frames.len(),
            elapsed_ms: at_ms,
            change_kbps: round1(change_kbps),
            rate_fps: decision.rate_fps,
        });
        let Some(keep) = decision.keep else {
            newest = Some(Newest {
                at_ms,
                frame,
                change_kbps,
                rate_fps: decision.rate_fps,
            });
            continue;
        };
        newest = None;
        match save(
            &session,
            frames.len() + 1,
            at_ms,
            frame,
            change_kbps,
            decision.rate_fps,
            keep,
        )
        .await
        {
            Ok((record, extras)) => {
                collector.add(frames.len() + 1, record.offset_ms, extras);
                frames.push(record);
            }
            Err(failure) => {
                error = Some(failure);
                break SessionEnd::Failed;
            }
        }
        if frames.len() >= session.max_frames as usize {
            break SessionEnd::FrameLimit;
        }
    };
    if let Some(last) =
        newest.filter(|_| frames.len() < session.max_frames as usize && error.is_none())
    {
        match save(
            &session,
            frames.len() + 1,
            last.at_ms,
            last.frame,
            last.change_kbps,
            last.rate_fps,
            KeepReason::Last,
        )
        .await
        {
            Ok((record, extras)) => {
                collector.add(frames.len() + 1, record.offset_ms, extras);
                frames.push(record);
            }
            Err(failure) => error = Some(failure),
        }
    }
    drop(camera);
    let overview = write_overview(&session, collector).await;
    let duration_ms = session
        .started
        .elapsed()
        .as_millis()
        .min(u128::from(limit_ms)) as u64;
    let manifest = session.dir.join(MANIFEST_FILE);
    let summary = Summary {
        reason,
        duration_ms,
        manifest: manifest.clone(),
        activity: activity.spans().to_vec(),
        threshold_kbps: pacer.threshold(),
        overview,
        frames,
        error,
        ended: Instant::now(),
    };
    let document = manifest_json(&session, &summary);
    let summary = match tokio::task::spawn_blocking(move || {
        secure_fs::write_owner_only_atomic(
            &manifest,
            &serde_json::to_vec_pretty(&document).unwrap_or_default(),
        )
    })
    .await
    {
        Ok(Ok(())) => summary,
        Ok(Err(failure)) => Summary {
            error: Some(format!("cannot write the manifest: {failure}")),
            ..summary
        },
        Err(failure) => Summary {
            error: Some(failure.to_string()),
            ..summary
        },
    };
    session.set_progress(Progress {
        frames: summary.frames.len(),
        elapsed_ms: summary.duration_ms,
        ..Progress::default()
    });
    let event = json!({
        "status": "ended",
        "sessionId": session.id,
        "reason": summary.reason.as_str(),
        "frames": summary.frames.len(),
        "durationMs": summary.duration_ms,
        "directory": session.dir,
    });
    session.done.send_replace(Some(Arc::new(summary)));
    if let Err(failure) = conversations
        .append_agent_event(&session.conversation_id, "desktop.camera.session", event)
        .await
    {
        tracing::warn!(error = %failure, "failed to journal the end of a camera session");
    }
}

/// Builds the overview and saves its sheets beside the frames. A failure
/// only costs the overview: the frames and the manifest still go out.
async fn write_overview(session: &Session, collector: Collector) -> Vec<OverviewImage> {
    let dir = session.dir.clone();
    let built = tokio::task::spawn_blocking(move || -> Result<Vec<OverviewImage>, String> {
        let images = collector.finish().map_err(|error| error.to_string())?;
        for image in &images {
            if let Some(file) = &image.file {
                secure_fs::write_owner_only_atomic(&dir.join(file), &image.jpeg)
                    .map_err(|error| format!("cannot save {file}: {error}"))?;
            }
        }
        Ok(images)
    })
    .await
    .map_err(|error| error.to_string())
    .and_then(|built| built);
    built.unwrap_or_else(|error| {
        tracing::warn!(%error, "camera session overview failed");
        Vec::new()
    })
}

/// Saves frame `number` and returns its record and its overview share.
async fn save(
    session: &Session,
    number: usize,
    at_ms: u64,
    frame: Frame,
    change_kbps: f64,
    rate_fps: f64,
    reason: KeepReason,
) -> Result<(FrameRecord, overview::Extras), String> {
    let file = engine::frame_name(number, at_ms);
    let path = session.dir.join(&file);
    let extras = tokio::task::spawn_blocking(move || -> Result<overview::Extras, String> {
        let jpeg = encode_jpeg(&frame, FRAME_MAX_WIDTH, FRAME_QUALITY)
            .map_err(|error| error.to_string())?;
        secure_fs::write_owner_only_atomic(&path, &jpeg)
            .map_err(|error| format!("cannot save a frame: {error}"))?;
        let small = number <= overview::INLINE_FRAMES_MAX;
        Ok(overview::extras(&frame, small).unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot shrink a camera frame for the overview");
            overview::Extras {
                cell: overview::fit(&frame, overview::CELL_WIDTH, overview::CELL_HEIGHT),
                small: None,
            }
        }))
    })
    .await
    .map_err(|error| error.to_string())??;
    let record = FrameRecord {
        file,
        offset_ms: at_ms,
        at: session.timestamp(at_ms),
        change_kbps: round1(change_kbps),
        rate_fps,
        reason,
    };
    Ok((record, extras))
}

fn manifest_json(session: &Session, summary: &Summary) -> serde_json::Value {
    json!({
        "sessionId": session.id,
        "camera": session.device,
        "startedAt": session.started_at.to_string(),
        "endedAt": session.timestamp(summary.duration_ms),
        "durationMs": summary.duration_ms,
        "endReason": summary.reason.as_str(),
        "capture": session.capture,
        "thresholdKbps": round1(summary.threshold_kbps),
        "frameCount": summary.frames.len(),
        "frames": summary.frames,
        "activity": summary.activity,
        "overview": summary
            .overview
            .iter()
            .filter(|image| image.file.is_some())
            .map(|image| json!({ "file": image.file, "frames": image.frames }))
            .collect::<Vec<_>>(),
        "notes": "offsetMs (also the tNNmNNsNNN part of each file name) is the time since the session started. \
                  Frames are unevenly spaced: dynamic capture keeps more while the picture changes. \
                  changeKbps is how much the picture changed around that frame. \
                  Each overview sheet is a grid of frames labelled #<frame number> <mm:ss.s>.",
    })
}
