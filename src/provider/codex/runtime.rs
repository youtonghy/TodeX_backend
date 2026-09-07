//! One process and one stdout consumer per native Codex session.
use super::super::types::{PendingProviderControl, ProviderControl};
use super::*;
use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, Mutex};

const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

pub(super) struct Sessions {
    workers: Mutex<HashMap<String, mpsc::Sender<Command>>>,
}

pub(super) struct Run {
    pub context: DriverContext,
    pub prompt: DriverPrompt,
    pub sink: DriverEventSink,
    pub cancel: watch::Receiver<bool>,
    pub permit: WorkspaceTrustPermit,
}

enum Command {
    Run(Run, oneshot::Sender<Result<DriverTurnResult, AppError>>),
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
            if workers.len() >= 64 && !workers.contains_key(&id) {
                return Err(AppError::Unsupported(
                    "Codex has reached the active worker limit; close an idle session first"
                        .to_owned(),
                ));
            }
            let worker = workers
                .entry(id)
                .or_insert_with(|| spawn_worker(binary.to_owned()));
            if worker.is_closed() {
                *worker = spawn_worker(binary.to_owned());
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
        if let Some(sender) = self.workers.lock().await.remove(id) {
            let (tx, rx) = oneshot::channel();
            if sender.send(Command::Shutdown(tx)).await.is_ok() {
                let _ = rx.await;
            }
        }
    }

    pub async fn stop_all(&self) {
        let workers = std::mem::take(&mut *self.workers.lock().await);
        for (_, sender) in workers {
            let (tx, rx) = oneshot::channel();
            if sender.send(Command::Shutdown(tx)).await.is_ok() {
                let _ = rx.await;
            }
        }
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

fn spawn_worker(binary: String) -> mpsc::Sender<Command> {
    let (tx, rx) = mpsc::channel(32);
    tokio::spawn(worker(binary, rx));
    tx
}

async fn worker(binary: String, mut commands: mpsc::Receiver<Command>) {
    let mut process: Option<JsonLineProcess> = None;
    let mut native_thread = None;
    let mut native_model: Option<String> = None;
    let mut last_sink: Option<DriverEventSink> = None;
    let mut known_queue = VecDeque::<Value>::new();
    loop {
        let command = if let Some(process) = process.as_mut() {
            tokio::select! {
                command = commands.recv() => command,
                message = process.read() => {
                    match message {
                        Ok(Some(message)) => {
                            if message.get("id").is_some() && message.get("method").is_some() {
                                // No active user turn exists to own a new permission dialog.
                                let _ = process.send(&json!({"id":message["id"],"error":{"code":-32000,"message":"TodeX session is idle"}})).await;
                            } else if let Some(sink) = &last_sink {
                                let _ = sink.emit("provider.event", json!({"provider":"codex", "providerMethod":message.get("method"), "metadata":message.get("params")})).await;
                            }
                            continue;
                        }
                        _ => break,
                    }
                }
                _ = tokio::time::sleep(IDLE_TIMEOUT) => break,
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
                    permit,
                } = run;
                if *cancel.borrow() {
                    let _ = respond.send(Err(AppError::TurnCancelled));
                    continue;
                }
                if process.is_none() {
                    let mut spec = CommandSpec::new(&binary, &context.manifest.workspace);
                    spec.args = vec!["app-server".into(), "--listen".into(), "stdio://".into()];
                    match JsonLineProcess::spawn_trusted(&spec, permit).await {
                        Ok(child) => process = Some(child),
                        Err(error) => {
                            let _ = respond.send(Err(error));
                            break;
                        }
                    }
                } else {
                    drop(permit);
                }
                let child = process.as_mut().expect("spawned process");
                let result = async {
                    if native_thread.is_none() {
                        let (thread, model) = super::prepare_codex_thread(
                            child,
                            context,
                            &prompt,
                            &sink,
                            &mut cancel,
                        )
                        .await?;
                        native_thread = Some(thread);
                        native_model = model;
                    }
                    active_turn(
                        child,
                        native_thread.as_deref().unwrap(),
                        &prompt,
                        &mut native_model,
                        &sink,
                        &mut cancel,
                        &mut commands,
                        &mut known_queue,
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
            Command::Control(control) => {
                let _ = control.respond_to.send(Err(unavailable()));
            }
            Command::Shutdown(done) => {
                let _ = done.send(());
            }
        }
    }
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
    prompt: &DriverPrompt,
    native_model: &mut Option<String>,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
    commands: &mut mpsc::Receiver<Command>,
    known_queue: &mut VecDeque<Value>,
) -> Result<DriverTurnResult, AppError> {
    let controls = resolve_execution_config(
        ProviderKind::Codex,
        prompt.permission_mode.as_deref(),
        prompt.work_mode.as_deref(),
        prompt.permission_profile.as_deref(),
        prompt.sandbox_mode.as_deref(),
        prompt.approval_policy.as_deref(),
    )?;
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
    process
        .send(&json!({"id": start_id, "method": "turn/start", "params": params}))
        .await?;
    let mut start_request = Some(start_id);
    let mut queued_start: Option<Value> = None;
    let mut start_deadline =
        tokio::time::Instant::now() + super::super::process::control_timeout()?;
    let mut native_turn: Option<String> = None;
    let mut terminal: Option<String> = None;
    let mut pending = HashMap::<String, Pending>::new();
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
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        // All writes already accepted by this worker settle before finishing or advancing the queue.
        if start_request.is_none()
            && terminal.is_some()
            && pending.is_empty()
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
            _ = tick.tick() => {
                let now = tokio::time::Instant::now();
                if start_request.is_some() && now >= start_deadline && permissions.is_empty() { return Err(AppError::ProviderUnavailable("Codex turn start acknowledgement timed out".to_owned())); }
                if cancel_deadline.is_some_and(|deadline| now >= deadline) { return Err(AppError::ProviderUnavailable("Codex interrupt timed out; worker was stopped".to_owned())); }
                let expired: Vec<_> = pending.iter().filter(|(_, p)| now >= p.deadline).map(|(id,_)|id.clone()).collect();
                for id in expired { if let Some(p) = pending.remove(&id) { let _ = p.control.respond_to.send(Err(AppError::ProviderUnavailable("Codex control acknowledgement timed out; its outcome is unknown".to_owned()))); } }
            }
            message = process.read() => {
                let message = message?.ok_or_else(unavailable)?;
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
                                codex_permission_kind(&method),codex_permission_title(&method,&params),params.clone(),codex_permission_options(&method),&mut cancel).await;
                            (request_id, method, params, decision)
                        });
                    } else { process.send(&json!({"id":request_id,"error":{"code":-32601,"message":"request is not supported by TodeX"}})).await?; }
                    continue;
                }
                if matches!(method, "item/started" | "item/completed") {
                    if let (Some(id), Some(phase)) = (params.pointer("/item/id").and_then(Value::as_str),params.pointer("/item/phase").and_then(Value::as_str)) {
                        item_phases.insert(id.to_owned(),phase.to_owned());
                    }
                }
                if method == "item/agentMessage/delta" && params.get("itemId").and_then(Value::as_str).and_then(|id| item_phases.get(id)).is_some_and(|phase| phase == "commentary") {
                    sink.emit("message.delta", json!({"role":"assistant","provider":"codex","delta":params.get("delta"),"nativeTurnId":params.get("turnId"),"nativeSessionId":thread,"block":codex_block(&params,"assistant_progress","delta",&prompt.turn_id)})).await?;
                } else {
                    handle_codex_message(process, message, sink, cancel, &prompt.turn_id).await?;
                }
            }
        }
    }
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
    use crate::conversation::{
        ConversationEventHub, ConversationManifest, ConversationStore, ProviderState,
    };
    use crate::provider::types::PermissionBroker;
    use crate::workspace_trust::WorkspaceTrustStore;
    use std::sync::Arc;

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
        let trust = WorkspaceTrustStore::new(root.join("trust"), root.clone())
            .await
            .unwrap();
        trust.set_owned("local", &root, true).await.unwrap();
        let driver = Arc::new(CodexDriver {
            binary: script.display().to_string(),
            sessions: Sessions::new(),
            control_probe: tokio::sync::OnceCell::new(),
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
        };
        let permit = trust.acquire_owned("local", &root).await.unwrap();
        let runner = {
            let driver = driver.clone();
            let context = context.clone();
            let prompt = prompt.clone();
            let sink = sink.clone();
            let cancel = cancel.clone();
            tokio::spawn(
                async move { driver.run_turn(context, prompt, sink, cancel, permit).await },
            )
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while !root.join("ready").exists() {
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
            .run_turn(
                context.clone(),
                prompt.clone(),
                sink.clone(),
                cancel,
                trust.acquire_owned("local", &root).await.unwrap(),
            )
            .await
            .unwrap();
        tokio::fs::remove_file(root.join("ready")).await.unwrap();
        let (cancel_tx, cancel) = watch::channel(false);
        let permit = trust.acquire_owned("local", &root).await.unwrap();
        let cancelling = {
            let driver = driver.clone();
            tokio::spawn(
                async move { driver.run_turn(context, prompt, sink, cancel, permit).await },
            )
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
