use std::{
    collections::VecDeque,
    io::{Read, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc, Arc, Mutex,
    },
    time::Duration,
};

use dashmap::DashMap;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use serde_json::json;
use tokio::sync::{mpsc, Mutex as AsyncMutex, RwLock};
use tracing::warn;
use uuid::Uuid;

use crate::{
    error::AppError,
    event::{EventBus, EventRecord},
    ssh::TerminalCommand,
    terminal_mirror::{snapshots_enabled, TerminalMirror},
};

const TERMINAL_READ_BUFFER_SIZE: usize = 64 * 1024;
/// Largest `terminal.output` batch: a flood becomes a few large events while
/// an interactive echo still goes out on its own, at once.
const TERMINAL_OUTPUT_BATCH_LIMIT: usize = 256 * 1024;
/// Reads waiting to be published; a full queue holds the PTY reader back.
const TERMINAL_READ_QUEUE: usize = 64;
/// Output arriving this soon after the previous batch counts as a stream
/// (a program printing): its next batch waits briefly for more. Interactive
/// echoes come far apart and go out at once.
const TERMINAL_STREAMING_GAP: Duration = Duration::from_millis(10);
/// How long a streaming batch gathers further reads, as Ghostty's read
/// thread does.
const TERMINAL_BATCH_WINDOW: Duration = Duration::from_millis(4);
/// Output kept per terminal for `terminal.attach`.
const TERMINAL_REPLAY_LIMIT: usize = 2 * 1024 * 1024;
const TERMINAL_INPUT_QUEUE: usize = 256;
/// Advertised as `terminalReplay` by `GET /v2/version`: output carries a
/// per-terminal `seq`, and `terminal.attach` / `terminal.clear` exist.
pub const TERMINAL_REPLAY_VERSION: u8 = 1;
/// Advertised as `terminalSnapshot`: an attach that cannot continue from the
/// client's `seq` gets the screen as a snapshot (`snapshot: true`).
pub const TERMINAL_SNAPSHOT_VERSION: u8 = 1;
/// How long `terminal.exited` waits for the reader to reach end of output.
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_TERMINAL_ROWS: u16 = 24;
const DEFAULT_TERMINAL_COLS: u16 = 80;

#[derive(Clone)]
pub struct LocalTerminalManager {
    sessions: Arc<DashMap<String, TerminalHandle>>,
    events: EventBus,
    /// Where shell integration scripts are installed (the data directory);
    /// unset, terminals never get the integration.
    shell_integration_root: Option<PathBuf>,
    shell_integration_dir: Arc<tokio::sync::OnceCell<Option<PathBuf>>>,
}

#[derive(Clone)]
struct TerminalHandle {
    tenant_id: String,
    workspace_id: Option<String>,
    cwd: String,
    shell: String,
    ssh_host: Option<String>,
    pid: Option<u32>,
    started_at: i64,
    input_tx: mpsc::Sender<String>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    stop_requested: Arc<AtomicBool>,
    size: Arc<RwLock<TerminalSize>>,
    replay: Arc<AsyncMutex<TerminalReplay>>,
}

/// The retained tail of a terminal's output, in published batches. Its lock
/// is held while a batch is published, so a snapshot taken under it is
/// exactly the output published before it.
#[derive(Default)]
struct TerminalReplay {
    chunks: VecDeque<(u64, String)>,
    bytes: usize,
    last_seq: u64,
    /// The screen the published output produced; absent when snapshots are
    /// turned off.
    mirror: Option<TerminalMirror>,
}

impl TerminalReplay {
    fn new(size: TerminalSize) -> Self {
        Self {
            mirror: snapshots_enabled().then(|| TerminalMirror::new(size.rows, size.cols)),
            ..Self::default()
        }
    }

    /// What a client that cannot continue from `since` starts from: the
    /// screen snapshot when there is a mirror, else the retained output.
    fn attach_data(&mut self, since: Option<u64>) -> (bool, bool, String) {
        let (reset, data) = self.since(since);
        match (reset, self.mirror.as_mut()) {
            (true, Some(mirror)) => (true, true, mirror.snapshot()),
            _ => (reset, false, data),
        }
    }

    fn push(&mut self, data: String) -> u64 {
        self.last_seq += 1;
        self.bytes += data.len();
        self.chunks.push_back((self.last_seq, data));
        while self.bytes > TERMINAL_REPLAY_LIMIT && self.chunks.len() > 1 {
            if let Some((_, dropped)) = self.chunks.pop_front() {
                self.bytes -= dropped.len();
            }
        }
        self.last_seq
    }

    /// Output published after `since`, or with `reset` everything retained
    /// when `since` is absent or no longer covered by the retained tail.
    fn since(&self, since: Option<u64>) -> (bool, String) {
        let first = self
            .chunks
            .front()
            .map_or(self.last_seq + 1, |(seq, _)| *seq);
        match since {
            Some(since) if since <= self.last_seq && since + 1 >= first => (
                false,
                self.chunks
                    .iter()
                    .filter(|(seq, _)| *seq > since)
                    .map(|(_, data)| data.as_str())
                    .collect(),
            ),
            _ => (
                true,
                self.chunks.iter().map(|(_, data)| data.as_str()).collect(),
            ),
        }
    }

    fn clear(&mut self) {
        self.chunks.clear();
        self.bytes = 0;
    }
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
    /// Runs this program (with its own complete environment) instead of a
    /// shell; `cwd` is then ignored in favour of the command's directory.
    pub command: Option<TerminalCommand>,
    /// Load the bash/zsh/fish integration (prompt marks, working directory)
    /// into a local shell; ignored for commands and other shells.
    pub shell_integration: bool,
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
pub struct TerminalAttachOptions {
    pub request_id: String,
    pub terminal_id: String,
    pub tenant_id: String,
    /// Last `seq` the client has; absent to get everything retained.
    pub since: Option<u64>,
    pub rows: Option<u16>,
    pub cols: Option<u16>,
}

#[derive(Clone, Debug)]
pub struct TerminalClearOptions {
    pub request_id: String,
    pub terminal_id: String,
    pub tenant_id: String,
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
            shell_integration_root: None,
            shell_integration_dir: Arc::default(),
        }
    }

    pub fn with_shell_integration_root(mut self, root: PathBuf) -> Self {
        self.shell_integration_root = Some(root);
        self
    }

    /// The installed integration scripts, written on first use. A failed
    /// install is logged once and leaves terminals without the integration.
    async fn shell_integration_dir(&self) -> Option<PathBuf> {
        let root = self.shell_integration_root.as_ref()?;
        self.shell_integration_dir
            .get_or_init(|| async {
                crate::shell_integration::install(root)
                    .await
                    .inspect_err(|error| warn!(%error, "failed to install shell integration"))
                    .ok()
            })
            .await
            .clone()
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

        let cwd = match &options.command {
            Some(command) => command.cwd.clone(),
            None => PathBuf::from(options.cwd.trim()),
        };
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

        let shell = match &options.command {
            Some(command) => command.program.clone(),
            None => options
                .shell
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(default_shell),
        };
        let size = TerminalSize {
            rows: options.rows.unwrap_or(DEFAULT_TERMINAL_ROWS).clamp(8, 200),
            cols: options.cols.unwrap_or(DEFAULT_TERMINAL_COLS).clamp(20, 400),
        };

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(size.into())
            .map_err(pty_error("failed to open terminal PTY"))?;
        let mut command = CommandBuilder::new(&shell);
        if let Some(launch) = &options.command {
            command.args(&launch.args);
            command.env_clear();
            for (key, value) in &launch.env {
                command.env(key, value);
            }
        } else {
            crate::shell_integration::clean_environment(&mut command);
            if options.shell_integration {
                if let Some(dir) = self.shell_integration_dir().await {
                    crate::shell_integration::apply(&mut command, &shell, &dir);
                }
            }
        }
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

        let (input_tx, input_rx) = mpsc::channel::<String>(TERMINAL_INPUT_QUEUE);
        let size_state = Arc::new(RwLock::new(size));
        let replay = Arc::new(AsyncMutex::new(TerminalReplay::new(size)));
        let handle = TerminalHandle {
            tenant_id: options.tenant_id.clone(),
            workspace_id: options.workspace_id.clone(),
            cwd: cwd.display().to_string(),
            shell: shell.clone(),
            ssh_host: options
                .command
                .as_ref()
                .map(|command| command.ssh_host.clone()),
            pid,
            started_at: chrono::Utc::now().timestamp_millis(),
            input_tx,
            killer,
            master,
            stop_requested: stop_requested.clone(),
            size: size_state.clone(),
            replay: replay.clone(),
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
        let (reads_tx, reads_rx) = std_mpsc::sync_channel::<Vec<u8>>(TERMINAL_READ_QUEUE);
        let reader_events = self.events.clone();
        let reader_runtime = tokio::runtime::Handle::current();
        let reader_info = TerminalEventInfo::from_handle(&terminal_id, &handle);
        tokio::task::spawn_blocking(move || {
            read_terminal_output(reader, reads_tx, reader_events, reader_info, reader_runtime)
        });
        let output_events = self.events.clone();
        let output_runtime = tokio::runtime::Handle::current();
        let output_info = TerminalEventInfo::from_handle(&terminal_id, &handle);
        // Ends once the reader has hit end of output and everything it read
        // is published.
        let output_task = tokio::task::spawn_blocking(move || {
            publish_terminal_output(reads_rx, replay, output_events, output_info, output_runtime)
        });

        let sessions = self.sessions.clone();
        let wait_events = self.events.clone();
        let wait_info = TerminalEventInfo::from_handle(&terminal_id, &handle);
        tokio::spawn(async move {
            let status_result = tokio::task::spawn_blocking(move || child.wait()).await;
            let stopped_by_request = stop_requested.load(Ordering::Acquire);

            sessions.remove(&wait_info.terminal_id);
            // Output is published by the reader thread: let it drain the PTY
            // first so `terminal.exited` never overtakes the last output. A
            // descendant that keeps the PTY open must not hold the exit back.
            if tokio::time::timeout(OUTPUT_DRAIN_TIMEOUT, output_task)
                .await
                .is_err()
            {
                warn!(terminal_id = %wait_info.terminal_id, "terminal output still open after exit; not waiting for it");
            }
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
                    "ssh": handle.ssh_host.as_ref().map(|host| json!({ "host": host })),
                }),
            ))
            .await;

        Ok(terminal_id)
    }

    pub async fn input(&self, options: TerminalInputOptions) -> Result<(), AppError> {
        let handle = self.authorized_handle(&options.terminal_id, &options.tenant_id)?;
        // Never wait for room: a program that stops reading its input must
        // not hold up the rest of the connection's commands.
        handle
            .input_tx
            .try_send(options.data.clone())
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => AppError::InvalidRequest(format!(
                    "terminal {} is not reading its input; input dropped",
                    options.terminal_id
                )),
                mpsc::error::TrySendError::Closed(_) => AppError::InvalidRequest(format!(
                    "terminal {} input channel is closed",
                    options.terminal_id
                )),
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
        if let Some(mirror) = handle.replay.lock().await.mirror.as_mut() {
            mirror.resize(next_size.rows, next_size.cols);
        }

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

    /// Re-joins a terminal: resizes it to the client's view first (so the
    /// program redraws for it), then publishes `terminal.attached` with the
    /// output after `since`. A terminal that is not running answers with
    /// `running: false` instead of an error, so clients can start it.
    pub async fn attach(&self, options: TerminalAttachOptions) -> Result<(), AppError> {
        let handle = match self.sessions.get(&options.terminal_id) {
            Some(entry) => entry.value().clone(),
            None => {
                self.events
                    .publish(EventRecord::new(
                        "terminal.attached",
                        None,
                        None,
                        Some(options.terminal_id.clone()),
                        json!({
                            "requestId": options.request_id,
                            "terminalId": options.terminal_id,
                            "tenantId": options.tenant_id,
                            "running": false,
                        }),
                    ))
                    .await;
                return Ok(());
            }
        };
        if handle.tenant_id != options.tenant_id {
            return Err(AppError::Unauthorized("tenant mismatch".to_string()));
        }
        if options.rows.is_some() || options.cols.is_some() {
            let current = *handle.size.read().await;
            let rows = options.rows.map(|rows| rows.clamp(8, 200));
            let cols = options.cols.map(|cols| cols.clamp(20, 400));
            if rows.is_some_and(|rows| rows != current.rows)
                || cols.is_some_and(|cols| cols != current.cols)
            {
                self.resize(TerminalResizeOptions {
                    request_id: options.request_id.clone(),
                    terminal_id: options.terminal_id.clone(),
                    tenant_id: options.tenant_id.clone(),
                    rows,
                    cols,
                })
                .await?;
            }
        }
        let size = *handle.size.read().await;
        // Published under the replay lock: batches up to `seq` went out
        // before this event, later ones follow it.
        let mut replay = handle.replay.lock().await;
        let (reset, snapshot, data) = replay.attach_data(options.since);
        self.events
            .publish(EventRecord::new(
                "terminal.attached",
                handle.workspace_id.clone(),
                None,
                Some(options.terminal_id.clone()),
                json!({
                    "requestId": options.request_id,
                    "terminalId": options.terminal_id,
                    "tenantId": options.tenant_id,
                    "workspaceId": handle.workspace_id,
                    "running": true,
                    "reset": reset,
                    "snapshot": snapshot,
                    "seq": replay.last_seq,
                    "data": data,
                    "cwd": handle.cwd,
                    "shell": handle.shell,
                    "pid": handle.pid,
                    "rows": size.rows,
                    "cols": size.cols,
                    "startedAt": handle.started_at,
                    "lifecycleState": "running",
                    "ssh": handle.ssh_host.as_ref().map(|host| json!({ "host": host })),
                }),
            ))
            .await;
        Ok(())
    }

    /// Drops the retained output, so a later attach does not bring back what
    /// the user cleared. Output keeps its `seq` numbering.
    pub async fn clear(&self, options: TerminalClearOptions) -> Result<(), AppError> {
        let handle = self.authorized_handle(&options.terminal_id, &options.tenant_id)?;
        let mut replay = handle.replay.lock().await;
        replay.clear();
        if let Some(mirror) = replay.mirror.as_mut() {
            mirror.clear();
        }
        self.events
            .publish(EventRecord::new(
                "terminal.cleared",
                handle.workspace_id.clone(),
                None,
                Some(options.terminal_id.clone()),
                json!({
                    "requestId": options.request_id,
                    "terminalId": options.terminal_id,
                    "tenantId": options.tenant_id,
                    "seq": replay.last_seq,
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
    reads: std_mpsc::SyncSender<Vec<u8>>,
    events: EventBus,
    info: TerminalEventInfo,
    runtime: tokio::runtime::Handle,
) {
    let mut buffer = vec![0_u8; TERMINAL_READ_BUFFER_SIZE];
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
        if reads.send(buffer[..read].to_vec()).is_err() {
            break;
        }
    }
}

/// Publishes what the reader read, merging reads that queued up meanwhile
/// (or, while output streams, arrive within `TERMINAL_BATCH_WINDOW`) into one
/// `terminal.output` of up to `TERMINAL_OUTPUT_BATCH_LIMIT`.
fn publish_terminal_output(
    reads: std_mpsc::Receiver<Vec<u8>>,
    replay: Arc<AsyncMutex<TerminalReplay>>,
    events: EventBus,
    info: TerminalEventInfo,
    runtime: tokio::runtime::Handle,
) {
    let mut pending_utf8 = Vec::new();
    let mut last_batch: Option<std::time::Instant> = None;
    while let Ok(mut batch) = reads.recv() {
        let now = std::time::Instant::now();
        let streaming =
            last_batch.is_some_and(|last| now.duration_since(last) < TERMINAL_STREAMING_GAP);
        let deadline = now + TERMINAL_BATCH_WINDOW;
        while batch.len() < TERMINAL_OUTPUT_BATCH_LIMIT {
            let more = match reads.try_recv() {
                Ok(more) => Some(more),
                Err(std_mpsc::TryRecvError::Empty) if streaming => deadline
                    .checked_duration_since(std::time::Instant::now())
                    .and_then(|left| reads.recv_timeout(left).ok()),
                Err(_) => None,
            };
            match more {
                Some(more) => batch.extend_from_slice(&more),
                None => break,
            }
        }
        last_batch = Some(std::time::Instant::now());
        let data = decode_terminal_output(&mut pending_utf8, &batch, false);
        if !data.is_empty() {
            publish_terminal_output_batch(&events, &info, &runtime, &replay, data);
        }
    }

    let trailing = decode_terminal_output(&mut pending_utf8, &[], true);
    if !trailing.is_empty() {
        publish_terminal_output_batch(&events, &info, &runtime, &replay, trailing);
    }
}

pub(crate) fn decode_terminal_output(pending: &mut Vec<u8>, chunk: &[u8], flush: bool) -> String {
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

fn publish_terminal_output_batch(
    events: &EventBus,
    info: &TerminalEventInfo,
    runtime: &tokio::runtime::Handle,
    replay: &AsyncMutex<TerminalReplay>,
    data: String,
) {
    let mut replay = replay.blocking_lock();
    if let Some(mirror) = replay.mirror.as_mut() {
        mirror.advance(data.as_bytes());
    }
    let seq = replay.push(data.clone());
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
            "seq": seq,
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

pub(crate) fn default_shell() -> String {
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
                command: None,
                shell_integration: false,
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

    #[cfg(unix)]
    #[tokio::test]
    async fn exit_is_published_after_the_last_output() {
        let events = EventBus::new(64);
        let mut rx = events.subscribe();
        let manager = LocalTerminalManager::new(events);
        let cwd = make_temp_workspace("terminal-exit-order");

        manager
            .start(TerminalStartOptions {
                request_id: "terminal-exit-order".to_string(),
                terminal_id: Some("term-exit-order".to_string()),
                tenant_id: "local".to_string(),
                workspace_id: None,
                cwd: cwd.display().to_string(),
                shell: Some("/bin/sh".to_string()),
                rows: None,
                cols: None,
                command: Some(TerminalCommand {
                    program: "/bin/sh".to_string(),
                    args: vec!["-c".into(), "printf '__LAST__'".into()],
                    env: Vec::new(),
                    cwd: cwd.clone(),
                    ssh_host: "web".to_string(),
                }),
                shell_integration: false,
            })
            .await
            .unwrap();

        let mut output = String::new();
        let exited = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = rx.recv().await.unwrap();
                match event.event_type.as_str() {
                    "terminal.output" => output.push_str(&payload_text(&event.payload, "data")),
                    "terminal.exited" => break,
                    _ => {}
                }
            }
        })
        .await;
        assert!(exited.is_ok(), "terminal should publish an exit event");
        assert!(output.contains("__LAST__"), "{output:?}");
        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn command_terminal_uses_its_own_args_env_and_directory() {
        std::env::set_var("TODEX_AGENTD_TERMINAL_LEAK_TEST", "leak");
        let events = EventBus::new(64);
        let mut rx = events.subscribe();
        let manager = LocalTerminalManager::new(events);
        let cwd = make_temp_workspace("terminal-command");

        manager
            .start(TerminalStartOptions {
                request_id: "terminal-command-test".to_string(),
                terminal_id: Some("term-command".to_string()),
                tenant_id: "local".to_string(),
                workspace_id: None,
                cwd: "/definitely/not/used".to_string(),
                shell: Some("/bin/bash".to_string()),
                rows: None,
                cols: None,
                command: Some(TerminalCommand {
                    program: "/bin/sh".to_string(),
                    args: vec![
                        "-c".into(),
                        "printf '[%s|%s|%s]' \"$1\" \"$KEPT\" \"$TODEX_AGENTD_TERMINAL_LEAK_TEST\"; pwd"
                            .into(),
                        "sh".into(),
                        "arg-one".into(),
                    ],
                    env: vec![("KEPT".to_string(), "yes".into())],
                    cwd: cwd.clone(),
                    ssh_host: "web".to_string(),
                }),
                shell_integration: false,
            })
            .await
            .unwrap();

        let deadline = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(deadline);
        let mut output = String::new();
        let mut started_host = None;
        let cwd_name = cwd.file_name().unwrap().to_string_lossy().into_owned();
        // The exit event is published by a different task than the output,
        // so it can arrive first: wait for the output itself.
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                event = rx.recv() => {
                    let event = event.unwrap();
                    match event.event_type.as_str() {
                        "terminal.started" => started_host = event.payload["ssh"]["host"].as_str().map(str::to_owned),
                        "terminal.output" => output.push_str(&payload_text(&event.payload, "data")),
                        _ => {}
                    }
                    if output.contains(&cwd_name) {
                        break;
                    }
                }
            }
        }

        assert!(output.contains("[arg-one|yes|]"), "{output:?}");
        assert!(output.contains(&cwd_name), "{output:?}");
        assert_eq!(started_host.as_deref(), Some("web"));
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
                command: None,
                shell_integration: false,
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

    #[test]
    fn replay_keeps_the_tail_and_answers_since() {
        let mut replay = TerminalReplay::default();
        assert_eq!(replay.since(None), (true, String::new()));
        assert_eq!(replay.since(Some(0)), (false, String::new()));
        assert_eq!(replay.push("a".into()), 1);
        assert_eq!(replay.push("b".into()), 2);
        assert_eq!(replay.since(Some(1)), (false, "b".to_string()));
        assert_eq!(replay.since(Some(2)), (false, String::new()));
        // A client ahead of this terminal (e.g. a restarted backend) resets.
        assert_eq!(replay.since(Some(9)), (true, "ab".to_string()));

        let big = "x".repeat(TERMINAL_REPLAY_LIMIT);
        replay.push(big.clone());
        assert_eq!(
            replay.chunks.len(),
            1,
            "older batches give way to the limit"
        );
        assert_eq!(replay.since(Some(1)), (true, big.clone()));
        assert_eq!(replay.since(Some(2)), (false, big));

        replay.clear();
        assert_eq!(replay.since(Some(3)), (false, String::new()));
        assert_eq!(replay.since(Some(1)), (true, String::new()));
        assert_eq!(replay.push("c".into()), 4);
    }

    #[tokio::test]
    async fn attach_replays_retained_output_after_since() {
        let events = EventBus::new(256);
        let mut rx = events.subscribe();
        let manager = LocalTerminalManager::new(events);
        let cwd = make_temp_workspace("terminal-attach");
        let terminal_id = "term-attach".to_string();
        manager
            .start(TerminalStartOptions {
                request_id: "start".to_string(),
                terminal_id: Some(terminal_id.clone()),
                tenant_id: "local".to_string(),
                workspace_id: None,
                cwd: cwd.display().to_string(),
                shell: Some("/bin/sh".to_string()),
                rows: Some(24),
                cols: Some(80),
                command: None,
                shell_integration: false,
            })
            .await
            .unwrap();
        manager
            .input(TerminalInputOptions {
                request_id: "input".to_string(),
                terminal_id: terminal_id.clone(),
                tenant_id: "local".to_string(),
                data: "printf '__AT%s__\\n' TACH\n".to_string(),
            })
            .await
            .unwrap();

        let mut output = String::new();
        let mut last_seq = 0;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !output.contains("__ATTACH__") {
                let event = rx.recv().await.unwrap();
                if event.event_type == "terminal.output" {
                    let seq = event.payload["seq"].as_u64().unwrap();
                    assert_eq!(seq, last_seq + 1, "seq counts every batch");
                    last_seq = seq;
                    output.push_str(&payload_text(&event.payload, "data"));
                }
            }
        })
        .await
        .expect("marker output");

        let attach = |since: Option<u64>| TerminalAttachOptions {
            request_id: "attach".to_string(),
            terminal_id: terminal_id.clone(),
            tenant_id: "local".to_string(),
            since,
            rows: None,
            cols: None,
        };
        manager.attach(attach(None)).await.unwrap();
        let attached = next_attached(&mut rx).await;
        assert_eq!(attached["running"], true);
        assert_eq!(attached["reset"], true);
        assert_eq!(attached["snapshot"], snapshots_enabled());
        assert!(attached["seq"].as_u64().unwrap() >= last_seq);
        assert!(payload_text(&attached, "data").contains("__ATTACH__"));

        let seq = attached["seq"].as_u64().unwrap();
        manager.attach(attach(Some(seq))).await.unwrap();
        let attached = next_attached(&mut rx).await;
        assert_eq!(attached["reset"], false);
        assert_eq!(payload_text(&attached, "data"), "");

        manager
            .clear(TerminalClearOptions {
                request_id: "clear".to_string(),
                terminal_id: terminal_id.clone(),
                tenant_id: "local".to_string(),
            })
            .await
            .unwrap();
        manager.attach(attach(Some(0))).await.unwrap();
        let attached = next_attached(&mut rx).await;
        assert_eq!(attached["reset"], true);
        assert!(!payload_text(&attached, "data").contains("__ATTACH__"));

        manager
            .stop(TerminalStopOptions {
                request_id: "stop".to_string(),
                terminal_id: terminal_id.clone(),
                tenant_id: "local".to_string(),
                force: true,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while rx.recv().await.unwrap().event_type != "terminal.exited" {}
        })
        .await
        .expect("exit");
        manager.attach(attach(Some(seq))).await.unwrap();
        let attached = next_attached(&mut rx).await;
        assert_eq!(attached["running"], false);
        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn shell_integration_marks_prompts_and_commands() {
        let root = make_temp_workspace("terminal-shell-integration");
        for shell in ["bash", "zsh", "fish"] {
            let Some(real) = std::env::var_os("PATH").and_then(|paths| {
                std::env::split_paths(&paths)
                    .map(|dir| dir.join(shell))
                    .find(|candidate| candidate.is_file())
            }) else {
                eprintln!("skipping {shell}: not installed");
                continue;
            };
            // todex.bash marks commands only on bash 4.4+ (PS0); macOS ships
            // bash 3.2, which keeps just the rc file.
            if shell == "bash" {
                let version = std::process::Command::new(&real)
                    .args(["-c", "echo $((BASH_VERSINFO[0] * 100 + BASH_VERSINFO[1]))"])
                    .output()
                    .ok()
                    .and_then(|out| {
                        String::from_utf8_lossy(&out.stdout)
                            .trim()
                            .parse::<u32>()
                            .ok()
                    })
                    .unwrap_or(0);
                if version < 404 {
                    eprintln!("skipping bash: {} has no shell integration", real.display());
                    continue;
                }
            }
            // A wrapper under the shell's own name gives it an empty home, so
            // the developer's rc files stay out of the test.
            let home = root.join(format!("home-{shell}"));
            fs::create_dir_all(&home).unwrap();
            let bin = root.join(format!("bin-{shell}"));
            fs::create_dir_all(&bin).unwrap();
            let path = bin.join(shell);
            fs::write(
                &path,
                format!(
                    "#!/bin/sh\nHOME='{home}' XDG_CONFIG_HOME='{home}/.config' exec '{real}' \"$@\"\n",
                    home = home.display(),
                    real = real.display()
                ),
            )
            .unwrap();
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
            let events = EventBus::new(256);
            let mut rx = events.subscribe();
            let manager =
                LocalTerminalManager::new(events).with_shell_integration_root(root.clone());
            let terminal_id = format!("term-integration-{shell}");
            manager
                .start(TerminalStartOptions {
                    request_id: "start".to_string(),
                    terminal_id: Some(terminal_id.clone()),
                    tenant_id: "local".to_string(),
                    workspace_id: None,
                    cwd: root.display().to_string(),
                    shell: Some(path.display().to_string()),
                    rows: Some(24),
                    cols: Some(80),
                    command: None,
                    shell_integration: true,
                })
                .await
                .unwrap();
            let mut output = String::new();
            let mut sent = false;
            let mut answered = 0;
            let done = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let event = rx.recv().await.unwrap();
                    if event.event_type != "terminal.output" {
                        continue;
                    }
                    output.push_str(&payload_text(&event.payload, "data"));
                    // fish 4 waits for the terminal to answer its device
                    // attribute and cursor queries before it reads input.
                    let queries =
                        output.matches("\x1b[0c").count() + output.matches("\x1b[6n").count();
                    while answered < queries {
                        answered += 1;
                        manager
                            .input(TerminalInputOptions {
                                request_id: "reply".to_string(),
                                terminal_id: terminal_id.clone(),
                                tenant_id: "local".to_string(),
                                data: "\x1b[1;1R\x1b[?62;22c".to_string(),
                            })
                            .await
                            .unwrap();
                    }
                    if !sent && output.contains("\x1b]133;A") {
                        sent = true;
                        manager
                            .input(TerminalInputOptions {
                                request_id: "input".to_string(),
                                terminal_id: terminal_id.clone(),
                                tenant_id: "local".to_string(),
                                data: "false\r".to_string(),
                            })
                            .await
                            .unwrap();
                    }
                    if output.contains("\x1b]133;D;1") {
                        break;
                    }
                }
            })
            .await;
            manager
                .stop(TerminalStopOptions {
                    request_id: "stop".to_string(),
                    terminal_id,
                    tenant_id: "local".to_string(),
                    force: true,
                })
                .await
                .unwrap();
            assert!(
                done.is_ok(),
                "{shell}: no command-finished mark in {output:?}"
            );
            assert!(output.contains("\x1b]133;C"), "{shell}: {output:?}");
            assert!(output.contains("\x1b]7;"), "{shell}: {output:?}");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn streaming_output_is_published_in_few_batches() {
        let events = EventBus::new(8192);
        let mut rx = events.subscribe();
        let manager = LocalTerminalManager::new(events);
        let cwd = make_temp_workspace("terminal-batching");
        // `seq` writes line by line to the TTY, the case batching is for.
        manager
            .start(TerminalStartOptions {
                request_id: "start".to_string(),
                terminal_id: Some("term-batching".to_string()),
                tenant_id: "local".to_string(),
                workspace_id: None,
                cwd: cwd.display().to_string(),
                shell: None,
                rows: None,
                cols: None,
                command: Some(TerminalCommand {
                    program: "/bin/sh".to_string(),
                    args: vec!["-c".into(), "seq 1 200000".into()],
                    env: vec![("PATH".into(), "/usr/bin:/bin".into())],
                    cwd: cwd.clone(),
                    ssh_host: "local".to_string(),
                }),
                shell_integration: false,
            })
            .await
            .unwrap();
        let mut batches = 0;
        let mut output = String::new();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let event = rx.recv().await.unwrap();
                match event.event_type.as_str() {
                    "terminal.output" => {
                        batches += 1;
                        output.push_str(&payload_text(&event.payload, "data"));
                    }
                    "terminal.exited" => break,
                    _ => {}
                }
            }
        })
        .await
        .expect("exit");
        assert!(output.contains("200000"), "all output arrives");
        // About 1.3 MB: thousands of reads, a few hundred batches at most.
        assert!(batches < 1000, "{batches} batches");
        let _ = fs::remove_dir_all(cwd);
    }

    async fn next_attached(
        rx: &mut tokio::sync::broadcast::Receiver<Arc<crate::event::PublishedEvent>>,
    ) -> Value {
        loop {
            let event = rx.recv().await.unwrap();
            if event.event_type == "terminal.attached" {
                return event.payload.clone();
            }
        }
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
