//! `camera_*` tools of the `todex_desktop` server: the agent camera on the
//! daemon's own host ([`crate::camera`]).
//!
//! `camera_start` asks any paired device (a `desktop_camera` card with a
//! live preview, where the person picks the camera and how frames are
//! captured) unless the conversation was allowed a camera for the rest of
//! it. Plan mode refuses it; every other mode still asks, as a camera is
//! never something an agent turns on unasked. The session records until
//! `camera_stop`, a limit, the end of the turn or a revoke. Its result
//! shows an overview inline ([`crate::camera::overview`]): the frames
//! themselves when few, else labelled overview sheets. The full-size frames
//! are files the agent opens only where it needs detail; long sessions are
//! best handed to sub-agents.

use std::{path::PathBuf, time::Duration};

use rmcp::model::{CallToolResult, ContentBlock, Tool, ToolAnnotations};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{
    authorizer::{Answerer, Prompt, ToolMode},
    desktop_server::{DesktopCall, DesktopTools},
    registry::{parse, schema, tool_error, Handler, Invocation, Prepared, ToolEntry},
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

use crate::camera::{
    engine::{self, CaptureMode, CaptureSettings},
    overview::{INLINE_FRAMES_MAX, SHEETS_MAX, SUBAGENT_FRAMES_MIN},
    CameraDevice, CameraGrant, CameraPermission, Session, SessionEnd, Summary, MAX_SECONDS_RANGE,
};

/// `permission.requested` kind of the camera card.
pub(super) const CAMERA_KIND: &str = "desktop_camera";
const GRANT_KEY: &str = "camera-grant";
/// The longest `camera_stop` lets a session run on before stopping it.
pub(super) const MAX_WAIT_SECONDS: u64 = 600;
/// How long the end of a session (last frame, manifest) may take.
const END_TIMEOUT: Duration = Duration::from_secs(30);

/// How to read the frames once the overview is in view, by how many
/// there are.
pub(super) fn frames_guidance(frames: usize) -> &'static str {
    if frames <= INLINE_FRAMES_MAX {
        "Every frame is attached below in order (512 px wide). Open a file only when you need \
         more detail than that."
    } else if frames <= SUBAGENT_FRAMES_MIN {
        "The overview sheets attached below show the session at a glance: each cell is labelled \
         #<frame number> <mm:ss.s since the start>, and frame #n is the file whose name starts \
         with frame_<n, 4 digits>. Use them to find the moments that matter, then open only those \
         frame files (1280 px) for detail. Do not open every frame."
    } else {
        "The overview sheets attached below show the session at a glance: each cell is labelled \
         #<frame number> <mm:ss.s since the start>, and frame #n is the file whose name starts \
         with frame_<n, 4 digits>. There are too many frames to open yourself without filling \
         your context: use the sheets and manifest.json (times, change bitrate, activity ranges) \
         to pick ranges, and hand those frames to sub-agents in batches (for example one activity \
         range or 10 to 20 frames each), asking each to report in text what happens and when. \
         Combine their reports. Without sub-agents, open only a few frames per range yourself."
    }
}

pub(super) enum CameraCall {
    List,
    Start {
        device_id: Option<String>,
        max_seconds: Option<u32>,
    },
    Stop {
        session_id: Option<String>,
        wait_seconds: u64,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StartArgs {
    #[serde(default)]
    device_id: Option<String>,
    #[serde(default)]
    max_seconds: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StopArgs {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    wait_seconds: Option<u64>,
}

/// What the camera card sends back with an allow.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CardChoice {
    #[serde(default)]
    device_id: Option<String>,
    #[serde(default)]
    capture: Option<CaptureSettings>,
}

fn validate(name: &str, arguments: Value) -> Result<(CameraCall, String), String> {
    match name {
        "camera_list" => {
            let _: ListArgs = parse(name, arguments)?;
            Ok((CameraCall::List, "list cameras".to_owned()))
        }
        "camera_start" => {
            let args: StartArgs = parse(name, arguments)?;
            if let Some(seconds) = args.max_seconds {
                if !(MAX_SECONDS_RANGE.0..=MAX_SECONDS_RANGE.1).contains(&seconds) {
                    return Err(format!(
                        "maxSeconds must be between {} and {}",
                        MAX_SECONDS_RANGE.0, MAX_SECONDS_RANGE.1
                    ));
                }
            }
            if args.device_id.as_deref().is_some_and(str::is_empty) {
                return Err("deviceId must not be empty".to_owned());
            }
            Ok((
                CameraCall::Start {
                    device_id: args.device_id,
                    max_seconds: args.max_seconds,
                },
                "start the camera".to_owned(),
            ))
        }
        "camera_stop" => {
            let args: StopArgs = parse(name, arguments)?;
            let wait_seconds = args.wait_seconds.unwrap_or(0);
            if wait_seconds > MAX_WAIT_SECONDS {
                return Err(format!("waitSeconds is limited to {MAX_WAIT_SECONDS}"));
            }
            Ok((
                CameraCall::Stop {
                    session_id: args.session_id,
                    wait_seconds,
                },
                "stop the camera".to_owned(),
            ))
        }
        other => Err(format!("unknown tool {other}")),
    }
}

fn camera(name: &str, arguments: Value) -> Result<Prepared<DesktopCall>, String> {
    let (call, summary) = validate(name, arguments)?;
    Ok(Prepared {
        escapes_sandbox: false,
        input: json!({ "action": summary }),
        summary,
        call: DesktopCall::Camera(call),
    })
}

fn prepare_list(arguments: Value) -> Result<Prepared<DesktopCall>, String> {
    camera("camera_list", arguments)
}

fn prepare_start(arguments: Value) -> Result<Prepared<DesktopCall>, String> {
    camera("camera_start", arguments)
}

fn prepare_stop(arguments: Value) -> Result<Prepared<DesktopCall>, String> {
    camera("camera_stop", arguments)
}

pub(super) fn tools(
    run: Handler<DesktopTools, DesktopCall>,
) -> Vec<ToolEntry<DesktopTools, DesktopCall>> {
    // Camera approvals are their own card in every permission mode, so the
    // mode's generic per-call approval is not added on top
    // (`side_effect: false`); Plan mode is refused in `camera_start`.
    let entry = |tool: Tool, prepare| ToolEntry {
        tool,
        side_effect: false,
        prepare,
        run,
    };
    vec![
        entry(
            Tool::new(
                "camera_list",
                "List the cameras on the TodeX backend's computer that the user lets agents use, the \
                 default one, and any camera session of this conversation that is running.",
                schema(json!({ "type": "object", "properties": {}, "additionalProperties": false })),
            )
            .with_annotations(ToolAnnotations::new().read_only(true).open_world(false)),
            prepare_list,
        ),
        entry(
            Tool::new(
                "camera_start",
                "Start recording a camera on the TodeX backend's computer, to see what happens in front \
                 of it (a device, a screen, a person demonstrating something). The user approves each \
                 use on a card with a live preview and picks the camera and how frames are captured: \
                 fixed (evenly spaced) or dynamic (frames only while the picture changes, more the more \
                 it changes). Returns at once with a session id while recording goes on; tell the user \
                 what to do in front of the camera, then call camera_stop to end it and get the frames. \
                 Recording also ends at the time or frame limit and when your turn ends. Unavailable in \
                 Plan mode.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "deviceId": { "type": "string", "description": "Camera id from camera_list; default: the user's default camera. The user may pick another." },
                        "maxSeconds": { "type": "integer", "minimum": MAX_SECONDS_RANGE.0, "maximum": MAX_SECONDS_RANGE.1, "description": "Stop after this long; capped by the user's limit." }
                    },
                    "additionalProperties": false
                })),
            )
            .with_annotations(ToolAnnotations::new().read_only(false).open_world(true)),
            prepare_start,
        ),
        entry(
            Tool::new(
                "camera_stop",
                format!(
                    "End the camera session (this conversation's newest one unless sessionId is given) \
                     and get its frames: a directory of JPEG files named \
                     frame_<n>_t<MM>m<SS>s<mmm>.jpg (the time since the session started) plus \
                     manifest.json, and inline an overview: the frames themselves when there are \
                     at most {INLINE_FRAMES_MAX}, else up to {SHEETS_MAX} sheets of labelled \
                     thumbnails. waitSeconds (max {MAX_WAIT_SECONDS}) keeps recording that much \
                     longer first, e.g. while the user demonstrates something; the call returns \
                     early if the session ends on its own. Also collects a session that already \
                     ended. Open full frames only where the overview leaves a question; for long \
                     sessions have sub-agents look at them."
                ),
                schema(json!({
                    "type": "object",
                    "properties": {
                        "sessionId": { "type": "string" },
                        "waitSeconds": { "type": "integer", "minimum": 0, "maximum": MAX_WAIT_SECONDS }
                    },
                    "additionalProperties": false
                })),
            )
            .with_annotations(ToolAnnotations::new().read_only(false).open_world(false)),
            prepare_stop,
        ),
    ]
}

fn duration_text(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}.{}s", seconds, (ms % 1000) / 100)
    }
}

/// What `camera_stop` tells the agent about an ended session.
pub(super) fn summary_text(session: &Session, summary: &Summary) -> String {
    let mut text = format!(
        "Camera session {} on \"{}\" ended ({}) after {}.\nCapture: {}.\n",
        session.id,
        session.device.name,
        summary.reason.describe(),
        duration_text(summary.duration_ms),
        session.capture.describe(),
    );
    if let Some(error) = &summary.error {
        text.push_str(&format!("Problem: {error}\n"));
    }
    if summary.frames.is_empty() {
        text.push_str("No frames were kept.\n");
        return text;
    }
    text.push_str(&format!(
        "{} frame(s) in {}\nManifest: {}\n",
        summary.frames.len(),
        session.dir.display(),
        summary.manifest.display(),
    ));
    let first = &summary.frames[0];
    let last = &summary.frames[summary.frames.len() - 1];
    text.push_str(&format!(
        "Files: {} … {}. The t part of each name is the time since the session started; frames are \
         unevenly spaced when capture is dynamic.\n",
        first.file, last.file
    ));
    if session.capture.mode == CaptureMode::Dynamic {
        if summary.activity.is_empty() {
            text.push_str("Activity: the picture barely changed.\n");
        } else {
            text.push_str("Activity (the picture kept changing):");
            for span in &summary.activity {
                let frames = summary
                    .frames
                    .iter()
                    .filter(|frame| {
                        (span.from_ms..=span.to_ms + engine::HOLD_MS).contains(&frame.offset_ms)
                    })
                    .count();
                text.push_str(&format!(
                    "\n- {}–{}: {} frame(s), peak {:.0} kbps",
                    engine::clock(span.from_ms),
                    engine::clock(span.to_ms),
                    frames,
                    span.peak_kbps
                ));
            }
            text.push('\n');
        }
    }
    let sheets: Vec<&str> = summary
        .overview
        .iter()
        .filter_map(|image| image.file.as_deref())
        .collect();
    if !sheets.is_empty() {
        let shown: usize = summary
            .overview
            .iter()
            .map(|image| image.frames.len())
            .sum();
        text.push_str(&format!(
            "Overview: {} sheet(s) ({}) showing {} of the {} frames, evenly spread.\n",
            sheets.len(),
            sheets.join(", "),
            shown,
            summary.frames.len(),
        ));
    }
    text.push('\n');
    if summary.overview.is_empty() {
        // No overview (making it failed): the files are all there is.
        text.push_str(
            "No overview could be attached. Read manifest.json (times, change bitrate, activity \
             ranges) and open only the frames you need; with many frames, have sub-agents look \
             at them in batches and report in text what happens and when.",
        );
    } else {
        text.push_str(frames_guidance(summary.frames.len()));
    }
    text
}

/// `camera_stop`'s result: the text, then the overview images.
pub(super) fn summary_result(session: &Session, summary: &Summary) -> CallToolResult {
    let mut content = vec![ContentBlock::text(summary_text(session, summary))];
    for image in &summary.overview {
        content.push(ContentBlock::image(
            BASE64.encode(&image.jpeg),
            "image/jpeg",
        ));
    }
    CallToolResult::success(content)
}

impl DesktopTools {
    pub(super) async fn run_camera(
        &self,
        call: CameraCall,
        invocation: &Invocation<'_>,
    ) -> CallToolResult {
        let conversation_id = &invocation.caller.conversation_id;
        match call {
            CameraCall::List => self.camera_list(conversation_id).await,
            CameraCall::Start {
                device_id,
                max_seconds,
            } => self
                .camera_start(conversation_id, device_id, max_seconds, invocation)
                .await
                .unwrap_or_else(tool_error),
            CameraCall::Stop {
                session_id,
                wait_seconds,
            } => self
                .camera_stop(
                    conversation_id,
                    session_id.as_deref(),
                    wait_seconds,
                    invocation,
                )
                .await
                .unwrap_or_else(tool_error),
        }
    }

    /// Allowed cameras that are connected now, in the user's order.
    async fn offered_cameras(&self) -> Result<Vec<CameraDevice>, String> {
        let camera = self.mcp.desktop().camera();
        let settings = camera.settings().await;
        let connected = camera
            .host()
            .devices()
            .await
            .map_err(|error| error.to_string())?;
        Ok(settings
            .allowed
            .iter()
            .filter_map(|allowed| {
                connected
                    .iter()
                    .find(|device| device.id == allowed.id)
                    .cloned()
            })
            .collect())
    }

    async fn camera_list(&self, conversation_id: &str) -> CallToolResult {
        let camera = self.mcp.desktop().camera();
        if let Some(reason) = camera.host().unsupported_reason() {
            return tool_error(format!("UNSUPPORTED: {reason}"));
        }
        let settings = camera.settings().await;
        let offered = match self.offered_cameras().await {
            Ok(offered) => offered,
            Err(error) => return tool_error(error),
        };
        let preferred = settings.preferred().map(|camera| camera.id.clone());
        let cameras: Vec<Value> = offered
            .iter()
            .map(|device| {
                json!({
                    "id": device.id,
                    "name": device.name,
                    "default": preferred.as_deref() == Some(device.id.as_str()),
                })
            })
            .collect();
        let running = camera.running_for(conversation_id).map(|session| {
            json!({ "sessionId": session.id, "camera": session.device.name, "frames": session.progress().frames })
        });
        let mut result = json!({
            "cameras": cameras,
            "capture": settings.capture.describe(),
            "maxSeconds": settings.max_seconds,
            "maxFrames": settings.max_frames,
        });
        if let Some(running) = running {
            result["running"] = running;
        }
        if offered.is_empty() {
            result["note"] = Value::from(if settings.allowed.is_empty() {
                "The user has not allowed any camera for agents; they can in TodeX settings (Agent desktop > Camera)."
            } else {
                "None of the cameras the user allowed is connected."
            });
        }
        CallToolResult::success(vec![ContentBlock::text(result.to_string())])
    }

    async fn camera_start(
        &self,
        conversation_id: &str,
        requested: Option<String>,
        max_seconds: Option<u32>,
        invocation: &Invocation<'_>,
    ) -> Result<CallToolResult, String> {
        if self.authorizer().state.tool_mode(conversation_id) == ToolMode::Plan {
            return Err(
                "PLAN_MODE: camera_start is not available while the conversation is in Plan mode; \
                 describe the step in your plan instead."
                    .to_owned(),
            );
        }
        let camera = self.mcp.desktop().camera().clone();
        let host = camera.host();
        if let Some(reason) = host.unsupported_reason() {
            return Err(format!("UNSUPPORTED: {reason}"));
        }
        match host.permission() {
            CameraPermission::Granted => {}
            CameraPermission::NotDetermined => {
                return Err(
                    "NO_PERMISSION: macOS has not given the TodeX backend camera access yet. Ask the \
                     user to grant it in TodeX settings (Agent desktop > Camera); macOS then asks at \
                     this computer."
                        .to_owned(),
                )
            }
            CameraPermission::Denied | CameraPermission::Restricted => {
                return Err(
                    "NO_PERMISSION: camera access for the TodeX backend (todex-agentd) is turned off \
                     in macOS (System Settings > Privacy & Security > Camera). Ask the user to turn \
                     it on."
                        .to_owned(),
                )
            }
        }
        if let Some(session) = camera.running_for(conversation_id) {
            return Err(format!(
                "ALREADY_RUNNING: camera session {} is still recording; call camera_stop first.",
                session.id
            ));
        }
        let settings = camera.settings().await;
        let offered = self.offered_cameras().await?;
        if offered.is_empty() {
            return Err(if settings.allowed.is_empty() {
                "NO_CAMERA: the user has not allowed any camera for agents; they can in TodeX \
                 settings (Agent desktop > Camera)."
                    .to_owned()
            } else {
                "NO_CAMERA: none of the cameras the user allowed is connected.".to_owned()
            });
        }
        if let Some(id) = &requested {
            if !offered.iter().any(|device| &device.id == id) {
                let ids: Vec<&str> = offered.iter().map(|device| device.id.as_str()).collect();
                return Err(format!(
                    "INVALID_ARGUMENT: {id} is not an allowed, connected camera; use camera_list (allowed: {})",
                    ids.join(", ")
                ));
            }
        }
        let max_seconds = max_seconds
            .unwrap_or(settings.max_seconds)
            .min(settings.max_seconds);
        let max_frames = settings.max_frames;

        let granted = camera.grant(conversation_id).filter(|grant| {
            requested.as_ref().is_none_or(|id| id == &grant.device.id)
                && offered.iter().any(|device| device.id == grant.device.id)
        });
        let (device, capture) = match granted {
            Some(grant) => (grant.device, grant.capture),
            None => {
                let suggested = requested
                    .clone()
                    .or_else(|| {
                        settings
                            .preferred()
                            .map(|camera| camera.id.clone())
                            .filter(|id| offered.iter().any(|device| &device.id == id))
                    })
                    .unwrap_or_else(|| offered[0].id.clone());
                self.ask_camera(
                    conversation_id,
                    &offered,
                    &suggested,
                    settings.capture,
                    max_seconds,
                    max_frames,
                    invocation,
                )
                .await?
            }
        };
        if let Some(busy) = camera.running_on(&device.id) {
            if busy.conversation_id != conversation_id {
                return Err(format!(
                    "CAMERA_BUSY: \"{}\" is recording for another conversation; try again later or use another camera.",
                    device.name
                ));
            }
        }
        let dir = self.camera_dir(conversation_id).await?;
        let session = camera
            .start(
                self.conversations.clone(),
                conversation_id,
                device.clone(),
                capture,
                max_seconds,
                max_frames,
                dir.clone(),
            )
            .await
            .map_err(|error| error.to_string())?;
        if let Err(error) = self
            .conversations
            .append_agent_event(
                conversation_id,
                "desktop.camera.session",
                json!({
                    "status": "started",
                    "sessionId": session.id,
                    "deviceId": device.id,
                    "deviceName": device.name,
                    "capture": capture,
                    "maxSeconds": max_seconds,
                    "maxFrames": max_frames,
                    "directory": dir,
                }),
            )
            .await
        {
            tracing::warn!(%error, "failed to journal a camera session start");
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "Camera session {} is recording \"{}\" (capture: {}, chosen by the user; at most {} s and \
             {} frames). Frames go to {}. Tell the user what to show, then call camera_stop (with \
             waitSeconds to keep recording a while first). Recording also ends with your turn.",
            session.id,
            device.name,
            capture.describe(),
            max_seconds,
            max_frames,
            dir.display(),
        ))]))
    }

    /// The camera card: the person picks the camera and the capture
    /// settings, and allows once or for the rest of the conversation.
    #[allow(clippy::too_many_arguments)]
    async fn ask_camera(
        &self,
        conversation_id: &str,
        offered: &[CameraDevice],
        suggested: &str,
        capture: CaptureSettings,
        max_seconds: u32,
        max_frames: u32,
        invocation: &Invocation<'_>,
    ) -> Result<(CameraDevice, CaptureSettings), String> {
        let camera = self.mcp.desktop().camera();
        let authorizer = self.authorizer();
        let _asking = authorizer
            .serialize(conversation_id, GRANT_KEY, &invocation.cancel)
            .await
            .map_err(|denied| denied.to_string())?;
        let host = crate::computer::host_name();
        let title = format!("Allow the agent to use a camera on {host}?");
        camera.set_pending(
            conversation_id,
            offered.iter().map(|device| device.id.clone()).collect(),
        );
        let answer = authorizer
            .ask(
                conversation_id,
                Prompt {
                    key: GRANT_KEY.to_owned(),
                    answerer: Answerer::AnyDevice,
                    kind: CAMERA_KIND,
                    message: title.clone(),
                    title,
                    details: json!({
                        "host": host,
                        "devices": offered,
                        "deviceId": suggested,
                        "capture": capture,
                        "maxSeconds": max_seconds,
                        "maxFrames": max_frames,
                        "limits": {
                            "fps": { "min": engine::MIN_FIXED_FPS, "max": engine::MAX_FIXED_FPS },
                            "sensitivity": { "min": 1, "max": 100 },
                        },
                    }),
                    options: json!([
                        { "id": "once", "kind": "allow_once", "name": "Allow once" },
                        { "id": "conversation", "kind": "allow_always", "name": "Allow for this conversation" },
                        { "id": "reject", "kind": "reject_once", "name": "Deny" }
                    ]),
                    once: false,
                },
                &invocation.cancel,
            )
            .await;
        camera.clear_pending(conversation_id);
        let approval = answer.map_err(|denied| {
            denied
                .or_declined("the user did not allow the camera.")
                .to_string()
        })?;
        // Older clients answer without choices: the suggestion and defaults.
        let choice: CardChoice = approval
            .data
            .clone()
            .and_then(|data| serde_json::from_value(data).ok())
            .unwrap_or_default();
        let device_id = choice.device_id.unwrap_or_else(|| suggested.to_owned());
        let device = offered
            .iter()
            .find(|device| device.id == device_id)
            .cloned()
            .ok_or_else(|| {
                format!("FAILED: the chosen camera {device_id} is not an allowed, connected camera")
            })?;
        let capture = match choice.capture {
            Some(chosen) => {
                chosen
                    .validate()
                    .map_err(|error| format!("FAILED: {error}"))?;
                chosen
            }
            None => capture,
        };
        if approval.always {
            camera.set_grant(
                conversation_id,
                CameraGrant {
                    device: device.clone(),
                    capture,
                },
            );
        }
        if let Err(error) = self
            .conversations
            .append_agent_event(
                conversation_id,
                "desktop.camera.grant",
                json!({
                    "status": "granted",
                    "scope": if approval.always { "conversation" } else { "once" },
                    "deviceId": device.id,
                    "deviceName": device.name,
                    "capture": capture,
                    "approvedBy": approval.device_id,
                }),
            )
            .await
        {
            tracing::warn!(%error, "failed to journal a camera grant");
        }
        Ok((device, capture))
    }

    /// A new directory the agent can read: the shared upload directory, or
    /// the private conversation's own folder.
    async fn camera_dir(&self, conversation_id: &str) -> Result<PathBuf, String> {
        let name = Uuid::new_v4().simple().to_string();
        let private = self.conversations.is_private(conversation_id);
        let conversation_id = conversation_id.to_owned();
        let root = if private {
            tokio::task::spawn_blocking(move || {
                crate::private_dirs::root()
                    .ensure_subdir(&conversation_id, crate::private_dirs::UPLOADS_DIR)
            })
            .await
            .map_err(|error| error.to_string())?
        } else {
            crate::prompt_uploads::ensure_root(&crate::prompt_uploads::upload_root()).await
        }
        .map_err(|error| format!("FAILED: the frame directory is unavailable: {error}"))?;
        let dir = root.join(name);
        let created = dir.clone();
        tokio::task::spawn_blocking(move || crate::prompt_uploads::ensure_private_dir(&created))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| format!("FAILED: cannot create the frame directory: {error}"))?;
        Ok(dir)
    }

    async fn camera_stop(
        &self,
        conversation_id: &str,
        session_id: Option<&str>,
        wait_seconds: u64,
        invocation: &Invocation<'_>,
    ) -> Result<CallToolResult, String> {
        let camera = self.mcp.desktop().camera();
        let Some(session) = camera.session_for(conversation_id, session_id) else {
            return Err(match session_id {
                Some(id) => format!("NO_SESSION: no camera session {id} in this conversation (results are kept 30 minutes)."),
                None => "NO_SESSION: this conversation has no camera session; start one with camera_start.".to_owned(),
            });
        };
        if session.is_running() && wait_seconds > 0 {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(wait_seconds)) => {}
                _ = session.ended() => {}
                () = invocation.cancel.cancelled() => {
                    return Err(format!(
                        "CANCELLED: the call was cancelled; camera session {} keeps recording.",
                        session.id
                    ));
                }
            }
        }
        session.stop(SessionEnd::Agent);
        let summary = tokio::time::timeout(END_TIMEOUT, session.ended())
            .await
            .map_err(|_| "FAILED: the camera session did not finish saving in time".to_owned())?;
        Ok(summary_result(&session, &summary))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guidance_follows_the_number_of_frames() {
        assert!(frames_guidance(1).contains("Every frame is attached"));
        assert!(frames_guidance(INLINE_FRAMES_MAX).contains("Every frame is attached"));
        let middle = frames_guidance(INLINE_FRAMES_MAX + 1);
        assert!(middle.contains("overview sheets") && !middle.contains("sub-agents"));
        assert_eq!(frames_guidance(SUBAGENT_FRAMES_MIN), middle);
        assert!(frames_guidance(SUBAGENT_FRAMES_MIN + 1).contains("sub-agents"));
    }
}
