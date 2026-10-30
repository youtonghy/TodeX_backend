//! One process and one stdout consumer per native Codex session.
use super::super::types::{
    GoalRequest, PendingProviderControl, ProviderControl, ProviderPrewarm, ProviderWork,
    ProviderWorkHandle,
};
use super::*;
use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, Mutex};

const MAX_WORKERS: usize = super::PROFILE.process_model.max_sessions().unwrap();

pub(super) struct Sessions {
    workers: Mutex<HashMap<String, mpsc::Sender<Command>>>,
}

pub(super) struct Run {
    pub context: DriverContext,
    pub prompt: DriverPrompt,
    pub sink: DriverEventSink,
    pub cancel: watch::Receiver<bool>,
}

/// A prewarm: the worker starts its process and prepares the thread.
pub(super) struct Warm {
    pub context: DriverContext,
    pub settings: DriverPrompt,
    pub sink: DriverEventSink,
}

/// `conversation.goal`: prepares the thread if needed, then reads or changes
/// its goal.
pub(super) struct GoalRun {
    pub context: DriverContext,
    pub settings: DriverPrompt,
    pub sink: DriverEventSink,
    pub request: GoalRequest,
}

enum Command {
    Run(Run, oneshot::Sender<Result<DriverTurnResult, AppError>>),
    Warm(Warm, oneshot::Sender<Result<(), AppError>>),
    Goal(GoalRun, oneshot::Sender<Result<Value, AppError>>),
    Control(PendingProviderControl),
    Shutdown(oneshot::Sender<()>),
}

impl Sessions {
    pub fn new() -> Self {
        Self {
            workers: Mutex::new(HashMap::new()),
        }
    }

    pub async fn run(&self, binary: &str, run: Run) -> Result<DriverTurnResult, AppError> {
        let id = run.context.manifest.id.clone();
        let sender = {
            let mut workers = self.workers.lock().await;
            workers.retain(|_, worker| !worker.is_closed());
            if workers.len() >= MAX_WORKERS && !workers.contains_key(&id) {
                return Err(AppError::Unsupported(
                    "Codex has reached the active worker limit; close an idle session first"
                        .to_owned(),
                ));
            }
            let pinned = run.context.manifest.ephemeral;
            let worker = workers
                .entry(id)
                .or_insert_with(|| spawn_worker(binary.to_owned(), pinned));
            if worker.is_closed() {
                *worker = spawn_worker(binary.to_owned(), pinned);
            }
            worker.clone()
        };
        let (tx, rx) = oneshot::channel();
        sender
            .send(Command::Run(run, tx))
            .await
            .map_err(|_| unavailable())?;
        rx.await.map_err(|_| unavailable())?
    }

    /// Runs a goal command on the conversation's worker, starting one if
    /// needed: an active goal keeps Codex working after the command.
    pub async fn goal(&self, binary: &str, goal: GoalRun) -> Result<Value, AppError> {
        let id = goal.context.manifest.id.clone();
        let sender = {
            let mut workers = self.workers.lock().await;
            workers.retain(|_, worker| !worker.is_closed());
            if workers.len() >= MAX_WORKERS && !workers.contains_key(&id) {
                return Err(AppError::Unsupported(
                    "Codex has reached the active worker limit; close an idle session first"
                        .to_owned(),
                ));
            }
            let pinned = goal.context.manifest.ephemeral;
            workers
                .entry(id)
                .or_insert_with(|| spawn_worker(binary.to_owned(), pinned))
                .clone()
        };
        let (tx, rx) = oneshot::channel();
        sender
            .send(Command::Goal(goal, tx))
            .await
            .map_err(|_| unavailable())?;
        rx.await.map_err(|_| unavailable())?
    }

    /// Starts the conversation's worker and prepares its thread (initialize
    /// and `thread/resume` or `thread/start`) without starting a turn. A
    /// conversation with a worker, or a full worker table, is left alone.
    pub async fn prewarm(&self, binary: &str, warm: Warm) -> Result<ProviderPrewarm, AppError> {
        let id = warm.context.manifest.id.clone();
        let (tx, rx) = oneshot::channel();
        {
            let mut workers = self.workers.lock().await;
            workers.retain(|_, worker| !worker.is_closed());
            if workers.contains_key(&id) {
                return Ok(ProviderPrewarm::AlreadyWarm);
            }
            if workers.len() >= MAX_WORKERS {
                return Ok(ProviderPrewarm::Limit);
            }
            let worker = spawn_worker(binary.to_owned(), warm.context.manifest.ephemeral);
            // Queued before any turn can find the worker.
            worker
                .try_send(Command::Warm(warm, tx))
                .map_err(|_| unavailable())?;
            workers.insert(id, worker);
        }
        rx.await.map_err(|_| unavailable())??;
        Ok(ProviderPrewarm::Warmed)
    }

    pub async fn control(
        &self,
        id: &str,
        expected: &str,
        request: &str,
        control: ProviderControl,
    ) -> Result<Value, AppError> {
        let sender = self
            .workers
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(unavailable)?;
        let (tx, rx) = oneshot::channel();
        sender
            .send(Command::Control(PendingProviderControl {
                expected_turn_id: expected.to_owned(),
                request_id: request.to_owned(),
                control,
                respond_to: tx,
            }))
            .await
            .map_err(|_| unavailable())?;
        // The worker owns timeout/correlation; dropping this waiter must never replay a write.
        rx.await.map_err(|_| unavailable())?
    }

    pub async fn stop(&self, id: &str) {
        // Its own statement: in edition 2021 the guard of a lock taken in an
        // `if let` scrutinee lives through the whole block, which would hold
        // every other session's run/control/stop behind this shutdown.
        let sender = self.workers.lock().await.remove(id);
        if let Some(sender) = sender {
            shutdown_worker(id, sender).await;
        }
    }

    pub async fn stop_all(&self) {
        let workers = std::mem::take(&mut *self.workers.lock().await);
        futures_util::future::join_all(
            workers
                .into_iter()
                .map(|(id, sender)| async move { shutdown_worker(&id, sender).await }),
        )
        .await;
    }
}

/// How long a worker may take to acknowledge a shutdown: a busy worker reads
/// it between protocol frames, then gives its process the graceful-stop
/// window before killing it.
const WORKER_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Asks the worker to stop its process and waits, bounded, for it. A worker
/// that does not answer in time still exits once its command channel closes,
/// which dropping `sender` (already out of the map) does.
async fn shutdown_worker(id: &str, sender: mpsc::Sender<Command>) {
    let (tx, rx) = oneshot::channel();
    let stopped = tokio::time::timeout(WORKER_STOP_TIMEOUT, async {
        if sender.send(Command::Shutdown(tx)).await.is_ok() {
            let _ = rx.await;
        }
    })
    .await;
    if stopped.is_err() {
        tracing::warn!(
            conversation_id = id,
            timeout_secs = WORKER_STOP_TIMEOUT.as_secs(),
            "Codex session worker did not acknowledge shutdown in time; closing its command channel"
        );
    }
}

fn unavailable() -> AppError {
    AppError::ProviderUnavailable(
        "Codex session worker is unavailable; refresh its state before retrying".to_owned(),
    )
}
fn inactive() -> AppError {
    AppError::Unsupported("The target Codex turn is no longer active".to_owned())
}

/// `pinned`: the worker serves a private conversation, whose process holds
/// the only copy of the thread, so it never stops for being idle.
fn spawn_worker(binary: String, pinned: bool) -> mpsc::Sender<Command> {
    let (tx, rx) = mpsc::channel(32);
    tokio::spawn(worker(binary, pinned, rx));
    tx
}

async fn worker(binary: String, pinned: bool, mut commands: mpsc::Receiver<Command>) {
    // `[agent].provider_keepalive_seconds` of the loaded configuration.
    let idle_timeout = if pinned {
        super::super::types::PINNED_IDLE_TIMEOUT
    } else {
        crate::config::provider_keepalive()
    };
    let mut process: Option<JsonLineProcess> = None;
    let mut native_thread = None;
    let mut native_model: Option<String> = None;
    let mut last_sink: Option<DriverEventSink> = None;
    let mut known_queue = VecDeque::<Value>::new();
    // A new thread a prewarm started, saved with the next turn.
    let mut warm_state: Option<ProviderState> = None;
    // The prewarm's thread response and settings: the next turn journals
    // its `turn.configuration` (a muted warm journals nothing).
    let mut warm_configuration: Option<(Value, DriverPrompt)> = None;
    // A turn Codex starts on its own (an active goal continues) opens a
    // provider-initiated turn with these: the handle and launch settings of
    // the last user turn or goal command.
    let mut provider_work: Option<(ProviderWorkHandle, DriverPrompt)> = None;
    // Offered to the supervisor: the turn's frames stay unread until the
    // provider-initiated turn takes it over or the offer expires.
    let mut awaiting: Option<AwaitedTurn> = None;
    // A Codex turn no TodeX turn took over, journalled between turns.
    let mut unadopted: Option<String> = None;
    // Journals goal changes that arrive between turns.
    let mut goal_sink: Option<DriverEventSink> = None;
    loop {
        let command = if let Some(process) = process.as_mut() {
            let offer_deadline = awaiting.as_ref().map(|turn| turn.deadline);
            tokio::select! {
                command = commands.recv() => command,
                message = process.read(), if awaiting.is_none() => {
                    match message {
                        Ok(Some(message)) => {
                            if message.get("method").and_then(Value::as_str) == Some("turn/started") {
                                let turn = message.pointer("/params/turn/id").and_then(Value::as_str).map(ToOwned::to_owned);
                                if let Some(offer) = turn.filter(|turn| unadopted.as_ref() != Some(turn)).and_then(|turn| offer_codex_turn(provider_work.as_ref(), turn)) {
                                    process.unread([message]);
                                    awaiting = Some(offer);
                                    continue;
                                }
                            }
                            if message.get("id").is_some() && message.get("method").is_some() {
                                // No active user turn exists to own a new permission dialog.
                                let _ = process.send(&json!({"id":message["id"],"error":{"code":-32000,"message":"TodeX session is idle"}})).await;
                            } else if let Some(goal) = codex_goal_event(&message) {
                                if let Some(sink) = goal_sink.as_ref().or(last_sink.as_ref()) {
                                    let _ = sink.emit("goal.updated", goal).await;
                                }
                            } else if let Some(sink) = &last_sink {
                                let _ = sink.emit("provider.event", json!({"provider":"codex", "providerMethod":message.get("method"), "metadata":message.get("params")})).await;
                            }
                            continue;
                        }
                        _ => break,
                    }
                }
                _ = tokio::time::sleep_until(offer_deadline.unwrap_or_else(tokio::time::Instant::now)), if offer_deadline.is_some() => {
                    if let Some(turn) = awaiting.take() {
                        tracing::warn!("no turn took over Codex's own turn; journalling it between turns");
                        unadopted = Some(turn.native_turn);
                    }
                    continue;
                }
                _ = tokio::time::sleep(idle_timeout), if awaiting.is_none() => break,
            }
        } else {
            commands.recv().await
        };
        match command {
            Some(Command::Run(run, respond)) => {
                let Run {
                    context,
                    prompt,
                    sink,
                    mut cancel,
                } = run;
                if *cancel.borrow() {
                    let _ = respond.send(Err(AppError::TurnCancelled));
                    continue;
                }
                let adopt = match (&context.handles.adopt, &awaiting) {
                    (Some(token), Some(turn)) if *token == turn.adoption => {
                        awaiting = None;
                        true
                    }
                    // The turn it was opened for is gone (journalled without
                    // one, or the process restarted): nothing to take over.
                    (Some(_), _) => {
                        let _ = respond.send(Ok(DriverTurnResult {
                            native_session_id: native_thread.clone(),
                            stop_reason: "no_work".to_owned(),
                            cancelled: false,
                        }));
                        continue;
                    }
                    (None, Some(_)) => {
                        let _ = respond.send(Err(AppError::Conflict(
                            "Codex started a turn of its own; wait for it to finish".to_owned(),
                        )));
                        continue;
                    }
                    (None, None) => false,
                };
                if !adopt {
                    provider_work = context
                        .handles
                        .provider_work
                        .clone()
                        .map(|handle| (handle, provider_turn_settings(&prompt)))
                        .or(provider_work.take());
                }
                if process.is_none() {
                    match spawn_app_server(&binary, &context).await {
                        Ok(child) => process = Some(child),
                        Err(error) => {
                            let _ = respond.send(Err(error));
                            break;
                        }
                    }
                }
                let child = process.as_mut().expect("spawned process");
                let result = async {
                    let mut state_to_save = warm_state.take();
                    let mut configuration = warm_configuration.take();
                    if native_thread.is_none() {
                        let prepared = super::prepare_codex_thread(
                            child,
                            context,
                            &prompt,
                            &sink,
                            &mut cancel,
                        )
                        .await?;
                        native_thread = Some(prepared.thread);
                        native_model = prepared.model;
                        state_to_save = prepared.state;
                        configuration = Some((prepared.response, prompt.clone()));
                    }
                    if let Some(event) = configuration.and_then(|(response, prepared_with)| {
                        super::codex_configuration_event(&response, &prompt, &prepared_with)
                    }) {
                        sink.emit("turn.configuration", event).await?;
                    }
                    active_turn(
                        child,
                        native_thread.as_deref().unwrap(),
                        state_to_save,
                        &prompt,
                        &mut native_model,
                        &sink,
                        &mut cancel,
                        &mut commands,
                        &mut known_queue,
                        adopt,
                    )
                    .await
                }
                .await;
                last_sink = Some(sink);
                let failed = result.is_err();
                if failed {
                    let _ = emit_queue(last_sink.as_ref().unwrap(), &known_queue, true).await;
                }
                let _ = respond.send(result);
                // A failed transport cannot be reused. The saved native id permits an explicit resume.
                if failed {
                    break;
                }
            }
            Some(Command::Warm(warm, respond)) => {
                let Warm {
                    context,
                    settings,
                    sink,
                } = warm;
                if process.is_none() {
                    match spawn_app_server(&binary, &context).await {
                        Ok(child) => process = Some(child),
                        Err(error) => {
                            let _ = respond.send(Err(error));
                            break;
                        }
                    }
                }
                if native_thread.is_some() {
                    let _ = respond.send(Ok(()));
                    continue;
                }
                let child = process.as_mut().expect("spawned process");
                let (_cancel_tx, mut cancel) = watch::channel(false);
                match super::prepare_codex_thread(child, context, &settings, &sink, &mut cancel)
                    .await
                {
                    Ok(prepared) => {
                        native_thread = Some(prepared.thread);
                        native_model = prepared.model;
                        warm_state = prepared.state;
                        warm_configuration = Some((prepared.response, settings));
                        let _ = respond.send(Ok(()));
                    }
                    Err(error) => {
                        let _ = respond.send(Err(error));
                        break;
                    }
                }
            }
            Some(Command::Goal(goal, respond)) => {
                let GoalRun {
                    context,
                    settings,
                    sink,
                    request,
                } = goal;
                if process.is_none() {
                    match spawn_app_server(&binary, &context).await {
                        Ok(child) => process = Some(child),
                        Err(error) => {
                            let _ = respond.send(Err(error));
                            break;
                        }
                    }
                }
                let child = process.as_mut().expect("spawned process");
                if let Some(handle) = context.handles.provider_work.clone() {
                    provider_work = Some((handle, provider_turn_settings(&settings)));
                }
                if native_thread.is_none() {
                    let (_cancel_tx, mut cancel) = watch::channel(false);
                    match super::prepare_codex_thread(child, context, &settings, &sink, &mut cancel)
                        .await
                    {
                        Ok(prepared) => {
                            native_thread = Some(prepared.thread);
                            native_model = prepared.model;
                            warm_state = prepared.state;
                        }
                        Err(error) => {
                            let _ = respond.send(Err(error));
                            break;
                        }
                    }
                }
                // An active goal starts a turn at once, which resumes this
                // thread: a new one is saved first.
                if let Some(state) = warm_state.take() {
                    if let Err(error) = sink.save_provider_state(state).await {
                        let _ = respond.send(Err(error));
                        continue;
                    }
                }
                goal_sink = Some(sink.clone());
                let thread = native_thread.clone().expect("prepared thread");
                let result = codex_goal(child, &thread, &request).await;
                if let Ok(goal) = &result {
                    if request != GoalRequest::Get {
                        let _ = sink
                            .emit("goal.updated", json!({"provider":"codex","goal":goal}))
                            .await;
                    }
                }
                let _ = respond.send(result);
            }
            Some(Command::Control(control)) => {
                let _ = control.respond_to.send(Err(inactive()));
            }
            Some(Command::Shutdown(done)) => {
                if let Some(process) = process.as_mut() {
                    process.terminate().await;
                }
                let _ = done.send(());
                return;
            }
            None => break,
        }
    }
    if let Some(process) = process.as_mut() {
        process.terminate().await;
    }
    while let Ok(command) = commands.try_recv() {
        match command {
            Command::Run(_, reply) => {
                let _ = reply.send(Err(unavailable()));
            }
            Command::Warm(_, reply) => {
                let _ = reply.send(Err(unavailable()));
            }
            Command::Goal(_, reply) => {
                let _ = reply.send(Err(unavailable()));
            }
            Command::Control(control) => {
                let _ = control.respond_to.send(Err(unavailable()));
            }
            Command::Shutdown(done) => {
                let _ = done.send(());
            }
        }
    }
}

/// Starts the conversation's `codex app-server`.
async fn spawn_app_server(
    binary: &str,
    context: &DriverContext,
) -> Result<JsonLineProcess, AppError> {
    let mut spec = CommandSpec::new(binary, &context.manifest.workspace);
    spec.args = vec!["app-server".into(), "--listen".into(), "stdio://".into()];
    if context.manifest.ephemeral {
        spec.private = true;
        private_codex_storage(&mut spec, &context.manifest.id)?;
    }
    JsonLineProcess::spawn(&spec).await
}

/// Points the app-server of a private conversation at its private folder
/// for everything Codex keeps besides the (ephemeral) thread: the state,
/// thread-history and goal databases (`sqlite_home`) and the log database
/// (`log_dir`). `CODEX_HOME` stays, for the user's login and config.
fn private_codex_storage(spec: &mut CommandSpec, conversation_id: &str) -> Result<(), AppError> {
    let root = crate::private_dirs::root();
    let folder = |name: &str| {
        root.ensure_subdir(conversation_id, name).map_err(|error| {
            AppError::Io(std::io::Error::other(format!(
                "the private conversation folder is unavailable: {error}"
            )))
        })
    };
    crate::private_dirs::private_temp_env(spec, conversation_id)?;
    let sqlite = folder("codex-sqlite")?;
    let logs = folder("codex-log")?;
    spec.env.insert(
        "CODEX_SQLITE_HOME".to_owned(),
        sqlite.to_string_lossy().into_owned(),
    );
    for (key, path) in [("sqlite_home", &sqlite), ("log_dir", &logs)] {
        // A TOML string: `-c` parses the value as TOML.
        let value = serde_json::to_string(&path.to_string_lossy())?;
        spec.args.push("-c".to_owned());
        spec.args.push(format!("{key}={value}"));
    }
    Ok(())
}

struct Pending {
    control: PendingProviderControl,
    deadline: tokio::time::Instant,
    queue_pages: Vec<Value>,
    queue_cursors: std::collections::HashSet<String>,
    ordinal: u64,
}

async fn active_turn(
    process: &mut JsonLineProcess,
    thread: &str,
    state_to_save: Option<ProviderState>,
    prompt: &DriverPrompt,
    native_model: &mut Option<String>,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
    commands: &mut mpsc::Receiver<Command>,
    known_queue: &mut VecDeque<Value>,
    // Take over the turn Codex started on its own, whose `turn/started` is
    // the next frame, instead of starting one.
    adopt: bool,
) -> Result<DriverTurnResult, AppError> {
    let controls = resolve_execution_config(
        ProviderKind::Codex,
        prompt.permission_mode.as_deref(),
        prompt.work_mode.as_deref(),
        prompt.permission_profile.as_deref(),
        prompt.sandbox_mode.as_deref(),
        prompt.approval_policy.as_deref(),
    )?;
    let mut start_request = None;
    if adopt {
        if let Some(state) = state_to_save {
            sink.save_provider_state(state).await?;
        }
    } else {
        let start_id = format!("todex-start-{}", uuid::Uuid::new_v4());
        let mut params = json!({
            "threadId": thread, "input": codex_prompt_input(prompt), "model": prompt.model,
            "effort": prompt.reasoning_effort, "approvalPolicy": controls.approval_policy,
            "approvalsReviewer": controls.approvals_reviewer,
            "sandboxPolicy": codex_sandbox_policy(controls.sandbox_mode.as_deref())
        });
        let selected_model = prompt.model.as_ref().or(native_model.as_ref());
        if let Some(model) = selected_model.filter(|_| prompt.work_mode.is_some()) {
            params["collaborationMode"] = json!({
                "mode": if controls.work_mode == "plan" { "plan" } else { "default" },
                "settings": { "model": model, "reasoning_effort": prompt.reasoning_effort, "developer_instructions": null }
            });
        } else if controls.work_mode == "plan" {
            return Err(AppError::Unsupported(
                "Codex Plan mode requires a provider-confirmed or selected model".to_owned(),
            ));
        }
        if let Some(model) = &prompt.model {
            *native_model = Some(model.clone());
        }
        let start = json!({"id": start_id, "method": "turn/start", "params": params});
        match state_to_save {
            // A new native thread is saved while the provider gets the turn.
            Some(state) => {
                let (sent, saved) =
                    tokio::join!(process.send(&start), sink.save_provider_state(state));
                saved?;
                sent?;
            }
            None => process.send(&start).await?,
        }
        start_request = Some(start_id);
    }
    let mut queued_start: Option<Value> = None;
    let mut start_deadline =
        tokio::time::Instant::now() + super::super::process::control_timeout()?;
    let mut native_turn: Option<String> = None;
    let mut terminal: Option<String> = None;
    let mut pending = HashMap::<String, Pending>::new();
    // Goal commands sent during the turn, by request id.
    let mut goals =
        HashMap::<String, (GoalRequest, oneshot::Sender<Result<Value, AppError>>)>::new();
    let mut queue = VecDeque::<Value>::new();
    let mut control_ordinal = 0_u64;
    let mut queue_order = HashMap::<String, u64>::new();
    let mut permission_requests = HashMap::<String, (Value, String, Value)>::new();
    let mut permissions = tokio::task::JoinSet::<(
        Value,
        String,
        Value,
        Result<super::super::types::PermissionDecision, AppError>,
    )>::new();
    let mut interrupted = false;
    let mut interrupt_id: Option<String> = None;
    let mut interrupt_ack = false;
    let mut cancel_deadline = None;
    let mut item_phases = HashMap::<String, String>::new();
    loop {
        // All writes already accepted by this worker settle before finishing or advancing the queue.
        if start_request.is_none()
            && terminal.is_some()
            && pending.is_empty()
            && goals.is_empty()
            && (!interrupted || interrupt_ack)
        {
            let status = terminal.take().unwrap();
            permissions.abort_all();
            for (_, (wire_id, method, params)) in permission_requests.drain() {
                let result = cancelled_permission(&method, &params);
                process.send(&json!({"id":wire_id,"result":result})).await?;
            }
            if status == "completed" && !interrupted && !queue.is_empty() {
                let queued = queue.pop_front().unwrap();
                let id = format!("todex-queue-start-{}", uuid::Uuid::new_v4());
                process.send(&json!({"id":id,"method":"thread/queue/start", "params":{"threadId":thread,"queuedSubmissionId":queued["id"]}})).await?;
                start_request = Some(id);
                start_deadline =
                    tokio::time::Instant::now() + super::super::process::control_timeout()?;
                native_turn = None;
                queued_start = Some(queued);
                continue;
            }
            emit_queue(sink, known_queue, status != "completed" || interrupted).await?;
            return Ok(DriverTurnResult {
                native_session_id: Some(thread.to_owned()),
                stop_reason: status.clone(),
                cancelled: status == "interrupted",
            });
        }
        // Sleep until the earliest timeout instead of polling for it.
        let next_deadline = [
            (start_request.is_some() && permissions.is_empty()).then_some(start_deadline),
            cancel_deadline,
            pending.values().map(|p| p.deadline).min(),
        ]
        .into_iter()
        .flatten()
        .min();
        tokio::select! {
            biased;
            _ = cancel.changed(), if !interrupted => {
                interrupted = true;
                cancel_deadline = Some(tokio::time::Instant::now() + super::super::process::cancel_timeout()?);
                if let Some(turn) = &native_turn {
                    let id = format!("todex-interrupt-{}", uuid::Uuid::new_v4());
                    process.send(&json!({"id":id,"method":"turn/interrupt","params":{"threadId":thread,"turnId":turn}})).await?;
                    interrupt_id = Some(id);
                } else if start_request.is_none() { return Err(AppError::TurnCancelled); }
            }
            command = commands.recv() => match command {
                Some(Command::Control(control)) => {
                    if control.expected_turn_id != prompt.turn_id || interrupted || terminal.is_some() {
                        let _ = control.respond_to.send(Err(inactive())); continue;
                    }
                    let Some(turn) = &native_turn else { let _ = control.respond_to.send(Err(inactive())); continue; };
                    if let ProviderControl::QueueAdd {item_id,..} = &control.control {
                        if pending.values().any(|p| matches!(&p.control.control, ProviderControl::QueueAdd {item_id:other,..} if other == item_id)) {
                            let _ = control.respond_to.send(Err(AppError::Conflict("This queue item is already being submitted".to_owned()))); continue;
                        }
                    }
                    let request = control_request(thread, turn, &control.control, known_queue);
                    match request {
                        Ok((method, params)) => {
                            let id = format!("todex-control-{}", uuid::Uuid::new_v4());
                            if let Err(error) = process.send(&json!({"id":id,"method":method,"params":params})).await {
                                let _ = control.respond_to.send(Err(error)); return Err(unavailable());
                            }
                            control_ordinal += 1;
                            pending.insert(id, Pending {control,ordinal:control_ordinal,deadline:tokio::time::Instant::now() + super::super::process::control_timeout()?,queue_pages:Vec::new(),queue_cursors:std::collections::HashSet::new()});
                        }
                        Err(error) => { let _ = control.respond_to.send(Err(error)); }
                    }
                }
                Some(Command::Shutdown(done)) => {
                    process.terminate().await; let _ = done.send(()); return Err(AppError::TurnCancelled);
                }
                Some(Command::Run(_, reply)) => { let _ = reply.send(Err(AppError::Unsupported("Codex session already has an active turn".to_owned()))); }
                // A goal can be paused or changed while Codex works on it.
                Some(Command::Goal(goal, reply)) => {
                    let id = format!("todex-goal-{}", uuid::Uuid::new_v4());
                    let (method, params) = goal_method(thread, &goal.request);
                    if let Err(error) = process.send(&json!({"id":id,"method":method,"params":params})).await {
                        let _ = reply.send(Err(error)); return Err(unavailable());
                    }
                    goals.insert(id, (goal.request, reply));
                }
                // The thread is already prepared.
                Some(Command::Warm(_, reply)) => { let _ = reply.send(Ok(())); }
                None => return Err(unavailable()),
            },
            result = permissions.join_next(), if !permissions.is_empty() => {
                if let Some(Ok((request_id, method, params, decision))) = result {
                    permission_requests.remove(&request_id.to_string());
                    let result = match decision {
                        Ok(decision) => codex_permission_response(&method, &params, decision),
                        Err(_) if method == "mcpServer/elicitation/request" => json!({"action":"cancel","content":null,"_meta":null}),
                        Err(_) => codex_permission_response(&method, &params, super::super::types::PermissionDecision { outcome:PermissionOutcome::RejectOnce,option_id:None,data:None }),
                    };
                    process.send(&json!({"id":request_id,"result":result})).await?;
                    if start_request.is_some() { start_deadline = tokio::time::Instant::now() + super::super::process::control_timeout()?; }
                }
            }
            _ = tokio::time::sleep_until(next_deadline.unwrap_or_else(tokio::time::Instant::now)), if next_deadline.is_some() => {
                let now = tokio::time::Instant::now();
                if start_request.is_some() && now >= start_deadline && permissions.is_empty() { return Err(AppError::ProviderUnavailable("Codex turn start acknowledgement timed out".to_owned())); }
                if cancel_deadline.is_some_and(|deadline| now >= deadline) { return Err(AppError::ProviderUnavailable("Codex interrupt timed out; worker was stopped".to_owned())); }
                let expired: Vec<_> = pending.iter().filter(|(_, p)| now >= p.deadline).map(|(id,_)|id.clone()).collect();
                for id in expired { if let Some(p) = pending.remove(&id) { let _ = p.control.respond_to.send(Err(AppError::ProviderUnavailable("Codex control acknowledgement timed out; its outcome is unknown".to_owned()))); } }
            }
            message = process.read_frame() => {
                let message = sink.provider_frame(message?).await?.ok_or_else(unavailable)?;
                if message.is_null() { continue; }
                if let Some(id) = message.get("id").and_then(Value::as_str) {
                    if start_request.as_deref() == Some(id) {
                        if let Some(error) = message.get("error") { return Err(rpc_error(error)); }
                        let turn = message.pointer("/result/turn/id").and_then(Value::as_str).ok_or_else(unavailable)?;
                        native_turn = Some(turn.to_owned());
                        start_request = None;
                        if let Some(queued) = queued_start.take() {
                            known_queue.retain(|item| item["id"] != queued["id"]);
                            emit_queue(sink, known_queue, false).await?;
                            sink.emit("message.created", json!({"role":"user", "provider":"codex", "message":{"role":"user","content":queued["input"]},"queueItemId":queued["clientUserMessageId"],"clientRequestId":queued["clientUserMessageId"]})).await?;
                        }
                        if interrupted && interrupt_id.is_none() {
                            let id = format!("todex-interrupt-{}", uuid::Uuid::new_v4());
                            process.send(&json!({"id":id,"method":"turn/interrupt","params":{"threadId":thread,"turnId":turn}})).await?;
                            interrupt_id = Some(id);
                        }
                        continue;
                    }
                    if interrupt_id.as_deref() == Some(id) {
                        if let Some(error) = message.get("error") {
                            // Completion can race an interrupt. Continue to the authoritative
                            // terminal notification, with the existing bounded cancel deadline.
                            sink.emit("provider.event",json!({"provider":"codex","providerMethod":"turn/interrupt","metadata":{"error":error}})).await?;
                        }
                        interrupt_ack = true; continue;
                    }
                    if let Some((request, reply)) = goals.remove(id) {
                        let result = goal_result(&request, &message);
                        if let (Ok(goal), false) = (&result, request == GoalRequest::Get) {
                            sink.emit("goal.updated", json!({"provider":"codex","goal":goal})).await?;
                        }
                        let _ = reply.send(result); continue;
                    }
                    if let Some(mut p) = pending.remove(id) {
                        let result = if let Some(error) = message.get("error") { Err(rpc_error(error)) } else {
                            let mut result = message.get("result").cloned().ok_or_else(unavailable)?;
                            if matches!(&p.control.control, ProviderControl::QueueList) {
                                let Some(page) = result.get("data").and_then(Value::as_array) else {
                                    let _ = p.control.respond_to.send(Err(AppError::ProviderUnavailable("Codex queue list did not contain an item array".to_owned())));
                                    continue;
                                };
                                p.queue_pages.extend(page.iter().cloned());
                                if let Some(cursor) = result.get("nextCursor").and_then(Value::as_str).filter(|cursor| !cursor.is_empty()) {
                                    if !p.queue_cursors.insert(cursor.to_owned()) {
                                        let _ = p.control.respond_to.send(Err(AppError::ProviderUnavailable("Codex queue list repeated a cursor".to_owned())));
                                        continue;
                                    }
                                    let id = format!("todex-control-{}", uuid::Uuid::new_v4());
                                    process.send(&json!({"id":id,"method":"thread/queue/list","params":{"threadId":thread,"cursor":cursor}})).await?;
                                    pending.insert(id,p);
                                    continue;
                                }
                                result["data"] = json!(p.queue_pages);
                            }
                            apply_control_result(&p.control.control, &result, &mut queue);
                            apply_control_result(&p.control.control, &result, known_queue);
                            if let ProviderControl::QueueAdd {item_id,..} = &p.control.control {
                                queue_order.insert(item_id.clone(),p.ordinal);
                                for items in [&mut queue, &mut *known_queue] {
                                    items.make_contiguous().sort_by_key(|item| item.get("clientUserMessageId").and_then(Value::as_str).and_then(|id|queue_order.get(id)).copied().unwrap_or(0));
                                }
                            }
                            if matches!(&p.control.control, ProviderControl::QueueList) {
                                if let Some(items) = result.get("data").and_then(Value::as_array) {
                                    *known_queue = items.iter().cloned().collect();
                                }
                                result["items"] = queue_items(known_queue);
                            }
                            if matches!(&p.control.control, ProviderControl::QueueAdd {..} | ProviderControl::QueueRemove {..} | ProviderControl::QueueList) {
                                emit_queue(sink, known_queue, false).await?;
                            }
                            if let ProviderControl::Configure {model, reasoning_effort} = &p.control.control {
                                if result.get("status").and_then(Value::as_str) == Some("applied") {
                                    let mut effective = json!({"source":"provider-confirmed","scope":"active-turn","effectiveFrom":"subsequent-captures"});
                                    if let Some(model) = model { effective["model"] = json!(model); *native_model = Some(model.clone()); }
                                    if let Some(effort) = reasoning_effort { effective["reasoningEffort"] = json!(effort); }
                                    sink.emit("turn.configuration", json!({"provider":"codex", "requestId":p.control.request_id, "requested":{"model":model,"reasoningEffort":reasoning_effort}, "effective":effective})).await?;
                                }
                            }
                            Ok(result)
                        };
                        let _ = p.control.respond_to.send(result); continue;
                    }
                }
                let method = message.get("method").and_then(Value::as_str).unwrap_or("");
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                if params.get("threadId").and_then(Value::as_str).is_some_and(|id| id != thread) {
                    sink.emit("provider.event", json!({"provider":"codex","providerMethod":method,"metadata":params})).await?;
                    continue;
                }
                if method == "turn/started" {
                    if let Some(id) = params.pointer("/turn/id").and_then(Value::as_str) { native_turn = Some(id.to_owned()); }
                }
                if let Some(status) = turn_completion_status(&message, native_turn.as_deref())? {
                    if status == "failed" { return Err(codex_turn_failure(&message)); }
                    terminal = Some(status.to_owned()); continue;
                }
                if let Some(request_id) = message.get("id").cloned() {
                    if is_codex_permission_method(method) {
                        permission_requests.insert(request_id.to_string(),(request_id.clone(),method.to_owned(),params.clone()));
                        let sink = sink.clone(); let mut cancel = cancel.clone(); let method = method.to_owned();
                        permissions.spawn(async move {
                            let decision = sink.request_permission(jsonrpc_id_text(&request_id).unwrap_or_default(),
                                codex_permission_kind(&method,&params),codex_permission_title(&method,&params),codex_permission_details(&method,&params),codex_permission_options(&method,&params),&mut cancel).await;
                            (request_id, method, params, decision)
                        });
                    } else { process.send(&json!({"id":request_id,"error":{"code":-32601,"message":"request is not supported by TodeX"}})).await?; }
                    continue;
                }
                if let Some(goal) = codex_goal_event(&message) {
                    sink.emit("goal.updated", goal).await?;
                    continue;
                }
                if matches!(method, "item/started" | "item/completed") {
                    if let (Some(id), Some(phase)) = (params.pointer("/item/id").and_then(Value::as_str),params.pointer("/item/phase").and_then(Value::as_str)) {
                        item_phases.insert(id.to_owned(),phase.to_owned());
                    }
                }
                if method == "item/agentMessage/delta" && params.get("itemId").and_then(Value::as_str).and_then(|id| item_phases.get(id)).is_some_and(|phase| phase == "commentary") {
                    let payload = json!({"role":"assistant","provider":"codex","delta":params.get("delta"),"nativeTurnId":params.get("turnId"),"nativeSessionId":thread,"block":codex_block(&params,"assistant_progress","delta",&prompt.turn_id)});
                    let key = codex_delta_key(&payload);
                    sink.emit_delta_keyed("message.delta", payload, key, CODEX_MESSAGE_TEXT).await?;
                } else {
                    handle_codex_message(process, message, sink, cancel, &prompt.turn_id).await?;
                }
            }
        }
    }
}

/// A turn Codex started on its own, offered as a provider-initiated turn.
struct AwaitedTurn {
    adoption: String,
    native_turn: String,
    deadline: tokio::time::Instant,
}

/// How long an offered turn waits for the supervisor to open its TodeX turn.
const ADOPT_TIMEOUT: Duration = Duration::from_secs(10);

/// Asks the supervisor to open a provider-initiated turn for `native_turn`;
/// `None` when no turn can be opened (no handle yet).
fn offer_codex_turn(
    provider_work: Option<&(ProviderWorkHandle, DriverPrompt)>,
    native_turn: String,
) -> Option<AwaitedTurn> {
    let (handle, settings) = provider_work?;
    let adoption = uuid::Uuid::new_v4().simple().to_string();
    let work = ProviderWork {
        adoption: adoption.clone(),
        settings: settings.clone(),
        trigger: json!({ "kind": "goal" }),
    };
    let handle = handle.clone();
    tokio::spawn(async move {
        if let Err(error) = handle.start(work).await {
            tracing::warn!(%error, "could not open a turn for Codex's own turn");
        }
    });
    Some(AwaitedTurn {
        adoption,
        native_turn,
        deadline: tokio::time::Instant::now() + ADOPT_TIMEOUT,
    })
}

/// The launch settings a later provider-initiated turn reports.
fn provider_turn_settings(prompt: &DriverPrompt) -> DriverPrompt {
    DriverPrompt {
        text: String::new(),
        content: Vec::new(),
        skills: Vec::new(),
        ..prompt.clone()
    }
}

/// `goal.updated` for a Codex goal notification.
fn codex_goal_event(message: &Value) -> Option<Value> {
    match message.get("method").and_then(Value::as_str)? {
        "thread/goal/updated" => Some(json!({
            "provider": "codex",
            "goal": message.pointer("/params/goal"),
            "nativeTurnId": message.pointer("/params/turnId"),
        })),
        "thread/goal/cleared" => Some(json!({ "provider": "codex", "goal": null })),
        _ => None,
    }
}

fn goal_method(thread: &str, request: &GoalRequest) -> (&'static str, Value) {
    let set = |fields: Value| {
        let mut params = json!({ "threadId": thread, "origin": "user" });
        params
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        ("thread/goal/set", params)
    };
    match request {
        GoalRequest::Get => ("thread/goal/get", json!({ "threadId": thread })),
        GoalRequest::Clear => ("thread/goal/clear", json!({ "threadId": thread })),
        GoalRequest::Set { objective } => {
            set(json!({ "objective": objective, "status": "active" }))
        }
        GoalRequest::Pause => set(json!({ "status": "paused" })),
        GoalRequest::Resume => set(json!({ "status": "active" })),
    }
}

/// The goal a goal response leaves (`null` when none or cleared).
fn goal_result(request: &GoalRequest, message: &Value) -> Result<Value, AppError> {
    if let Some(error) = message.get("error") {
        return Err(rpc_error(error));
    }
    if *request == GoalRequest::Clear {
        return Ok(Value::Null);
    }
    Ok(message
        .pointer("/result/goal")
        .cloned()
        .unwrap_or(Value::Null))
}

/// Runs a goal command while no turn is active. Frames that arrive before
/// its response (an active goal starts a turn at once) are handed back for
/// the worker to read.
async fn codex_goal(
    process: &mut JsonLineProcess,
    thread: &str,
    request: &GoalRequest,
) -> Result<Value, AppError> {
    let id = format!("todex-goal-{}", uuid::Uuid::new_v4());
    let (method, params) = goal_method(thread, request);
    process
        .send(&json!({ "id": id, "method": method, "params": params }))
        .await?;
    let deadline = tokio::time::Instant::now() + super::super::process::control_timeout()?;
    let mut held = Vec::new();
    let result = loop {
        match process.read_control_until(deadline).await {
            Ok(Some(message)) if message.get("id").and_then(Value::as_str) == Some(id.as_str()) => {
                break goal_result(request, &message);
            }
            Ok(Some(message)) => held.push(message),
            Ok(None) => break Err(unavailable()),
            Err(error) => break Err(error),
        }
    };
    process.unread(held);
    result
}

fn cancelled_permission(method: &str, params: &Value) -> Value {
    if method == "mcpServer/elicitation/request" {
        json!({"action":"cancel","content":null,"_meta":null})
    } else {
        codex_permission_response(
            method,
            params,
            super::super::types::PermissionDecision {
                outcome: PermissionOutcome::RejectOnce,
                option_id: None,
                data: None,
            },
        )
    }
}

fn queue_items(queue: &VecDeque<Value>) -> Value {
    json!(queue.iter().map(|item| json!({"id":item["clientUserMessageId"],"nativeId":item["id"],
        "text":item.get("input").and_then(Value::as_array).into_iter().flatten()
            .filter_map(|input| input.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"), "status":"queued"})).collect::<Vec<_>>())
}

async fn emit_queue(
    sink: &DriverEventSink,
    queue: &VecDeque<Value>,
    paused: bool,
) -> Result<(), AppError> {
    sink.emit(
        "queue.updated",
        json!({"provider":"codex","items":queue_items(queue),"paused":paused}),
    )
    .await?;
    Ok(())
}

fn rpc_error(error: &Value) -> AppError {
    if error.get("code").and_then(Value::as_i64) == Some(-32601) {
        AppError::Unsupported("The installed Codex does not support this control API".to_owned())
    } else {
        AppError::ProviderUnavailable(safe_error_text(error))
    }
}

fn control_request(
    thread: &str,
    turn: &str,
    control: &ProviderControl,
    queue: &VecDeque<Value>,
) -> Result<(&'static str, Value), AppError> {
    Ok(match control {
        ProviderControl::Steer { text } => (
            "turn/steer",
            json!({"threadId":thread,"expectedTurnId":turn,"input":[{"type":"text","text":text}]}),
        ),
        ProviderControl::Configure {
            model,
            reasoning_effort,
        } => (
            "turn/settings/update",
            json!({"threadId":thread,"turnId":turn,"model":model,"effort":reasoning_effort}),
        ),
        ProviderControl::QueueAdd { item_id, text } => {
            if queue
                .iter()
                .any(|item| item["clientUserMessageId"] == *item_id)
            {
                return Err(AppError::InvalidRequest(
                    "This queue item already exists".to_owned(),
                ));
            }
            (
                "thread/queue/add",
                json!({"threadId":thread,"clientUserMessageId":item_id,"input":[{"type":"text","text":text}]}),
            )
        }
        ProviderControl::QueueRemove { item_id } => {
            let native_id = queue
                .iter()
                .find(|item| item["clientUserMessageId"] == *item_id || item["id"] == *item_id)
                .and_then(|item| item.get("id"))
                .cloned()
                .unwrap_or(json!(item_id));
            (
                "thread/queue/delete",
                json!({"threadId":thread,"queuedSubmissionId":native_id}),
            )
        }
        ProviderControl::QueueList => ("thread/queue/list", json!({"threadId":thread})),
        ProviderControl::QueueClear => {
            return Err(AppError::Unsupported(
                "Codex queue items must be removed individually".to_owned(),
            ))
        }
    })
}

fn apply_control_result(control: &ProviderControl, result: &Value, queue: &mut VecDeque<Value>) {
    match control {
        ProviderControl::QueueAdd { .. } => {
            if let Some(item) = result
                .get("queuedSubmission")
                .filter(|item| item.get("id").and_then(Value::as_str).is_some())
            {
                queue.push_back(item.clone());
            }
        }
        ProviderControl::QueueRemove { item_id }
            if result.get("deleted").and_then(Value::as_bool) == Some(true) =>
        {
            queue.retain(|item| item["clientUserMessageId"] != *item_id && item["id"] != *item_id)
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::conversation::{
        ConversationEvent, ConversationEventHub, ConversationManifest, ConversationStore,
        ProviderState,
    };
    #[cfg(unix)]
    use crate::provider::types::PermissionBroker;
    #[cfg(unix)]
    #[cfg(unix)]
    use std::sync::Arc;

    #[tokio::test]
    async fn stopping_a_session_does_not_hold_the_worker_map() {
        let sessions = std::sync::Arc::new(Sessions::new());
        let (tx, mut commands) = mpsc::channel(1);
        sessions.workers.lock().await.insert("slow".to_owned(), tx);
        let stopping = tokio::spawn({
            let sessions = sessions.clone();
            async move { sessions.stop("slow").await }
        });
        let Some(Command::Shutdown(done)) = commands.recv().await else {
            panic!("the worker should be asked to shut down");
        };
        // While that worker is still stopping its process, other sessions
        // can run, control and stop.
        let workers = tokio::time::timeout(Duration::from_secs(1), sessions.workers.lock())
            .await
            .expect("the worker map is free while a worker stops");
        drop(workers);
        assert!(!stopping.is_finished());
        done.send(()).unwrap();
        stopping.await.unwrap();
        assert!(sessions.workers.lock().await.is_empty());
    }

    /// Prewarms a thread with the default settings, then runs its first
    /// turn with `turn_model`; returns the conversation's journal.
    #[cfg(unix)]
    async fn prewarm_then_turn(turn_model: Option<&str>) -> Vec<ConversationEvent> {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("todex-codex-prewarm-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let root = tokio::fs::canonicalize(root).await.unwrap();
        let script = root.join("fake-codex");
        tokio::fs::write(
            &script,
            r#"#!/bin/sh
dir=$(dirname "$0")
printf 'launch\n' >> "$dir/launches"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$dir/wire.jsonl"
  id=$(printf '%s' "$line" | /usr/bin/sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"id":"%s","result":{}}\n' "$id" ;;
    *'"method":"thread/start"'*) printf '{"id":"%s","result":{"thread":{"id":"native"},"model":"fixture-model"}}\n' "$id" ;;
    *'"method":"turn/start"'*)
      printf '{"id":"%s","result":{"turn":{"id":"active","status":"inProgress"}}}\n' "$id"
      printf '{"method":"turn/started","params":{"threadId":"native","turn":{"id":"active","status":"inProgress"}}}\n'
      printf '{"method":"turn/completed","params":{"threadId":"native","turn":{"id":"active","status":"completed"}}}\n' ;;
  esac
done
"#,
        )
        .await
        .unwrap();
        tokio::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        let store = ConversationStore::new(root.join("data")).await.unwrap();
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        let sink = |turn: &str| {
            DriverEventSink::new(
                store.clone(),
                ConversationEventHub::default(),
                PermissionBroker::default(),
                manifest.id.clone(),
            )
            .with_turn_id(turn)
        };
        let prompt = |turn: &str, text: &str| DriverPrompt {
            turn_id: turn.into(),
            text: text.into(),
            content: vec![],
            skills: vec![],
            model: None,
            reasoning_effort: None,
            permission_mode: None,
            work_mode: None,
            permission_profile: None,
            sandbox_mode: None,
            approval_policy: None,
        };
        let context = || DriverContext {
            manifest: manifest.clone(),
            provider_state: ProviderState::new(ProviderKind::Codex),
            agent_mcp: None,
            handles: Default::default(),
        };
        let binary = script.display().to_string();
        let sessions = Sessions::new();
        let warm = Warm {
            context: context(),
            settings: prompt("prewarm", ""),
            sink: sink("prewarm").muted(),
        };
        assert_eq!(
            sessions.prewarm(&binary, warm).await.unwrap(),
            ProviderPrewarm::Warmed
        );
        let wire = || std::fs::read_to_string(root.join("wire.jsonl")).unwrap_or_default();
        assert_eq!(wire().matches(r#""method":"thread/start""#).count(), 1);
        assert_eq!(wire().matches(r#""method":"turn/start""#).count(), 0);
        assert!(store
            .provider_state(&manifest.id)
            .await
            .unwrap()
            .native_session_id
            .is_none());
        let (_cancel, cancel) = watch::channel(false);
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            sessions.run(
                &binary,
                Run {
                    context: context(),
                    prompt: DriverPrompt {
                        model: turn_model.map(str::to_owned),
                        ..prompt("turn-1", "hello")
                    },
                    sink: sink("turn-1"),
                    cancel,
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!result.cancelled);
        let launches = std::fs::read_to_string(root.join("launches")).unwrap();
        assert_eq!(launches.lines().count(), 1);
        assert_eq!(wire().matches(r#""method":"initialize""#).count(), 1);
        assert_eq!(wire().matches(r#""method":"thread/start""#).count(), 1);
        assert_eq!(wire().matches(r#""method":"turn/start""#).count(), 1);
        // The thread the prewarm started is saved with the turn.
        assert_eq!(
            store
                .provider_state(&manifest.id)
                .await
                .unwrap()
                .native_session_id
                .as_deref(),
            Some("native")
        );
        sessions.stop_all().await;
        let history = store.complete_history(&manifest.id).await.unwrap();
        let _ = std::fs::remove_dir_all(root);
        history
    }

    /// The `turn.configuration` events of `history`.
    #[cfg(unix)]
    fn configurations(history: &[ConversationEvent]) -> Vec<Value> {
        history
            .iter()
            .filter(|event| event.event_type == "turn.configuration")
            .map(|event| event.payload.clone())
            .collect()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_prewarm_prepares_the_thread_the_first_turn_uses() {
        // Same settings: the warm's provider-confirmed configuration is
        // journalled once, by the turn.
        let configurations = configurations(&prewarm_then_turn(None).await);
        assert_eq!(configurations.len(), 1);
        assert_eq!(configurations[0]["turnId"], "turn-1");
        assert_eq!(configurations[0]["effective"]["model"], "fixture-model");
        assert_eq!(
            configurations[0]["effective"]["source"],
            "provider-confirmed"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_first_turn_with_other_settings_than_its_prewarm_reports_its_own() {
        let configurations = configurations(&prewarm_then_turn(Some("other-model")).await);
        assert_eq!(configurations.len(), 1);
        assert_eq!(configurations[0]["turnId"], "turn-1");
        assert_eq!(configurations[0]["requested"]["model"], "other-model");
        assert_eq!(configurations[0]["effective"]["model"], "other-model");
        assert_eq!(configurations[0]["effective"]["source"], "requested");
    }

    #[test]
    fn controls_use_exact_upstream_preconditions_and_queue_identity() {
        let queue = VecDeque::from([json!({"id":"native-q","clientUserMessageId":"local-q"})]);
        let (method, params) = control_request(
            "thread",
            "turn",
            &ProviderControl::Steer {
                text: "more".into(),
            },
            &queue,
        )
        .unwrap();
        assert_eq!(method, "turn/steer");
        assert_eq!(params["expectedTurnId"], "turn");
        assert!(params.get("turnId").is_none());
        let (_, params) = control_request(
            "thread",
            "turn",
            &ProviderControl::QueueRemove {
                item_id: "local-q".into(),
            },
            &queue,
        )
        .unwrap();
        assert_eq!(params["queuedSubmissionId"], "native-q");
        assert!(control_request(
            "thread",
            "turn",
            &ProviderControl::QueueAdd {
                item_id: "local-q".into(),
                text: "duplicate".into()
            },
            &queue
        )
        .is_err());
        assert!(matches!(
            rpc_error(&json!({"code":-32601,"message":"Method not found"})),
            AppError::Unsupported(_)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persistent_worker_controls_queue_and_pre_ack_events_match_native_wire() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("todex-codex-runtime-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let root = tokio::fs::canonicalize(root).await.unwrap();
        let script = root.join("fake-codex");
        tokio::fs::write(&script, r#"#!/bin/sh
turns=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> wire.jsonl
  id=$(printf '%s' "$line" | /usr/bin/sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"id":"%s","result":{}}\n' "$id" ;;
    *'"method":"thread/start"'*|*'"method":"thread/resume"'*) printf '{"id":"%s","result":{"thread":{"id":"native"},"model":"fixture-model","reasoningEffort":"high","approvalPolicy":"never","sandbox":{"type":"readOnly"}}}\n' "$id" ;;
    *'"method":"turn/start"'*)
      turns=$((turns+1))
      printf '{"method":"turn/started","params":{"threadId":"native","turn":{"id":"active","status":"inProgress"}}}\n'
      printf '{"id":"%s","result":{"turn":{"id":"active","status":"inProgress"}}}\n' "$id"
      printf '{"method":"item/started","params":{"threadId":"native","turnId":"active","item":{"id":"commentary","type":"agentMessage","phase":"commentary","text":""}}}\n'
      printf '{"method":"item/agentMessage/delta","params":{"threadId":"native","turnId":"active","itemId":"commentary","delta":"working"}}\n'
      printf '{"method":"item/completed","params":{"threadId":"native","turnId":"active","item":{"id":"commentary","type":"agentMessage","phase":"commentary","text":"working"}}}\n'
      printf '{"id":42,"method":"mcpServer/elicitation/request","params":{"threadId":"native","turnId":"active","serverName":"fixture","mode":"form","message":"Your name","requestedSchema":{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}}}\n'
      : > ready
      if [ "$turns" -eq 2 ]; then printf '{"method":"turn/completed","params":{"threadId":"native","turn":{"id":"active","status":"completed"}}}\n'; fi ;;
    *'"method":"turn/settings/update"'*)
      printf '{"method":"item/agentMessage/delta","params":{"threadId":"native","turnId":"active","itemId":"final","delta":"before config ack"}}\n'
      printf '{"id":"%s","result":{"status":"applied"}}\n' "$id" ;;
    *'"method":"thread/queue/add"'*) printf '{"id":"%s","result":{"queuedSubmission":{"id":"q-native","clientUserMessageId":"local-q","input":[{"type":"text","text":"follow up"}]}}}\n' "$id" ;;
    *'"method":"thread/queue/list"'*) printf '{"id":"%s","result":{"data":[{"id":"q-native","clientUserMessageId":"local-q","input":[{"type":"text","text":"follow up"}]}],"nextCursor":null}}\n' "$id" ;;
    *'"id":42'*)
      printf '{"method":"item/agentMessage/delta","params":{"threadId":"native","turnId":"active","itemId":"final","delta":"elicitation answered"}}\n' ;;
    *'"method":"turn/interrupt"'*)
      printf '{"method":"turn/completed","params":{"threadId":"native","turn":{"id":"active","status":"interrupted"}}}\n'
      printf '{"id":"%s","result":{}}\n' "$id" ;;
    *'"method":"turn/steer"'*)
      printf '{"id":"%s","result":{"turnId":"active"}}\n' "$id"
      printf '{"method":"turn/completed","params":{"threadId":"native","turn":{"id":"active","status":"completed"}}}\n' ;;
    *'"method":"thread/queue/start"'*)
      printf '{"method":"turn/started","params":{"threadId":"native","turn":{"id":"queued-turn","status":"inProgress"}}}\n'
      printf '{"method":"item/completed","params":{"threadId":"native","turnId":"queued-turn","item":{"id":"new-item","type":"futureUpstreamItem","payload":{"answer":42}}}}\n'
      printf '{"method":"item/completed","params":{"threadId":"native","turnId":"queued-turn","item":{"id":"image","type":"imageGeneration","status":"completed","result":"fixture.png"}}}\n'
      printf '{"method":"turn/completed","params":{"threadId":"native","turn":{"id":"queued-turn","status":"completed"}}}\n'
      printf '{"id":"%s","result":{"turn":{"id":"queued-turn","status":"completed"}}}\n' "$id" ;;
  esac
done
"#).await.unwrap();
        tokio::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        let store = ConversationStore::new(root.join("data")).await.unwrap();
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        let broker = PermissionBroker::default();
        let sink = DriverEventSink::new(
            store.clone(),
            ConversationEventHub::default(),
            broker.clone(),
            manifest.id.clone(),
        )
        .with_turn_id("local-turn");
        let driver = Arc::new(CodexDriver {
            binary: script.display().to_string(),
            sessions: Sessions::new(),
            control_probe: ControlProbeCache::default(),
            models: CatalogCache::new(None),
        });
        let prompt = DriverPrompt {
            turn_id: "local-turn".into(),
            text: "start".into(),
            content: vec![],
            skills: vec![],
            model: Some("fixture-model".into()),
            reasoning_effort: Some("high".into()),
            permission_mode: None,
            work_mode: None,
            permission_profile: None,
            sandbox_mode: Some("read-only".into()),
            approval_policy: Some("never".into()),
        };
        let (_cancel_tx, cancel) = watch::channel(false);
        let context = DriverContext {
            manifest: manifest.clone(),
            provider_state: ProviderState::new(ProviderKind::Codex),
            agent_mcp: None,
            handles: Default::default(),
        };
        let runner = {
            let driver = driver.clone();
            let context = context.clone();
            let prompt = prompt.clone();
            let sink = sink.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { driver.run_turn(context, prompt, sink, cancel).await })
        };
        // `ready` only says the fake wrote the turn; the commentary delta is
        // journalled after the worker read the start acknowledgement, which
        // is when controls can target the native turn.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let started = root.join("ready").exists()
                    && store
                        .complete_history(&manifest.id)
                        .await
                        .unwrap()
                        .iter()
                        .any(|event| event.event_type == "message.delta");
                if started {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(driver
            .control(
                &manifest.id,
                "stale-turn",
                "stale",
                ProviderControl::Steer {
                    text: "must not be sent".into()
                }
            )
            .await
            .is_err());
        let response = driver
            .control(
                &manifest.id,
                "local-turn",
                "config",
                ProviderControl::Configure {
                    model: Some("fixture-model-2".into()),
                    reasoning_effort: Some("low".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(response["status"], "applied");
        let permission_id = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(event) = store
                    .complete_history(&manifest.id)
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|event| event.event_type == "permission.requested")
                {
                    break event.payload["permissionId"].as_str().unwrap().to_owned();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        // A control response and output arrived while the user dialog was still unanswered.
        assert!(store
            .complete_history(&manifest.id)
            .await
            .unwrap()
            .iter()
            .any(|event| event.payload["delta"] == "before config ack"));
        broker
            .resolve(
                &manifest.id,
                &permission_id,
                "local",
                super::super::super::types::PermissionDecision {
                    outcome: PermissionOutcome::Answer,
                    option_id: Some("answer".into()),
                    data: Some(json!({"name":"Ada"})),
                },
            )
            .await
            .unwrap();

        driver
            .control(
                &manifest.id,
                "local-turn",
                "queue",
                ProviderControl::QueueAdd {
                    item_id: "local-q".into(),
                    text: "follow up".into(),
                },
            )
            .await
            .unwrap();
        let listed = driver
            .control(
                &manifest.id,
                "local-turn",
                "list",
                ProviderControl::QueueList,
            )
            .await
            .unwrap();
        assert_eq!(listed["data"][0]["id"], "q-native");
        driver
            .control(
                &manifest.id,
                "local-turn",
                "steer",
                ProviderControl::Steer {
                    text: "finish now".into(),
                },
            )
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), runner)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result.stop_reason, "completed");
        driver
            .run_turn(context.clone(), prompt.clone(), sink.clone(), cancel)
            .await
            .unwrap();
        tokio::fs::remove_file(root.join("ready")).await.unwrap();
        let (cancel_tx, cancel) = watch::channel(false);
        let cancelling = {
            let driver = driver.clone();
            tokio::spawn(async move { driver.run_turn(context, prompt, sink, cancel).await })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while !root.join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        cancel_tx.send(true).unwrap();
        let cancelled = tokio::time::timeout(Duration::from_secs(5), cancelling)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(cancelled.cancelled);
        driver.shutdown().await;
        let events = store.complete_history(&manifest.id).await.unwrap();
        assert!(events
            .iter()
            .any(|event| event.event_type == "message.delta"
                && event.payload["delta"] == "before config ack"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "message.delta"
                && event.payload["delta"] == "working"
                && event.payload["block"]["category"] == "assistant_progress"));
        assert!(events
            .iter()
            .any(|event| event.payload.pointer("/metadata/item/type")
                == Some(&json!("futureUpstreamItem"))));
        assert!(events
            .iter()
            .any(|event| event.payload["queueItemId"] == "local-q"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "turn.configuration"
                && event.payload["effective"]["model"] == "fixture-model-2"));
        let wire = tokio::fs::read_to_string(root.join("wire.jsonl"))
            .await
            .unwrap();
        let frames: Vec<Value> = wire
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame["method"] == "initialize")
                .count(),
            1
        );
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame["method"] == "thread/start")
                .count(),
            1
        );
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame["method"] == "thread/queue/start")
                .count(),
            1
        );
        assert!(!wire.contains("must not be sent"));
        assert!(frames.iter().any(|frame| frame["id"] == 42
            && frame["result"]["action"] == "accept"
            && frame["result"]["content"]["name"] == "Ada"));
        assert!(frames
            .iter()
            .any(|frame| frame["method"] == "turn/interrupt"));
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
