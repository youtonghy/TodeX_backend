use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use dashmap::DashMap;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use serde_json::json;
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

use crate::{
    error::AppError,
    event::{EventBus, EventRecord},
};

const TERMINAL_OUTPUT_BUFFER_SIZE: usize = 8192;
const DEFAULT_TERMINAL_ROWS: u16 = 24;
const DEFAULT_TERMINAL_COLS: u16 = 80;

#[derive(Clone)]
pub struct LocalTerminalManager {
    sessions: Arc<DashMap<String, TerminalHandle>>,
    events: EventBus,
}

#[derive(Clone)]
struct TerminalHandle {
    tenant_id: String,
    workspace_id: Option<String>,
    cwd: String,
    shell: String,
    pid: Option<u32>,
    started_at: i64,
    input_tx: mpsc::Sender<String>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    stop_requested: Arc<AtomicBool>,
    size: Arc<RwLock<TerminalSize>>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TerminalSize {
    rows: u16,
    cols: u16,
}

impl From<TerminalSize> for PtySize {
    fn from(size: TerminalSize) -> Self {
        Self {
            rows: size.rows,
            cols: size.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TerminalStartOptions {
    pub request_id: String,
    pub terminal_id: Option<String>,
    pub tenant_id: String,
    pub workspace_id: Option<String>,
    pub cwd: String,
    pub shell: Option<String>,
    pub rows: Option<u16>,
    pub cols: Option<u16>,
}

#[derive(Clone, Debug)]
pub struct TerminalInputOptions {
    pub request_id: String,
    pub terminal_id: String,
    pub tenant_id: String,
    pub data: String,
}

#[derive(Clone, Debug)]
pub struct TerminalStopOptions {
    pub request_id: String,
    pub terminal_id: String,
    pub tenant_id: String,
    pub force: bool,
}

#[derive(Clone, Debug)]
pub struct TerminalResizeOptions {
    pub request_id: String,
    pub terminal_id: String,
    pub tenant_id: String,
    pub rows: Option<u16>,
    pub cols: Option<u16>,
}

#[derive(Clone, Debug)]
pub struct TerminalStatusOptions {
    pub request_id: String,
    pub tenant_id: String,
    pub workspace_id: Option<String>,
    pub terminal_id: Option<String>,
}

impl LocalTerminalManager {
    pub fn new(events: EventBus) -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            events,
        }
    }

    pub fn session_ids(
        &self,
        tenant_id: &str,
        workspace_id: Option<&str>,
        limit: usize,
    ) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|entry| {
                entry.tenant_id == tenant_id
                    && workspace_id.is_none_or(|workspace_id| {
                        entry.workspace_id.as_deref() == Some(workspace_id)
                    })
            })
            .take(limit)
            .map(|entry| entry.key().clone())
            .collect()
    }

    pub async fn start(&self, options: TerminalStartOptions) -> Result<String, AppError> {
        let terminal_id = options
            .terminal_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("term_{}", Uuid::new_v4().simple()));

        if self.sessions.contains_key(&terminal_id) {
            return Err(AppError::InvalidRequest(format!(
                "terminal {terminal_id} is already running"
            )));
        }

        let cwd = PathBuf::from(options.cwd.trim());
        let metadata = tokio::fs::metadata(&cwd).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                AppError::WorkspacePathNotFound
            } else {
                AppError::Io(error)
            }
        })?;
        if !metadata.is_dir() {
            return Err(AppError::InvalidRequest(
                "terminal cwd must be an existing directory".to_string(),
            ));
        }

        let shell = options
            .shell
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(default_shell);
        let size = TerminalSize {
            rows: options.rows.unwrap_or(DEFAULT_TERMINAL_ROWS).clamp(8, 200),
            cols: options.cols.unwrap_or(DEFAULT_TERMINAL_COLS).clamp(20, 400),
        };

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(size.into())
            .map_err(pty_error("failed to open terminal PTY"))?;
        let mut command = CommandBuilder::new(&shell);
        command.cwd(&cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TODEX_TERMINAL_ID", &terminal_id);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(pty_error("failed to open terminal PTY reader"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(pty_error("failed to open terminal PTY writer"))?;
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(pty_error("failed to spawn terminal process"))?;
        let pid = child.process_id();
        let killer = Arc::new(Mutex::new(child.clone_killer()));
        let master = Arc::new(Mutex::new(pair.master));
        let stop_requested = Arc::new(AtomicBool::new(false));
        drop(pair.slave);

        let (input_tx, input_rx) = mpsc::channel::<String>(64);
        let size_state = Arc::new(RwLock::new(size));
        let handle = TerminalHandle {
            tenant_id: options.tenant_id.clone(),
            workspace_id: options.workspace_id.clone(),
            cwd: cwd.display().to_string(),
            shell: shell.clone(),
            pid,
            started_at: chrono::Utc::now().timestamp_millis(),
            input_tx,
            killer,
            master,
            stop_requested: stop_requested.clone(),
            size: size_state.clone(),
        };
        self.sessions.insert(terminal_id.clone(), handle.clone());

        let events = self.events.clone();
        let input_runtime = tokio::runtime::Handle::current();
        let session_info = TerminalEventInfo::from_handle(&terminal_id, &handle);
        tokio::task::spawn_blocking(move || {
            write_terminal_input(
                writer,
                input_rx,
                events.clone(),
                session_info.clone(),
                input_runtime,
            )
        });
        let output_events = self.events.clone();
        let output_runtime = tokio::runtime::Handle::current();
        let output_info = TerminalEventInfo::from_handle(&terminal_id, &handle);
        tokio::task::spawn_blocking(move || {
            read_terminal_output(reader, output_events, output_info, output_runtime)
        });

        let sessions = self.sessions.clone();
        let wait_events = self.events.clone();
        let wait_info = TerminalEventInfo::from_handle(&terminal_id, &handle);
        tokio::spawn(async move {
            let status_result = tokio::task::spawn_blocking(move || child.wait()).await;
            let stopped_by_request = stop_requested.load(Ordering::Acquire);

            sessions.remove(&wait_info.terminal_id);
            let payload = match status_result {
                Ok(Ok(status)) => json!({
                    "terminalId": wait_info.terminal_id,
                    "tenantId": wait_info.tenant_id,
                    "workspaceId": wait_info.workspace_id,
                    "cwd": wait_info.cwd,
                    "shell": wait_info.shell,
                    "pid": wait_info.pid,
                    "exitCode": status.exit_code(),
                    "signal": status.signal(),
                    "success": status.success(),
                    "stoppedByRequest": stopped_by_request,
                    "lifecycleState": "exited",
                }),
                Ok(Err(error)) => json!({
                    "terminalId": wait_info.terminal_id,
                    "tenantId": wait_info.tenant_id,
                    "workspaceId": wait_info.workspace_id,
                    "cwd": wait_info.cwd,
                    "shell": wait_info.shell,
                    "pid": wait_info.pid,
                    "error": error.to_string(),
                    "lifecycleState": "error",
                }),
                Err(error) => json!({
                    "terminalId": wait_info.terminal_id,
                    "tenantId": wait_info.tenant_id,
                    "workspaceId": wait_info.workspace_id,
                    "cwd": wait_info.cwd,
                    "shell": wait_info.shell,
                    "pid": wait_info.pid,
                    "error": error.to_string(),
                    "lifecycleState": "error",
                }),
            };
            wait_events
                .publish(EventRecord::new(
                    "terminal.exited",
                    wait_info.workspace_id.clone(),
                    None,
                    Some(wait_info.terminal_id.clone()),
                    payload,
                ))
                .await;
        });

        self.events
            .publish(EventRecord::new(
                "terminal.started",
                options.workspace_id,
                None,
                Some(terminal_id.clone()),
                json!({
                    "requestId": options.request_id,
                    "terminalId": terminal_id,
                    "tenantId": options.tenant_id,
                    "workspaceId": handle.workspace_id,
                    "cwd": handle.cwd,
                    "shell": shell,
                    "pid": pid,
                    "rows": size.rows,
                    "cols": size.cols,
                    "lifecycleState": "running",
                }),
            ))
            .await;

        Ok(terminal_id)
    }

    pub async fn input(&self, options: TerminalInputOptions) -> Result<(), AppError> {
        let handle = self.authorized_handle(&options.terminal_id, &options.tenant_id)?;
        handle
            .input_tx
            .send(options.data.clone())
            .await
            .map_err(|_| {
                AppError::InvalidRequest("terminal input channel is closed".to_string())
            })?;
        self.events
            .publish(EventRecord::new(
                "terminal.input.accepted",
                handle.workspace_id.clone(),
                None,
                Some(options.terminal_id.clone()),
                json!({
                    "requestId": options.request_id,
                    "terminalId": options.terminal_id,
                    "tenantId": options.tenant_id,
                    "bytes": options.data.len(),
                    "lifecycleState": "running",
                }),
            ))
            .await;
        Ok(())
    }

    pub async fn stop(&self, options: TerminalStopOptions) -> Result<(), AppError> {
        let handle = self.authorized_handle(&options.terminal_id, &options.tenant_id)?;
        handle.stop_requested.store(true, Ordering::Release);
        if options.force {
            let result = handle
                .killer
                .lock()
                .map_err(|_| terminal_lock_error("terminal killer"))?
                .kill()
                .map_err(AppError::Io);
            if result.is_err() {
                handle.stop_requested.store(false, Ordering::Release);
            }
            result?;
        } else {
            let result = handle
                .input_tx
                .send("exit\n".to_string())
                .await
                .map_err(|_| {
                    AppError::InvalidRequest("terminal input channel is closed".to_string())
                });
            if result.is_err() {
                handle.stop_requested.store(false, Ordering::Release);
            }
            result?;
        }
        self.events
            .publish(EventRecord::new(
                "terminal.stopping",
                handle.workspace_id.clone(),
                None,
                Some(options.terminal_id.clone()),
                json!({
                    "requestId": options.request_id,
                    "terminalId": options.terminal_id,
                    "tenantId": options.tenant_id,
                    "force": options.force,
                    "lifecycleState": "stopping",
                }),
            ))
            .await;
        Ok(())
    }

    pub async fn resize(&self, options: TerminalResizeOptions) -> Result<(), AppError> {
        let handle = self.authorized_handle(&options.terminal_id, &options.tenant_id)?;
        let mut size = handle.size.write().await;
        let mut next_size = *size;
        if let Some(rows) = options.rows {
            next_size.rows = rows.clamp(8, 200);
        }
        if let Some(cols) = options.cols {
            next_size.cols = cols.clamp(20, 400);
        }
        handle
            .master
            .lock()
            .map_err(|_| terminal_lock_error("terminal PTY"))?
            .resize(next_size.into())
            .map_err(pty_error("failed to resize terminal PTY"))?;
        *size = next_size;
        drop(size);

        self.events
            .publish(EventRecord::new(
                "terminal.resized",
                handle.workspace_id.clone(),
                None,
                Some(options.terminal_id.clone()),
                json!({
                    "requestId": options.request_id,
                    "terminalId": options.terminal_id,
                    "tenantId": options.tenant_id,
                    "rows": next_size.rows,
                    "cols": next_size.cols,
                    "lifecycleState": "running",
                }),
            ))
            .await;
        Ok(())
    }

    pub async fn status(&self, options: TerminalStatusOptions) -> Result<(), AppError> {
        let handles = self
            .sessions
            .iter()
            .filter_map(|entry| {
                let handle = entry.value();
                if handle.tenant_id != options.tenant_id {
                    return None;
                }
                if let Some(workspace_id) = &options.workspace_id {
                    if handle.workspace_id.as_ref() != Some(workspace_id) {
                        return None;
                    }
                }
                if let Some(terminal_id) = &options.terminal_id {
                    if entry.key() != terminal_id {
                        return None;
                    }
                }
                Some((entry.key().clone(), handle.clone()))
            })
            .collect::<Vec<_>>();

        let mut terminals = Vec::with_capacity(handles.len());
        for (terminal_id, handle) in handles {
            let size = *handle.size.read().await;
            terminals.push(json!({
                "terminalId": terminal_id,
                "tenantId": handle.tenant_id,
                "workspaceId": handle.workspace_id,
                "cwd": handle.cwd,
                "shell": handle.shell,
                "pid": handle.pid,
                "rows": size.rows,
                "cols": size.cols,
                "startedAt": handle.started_at,
                "lifecycleState": "running",
            }));
        }

        self.events
            .publish(EventRecord::new(
                "terminal.status",
                options.workspace_id,
                None,
                options.terminal_id.clone(),
                json!({
                    "requestId": options.request_id,
                    "tenantId": options.tenant_id,
                    "terminalId": options.terminal_id,
                    "terminals": terminals,
                }),
            ))
            .await;
        Ok(())
    }

    fn authorized_handle(
        &self,
        terminal_id: &str,
        tenant_id: &str,
    ) -> Result<TerminalHandle, AppError> {
        let handle = self
            .sessions
            .get(terminal_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| {
                AppError::InvalidRequest(format!("terminal {terminal_id} is not running"))
            })?;
        if handle.tenant_id != tenant_id {
            return Err(AppError::Unauthorized("tenant mismatch".to_string()));
        }
        Ok(handle)
    }
}

#[derive(Clone)]
struct TerminalEventInfo {
    terminal_id: String,
    tenant_id: String,
    workspace_id: Option<String>,
    cwd: String,
    shell: String,
    pid: Option<u32>,
}

impl TerminalEventInfo {
    fn from_handle(terminal_id: &str, handle: &TerminalHandle) -> Self {
        Self {
            terminal_id: terminal_id.to_string(),
            tenant_id: handle.tenant_id.clone(),
            workspace_id: handle.workspace_id.clone(),
            cwd: handle.cwd.clone(),
            shell: handle.shell.clone(),
            pid: handle.pid,
        }
    }
}

fn write_terminal_input(
    mut writer: Box<dyn Write + Send>,
    mut input_rx: mpsc::Receiver<String>,
    events: EventBus,
    info: TerminalEventInfo,
    runtime: tokio::runtime::Handle,
) {
    while let Some(data) = input_rx.blocking_recv() {
        if let Err(error) = writer
            .write_all(data.as_bytes())
            .and_then(|_| writer.flush())
        {
            runtime.block_on(events.publish(terminal_error_event(&info, None, &error)));
            break;
        }
    }
}

fn read_terminal_output(
    mut reader: Box<dyn Read + Send>,
    events: EventBus,
    info: TerminalEventInfo,
    runtime: tokio::runtime::Handle,
) {
    let mut buffer = vec![0_u8; TERMINAL_OUTPUT_BUFFER_SIZE];
    let mut pending_utf8 = Vec::new();
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if is_pty_eof(&error) => break,
            Err(error) => {
                runtime.block_on(events.publish(terminal_error_event(
                    &info,
                    Some("stdout"),
                    &error,
                )));
                break;
            }
        };
        let data = decode_terminal_output(&mut pending_utf8, &buffer[..read], false);
        if !data.is_empty() {
            publish_terminal_output(&events, &info, &runtime, data);
        }
    }

    let trailing = decode_terminal_output(&mut pending_utf8, &[], true);
    if !trailing.is_empty() {
        publish_terminal_output(&events, &info, &runtime, trailing);
    }
}

fn decode_terminal_output(pending: &mut Vec<u8>, chunk: &[u8], flush: bool) -> String {
    pending.extend_from_slice(chunk);
    let mut output = String::new();

    loop {
        match std::str::from_utf8(pending) {
            Ok(text) => {
                output.push_str(text);
                pending.clear();
                break;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                let error_len = error.error_len();
                if valid_up_to > 0 {
                    output.push_str(
                        std::str::from_utf8(&pending[..valid_up_to])
                            .expect("validated UTF-8 prefix"),
                    );
                    pending.drain(..valid_up_to);
                }

                match error_len {
                    Some(error_len) => {
                        output.push('\u{fffd}');
                        pending.drain(..error_len);
                    }
                    None if flush => {
                        output.push_str(&String::from_utf8_lossy(pending));
                        pending.clear();
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    output
}

fn publish_terminal_output(
    events: &EventBus,
    info: &TerminalEventInfo,
    runtime: &tokio::runtime::Handle,
    data: String,
) {
    runtime.block_on(events.publish(EventRecord::new(
        "terminal.output",
        info.workspace_id.clone(),
        None,
        Some(info.terminal_id.clone()),
        json!({
            "terminalId": info.terminal_id,
            "tenantId": info.tenant_id,
            "workspaceId": info.workspace_id,
            "cwd": info.cwd,
            "shell": info.shell,
            "pid": info.pid,
            "stream": "stdout",
            "data": data,
            "encoding": "utf8",
            "pty": true,
        }),
    )));
}

fn terminal_error_event(
    info: &TerminalEventInfo,
    stream: Option<&str>,
    error: &std::io::Error,
) -> EventRecord {
    EventRecord::new(
        "terminal.error",
        info.workspace_id.clone(),
        None,
        Some(info.terminal_id.clone()),
        json!({
            "terminalId": info.terminal_id,
            "tenantId": info.tenant_id,
            "workspaceId": info.workspace_id,
            "cwd": info.cwd,
            "shell": info.shell,
            "pid": info.pid,
            "stream": stream,
            "error": error.to_string(),
        }),
    )
}

fn pty_error(context: &'static str) -> impl FnOnce(anyhow::Error) -> AppError {
    move |error| AppError::Anyhow(anyhow::anyhow!("{context}: {error}"))
}

fn terminal_lock_error(name: &str) -> AppError {
    AppError::Anyhow(anyhow::anyhow!("{name} lock is poisoned"))
}

fn is_pty_eof(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe
    ) {
        return true;
    }

    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::EIO) {
        return true;
    }

    false
}

fn default_shell() -> String {
    if cfg!(windows) {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{fs, time::Duration};

    use serde_json::Value;

    use super::*;

    #[test]
    fn terminal_output_preserves_split_utf8_sequences() {
        let mut pending = Vec::new();
        assert_eq!(
            decode_terminal_output(&mut pending, &[0xe4, 0xbd], false),
            ""
        );
        assert_eq!(decode_terminal_output(&mut pending, &[0xa0], false), "你");
        assert!(pending.is_empty());

        assert_eq!(
            decode_terminal_output(&mut pending, &[0xff, 0xe5, 0xa5], false),
            "\u{fffd}"
        );
        assert_eq!(decode_terminal_output(&mut pending, &[0xbd], false), "好");
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn terminal_process_has_a_real_pty() {
        let events = EventBus::new(64);
        let mut rx = events.subscribe();
        let manager = LocalTerminalManager::new(events);
        let cwd = make_temp_workspace("terminal");
        let terminal_id = "term-test".to_string();

        manager
            .start(TerminalStartOptions {
                request_id: "terminal-start-test".to_string(),
                terminal_id: Some(terminal_id.clone()),
                tenant_id: "local".to_string(),
                workspace_id: Some("workspace-test".to_string()),
                cwd: cwd.display().to_string(),
                shell: Some("/bin/sh".to_string()),
                rows: Some(31),
                cols: Some(97),
            })
            .await
            .unwrap();

        manager
            .input(TerminalInputOptions {
                request_id: "terminal-input-test".to_string(),
                terminal_id: terminal_id.clone(),
                tenant_id: "local".to_string(),
                data: "if [ -t 0 ] && [ -t 1 ]; then printf '__PTY_%s__ ' OK; else printf '__PTY_%s__ ' BAD; fi; stty size; exit\n".to_string(),
            })
            .await
            .unwrap();

        let deadline = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(deadline);
        let mut output = String::new();
        let mut saw_exit = false;

        loop {
            tokio::select! {
                _ = &mut deadline => break,
                event = rx.recv() => {
                    let event = event.unwrap();
                    if event.event_type == "terminal.output" {
                        output.push_str(&payload_text(&event.payload, "data"));
                    }
                    if event.event_type == "terminal.exited" {
                        saw_exit = true;
                    }
                    if output.contains("__PTY_OK__") && output.contains("31 97") && saw_exit {
                        break;
                    }
                }
            }
        }

        assert!(
            output.contains("__PTY_OK__"),
            "shell should run inside a PTY: {output:?}"
        );
        assert!(
            output.contains("31 97"),
            "PTY should receive its initial size: {output:?}"
        );
        assert!(saw_exit, "terminal should publish an exit event");

        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn terminal_resize_updates_the_pty_window() {
        let events = EventBus::new(64);
        let mut rx = events.subscribe();
        let manager = LocalTerminalManager::new(events);
        let cwd = make_temp_workspace("terminal-resize");
        let terminal_id = "term-resize-test".to_string();

        manager
            .start(TerminalStartOptions {
                request_id: "terminal-resize-start".to_string(),
                terminal_id: Some(terminal_id.clone()),
                tenant_id: "local".to_string(),
                workspace_id: Some("workspace-test".to_string()),
                cwd: cwd.display().to_string(),
                shell: Some("/bin/sh".to_string()),
                rows: Some(24),
                cols: Some(80),
            })
            .await
            .unwrap();

        manager
            .resize(TerminalResizeOptions {
                request_id: "terminal-resize-test".to_string(),
                terminal_id: terminal_id.clone(),
                tenant_id: "local".to_string(),
                rows: Some(41),
                cols: Some(132),
            })
            .await
            .unwrap();
        manager
            .input(TerminalInputOptions {
                request_id: "terminal-resize-input".to_string(),
                terminal_id: terminal_id.clone(),
                tenant_id: "local".to_string(),
                data: "printf '__SIZE__ '; stty size; exit\n".to_string(),
            })
            .await
            .unwrap();

        let deadline = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(deadline);
        let mut output = String::new();
        let mut saw_resize = false;
        let mut saw_exit = false;

        loop {
            tokio::select! {
                _ = &mut deadline => break,
                event = rx.recv() => {
                    let event = event.unwrap();
                    if event.event_type == "terminal.output" {
                        output.push_str(&payload_text(&event.payload, "data"));
                    }
                    if event.event_type == "terminal.resized" {
                        saw_resize = true;
                    }
                    if event.event_type == "terminal.exited" {
                        saw_exit = true;
                    }
                    if output.contains("41 132") && saw_resize && saw_exit {
                        break;
                    }
                }
            }
        }

        assert!(saw_resize, "terminal should publish a resize event");
        assert!(
            output.contains("41 132"),
            "shell should observe the resized PTY: {output:?}"
        );
        assert!(saw_exit, "terminal should publish an exit event");

        let _ = fs::remove_dir_all(cwd);
    }

    fn payload_text(payload: &Value, key: &str) -> String {
        payload
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    fn make_temp_workspace(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("todex-{name}-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        root
    }
}
