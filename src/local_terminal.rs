use std::{path::PathBuf, process::Stdio, sync::Arc};

use dashmap::DashMap;
use serde::Serialize;
use serde_json::json;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{mpsc, RwLock},
};
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
    stop_tx: mpsc::Sender<()>,
    size: Arc<RwLock<TerminalSize>>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TerminalSize {
    rows: u16,
    cols: u16,
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

        let mut command = Command::new(&shell);
        for arg in interactive_shell_args(&shell) {
            command.arg(arg);
        }
        command
            .current_dir(&cwd)
            .env("TERM", "dumb")
            .env("TODEX_TERMINAL_ID", &terminal_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn()?;
        let pid = child.id();
        let stdin = child.stdin.take().ok_or_else(|| {
            AppError::InvalidRequest("terminal process did not expose stdin".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            AppError::InvalidRequest("terminal process did not expose stdout".to_string())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            AppError::InvalidRequest("terminal process did not expose stderr".to_string())
        })?;

        let (input_tx, input_rx) = mpsc::channel::<String>(64);
        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let size_state = Arc::new(RwLock::new(size));
        let handle = TerminalHandle {
            tenant_id: options.tenant_id.clone(),
            workspace_id: options.workspace_id.clone(),
            cwd: cwd.display().to_string(),
            shell: shell.clone(),
            pid,
            started_at: chrono::Utc::now().timestamp_millis(),
            input_tx,
            stop_tx,
            size: size_state.clone(),
        };
        self.sessions.insert(terminal_id.clone(), handle.clone());

        let events = self.events.clone();
        let session_info = TerminalEventInfo::from_handle(&terminal_id, &handle);
        tokio::spawn(write_terminal_input(
            stdin,
            input_rx,
            events.clone(),
            session_info.clone(),
        ));
        tokio::spawn(read_terminal_output(
            stdout,
            events.clone(),
            session_info.clone(),
            "stdout",
        ));
        tokio::spawn(read_terminal_output(
            stderr,
            events.clone(),
            session_info.clone(),
            "stderr",
        ));

        let sessions = self.sessions.clone();
        let wait_events = self.events.clone();
        let wait_info = session_info.clone();
        tokio::spawn(async move {
            let mut stopped_by_request = false;
            let status_result = tokio::select! {
                status = child.wait() => status,
                stop = stop_rx.recv() => {
                    if stop.is_some() {
                        stopped_by_request = true;
                        let _ = child.kill().await;
                    }
                    child.wait().await
                }
            };

            sessions.remove(&wait_info.terminal_id);
            let payload = match status_result {
                Ok(status) => json!({
                    "terminalId": wait_info.terminal_id,
                    "tenantId": wait_info.tenant_id,
                    "workspaceId": wait_info.workspace_id,
                    "cwd": wait_info.cwd,
                    "shell": wait_info.shell,
                    "pid": wait_info.pid,
                    "exitCode": status.code(),
                    "success": status.success(),
                    "stoppedByRequest": stopped_by_request,
                    "lifecycleState": "exited",
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
        if options.force {
            let _ = handle.stop_tx.send(()).await;
        } else {
            let _ = handle.input_tx.send("exit\n".to_string()).await;
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
        if let Some(rows) = options.rows {
            size.rows = rows.clamp(8, 200);
        }
        if let Some(cols) = options.cols {
            size.cols = cols.clamp(20, 400);
        }
        let next_size = *size;
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

async fn write_terminal_input(
    mut stdin: tokio::process::ChildStdin,
    mut input_rx: mpsc::Receiver<String>,
    events: EventBus,
    info: TerminalEventInfo,
) {
    while let Some(data) = input_rx.recv().await {
        if let Err(error) = stdin.write_all(data.as_bytes()).await {
            events
                .publish(EventRecord::new(
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
                        "error": error.to_string(),
                    }),
                ))
                .await;
            break;
        }
    }
}

async fn read_terminal_output<R>(
    mut reader: R,
    events: EventBus,
    info: TerminalEventInfo,
    stream: &'static str,
) where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; TERMINAL_OUTPUT_BUFFER_SIZE];
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) => {
                events
                    .publish(EventRecord::new(
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
                    ))
                    .await;
                break;
            }
        };
        let data = String::from_utf8_lossy(&buffer[..read]).to_string();
        events
            .publish(EventRecord::new(
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
                    "stream": stream,
                    "data": data,
                    "encoding": "utf8",
                }),
            ))
            .await;
    }
}

fn default_shell() -> String {
    if cfg!(windows) {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

fn interactive_shell_args(shell: &str) -> Vec<&'static str> {
    if cfg!(windows) {
        return vec![];
    }

    let shell_name = shell.rsplit('/').next().unwrap_or(shell);
    match shell_name {
        "bash" | "zsh" | "sh" | "dash" | "fish" => vec!["-i"],
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use serde_json::Value;

    use super::*;

    #[tokio::test]
    async fn terminal_start_input_and_exit_emit_events() {
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
                rows: Some(24),
                cols: Some(80),
            })
            .await
            .unwrap();

        manager
            .input(TerminalInputOptions {
                request_id: "terminal-input-test".to_string(),
                terminal_id: terminal_id.clone(),
                tenant_id: "local".to_string(),
                data: "printf todex-terminal-ready\\n\nexit\n".to_string(),
            })
            .await
            .unwrap();

        let deadline = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(deadline);
        let mut saw_output = false;
        let mut saw_exit = false;

        loop {
            tokio::select! {
                _ = &mut deadline => break,
                event = rx.recv() => {
                    let event = event.unwrap();
                    if event.event_type == "terminal.output" && payload_text(&event.payload, "data").contains("todex-terminal-ready") {
                        saw_output = true;
                    }
                    if event.event_type == "terminal.exited" {
                        saw_exit = true;
                    }
                    if saw_output && saw_exit {
                        break;
                    }
                }
            }
        }

        assert!(saw_output, "terminal stdout should include command output");
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
