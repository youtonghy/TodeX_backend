//! One resident Claude Code process per conversation.
//!
//! A worker task owns each conversation's `claude -p --input-format
//! stream-json` process: turns are written to the same stdin, so only the
//! first turn pays for process start-up and `initialize`. Between turns the
//! worker drains stdout and refuses control requests; output that is not
//! part of a turn retires the process instead of reaching the journal,
//! except an invocation Claude starts on its own for background work (see
//! [`super::session_work`]): that becomes a provider-initiated turn. A
//! turn whose launch settings ([`LaunchKey`]) differ from the running
//! process restarts it with `--resume`. When every slot runs a turn, a new
//! conversation falls back to a process for just that turn.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio::time::Instant;

use crate::error::AppError;

use super::super::process::JsonLineProcess;
use super::super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, LostWork, ProviderPrewarm,
    ProviderWork, ProviderWorkHandle,
};
use super::session_work::SessionWork;
use super::{Catalogs, TurnStart};

/// How long a cancelled turn may take to settle after `interrupt` before
/// its process is stopped instead of reused.
const INTERRUPT_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long stopping a session waits for its worker to exit.
const STOP_TIMEOUT: Duration = Duration::from_secs(6);
/// How long work Claude started on its own waits for the supervisor's turn
/// before the process is stopped instead.
const ADOPT_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(1)
} else {
    Duration::from_secs(10)
};
/// The longest background work keeps a process past its keepalive, counted
/// from the end of its last turn: a backgrounded server never finishes.
const MAX_BACKGROUND_HOLD: Duration = Duration::from_secs(6 * 3600);
/// How long a stopping worker waits for the loss of its background work to
/// be journalled.
const REPORT_LOST_TIMEOUT: Duration = Duration::from_secs(2);

/// Everything a turn needs to start Claude.
#[derive(Clone)]
pub(in crate::provider) struct Launcher {
    pub binary: String,
    pub catalogs: Catalogs,
}

pub(in crate::provider) struct Run {
    pub context: DriverContext,
    pub prompt: DriverPrompt,
    pub sink: DriverEventSink,
    pub cancel: watch::Receiver<bool>,
}

struct Turn {
    run: Run,
    reply: oneshot::Sender<Reply>,
    /// Keeps the worker busy until it is done with this turn.
    pending: Pending,
}

/// A prewarm: start the process with `settings`' launch settings and
/// `initialize` it, without a turn.
pub(in crate::provider) struct Warm {
    pub context: DriverContext,
    pub settings: DriverPrompt,
}

/// What a worker is asked to do, one at a time. Unboxed: the worker's
/// queue holds a single request.
#[allow(clippy::large_enum_variant)]
enum Request {
    Turn(Turn),
    Warm(Warm, oneshot::Sender<Result<(), AppError>>),
}

enum Reply {
    Finished(Result<DriverTurnResult, AppError>),
    /// The worker stopped before starting this turn; `retry` is false when
    /// the stop was requested, so the turn must not reopen the session.
    NotStarted {
        run: Box<Run>,
        retry: bool,
    },
}

/// Why a worker is asked to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    Running,
    /// Its slot went to another conversation; the session itself is fine.
    Evicted,
    /// The session or the daemon was shut down.
    Stopped,
}

impl Stop {
    /// Whether a turn the stopping worker never started may reopen the
    /// session on a new one.
    fn retry(self) -> bool {
        self != Self::Stopped
    }

    /// The [`LostWork`] reason when this stop takes background work along.
    fn lost_reason(self) -> &'static str {
        match self {
            Self::Evicted => "evicted",
            Self::Running | Self::Stopped => "stopped",
        }
    }
}

/// Turns a worker owes and when it last finished one.
struct Activity {
    pending: usize,
    idle_since: Instant,
    /// The session holds background work (or Claude started some): evicted
    /// only after workers without.
    background: bool,
}

/// One turn handed to a worker, from `acquire` until the worker is done
/// with it (interrupt included). A worker holding one is never evicted.
struct Pending(Arc<std::sync::Mutex<Activity>>);

impl Drop for Pending {
    fn drop(&mut self) {
        let mut activity = self.0.lock().expect("Claude worker activity lock");
        activity.pending -= 1;
        activity.idle_since = Instant::now();
    }
}

/// Resolves once a process, and the task that owns it, has exited.
#[derive(Clone)]
struct Exit(watch::Receiver<bool>);

impl Exit {
    fn done(&self) -> bool {
        // A dropped sender means its owner is gone, process included.
        *self.0.borrow() || self.0.has_changed().is_err()
    }

    /// Waits, bounded by [`STOP_TIMEOUT`], for the exit.
    async fn wait(mut self) {
        let _ = tokio::time::timeout(STOP_TIMEOUT, self.0.wait_for(|exited| *exited)).await;
    }
}

#[derive(Clone)]
struct Worker {
    turns: mpsc::Sender<Request>,
    shutdown: watch::Sender<Stop>,
    stopped: Exit,
    activity: Arc<std::sync::Mutex<Activity>>,
    /// Serves a private conversation: its process holds the only copy of
    /// the agent's context, so it never idles out and is never evicted.
    pinned: bool,
}

impl Worker {
    fn reserve(&self) -> Pending {
        self.activity
            .lock()
            .expect("Claude worker activity lock")
            .pending += 1;
        Pending(self.activity.clone())
    }

    /// When the worker last finished a turn, and whether it holds
    /// background work; `None` while it owes a turn.
    fn idle_since(&self) -> Option<(bool, Instant)> {
        let activity = self.activity.lock().expect("Claude worker activity lock");
        (activity.pending == 0).then_some((activity.background, activity.idle_since))
    }

    /// Stopping or stopped: it takes no new turn, and its process may still
    /// be exiting.
    fn stopping(&self) -> bool {
        self.turns.is_closed() || *self.shutdown.borrow() != Stop::Running
    }

    fn stop(&self, stop: Stop) {
        // `send_replace`: the reason must stick even once the worker is gone.
        self.shutdown.send_replace(stop);
    }
}

#[derive(Default)]
struct Table {
    /// Every worker until its process has exited, stopping ones included,
    /// so a conversation never gets a second process on its transcript.
    workers: HashMap<String, Worker>,
    /// One-off turns' processes (see [`run_once`]) until they have exited.
    detached: HashMap<String, Exit>,
    /// Set while [`Sessions::stop_all`] runs: no process starts behind its
    /// back. Cleared afterwards, as the supervisor may serve turns again.
    closing: bool,
}

impl Table {
    fn prune(&mut self) {
        self.workers.retain(|_, worker| !worker.stopped.done());
        self.detached.retain(|_, exit| !exit.done());
    }

    /// The conversation's process that is exiting, if any.
    fn exiting(&self, id: &str) -> Option<Exit> {
        if let Some(exit) = self.detached.get(id) {
            return Some(exit.clone());
        }
        self.workers
            .get(id)
            .filter(|worker| worker.stopping())
            .map(|worker| worker.stopped.clone())
    }

    /// Workers holding a slot; stopping ones are already giving theirs up.
    fn slots_taken(&self) -> usize {
        self.workers
            .values()
            .filter(|worker| !worker.stopping())
            .count()
    }
}

enum Acquired {
    Worker(Worker, Pending),
    /// Every slot runs a turn: the turn gets a process of its own, and the
    /// conversation stays reserved until the sender reports it exited.
    Detached(watch::Sender<bool>),
}

pub(in crate::provider) struct Sessions {
    table: Mutex<Table>,
    idle_timeout: Duration,
    max_workers: usize,
}

impl Sessions {
    pub fn new(idle_timeout: Duration, max_workers: usize) -> Self {
        Self {
            table: Mutex::new(Table::default()),
            idle_timeout,
            max_workers,
        }
    }

    /// Runs a turn on the conversation's resident process. A turn that raced
    /// its worker's own exit (idle timeout, retired process, eviction) never
    /// reached Claude and is retried once on a new worker.
    pub async fn run(
        &self,
        launcher: Launcher,
        mut run: Run,
    ) -> Result<DriverTurnResult, AppError> {
        let id = run.context.manifest.id.clone();
        let mut retried = false;
        loop {
            let pinned = run.context.manifest.ephemeral;
            let (worker, pending) = match self.acquire(&id, &launcher, pinned).await? {
                Acquired::Worker(worker, pending) => (worker, pending),
                Acquired::Detached(exited) => return run_once(&launcher, run, exited).await,
            };
            let (reply, response) = oneshot::channel();
            let turn = Turn {
                run,
                reply,
                pending,
            };
            let retry = match worker.turns.send(Request::Turn(turn)).await {
                Ok(()) => match response.await {
                    Ok(Reply::Finished(result)) => return result,
                    Ok(Reply::NotStarted {
                        run: returned,
                        retry,
                    }) => {
                        run = *returned;
                        retry
                    }
                    Err(_) => return Err(unavailable()),
                },
                // Closed between lookup and send: the worker is stopping.
                Err(mpsc::error::SendError(Request::Turn(turn))) => {
                    run = turn.run;
                    worker.shutdown.borrow().retry()
                }
                Err(mpsc::error::SendError(Request::Warm(..))) => {
                    unreachable!("a turn was sent")
                }
            };
            if !retry || retried {
                return Err(AppError::Conflict(
                    "Claude Code session closed before the turn started; retry".to_owned(),
                ));
            }
            retried = true;
            worker.stopped.wait().await;
        }
    }

    /// The conversation's worker, with the turn reserved on it, or a
    /// detached slot when every worker is running a turn. At the limit the
    /// longest-idle worker is stopped to make room; private conversations'
    /// workers are never stopped for it. A private conversation (`pinned`)
    /// gets no detached slot: a one-off process would start without its
    /// context, so the turn fails instead. Never returns while the
    /// conversation's previous process is still exiting.
    async fn acquire(
        &self,
        id: &str,
        launcher: &Launcher,
        pinned: bool,
    ) -> Result<Acquired, AppError> {
        loop {
            let exiting = {
                let mut table = self.table.lock().await;
                if table.closing {
                    return Err(shutting_down());
                }
                table.prune();
                match table.exiting(id) {
                    Some(exit) => exit,
                    None => {
                        if let Some(worker) = table.workers.get(id) {
                            return Ok(Acquired::Worker(worker.clone(), worker.reserve()));
                        }
                        if table.slots_taken() >= self.max_workers {
                            let evict = table
                                .workers
                                .values()
                                .filter(|worker| !worker.stopping() && !worker.pinned)
                                .filter_map(|worker| Some((worker.idle_since()?, worker)))
                                // Without background work first, then the
                                // longest idle.
                                .min_by_key(|(idle_since, _)| *idle_since)
                                .map(|(_, worker)| worker);
                            let Some(evict) = evict else {
                                if pinned {
                                    return Err(no_private_slot());
                                }
                                let (exited, exit) = watch::channel(false);
                                table.detached.insert(id.to_owned(), Exit(exit));
                                return Ok(Acquired::Detached(exited));
                            };
                            tracing::debug!(
                                "Claude resident session limit reached; stopping the longest-idle session"
                            );
                            // It stays in the table until its process is gone.
                            evict.stop(Stop::Evicted);
                        }
                        let worker = spawn_worker(launcher.clone(), self.idle_timeout, pinned);
                        let pending = worker.reserve();
                        table.workers.insert(id.to_owned(), worker.clone());
                        return Ok(Acquired::Worker(worker, pending));
                    }
                }
            };
            // Never two processes on one transcript.
            exiting.wait().await;
        }
    }

    /// Starts the conversation's process with `warm.settings`' launch
    /// settings and initializes it. A conversation with a worker, or a full
    /// worker table, is left alone: a prewarm never evicts. The warm worker
    /// stays idle, so a turn needing a slot may evict it.
    pub async fn prewarm(
        &self,
        launcher: Launcher,
        warm: Warm,
    ) -> Result<ProviderPrewarm, AppError> {
        let id = warm.context.manifest.id.clone();
        let (reply, response) = oneshot::channel();
        loop {
            let exiting = {
                let mut table = self.table.lock().await;
                if table.closing {
                    return Err(shutting_down());
                }
                table.prune();
                if let Some(exit) = table.exiting(&id) {
                    exit
                } else {
                    if table.workers.contains_key(&id) {
                        return Ok(ProviderPrewarm::AlreadyWarm);
                    }
                    if table.slots_taken() >= self.max_workers {
                        return Ok(ProviderPrewarm::Limit);
                    }
                    let worker =
                        spawn_worker(launcher, self.idle_timeout, warm.context.manifest.ephemeral);
                    // Queued before any turn can find the worker.
                    worker
                        .turns
                        .try_send(Request::Warm(warm, reply))
                        .map_err(|_| unavailable())?;
                    table.workers.insert(id, worker);
                    break;
                }
            };
            exiting.wait().await;
        }
        response.await.map_err(|_| unavailable())??;
        Ok(ProviderPrewarm::Warmed)
    }

    /// Stops the conversation's process, if any, and waits for it to exit.
    pub async fn stop(&self, id: &str) {
        // Its own statement: a guard taken in an `if let` scrutinee would
        // hold every other session behind this shutdown.
        let worker = self.table.lock().await.workers.get(id).cloned();
        if let Some(worker) = worker {
            worker.stop(Stop::Stopped);
            worker.stopped.wait().await;
        }
    }

    /// Stops every process: all are signalled before any is awaited. No
    /// process starts until every one has exited.
    pub async fn stop_all(&self) {
        let (workers, detached) = {
            let mut table = self.table.lock().await;
            table.closing = true;
            let workers: Vec<_> = table.workers.values().cloned().collect();
            let detached: Vec<_> = table.detached.values().cloned().collect();
            (workers, detached)
        };
        for worker in &workers {
            worker.stop(Stop::Stopped);
        }
        let exits = workers
            .into_iter()
            .map(|worker| worker.stopped)
            .chain(detached);
        futures_util::future::join_all(exits.map(Exit::wait)).await;
        self.table.lock().await.closing = false;
    }

    #[cfg(all(test, unix))]
    async fn worker_count(&self) -> usize {
        let mut table = self.table.lock().await;
        table.prune();
        table.workers.len()
    }

    /// Whether the conversation has a worker that takes turns.
    #[cfg(all(test, unix))]
    async fn holds(&self, id: &str) -> bool {
        let mut table = self.table.lock().await;
        table.prune();
        table
            .workers
            .get(id)
            .is_some_and(|worker| !worker.stopping())
    }
}

fn unavailable() -> AppError {
    AppError::ProviderUnavailable(
        "Claude Code session stopped before the turn completed".to_owned(),
    )
}

fn no_private_slot() -> AppError {
    AppError::ProviderUnavailable(
        "Every Claude Code resident session is in use by a running turn or another private \
         conversation; a private conversation needs a resident process of its own. Wait for a \
         turn to finish or close a private conversation, then retry."
            .to_owned(),
    )
}

fn shutting_down() -> AppError {
    AppError::ProviderUnavailable("Claude Code sessions are shutting down".to_owned())
}

/// The result of a turn cancelled before its prompt reached Claude.
fn cancelled_before_start(session_id: String) -> DriverTurnResult {
    DriverTurnResult {
        native_session_id: Some(session_id),
        stop_reason: "cancelled".to_owned(),
        cancelled: true,
    }
}

/// The launch settings of a Claude process. Every one is fixed by the
/// command line or read at start-up, so a turn whose key differs from the
/// running process's needs a new process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::provider) struct LaunchKey {
    binary: String,
    workspace: PathBuf,
    session_id: String,
    permission_mode: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    /// Digest of the injected agent MCP servers (command, env, token).
    mcp: Option<[u8; 32]>,
    config_dir: PathBuf,
    /// Digest of the user `settings.json`, which carries the active provider
    /// profile's environment (API key, base URL) and is read at start-up.
    settings: Option<[u8; 32]>,
    /// A private conversation's process (`--no-session-persistence`).
    ephemeral: bool,
}

impl LaunchKey {
    pub(in crate::provider) async fn new(
        binary: &str,
        context: &DriverContext,
        prompt: &DriverPrompt,
    ) -> Result<Self, AppError> {
        let controls = super::super::types::resolve_execution_config(
            context.manifest.provider,
            prompt.permission_mode.as_deref(),
            prompt.work_mode.as_deref(),
            prompt.permission_profile.as_deref(),
            prompt.sandbox_mode.as_deref(),
            prompt.approval_policy.as_deref(),
        )?;
        let config_dir = crate::agent_providers::claude_config_dir();
        let settings = tokio::fs::read(config_dir.join("settings.json"))
            .await
            .ok()
            .map(|bytes| Sha256::digest(bytes).into());
        Ok(Self {
            binary: binary.to_owned(),
            workspace: context.manifest.workspace.clone(),
            session_id: super::claude_session_id(context),
            permission_mode: controls.provider_mode,
            model: prompt.model.clone(),
            effort: prompt.reasoning_effort.clone(),
            mcp: context
                .agent_mcp
                .as_ref()
                .map(|launch| Sha256::digest(format!("{launch:?}")).into()),
            config_dir,
            settings,
            ephemeral: context.manifest.ephemeral,
        })
    }
}

struct Live {
    process: JsonLineProcess,
    key: LaunchKey,
    /// The process's usage counters run across its turns.
    usage: super::ClaudeUsageLedger,
    work: SessionWork,
    /// From the last turn: how to open a turn for work Claude starts on its
    /// own, and the launch settings that turn reports.
    provider_work: Option<(ProviderWorkHandle, DriverPrompt)>,
    /// What the process reported about its permission mode at start-up.
    mode: super::permission_mode::ModeReport,
    /// The process's subagent runs, which background agents keep updating
    /// after the turn that started them.
    subagents: super::ClaudeSubagents,
    runtime_id: String,
    /// From the last turn: journals subagent progress while no turn runs,
    /// and the turn a run without one of its own is attributed to.
    idle_sink: Option<(DriverEventSink, String)>,
    /// A private conversation's earlier process is gone with its context;
    /// the next turn tells the user (see [`super::super::types::context_lost_event`]).
    context_lost: bool,
}

impl Live {
    fn new(process: JsonLineProcess, key: LaunchKey, runtime_id: String) -> Self {
        Self {
            process,
            key,
            usage: Default::default(),
            work: SessionWork::default(),
            provider_work: None,
            mode: Default::default(),
            subagents: Default::default(),
            runtime_id,
            idle_sink: None,
            context_lost: false,
        }
    }

    /// Asks the supervisor for a turn to take over the invocation Claude
    /// just started; returns the adoption token and the background tasks
    /// that invocation hands back, or `None` when no turn can be opened.
    fn offer_work(&mut self) -> Option<(String, Vec<String>)> {
        let (handle, settings) = self.provider_work.clone()?;
        let adoption = uuid::Uuid::new_v4().simple().to_string();
        let trigger = self.work.trigger(Instant::now());
        let tasks = trigger
            .get("taskIds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        let work = ProviderWork {
            adoption: adoption.clone(),
            settings,
            trigger,
        };
        tokio::spawn(async move {
            if let Err(error) = handle.start(work).await {
                tracing::warn!(%error, "could not open a turn for Claude Code's own work");
            }
        });
        Some((adoption, tasks))
    }

    /// Stops the process and journals the background work it took along
    /// (plus `handed_back`, tasks whose follow-up nobody took over yet).
    async fn terminate(mut self, handed_back: Vec<String>, reason: &'static str) {
        let mut task_ids = self.work.task_ids();
        task_ids.extend(handed_back);
        task_ids.sort();
        task_ids.dedup();
        self.process.terminate().await;
        if self.key.ephemeral {
            super::remove_private_session_leftovers(&self.key.config_dir, &self.key.session_id);
        }
        let Some((handle, _)) = self.provider_work.filter(|_| !task_ids.is_empty()) else {
            return;
        };
        let lost = handle.lost(LostWork { task_ids, reason });
        if tokio::time::timeout(REPORT_LOST_TIMEOUT, lost)
            .await
            .is_err()
        {
            tracing::warn!("journalling Claude Code's lost background work timed out");
        }
    }
}

/// When an idle process stops: after the keepalive, or later while its
/// session holds background work, bounded by [`MAX_BACKGROUND_HOLD`].
fn idle_deadline(ended: Instant, keepalive: Duration, work: &mut SessionWork) -> Instant {
    let base = ended + keepalive;
    match work.hold_until(Instant::now(), ended + MAX_BACKGROUND_HOLD) {
        Some(until) => base.max(until),
        None => base,
    }
}

fn mark_background(activity: &std::sync::Mutex<Activity>, background: bool) {
    activity
        .lock()
        .expect("Claude worker activity lock")
        .background = background;
}

/// What the idle worker waits for next. Unboxed like [`Request`]: it
/// lives for one loop iteration.
#[allow(clippy::large_enum_variant)]
enum Next {
    Request(Request),
    Between(Ended),
    Stop,
}

fn spawn_worker(launcher: Launcher, idle_timeout: Duration, pinned: bool) -> Worker {
    let idle_timeout = if pinned {
        super::super::types::PINNED_IDLE_TIMEOUT
    } else {
        idle_timeout
    };
    let (turns, turn_rx) = mpsc::channel(1);
    let (shutdown, shutdown_rx) = watch::channel(Stop::Running);
    let (stopped_tx, stopped) = watch::channel(false);
    let activity = Arc::new(std::sync::Mutex::new(Activity {
        pending: 0,
        idle_since: Instant::now(),
        background: false,
    }));
    tokio::spawn(worker(
        launcher,
        idle_timeout,
        turn_rx,
        shutdown_rx,
        stopped_tx,
        activity.clone(),
    ));
    Worker {
        turns,
        shutdown,
        stopped: Exit(stopped),
        activity,
        pinned,
    }
}

/// What a turn on the worker's process left behind.
enum Ended {
    /// The process is between turns and can take the next one.
    Reusable,
    /// The process must be stopped.
    Retire,
    /// Claude started work of its own between turns.
    Unsolicited,
}

async fn worker(
    launcher: Launcher,
    idle_timeout: Duration,
    mut turns: mpsc::Receiver<Request>,
    mut shutdown: watch::Receiver<Stop>,
    stopped: watch::Sender<bool>,
    activity: Arc<std::sync::Mutex<Activity>>,
) {
    let mut live: Option<Live> = None;
    let mut turn_ended = Instant::now();
    let mut idle_at = turn_ended + idle_timeout;
    // Work Claude started on its own, waiting for the supervisor's turn: the
    // adoption token, when to give up, and the tasks it hands back. Its
    // frames stay unread meanwhile.
    let mut awaiting: Option<(String, Instant, Vec<String>)> = None;
    // Why the loop ended, should background work go with the process.
    let mut reason = "stopped";
    loop {
        // Biased: a requested stop wins, then a waiting turn; the deadlines
        // only fire when nothing else is ready.
        let next = tokio::select! {
            biased;
            _ = shutdown.changed() => {
                reason = shutdown.borrow().lost_reason();
                Next::Stop
            }
            turn = turns.recv() => match turn { Some(turn) => Next::Request(turn), None => Next::Stop },
            between = async {
                match (live.as_mut(), &awaiting) {
                    (Some(live), None) => between_turns(live).await,
                    _ => std::future::pending().await,
                }
            } => Next::Between(between),
            _ = async {
                match &awaiting {
                    Some((_, deadline, _)) => tokio::time::sleep_until(*deadline).await,
                    None => tokio::time::sleep_until(idle_at).await,
                }
            } => {
                if awaiting.is_some() {
                    tracing::warn!("no turn took over Claude Code's own work; restarting its process");
                    reason = "not_adopted";
                } else {
                    // Work still held at the idle deadline outlived its cap.
                    reason = "expired";
                }
                Next::Stop
            }
        };
        let turn = match next {
            Next::Stop => break,
            Next::Request(request) => request,
            Next::Between(Ended::Retire) => {
                reason = "process_ended";
                break;
            }
            Next::Between(Ended::Reusable) => {
                // A background task may have started or ended.
                if let Some(live) = live.as_mut() {
                    idle_at = idle_deadline(turn_ended, idle_timeout, &mut live.work);
                    mark_background(&activity, live.work.pending());
                }
                continue;
            }
            Next::Between(Ended::Unsolicited) => {
                let offered = live.as_mut().and_then(Live::offer_work);
                let Some((adoption, tasks)) = offered else {
                    tracing::warn!(
                        "Claude Code started work between turns; restarting its process"
                    );
                    reason = "not_adopted";
                    break;
                };
                mark_background(&activity, true);
                awaiting = Some((adoption, Instant::now() + ADOPT_TIMEOUT, tasks));
                continue;
            }
        };
        let Turn {
            run,
            mut reply,
            pending,
        } = match turn {
            Request::Turn(turn) => turn,
            Request::Warm(warm, reply) => {
                if live.is_some() {
                    let _ = reply.send(Ok(()));
                    continue;
                }
                let Warm { context, settings } = warm;
                let (_cancel_tx, mut cancel) = watch::channel(false);
                let started = tokio::select! {
                    started = async {
                        let key = LaunchKey::new(&launcher.binary, &context, &settings).await?;
                        let (process, runtime_id) =
                            super::spawn_claude(&launcher.binary, &context, &settings)
                                .await?;
                        // Held before `initialize`: a failed process is
                        // stopped by this worker, which stays in the table
                        // until it has exited.
                        let started = live.insert(Live::new(process, key, runtime_id.clone()));
                        started.context_lost =
                            super::super::types::private_context_lost(&context);
                        let expected = started.key.permission_mode.clone();
                        started.mode = super::initialize_session(
                            &mut started.process,
                            &launcher.catalogs,
                            &context,
                            runtime_id,
                            expected.as_deref(),
                            &mut cancel,
                        )
                        .await?;
                        Ok::<(), AppError>(())
                    } => started,
                    _ = shutdown.changed() => {
                        let _ = reply.send(Err(AppError::TurnCancelled));
                        reason = shutdown.borrow().lost_reason();
                        break;
                    }
                };
                match started {
                    Ok(()) => {
                        let _ = reply.send(Ok(()));
                        turn_ended = Instant::now();
                        idle_at = turn_ended + idle_timeout;
                        continue;
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        break;
                    }
                }
            }
        };
        let Run {
            context,
            prompt,
            sink,
            mut cancel,
        } = run;
        let mut start = match (&context.handles.adopt, &awaiting) {
            (Some(token), Some((expected, _, _))) if token == expected && live.is_some() => {
                TurnStart::Adopt
            }
            // The work it was opened for is gone (the process restarted or
            // another turn read it): nothing to take over.
            (Some(_), _) => {
                let session_id = super::claude_session_id(&context);
                let _ = reply.send(Reply::Finished(Ok(nothing_to_adopt(session_id))));
                continue;
            }
            // A prompt arriving first reads Claude's own invocation along
            // with its answer: Claude queues the prompt behind it.
            (None, Some(_)) => TurnStart::AfterOwnWork,
            (None, None) => TurnStart::Prompt,
        };
        let adopt = start == TurnStart::Adopt;
        // Tasks Claude was about to report on: this turn reads that report,
        // unless the process goes first.
        let handed_back = awaiting
            .take()
            .map(|(_, _, tasks)| tasks)
            .unwrap_or_default();
        // Cancelled while it waited in the queue: it never reaches Claude.
        if *cancel.borrow() {
            let session_id = super::claude_session_id(&context);
            let _ = reply.send(Reply::Finished(Ok(cancelled_before_start(session_id))));
            continue;
        }
        let key = match LaunchKey::new(&launcher.binary, &context, &prompt).await {
            Ok(key) => key,
            Err(error) => {
                // Rejected before reaching Claude; the process is untouched.
                let _ = reply.send(Reply::Finished(Err(error)));
                continue;
            }
        };
        // An adopted invocation runs on the process that started it.
        if let Some(stale) = live.take_if(|live| !adopt && live.key != key) {
            // Never two processes on one transcript: the old one is gone
            // before the new one resumes it, Claude's own work with it.
            stale.terminate(handed_back, "settings_changed").await;
            start = TurnStart::Prompt;
        }
        let session_id = live.as_ref().filter(|_| adopt).map_or_else(
            || key.session_id.clone(),
            |live| live.key.session_id.clone(),
        );
        let mut pending_provider_work = None;
        if !adopt {
            // The settings a later provider-initiated turn reports.
            let settings = DriverPrompt {
                text: String::new(),
                content: Vec::new(),
                skills: Vec::new(),
                ..prompt.clone()
            };
            pending_provider_work = context
                .handles
                .provider_work
                .clone()
                .map(|handle| (handle, settings));
        }
        let outcome = tokio::select! {
            outcome = async {
                if live.is_none() {
                    let (process, runtime_id) =
                        super::spawn_claude(&launcher.binary, &context, &prompt).await?;
                    let started = live.insert(Live::new(process, key, runtime_id.clone()));
                    started.context_lost = super::super::types::private_context_lost(&context);
                    let expected = started.key.permission_mode.clone();
                    started.mode = super::initialize_session(
                        &mut started.process,
                        &launcher.catalogs,
                        &context,
                        runtime_id,
                        expected.as_deref(),
                        &mut cancel,
                    )
                    .await?;
                }
                if *cancel.borrow() {
                    return Ok(None);
                }
                let started = live.as_mut().expect("started process");
                if std::mem::take(&mut started.context_lost) {
                    super::super::types::journal_context_lost(&sink, context.manifest.provider)
                        .await;
                }
                if let Some(provider_work) = pending_provider_work.take() {
                    started.provider_work = Some(provider_work);
                }
                started.idle_sink = Some((
                    sink.clone().for_runtime(started.runtime_id.clone()),
                    prompt.turn_id.clone(),
                ));
                super::run_claude_turn(
                    &mut started.process,
                    &mut started.usage,
                    &mut started.mode,
                    context,
                    prompt,
                    session_id.clone(),
                    &sink,
                    &launcher.catalogs,
                    &mut cancel,
                    &mut started.work,
                    &mut started.subagents,
                    start,
                )
                .await
                .map(Some)
            } => Some(outcome),
            // The caller gave up on the turn (its task was aborted): nothing
            // will read the result, so stop the process mid-turn.
            _ = reply.closed() => None,
            _ = shutdown.changed() => {
                let _ = reply.send(Reply::Finished(Err(AppError::TurnCancelled)));
                reason = shutdown.borrow().lost_reason();
                break;
            }
        };
        let Some(outcome) = outcome else {
            reason = "turn_ended";
            break;
        };
        let (outcome, ended) = match outcome {
            // Cancelled before its prompt was sent: the process is idle.
            Ok(None) => (Ok(cancelled_before_start(session_id)), Ended::Reusable),
            Ok(Some(result)) if !result.cancelled => (Ok(result), Ended::Reusable),
            Ok(Some(result)) => (Ok(result), Ended::Retire),
            Err(error) => (Err(error), Ended::Retire),
        };
        let cancelled =
            matches!(ended, Ended::Retire) && outcome.as_ref().is_ok_and(|result| result.cancelled);
        // The caller has its result before any interrupt or teardown.
        let _ = reply.send(Reply::Finished(outcome));
        let ended = match (ended, live.as_mut()) {
            (Ended::Retire, Some(live)) if cancelled => {
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => {
                        reason = shutdown.borrow().lost_reason();
                        break;
                    }
                    ended = interrupt(&mut live.process) => ended,
                }
            }
            (ended, _) => ended,
        };
        if matches!(ended, Ended::Retire) {
            reason = "turn_ended";
            break;
        }
        // Done with the turn, interrupt included: the worker may be idle.
        turn_ended = Instant::now();
        idle_at = turn_ended + idle_timeout;
        if let Some(live) = live.as_mut() {
            idle_at = idle_deadline(turn_ended, idle_timeout, &mut live.work);
            mark_background(&activity, live.work.pending());
        }
        drop(pending);
    }
    turns.close();
    if let Some(live) = live {
        let handed_back = awaiting.map(|(_, _, tasks)| tasks).unwrap_or_default();
        live.terminate(handed_back, reason).await;
    }
    // A turn queued before the close never reached Claude: hand it back,
    // after the process is gone, so the caller can reopen the session
    // unless the stop was requested.
    let retry = shutdown.borrow().retry();
    while let Ok(request) = turns.try_recv() {
        match request {
            Request::Turn(Turn { run, reply, .. }) => {
                let _ = reply.send(Reply::NotStarted {
                    run: Box::new(run),
                    retry,
                });
            }
            // Dropping its reply tells the prewarm the worker stopped.
            Request::Warm(..) => {}
        }
    }
    let _ = stopped.send(true);
}

/// Reads one frame while no turn runs. Background task frames update the
/// session's work; of them only subagent lifecycle reaches the journal.
async fn between_turns(live: &mut Live) -> Ended {
    let process = &mut live.process;
    let message = match process.read().await {
        Ok(Some(message)) => message,
        Ok(None) | Err(_) => return Ended::Retire,
    };
    match message.get("type").and_then(Value::as_str) {
        Some("control_request") => {
            // No turn owns a permission dialog now.
            let Some(request_id) = message.get("request_id").and_then(Value::as_str) else {
                return Ended::Reusable;
            };
            let response = idle_control_response(request_id);
            if process.send(&response).await.is_err() {
                return Ended::Retire;
            }
            Ended::Reusable
        }
        // A background subagent Claude restarted because a command it left
        // behind finished: it works on its own, and the main session goes
        // `running` once it reports.
        Some("assistant" | "stream_event")
            if message
                .get("parent_tool_use_id")
                .is_some_and(|parent| !parent.is_null()) =>
        {
            Ended::Reusable
        }
        Some("assistant" | "stream_event" | "result") => {
            tracing::warn!("Claude Code produced output between turns; restarting its process");
            Ended::Retire
        }
        Some("system")
            if message.get("subtype").and_then(Value::as_str) == Some("session_state_changed")
                && message.get("state").and_then(Value::as_str) == Some("running") =>
        {
            Ended::Unsolicited
        }
        Some("system") => {
            live.work.apply_system(&message);
            let Some((sink, turn_id)) = &live.idle_sink else {
                return Ended::Reusable;
            };
            if let Some((event, payload)) = live.subagents.system_event(&message, turn_id) {
                if let Err(error) = sink.emit(event, payload).await {
                    tracing::warn!(%error, "could not journal a Claude Code subagent between turns");
                }
            }
            Ended::Reusable
        }
        _ => Ended::Reusable,
    }
}

/// The result of a provider-initiated turn whose work was already gone.
pub(super) fn nothing_to_adopt(session_id: String) -> DriverTurnResult {
    DriverTurnResult {
        native_session_id: Some(session_id),
        stop_reason: "no_work".to_owned(),
        cancelled: false,
    }
}

fn idle_control_response(request_id: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id,
            "error": "TodeX session is idle",
        }
    })
}

/// Interrupts a cancelled turn; the process is reusable when Claude settles
/// back to `idle` within [`INTERRUPT_SETTLE_TIMEOUT`].
async fn interrupt(process: &mut JsonLineProcess) -> Ended {
    let request_id = format!("todex-interrupt-{}", uuid::Uuid::new_v4());
    let request = json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "interrupt" },
    });
    if process.send(&request).await.is_err() {
        return Ended::Retire;
    }
    let settle = async {
        loop {
            let message = match process.read().await {
                Ok(Some(message)) => message,
                Ok(None) | Err(_) => return Ended::Retire,
            };
            match message.get("type").and_then(Value::as_str) {
                Some("control_request") => {
                    let Some(id) = message.get("request_id").and_then(Value::as_str) else {
                        continue;
                    };
                    if process.send(&idle_control_response(id)).await.is_err() {
                        return Ended::Retire;
                    }
                }
                Some("system")
                    if message.get("subtype").and_then(Value::as_str)
                        == Some("session_state_changed")
                        && message.get("state").and_then(Value::as_str) == Some("idle") =>
                {
                    return Ended::Reusable;
                }
                _ => {}
            }
        }
    };
    tokio::time::timeout(INTERRUPT_SETTLE_TIMEOUT, settle)
        .await
        .unwrap_or(Ended::Retire)
}

/// A turn on a process of its own, for when every resident slot is busy.
/// The process is stopped after the turn, off the caller's path; `exited`
/// reports when it is gone, so the conversation's next process waits.
async fn run_once(
    launcher: &Launcher,
    run: Run,
    exited: watch::Sender<bool>,
) -> Result<DriverTurnResult, AppError> {
    let Run {
        context,
        prompt,
        sink,
        mut cancel,
    } = run;
    let session_id = super::claude_session_id(&context);
    // Claude's own work lived on a resident process this one is not.
    if context.handles.adopt.is_some() {
        let _ = exited.send(true);
        return Ok(nothing_to_adopt(session_id));
    }
    if *cancel.borrow() {
        return Ok(cancelled_before_start(session_id));
    }
    let expected = super::super::types::resolve_execution_config(
        context.manifest.provider,
        prompt.permission_mode.as_deref(),
        prompt.work_mode.as_deref(),
        prompt.permission_profile.as_deref(),
        prompt.sandbox_mode.as_deref(),
        prompt.approval_policy.as_deref(),
    )?
    .provider_mode;
    let (mut process, runtime_id) =
        super::spawn_claude(&launcher.binary, &context, &prompt).await?;
    let result = async {
        let mut mode = super::initialize_session(
            &mut process,
            &launcher.catalogs,
            &context,
            runtime_id,
            expected.as_deref(),
            &mut cancel,
        )
        .await?;
        if *cancel.borrow() {
            return Ok(cancelled_before_start(session_id.clone()));
        }
        // The process ends with the turn, and its background work with it.
        super::run_claude_turn(
            &mut process,
            &mut super::ClaudeUsageLedger::default(),
            &mut mode,
            context,
            prompt,
            session_id.clone(),
            &sink,
            &launcher.catalogs,
            &mut cancel,
            &mut SessionWork::default(),
            &mut super::ClaudeSubagents::default(),
            TurnStart::Prompt,
        )
        .await
    }
    .await;
    tokio::spawn(async move {
        process.terminate().await;
        let _ = exited.send(true);
    });
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::conversation::{
        ConversationEventHub, ConversationManifest, ConversationStore, ProviderKind, ProviderState,
    };
    use crate::provider::types::{PermissionBroker, TurnHandles};
    use std::path::Path;

    /// A stream-json Claude stand-in. Each prompt's text picks a behavior:
    /// `block` waits for a `release` file, `silent` never answers nor
    /// honors `interrupt`, `interruptible` streams until interrupted
    /// (`slow-interruptible` takes a moment to honor it), `slow-exit` makes
    /// SIGTERM take a moment, `late-permission` / `late-output` follow the
    /// turn with a frame while no turn runs, `background` leaves a background
    /// task that finishes once a `finish-bg` file appears, after which Claude
    /// starts an invocation of its own (as Claude Code 2.1.299 does);
    /// `subagent-resume` leaves a background agent waiting on a command of
    /// its own, which restarts the agent between turns once `finish-bg`
    /// appears (as Claude Code 2.1.310 does). A
    /// `reported-mode` file sets initialize's `current_permission_mode` and
    /// an `init-mode` file each turn's `init` frame mode; with an
    /// `early-request` file a permission request precedes the initialize
    /// response, which waits for its answer. User
    /// lines are replayed, as with `--replay-user-messages`. Every launch's argv and every
    /// stdin line are logged; a launch while another process of the same
    /// session still runs is logged to `overlap.log`.
    const FAKE_CLAUDE: &str = r#"#!/bin/sh
dir=$(dirname "$0")
printf '%s\n' "$*" >> "$dir/argv.log"
session=
prev=
for arg in "$@"; do
  case "$prev" in --session-id|--resume) session=$arg ;; esac
  prev=$arg
done
if [ -f "$dir/live-$session" ] && kill -0 "$(cat "$dir/live-$session")" 2>/dev/null; then
  printf '%s\n' "$session" >> "$dir/overlap.log"
fi
printf '%s\n' "$$" > "$dir/live-$session"
slow_exit=
trap 'if [ -n "$slow_exit" ]; then sleep 0.5; fi; exit 0' TERM
mode=
slow=
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$dir/wire.jsonl"
  case "$line" in
    *'"subtype":"initialize"'*)
      if [ -f "$dir/early-request" ]; then
        printf '{"type":"control_request","request_id":"early-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}\n'
        IFS= read -r reply
        printf '%s\n' "$reply" >> "$dir/wire.jsonl"
      fi
      if [ -f "$dir/reported-mode" ]; then
        printf '{"type":"control_response","response":{"subtype":"success","request_id":"todex-initialize","response":{"commands":[{"name":"review","description":"Review"}],"current_permission_mode":"%s"}}}\n' "$(cat "$dir/reported-mode")"
      else
        printf '{"type":"control_response","response":{"subtype":"success","request_id":"todex-initialize","response":{"commands":[{"name":"review","description":"Review"}]}}}\n'
      fi
      continue ;;
    *'"subtype":"get_settings"'*)
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"todex-get-settings","response":{"sources":[{"source":"localSettings","settings":{"permissions":{"disableAutoMode":"disable"}}}]}}}\n'
      continue ;;
    *'"subtype":"interrupt"'*)
      if [ "$mode" = interruptible ]; then
        if [ -n "$slow" ]; then sleep 0.3; fi
        rid=$(printf '%s' "$line" | /usr/bin/sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
        printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$rid"
        printf '{"type":"assistant","message":{"content":[{"type":"text","text":"partial"}]}}\n'
        printf '{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":1}\n'
        printf '{"type":"system","subtype":"session_state_changed","state":"idle"}\n'
      fi
      continue ;;
    *'"type":"control_response"'*) continue ;;
  esac
  # A prompt Claude never takes, or one whose replay is too long to read.
  [ -f "$dir/ignore-prompts" ] && continue
  if [ -f "$dir/oversize-replay" ]; then
    head -c 4194400 /dev/zero | tr '\0' x
    printf '\n'
    continue
  fi
  printf '{"type":"system","subtype":"session_state_changed","state":"running"}\n'
  printf '%s\n' "$line"
  if [ -f "$dir/init-mode" ]; then
    printf '{"type":"system","subtype":"init","permissionMode":"%s","model":"claude-fake"}\n' "$(cat "$dir/init-mode")"
  fi
  case "$line" in *slow-exit*) slow_exit=1 ;; esac
  case "$line" in *slow-interrupt*) slow=1 ;; esac
  case "$line" in
    *block*) while [ ! -f "$dir/release" ]; do sleep 0.02; done ;;
  esac
  case "$line" in
    *silent*|*interruptible*)
      case "$line" in *silent*) mode=silent ;; *) mode=interruptible ;; esac
      printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"working"}}}\n'
      continue ;;
  esac
  mode=
  slow=
  case "$line" in
    *subagent-resume*)
      printf '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu-a","name":"Agent","input":{"description":"Scan","prompt":"scan","run_in_background":true}}]}}\n'
      printf '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu-a","content":"launched"}]},"tool_use_result":{"status":"async_launched","agentId":"agent-1"}}\n'
      printf '{"type":"system","subtype":"task_started","task_id":"agent-1","tool_use_id":"toolu-a","is_backgrounded":true,"task_type":"local_agent"}\n'
      printf '{"type":"system","subtype":"task_started","task_id":"sh-1","owned_by_subagent":true,"parent_task_id":"agent-1","is_backgrounded":true,"task_type":"local_bash"}\n'
      printf '{"type":"system","subtype":"task_notification","task_id":"agent-1","tool_use_id":"toolu-a","status":"completed","summary":"still waiting"}\n'
      printf '{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"sh-1","parent_task_id":"agent-1"}]}\n'
      parent=$$
      (
        while [ ! -f "$dir/finish-bg" ] && kill -0 "$parent" 2>/dev/null; do sleep 0.02; done
        [ -f "$dir/finish-bg" ] || exit 0
        rm -f "$dir/finish-bg"
        printf '{"type":"system","subtype":"task_notification","task_id":"sh-1","status":"completed"}\n'
        printf '{"type":"system","subtype":"task_started","task_id":"agent-1","tool_use_id":"toolu-a","is_backgrounded":true,"task_type":"local_agent"}\n'
        printf '{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"agent-1"}]}\n'
        printf '{"type":"assistant","parent_tool_use_id":"toolu-a","message":{"content":[{"type":"text","text":"reading the output"}]}}\n'
        printf '{"type":"system","subtype":"task_progress","task_id":"agent-1","usage":{"total_tokens":42}}\n'
        printf '{"type":"system","subtype":"task_notification","task_id":"agent-1","tool_use_id":"toolu-a","status":"completed","summary":"alpha done"}\n'
        printf '{"type":"system","subtype":"background_tasks_changed","tasks":[]}\n'
        sleep 0.1
        printf '{"type":"system","subtype":"session_state_changed","state":"running"}\n'
        printf '{"type":"system","subtype":"init"}\n'
        printf '{"type":"assistant","message":{"content":[{"type":"text","text":"own work done"}]}}\n'
        printf '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"own work done"}\n'
        printf '{"type":"system","subtype":"session_state_changed","state":"idle"}\n'
      ) &
      ;;
    *background*)
      printf '{"type":"system","subtype":"task_started","task_id":"bg-1","is_backgrounded":true,"task_type":"local_bash"}\n'
      printf '{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"bg-1"}]}\n'
      parent=$$
      (
        while [ ! -f "$dir/finish-bg" ] && kill -0 "$parent" 2>/dev/null; do sleep 0.02; done
        [ -f "$dir/finish-bg" ] || exit 0
        rm -f "$dir/finish-bg"
        printf '{"type":"system","subtype":"task_notification","task_id":"bg-1","status":"completed","task_type":"local_bash","summary":"Background command x completed (exit code 0)"}\n'
        printf '{"type":"system","subtype":"background_tasks_changed","tasks":[]}\n'
        # Claude dequeues the notification a moment later.
        sleep 0.1
        printf '{"type":"system","subtype":"session_state_changed","state":"running"}\n'
        printf '{"type":"system","subtype":"init"}\n'
        printf '{"type":"assistant","message":{"content":[{"type":"text","text":"own work done"}]}}\n'
        # Its `result` never reaches TodeX.
        [ -f "$dir/own-work-no-result" ] || printf '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"own work done"}\n'
        printf '{"type":"system","subtype":"session_state_changed","state":"idle"}\n'
      ) &
      ;;
  esac
  printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"ok"}}}\n'
  printf '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"ok"}\n'
  printf '{"type":"system","subtype":"session_state_changed","state":"idle"}\n'
  case "$line" in
    *late-permission*)
      sleep 0.1
      printf '{"type":"control_request","request_id":"late-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}\n' ;;
    *late-output*)
      sleep 0.1
      printf '{"type":"assistant","message":{"content":[{"type":"text","text":"unprompted"}]}}\n' ;;
  esac
done
"#;

    struct Fixture {
        root: PathBuf,
        store: ConversationStore,
        launcher: Launcher,
    }

    impl Fixture {
        async fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let root =
                std::env::temp_dir().join(format!("todex-claude-runtime-{}", uuid::Uuid::new_v4()));
            tokio::fs::create_dir_all(&root).await.unwrap();
            let root = tokio::fs::canonicalize(root).await.unwrap();
            let script = root.join("fake-claude");
            tokio::fs::write(&script, FAKE_CLAUDE).await.unwrap();
            tokio::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
                .await
                .unwrap();
            let store = ConversationStore::new(root.join("data")).await.unwrap();
            Self {
                launcher: Launcher {
                    binary: script.display().to_string(),
                    catalogs: Catalogs::default(),
                },
                root,
                store,
            }
        }

        async fn conversation(&self) -> ConversationManifest {
            self.store
                .create(ConversationManifest::new(
                    ProviderKind::ClaudeCode,
                    self.root.clone(),
                    None,
                    None,
                ))
                .await
                .unwrap()
        }

        async fn run(
            &self,
            sessions: &Sessions,
            manifest: &ConversationManifest,
            text: &str,
            model: Option<&str>,
            cancel: watch::Receiver<bool>,
        ) -> Result<DriverTurnResult, AppError> {
            self.run_with(
                sessions,
                manifest,
                text,
                model,
                cancel,
                TurnHandles::default(),
            )
            .await
        }

        async fn run_with(
            &self,
            sessions: &Sessions,
            manifest: &ConversationManifest,
            text: &str,
            model: Option<&str>,
            cancel: watch::Receiver<bool>,
            handles: TurnHandles,
        ) -> Result<DriverTurnResult, AppError> {
            let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
            let sink = DriverEventSink::new(
                self.store.clone(),
                ConversationEventHub::default(),
                PermissionBroker::default(),
                manifest.id.clone(),
            )
            .with_turn_id(turn_id.clone());
            let provider_state = self
                .store
                .provider_state(&manifest.id)
                .await
                .unwrap_or_else(|_| ProviderState::new(ProviderKind::ClaudeCode));
            let run = Run {
                context: DriverContext {
                    manifest: manifest.clone(),
                    provider_state,
                    agent_mcp: None,
                    handles,
                },
                prompt: DriverPrompt {
                    turn_id,
                    text: text.to_owned(),
                    content: vec![],
                    skills: vec![],
                    model: model.map(str::to_owned),
                    reasoning_effort: None,
                    permission_mode: None,
                    work_mode: None,
                    permission_profile: None,
                    sandbox_mode: None,
                    approval_policy: None,
                },
                sink,
                cancel,
            };
            tokio::time::timeout(
                Duration::from_secs(15),
                sessions.run(self.launcher.clone(), run),
            )
            .await
            .expect("turn finishes")
        }

        async fn turn(
            &self,
            sessions: &Sessions,
            manifest: &ConversationManifest,
            text: &str,
        ) -> DriverTurnResult {
            let (_cancel, cancel) = watch::channel(false);
            self.run(sessions, manifest, text, None, cancel)
                .await
                .unwrap()
        }

        fn launches(&self) -> Vec<String> {
            std::fs::read_to_string(self.root.join("argv.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn wire(&self) -> String {
            std::fs::read_to_string(self.root.join("wire.jsonl")).unwrap_or_default()
        }

        async fn wait_for_wire(&self, needle: &str) {
            wait_until(|| self.wire().contains(needle)).await;
        }

        /// Sessions that had two processes at once.
        fn overlaps(&self) -> String {
            std::fs::read_to_string(self.root.join("overlap.log")).unwrap_or_default()
        }
    }

    async fn wait_until(mut done: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("condition reached in time");
    }

    fn sessions(max: usize) -> Sessions {
        Sessions::new(Duration::from_secs(300), max)
    }

    fn cleanup(root: &Path) {
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn consecutive_turns_share_one_process_and_one_initialize() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        for text in ["first", "second"] {
            let result = fixture.turn(&sessions, &manifest, text).await;
            assert!(!result.cancelled);
            assert_eq!(
                result.native_session_id.as_deref(),
                Some(manifest.id.as_str())
            );
        }
        assert_eq!(fixture.launches().len(), 1);
        let wire = fixture.wire();
        assert_eq!(wire.matches(r#""subtype":"initialize""#).count(), 1);
        assert_eq!(wire.matches(r#""type":"user""#).count(), 2);
        // The catalog captured at spawn is still served between turns.
        let catalogs = fixture.launcher.catalogs.lock().await;
        assert_eq!(catalogs[&manifest.id].commands[0].name, "review");
        drop(catalogs);
        // The session id is persisted once, not after every turn.
        let state = fixture.store.provider_state(&manifest.id).await.unwrap();
        assert_eq!(
            state.native_session_id.as_deref(),
            Some(manifest.id.as_str())
        );
        sessions.stop_all().await;
        assert_eq!(sessions.worker_count().await, 0);
        cleanup(&fixture.root);
    }

    impl Fixture {
        async fn prewarm(
            &self,
            sessions: &Sessions,
            manifest: &ConversationManifest,
        ) -> Result<ProviderPrewarm, AppError> {
            let warm = Warm {
                context: DriverContext {
                    manifest: manifest.clone(),
                    provider_state: self.store.provider_state(&manifest.id).await.unwrap(),
                    agent_mcp: None,
                    handles: Default::default(),
                },
                settings: DriverPrompt {
                    turn_id: "prewarm".to_owned(),
                    text: String::new(),
                    content: vec![],
                    skills: vec![],
                    model: None,
                    reasoning_effort: None,
                    permission_mode: None,
                    work_mode: None,
                    permission_profile: None,
                    sandbox_mode: None,
                    approval_policy: None,
                },
            };
            tokio::time::timeout(
                Duration::from_secs(15),
                sessions.prewarm(self.launcher.clone(), warm),
            )
            .await
            .expect("prewarm finishes")
        }
    }

    #[tokio::test]
    async fn a_prewarm_starts_the_process_the_next_turn_uses() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        assert_eq!(
            fixture.prewarm(&sessions, &manifest).await.unwrap(),
            ProviderPrewarm::Warmed
        );
        assert_eq!(fixture.launches().len(), 1);
        let wire = fixture.wire();
        assert_eq!(wire.matches(r#""subtype":"initialize""#).count(), 1);
        assert_eq!(wire.matches(r#""type":"user""#).count(), 0);
        // Nothing was recorded for the conversation.
        let state = fixture.store.provider_state(&manifest.id).await.unwrap();
        assert!(state.native_session_id.is_none());
        assert_eq!(
            fixture.prewarm(&sessions, &manifest).await.unwrap(),
            ProviderPrewarm::AlreadyWarm
        );

        let result = fixture.turn(&sessions, &manifest, "first").await;
        assert!(!result.cancelled);
        assert_eq!(fixture.launches().len(), 1);
        let wire = fixture.wire();
        assert_eq!(wire.matches(r#""subtype":"initialize""#).count(), 1);
        assert_eq!(wire.matches(r#""type":"user""#).count(), 1);
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn a_prewarmed_process_explains_the_mode_claude_refused_to_its_turn() {
        let fixture = Fixture::new().await;
        std::fs::write(fixture.root.join("reported-mode"), "default").unwrap();
        std::fs::write(fixture.root.join("init-mode"), "default").unwrap();
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        assert_eq!(
            fixture.prewarm(&sessions, &manifest).await.unwrap(),
            ProviderPrewarm::Warmed
        );
        // Asked once, at start-up: the turn reuses what the probe found.
        let result = fixture.turn(&sessions, &manifest, "first").await;
        assert!(!result.cancelled);
        assert_eq!(fixture.launches().len(), 1);
        assert_eq!(
            fixture
                .wire()
                .matches(r#""subtype":"get_settings""#)
                .count(),
            1
        );
        let history = fixture.store.complete_history(&manifest.id).await.unwrap();
        let event = history
            .iter()
            .find(|event| event.payload["providerMethod"] == "auto_mode_unavailable")
            .unwrap_or_else(|| panic!("{history:?}"));
        assert_eq!(event.payload["metadata"]["settingsSource"], "localSettings");
        assert_eq!(event.payload["metadata"]["cause"], "settings");
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn a_control_request_during_initialize_is_refused() {
        let fixture = Fixture::new().await;
        std::fs::write(fixture.root.join("early-request"), "").unwrap();
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        let result = fixture.turn(&sessions, &manifest, "first").await;
        assert!(!result.cancelled);
        let wire = fixture.wire();
        let reply = wire
            .lines()
            .find(|line| line.contains("early-1"))
            .unwrap_or_else(|| panic!("{wire}"));
        let reply: Value = serde_json::from_str(reply).unwrap();
        assert_eq!(
            reply,
            json!({ "type": "control_response", "response": {
                "subtype": "error",
                "request_id": "early-1",
                "error": "TodeX session is starting",
            } })
        );
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn a_prewarm_at_the_limit_is_skipped_without_evicting() {
        let fixture = Fixture::new().await;
        let sessions = sessions(1);
        let first = fixture.conversation().await;
        let second = fixture.conversation().await;
        fixture.turn(&sessions, &first, "one").await;
        assert_eq!(
            fixture.prewarm(&sessions, &second).await.unwrap(),
            ProviderPrewarm::Limit
        );
        assert!(sessions.holds(&first.id).await);
        assert_eq!(fixture.launches().len(), 1);
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn a_changed_launch_setting_restarts_the_process() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        fixture.turn(&sessions, &manifest, "first").await;
        let (_cancel, cancel) = watch::channel(false);
        fixture
            .run(&sessions, &manifest, "second", Some("haiku"), cancel)
            .await
            .unwrap();
        let (_cancel, cancel) = watch::channel(false);
        fixture
            .run(&sessions, &manifest, "third", Some("haiku"), cancel)
            .await
            .unwrap();
        let launches = fixture.launches();
        assert_eq!(launches.len(), 2, "{launches:?}");
        assert!(!launches[0].contains("--model"));
        assert!(launches[1].contains("--model haiku"));
        assert!(launches[1].contains(&manifest.id));
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn the_limit_evicts_the_longest_idle_session_and_falls_back_when_all_are_busy() {
        let fixture = Fixture::new().await;
        let sessions = Arc::new(sessions(2));
        let first = fixture.conversation().await;
        let second = fixture.conversation().await;
        let third = fixture.conversation().await;
        fixture.turn(&sessions, &first, "one").await;
        fixture.turn(&sessions, &second, "two").await;
        // Both slots are taken; the third conversation stops the first,
        // which has been idle the longest.
        fixture.turn(&sessions, &third, "three").await;
        assert_eq!(sessions.table.lock().await.slots_taken(), 2);
        assert!(!sessions.holds(&first.id).await);
        assert_eq!(fixture.launches().len(), 3);

        // With both slots running a turn, the first conversation still runs,
        // on a process of its own, instead of failing.
        let block = |manifest: ConversationManifest| {
            let sessions = sessions.clone();
            let fixture = &fixture;
            async move {
                let (_cancel, cancel) = watch::channel(false);
                fixture
                    .run(&sessions, &manifest, "block", None, cancel)
                    .await
            }
        };
        let blocked = futures_util::future::join(block(second.clone()), block(third.clone()));
        let fallback = async {
            fixture.wait_for_wire("block").await;
            wait_until(|| fixture.wire().matches("block").count() == 2).await;
            let result = fixture.turn(&sessions, &first, "fallback").await;
            assert_eq!(sessions.worker_count().await, 2);
            assert!(!sessions.holds(&first.id).await);
            std::fs::write(fixture.root.join("release"), "").unwrap();
            result
        };
        let ((second_result, third_result), fallback) =
            futures_util::future::join(blocked, fallback).await;
        assert!(!fallback.cancelled);
        second_result.unwrap();
        third_result.unwrap();
        assert_eq!(fixture.launches().len(), 4);
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn private_sessions_keep_their_slot_and_never_fall_back() {
        let fixture = Fixture::new().await;
        let sessions = Arc::new(sessions(1));
        let private = |mut manifest: ConversationManifest| {
            manifest.ephemeral = true;
            manifest
        };
        // Private conversations live in memory; the fixture's store needs
        // them registered there too.
        let first = fixture
            .store
            .create(private(ConversationManifest::new(
                ProviderKind::ClaudeCode,
                fixture.root.clone(),
                None,
                None,
            )))
            .await
            .unwrap();
        let second = fixture
            .store
            .create(private(ConversationManifest::new(
                ProviderKind::ClaudeCode,
                fixture.root.clone(),
                None,
                None,
            )))
            .await
            .unwrap();
        let normal = fixture.conversation().await;
        fixture.turn(&sessions, &first, "one").await;
        assert!(sessions.holds(&first.id).await);
        // The only slot is pinned: another private conversation fails
        // instead of starting without context on a one-off process.
        let (_cancel, cancel) = watch::channel(false);
        let error = fixture
            .run(&sessions, &second, "two", None, cancel)
            .await
            .unwrap_err();
        assert_eq!(error.code(), "PROVIDER_UNAVAILABLE", "{error}");
        // A normal conversation runs on a process of its own and the
        // private one keeps its process.
        fixture.turn(&sessions, &normal, "three").await;
        assert!(sessions.holds(&first.id).await);
        assert!(!sessions.holds(&normal.id).await);
        let launches = fixture.launches();
        assert_eq!(launches.len(), 2, "{launches:?}");
        assert!(
            launches[0].contains("--no-session-persistence"),
            "{launches:?}"
        );
        assert!(!launches[0].contains("--resume"), "{launches:?}");
        assert!(
            !launches[1].contains("--no-session-persistence"),
            "{launches:?}"
        );
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    /// What the worker offers the supervisor, recorded instead of opening a
    /// turn.
    fn recorded_offers() -> (ProviderWorkHandle, mpsc::UnboundedReceiver<ProviderWork>) {
        let (offers, received) = mpsc::unbounded_channel();
        let handle = ProviderWorkHandle::new(move |work| {
            let _ = offers.send(work);
            Box::pin(async { Ok(()) })
        });
        (handle, received)
    }

    /// `handle`, recording the background work it is told was lost.
    fn recording_losses(
        handle: ProviderWorkHandle,
    ) -> (ProviderWorkHandle, mpsc::UnboundedReceiver<LostWork>) {
        let (losses, received) = mpsc::unbounded_channel();
        let handle = handle.with_lost(move |lost| {
            let _ = losses.send(lost);
            Box::pin(async {})
        });
        (handle, received)
    }

    fn offering(handle: &ProviderWorkHandle) -> TurnHandles {
        TurnHandles {
            provider_work: Some(handle.clone()),
            ..TurnHandles::default()
        }
    }

    fn adopting(token: &str) -> TurnHandles {
        TurnHandles {
            adopt: Some(token.to_owned()),
            ..TurnHandles::default()
        }
    }

    impl Fixture {
        async fn turn_with(
            &self,
            sessions: &Sessions,
            manifest: &ConversationManifest,
            text: &str,
            handles: TurnHandles,
        ) -> DriverTurnResult {
            let (_cancel, cancel) = watch::channel(false);
            self.run_with(sessions, manifest, text, None, cancel, handles)
                .await
                .unwrap()
        }

        fn finish_background(&self) {
            std::fs::write(self.root.join("finish-bg"), "").unwrap();
        }

        async fn history_text(&self, manifest: &ConversationManifest) -> String {
            self.store
                .complete_history(&manifest.id)
                .await
                .unwrap()
                .iter()
                .map(|event| event.payload.to_string())
                .collect()
        }
    }

    async fn next_offer(offers: &mut mpsc::UnboundedReceiver<ProviderWork>) -> ProviderWork {
        tokio::time::timeout(Duration::from_secs(10), offers.recv())
            .await
            .expect("work offered in time")
            .expect("offer channel open")
    }

    /// The incident behind this: a backgrounded command finished after the
    /// turn ended, Claude started an invocation of its own, and TodeX
    /// stopped the process, losing the work and any scheduled wakeup.
    #[tokio::test]
    async fn work_claude_starts_after_a_turn_is_offered_and_adopted_on_its_process() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        let (handle, mut offers) = recorded_offers();
        fixture
            .turn_with(&sessions, &manifest, "background", offering(&handle))
            .await;
        fixture.finish_background();
        let work = next_offer(&mut offers).await;
        assert_eq!(work.trigger["kind"], "background_task");
        assert_eq!(work.trigger["taskIds"], json!(["bg-1"]));
        assert_eq!(work.trigger["taskType"], "local_bash");
        assert!(work.settings.text.is_empty());

        let adopted = fixture
            .turn_with(&sessions, &manifest, "unused", adopting(&work.adoption))
            .await;
        assert!(!adopted.cancelled);
        assert!(fixture
            .history_text(&manifest)
            .await
            .contains("own work done"));
        // Nothing was sent for the adopted turn.
        assert_eq!(fixture.wire().matches(r#""type":"user""#).count(), 1);

        fixture.turn(&sessions, &manifest, "after").await;
        assert_eq!(fixture.launches().len(), 1);
        assert!(fixture.overlaps().is_empty());
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    /// The incident behind this: a background subagent backgrounded a command
    /// of its own, the turn ended, and when the command finished Claude
    /// restarted the subagent. Its output between turns made TodeX restart
    /// the process, losing the subagent and Claude's follow-up, and its card
    /// stayed as the turn left it.
    #[tokio::test]
    async fn a_subagent_restarted_between_turns_keeps_its_process_and_reports() {
        let fixture = Fixture::new().await;
        let sessions = Sessions::new(Duration::from_millis(200), 16);
        let manifest = fixture.conversation().await;
        let (handle, mut offers) = recorded_offers();
        fixture
            .turn_with(&sessions, &manifest, "subagent-resume", offering(&handle))
            .await;
        // The subagent's command holds the process past its keepalive.
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(sessions.holds(&manifest.id).await);

        fixture.finish_background();
        let work = next_offer(&mut offers).await;
        assert_eq!(work.trigger["kind"], "background_task");
        // The report the first turn already answered is not the trigger.
        assert_eq!(work.trigger["taskIds"], json!(["agent-1"]));
        assert_eq!(work.trigger["summary"], "alpha done");
        fixture
            .turn_with(&sessions, &manifest, "unused", adopting(&work.adoption))
            .await;
        assert_eq!(fixture.launches().len(), 1);
        let history = fixture.store.complete_history(&manifest.id).await.unwrap();
        assert!(history
            .iter()
            .any(|event| event.payload.to_string().contains("own work done")));
        let runs: Vec<_> = history
            .iter()
            .filter(|event| {
                event.event_type.starts_with("subagent.")
                    && event.payload["subagentId"] == "toolu-a"
            })
            .collect();
        let lifecycle: Vec<_> = runs
            .iter()
            .filter(|event| event.event_type != "subagent.updated")
            .map(|event| {
                (
                    event.event_type.as_str(),
                    event.payload["result"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            lifecycle,
            [
                ("subagent.started", ""),
                ("subagent.completed", "still waiting"),
                ("subagent.started", ""),
                ("subagent.completed", "alpha done"),
            ]
        );
        assert!(runs.iter().any(|event| {
            event.event_type == "subagent.updated"
                && event.payload.pointer("/metadata/usage/total_tokens") == Some(&json!(42))
        }));
        // Between turns the run still names the turn that started it.
        let turn = &runs[0].payload["turnId"];
        assert!(runs.iter().all(|event| &event.payload["turnId"] == turn));
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn background_work_keeps_the_process_past_its_keepalive() {
        let fixture = Fixture::new().await;
        let sessions = Sessions::new(Duration::from_millis(200), 16);
        let busy = fixture.conversation().await;
        let quiet = fixture.conversation().await;
        let (handle, mut offers) = recorded_offers();
        fixture
            .turn_with(&sessions, &busy, "background", offering(&handle))
            .await;
        fixture.turn(&sessions, &quiet, "plain").await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(sessions.holds(&busy.id).await);
        assert!(!sessions.holds(&quiet.id).await);

        // Once the work is done the keepalive applies again.
        fixture.finish_background();
        let work = next_offer(&mut offers).await;
        fixture
            .turn_with(&sessions, &busy, "unused", adopting(&work.adoption))
            .await;
        wait_until(|| {
            sessions
                .table
                .try_lock()
                .is_ok_and(|table| table.workers.values().all(Worker::stopping))
        })
        .await;
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn work_a_restart_takes_along_is_reported_lost() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        let (handle, _offers) = recorded_offers();
        let (handle, mut losses) = recording_losses(handle);
        fixture
            .turn_with(&sessions, &manifest, "background", offering(&handle))
            .await;
        // A new model needs a new process, which ends the running task.
        let (_cancel, cancel) = watch::channel(false);
        fixture
            .run_with(
                &sessions,
                &manifest,
                "plain",
                Some("haiku"),
                cancel,
                offering(&handle),
            )
            .await
            .unwrap();
        assert_eq!(
            losses.try_recv().unwrap(),
            LostWork {
                task_ids: vec!["bg-1".to_owned()],
                reason: "settings_changed"
            }
        );

        // The new process holds no work: stopping it loses nothing.
        sessions.stop(&manifest.id).await;
        assert!(losses.try_recv().is_err());

        fixture
            .turn_with(&sessions, &manifest, "background", offering(&handle))
            .await;
        sessions.stop(&manifest.id).await;
        assert_eq!(
            losses.try_recv().unwrap(),
            LostWork {
                task_ids: vec!["bg-1".to_owned()],
                reason: "stopped"
            }
        );
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn work_handed_back_is_not_reported_lost() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        let (handle, mut offers) = recorded_offers();
        let (handle, mut losses) = recording_losses(handle);
        fixture
            .turn_with(&sessions, &manifest, "background", offering(&handle))
            .await;
        fixture.finish_background();
        let work = next_offer(&mut offers).await;
        fixture
            .turn_with(&sessions, &manifest, "unused", adopting(&work.adoption))
            .await;
        sessions.stop(&manifest.id).await;
        assert!(losses.try_recv().is_err());
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn own_work_nobody_can_take_over_restarts_the_process() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        // No handle: the turn came from somewhere that cannot open turns.
        fixture.turn(&sessions, &manifest, "background").await;
        fixture.finish_background();
        wait_until(|| {
            sessions
                .table
                .try_lock()
                .is_ok_and(|table| table.workers.values().all(Worker::stopping))
        })
        .await;
        fixture.turn(&sessions, &manifest, "after").await;
        assert_eq!(fixture.launches().len(), 2);
        assert!(!fixture
            .history_text(&manifest)
            .await
            .contains("own work done"));
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn an_offer_not_taken_in_time_restarts_the_process_and_its_late_turn_finds_nothing() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        let (handle, mut offers) = recorded_offers();
        fixture
            .turn_with(&sessions, &manifest, "background", offering(&handle))
            .await;
        fixture.finish_background();
        let work = next_offer(&mut offers).await;
        wait_until(|| {
            sessions
                .table
                .try_lock()
                .is_ok_and(|table| table.workers.values().all(Worker::stopping))
        })
        .await;
        let late = fixture
            .turn_with(&sessions, &manifest, "unused", adopting(&work.adoption))
            .await;
        assert_eq!(late.stop_reason, "no_work");
        // The late turn started no process.
        assert_eq!(fixture.launches().len(), 1);
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn a_prompt_arriving_before_the_takeover_reads_claudes_work_and_its_answer() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        let (handle, mut offers) = recorded_offers();
        fixture
            .turn_with(&sessions, &manifest, "background", offering(&handle))
            .await;
        fixture.finish_background();
        let work = next_offer(&mut offers).await;
        let result = fixture
            .turn_with(&sessions, &manifest, "user-next", offering(&handle))
            .await;
        assert!(!result.cancelled);
        let history = fixture.history_text(&manifest).await;
        assert!(history.contains("own work done"));
        // Its own answer belongs to this turn, not to another offer.
        let late = fixture
            .turn_with(&sessions, &manifest, "unused", adopting(&work.adoption))
            .await;
        assert_eq!(late.stop_reason, "no_work");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(offers.try_recv().is_err());
        fixture.turn(&sessions, &manifest, "after").await;
        assert_eq!(fixture.launches().len(), 1);
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn a_queued_prompt_claude_never_takes_is_resent_three_times_then_fails() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        let (handle, mut offers) = recorded_offers();
        fixture
            .turn_with(&sessions, &manifest, "background", offering(&handle))
            .await;
        std::fs::write(fixture.root.join("ignore-prompts"), "").unwrap();
        fixture.finish_background();
        let _work = next_offer(&mut offers).await;
        let (_cancel, cancel) = watch::channel(false);
        let error = fixture
            .run_with(
                &sessions,
                &manifest,
                "user-next",
                None,
                cancel,
                offering(&handle),
            )
            .await
            .expect_err("a prompt Claude never takes fails the turn");
        assert!(
            error.to_string().contains("never took the queued prompt"),
            "{error}"
        );
        // The first send and three resends: each resend still counts.
        assert_eq!(fixture.wire().matches("user-next").count(), 4);
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn a_queued_prompt_is_not_resent_when_its_replay_may_have_been_dropped() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        let (handle, mut offers) = recorded_offers();
        fixture
            .turn_with(&sessions, &manifest, "background", offering(&handle))
            .await;
        std::fs::write(fixture.root.join("oversize-replay"), "").unwrap();
        fixture.finish_background();
        let _work = next_offer(&mut offers).await;
        let (_cancel, cancel) = watch::channel(false);
        let error = fixture
            .run_with(
                &sessions,
                &manifest,
                "user-next",
                None,
                cancel,
                offering(&handle),
            )
            .await
            .expect_err("an unconfirmed prompt fails the turn");
        assert!(error.to_string().contains("over 4 MiB"), "{error}");
        assert_eq!(fixture.wire().matches("user-next").count(), 1);
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn an_adopted_invocation_that_goes_idle_without_a_result_ends_with_no_work() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        let (handle, mut offers) = recorded_offers();
        fixture
            .turn_with(&sessions, &manifest, "background", offering(&handle))
            .await;
        std::fs::write(fixture.root.join("own-work-no-result"), "").unwrap();
        fixture.finish_background();
        let work = next_offer(&mut offers).await;
        let adopted = fixture
            .turn_with(&sessions, &manifest, "unused", adopting(&work.adoption))
            .await;
        assert_eq!(adopted.stop_reason, "no_work");
        assert!(fixture
            .history_text(&manifest)
            .await
            .contains("own work done"));
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn the_limit_evicts_sessions_without_background_work_first() {
        let fixture = Fixture::new().await;
        let sessions = sessions(2);
        let busy = fixture.conversation().await;
        let quiet = fixture.conversation().await;
        let third = fixture.conversation().await;
        let (handle, _offers) = recorded_offers();
        // Idle the longest, but holding background work.
        fixture
            .turn_with(&sessions, &busy, "background", offering(&handle))
            .await;
        fixture.turn(&sessions, &quiet, "plain").await;
        fixture.turn(&sessions, &third, "plain").await;
        assert!(sessions.holds(&busy.id).await);
        assert!(!sessions.holds(&quiet.id).await);
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn permission_requests_between_turns_are_refused_and_output_retires_the_process() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        fixture.turn(&sessions, &manifest, "late-permission").await;
        fixture.wait_for_wire("TodeX session is idle").await;
        let refusal = fixture
            .wire()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|frame| frame["response"]["request_id"] == "late-1")
            .unwrap();
        assert_eq!(refusal["response"]["subtype"], "error");
        fixture.turn(&sessions, &manifest, "again").await;
        assert_eq!(fixture.launches().len(), 1);

        // Unprompted model output: the process is retired unseen and the
        // next turn resumes on a new one.
        fixture.turn(&sessions, &manifest, "late-output").await;
        wait_until(|| {
            sessions
                .table
                .try_lock()
                .is_ok_and(|table| table.workers.values().all(Worker::stopping))
        })
        .await;
        fixture.turn(&sessions, &manifest, "after").await;
        assert_eq!(fixture.launches().len(), 2);
        let history = fixture.store.complete_history(&manifest.id).await.unwrap();
        assert!(!history
            .iter()
            .any(|event| event.event_type == "permission.requested"));
        assert!(!history
            .iter()
            .any(|event| event.payload.to_string().contains("unprompted")));
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn a_cancelled_turn_that_settles_keeps_its_process() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        let (cancel_tx, cancel) = watch::channel(false);
        let cancelling = async {
            fixture.wait_for_wire("interruptible").await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_tx.send(true).unwrap();
        };
        let (result, ()) = futures_util::future::join(
            fixture.run(&sessions, &manifest, "interruptible", None, cancel),
            cancelling,
        )
        .await;
        let result = result.unwrap();
        assert!(result.cancelled);
        assert_eq!(result.stop_reason, "cancelled");
        fixture.turn(&sessions, &manifest, "next").await;
        assert_eq!(fixture.launches().len(), 1);
        assert!(fixture.wire().contains(r#""subtype":"interrupt""#));
        // Nothing the interrupted turn printed afterwards was journalled.
        let history = fixture.store.complete_history(&manifest.id).await.unwrap();
        assert!(!history
            .iter()
            .any(|event| event.payload.to_string().contains("partial")));
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn a_cancelled_turn_that_ignores_interrupt_is_stopped() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        let (cancel_tx, cancel) = watch::channel(false);
        let cancelling = async {
            fixture.wait_for_wire("silent").await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_tx.send(true).unwrap();
        };
        let started = Instant::now();
        let (result, ()) = futures_util::future::join(
            fixture.run(&sessions, &manifest, "silent", None, cancel),
            cancelling,
        )
        .await;
        assert!(result.unwrap().cancelled);
        // The caller is not held for the interrupt window.
        assert!(started.elapsed() < INTERRUPT_SETTLE_TIMEOUT);
        // The next turn waits for the stuck process to be stopped, then
        // resumes on a new one.
        fixture.turn(&sessions, &manifest, "next").await;
        assert_eq!(fixture.launches().len(), 2);
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn stopping_a_session_ends_its_process() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        fixture.turn(&sessions, &manifest, "first").await;
        assert_eq!(sessions.worker_count().await, 1);
        sessions.stop(&manifest.id).await;
        assert_eq!(sessions.worker_count().await, 0);
        fixture.turn(&sessions, &manifest, "second").await;
        assert_eq!(fixture.launches().len(), 2);
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn an_evicted_session_resumes_only_after_its_process_exits() {
        let fixture = Fixture::new().await;
        let sessions = sessions(1);
        let first = fixture.conversation().await;
        let second = fixture.conversation().await;
        fixture.turn(&sessions, &first, "slow-exit").await;
        // Evicts the first conversation, whose process takes a while to
        // exit; its next turn must not start a second one meanwhile.
        fixture.turn(&sessions, &second, "two").await;
        fixture.turn(&sessions, &first, "again").await;
        assert_eq!(fixture.launches().len(), 3);
        assert_eq!(fixture.overlaps(), "");
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn a_turn_queued_during_an_interrupt_keeps_its_worker_from_eviction() {
        let fixture = Fixture::new().await;
        let sessions = sessions(1);
        let first = fixture.conversation().await;
        let second = fixture.conversation().await;
        let (cancel_tx, cancel) = watch::channel(false);
        let cancelling = async {
            fixture.wait_for_wire("slow-interruptible").await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_tx.send(true).unwrap();
        };
        let (result, ()) = futures_util::future::join(
            fixture.run(&sessions, &first, "slow-interruptible", None, cancel),
            cancelling,
        )
        .await;
        assert!(result.unwrap().cancelled);
        // The worker is still interrupting: this turn queues behind it, and
        // the worker stays busy through it.
        let (_cancel, cancel) = watch::channel(false);
        let queued = fixture.run(&sessions, &first, "block", None, cancel);
        let other = async {
            fixture.wait_for_wire("block").await;
            // Every slot runs a turn: a one-off process, not an eviction.
            let result = fixture.turn(&sessions, &second, "two").await;
            assert!(sessions.holds(&first.id).await);
            std::fs::write(fixture.root.join("release"), "").unwrap();
            result
        };
        let (queued, other) = futures_util::future::join(queued, other).await;
        assert!(!queued.unwrap().cancelled);
        assert!(!other.cancelled);
        assert_eq!(fixture.launches().len(), 2);
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn a_one_off_process_exits_before_its_session_resumes() {
        let fixture = Fixture::new().await;
        let sessions = sessions(1);
        let first = fixture.conversation().await;
        let second = fixture.conversation().await;
        let (_cancel, cancel) = watch::channel(false);
        let blocked = fixture.run(&sessions, &first, "block", None, cancel);
        let one_off = async {
            fixture.wait_for_wire("block").await;
            // A one-off process, stopped (slowly) after the turn.
            fixture.turn(&sessions, &second, "slow-exit").await;
            std::fs::write(fixture.root.join("release"), "").unwrap();
        };
        let (blocked, ()) = futures_util::future::join(blocked, one_off).await;
        blocked.unwrap();
        // The first conversation is idle now: the second evicts it, and its
        // worker waits for the one-off process to exit.
        fixture.turn(&sessions, &second, "again").await;
        assert_eq!(fixture.launches().len(), 3);
        assert_eq!(fixture.overlaps(), "");
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn a_turn_cancelled_while_queued_never_reaches_claude() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        let (_cancel, cancel) = watch::channel(false);
        let blocked = fixture.run(&sessions, &manifest, "block", None, cancel);
        let (cancel_tx, cancel) = watch::channel(false);
        let queued = async {
            fixture.wait_for_wire("block").await;
            let queued = fixture.run(&sessions, &manifest, "never-sent", None, cancel);
            let cancelling = async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                cancel_tx.send(true).unwrap();
                std::fs::write(fixture.root.join("release"), "").unwrap();
            };
            futures_util::future::join(queued, cancelling).await.0
        };
        let (blocked, queued) = futures_util::future::join(blocked, queued).await;
        blocked.unwrap();
        let queued = queued.unwrap();
        assert!(queued.cancelled);
        assert_eq!(queued.stop_reason, "cancelled");
        assert_eq!(
            queued.native_session_id.as_deref(),
            Some(manifest.id.as_str())
        );
        // The process was left idle and serves the next turn, which also
        // means it has read every line sent before.
        fixture.turn(&sessions, &manifest, "next").await;
        assert_eq!(fixture.launches().len(), 1);
        let wire = fixture.wire();
        assert!(wire.contains("next"));
        assert!(!wire.contains("never-sent"));
        assert!(!wire.contains(r#""subtype":"interrupt""#));
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn nothing_starts_while_sessions_are_shutting_down() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let first = fixture.conversation().await;
        let second = fixture.conversation().await;
        // Its process takes a while to exit, which keeps the shutdown open.
        fixture.turn(&sessions, &first, "slow-exit").await;
        let late = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let (_cancel, cancel) = watch::channel(false);
            let turn = fixture.run(&sessions, &second, "late", None, cancel).await;
            let prewarm = fixture.prewarm(&sessions, &second).await;
            (turn, prewarm)
        };
        let ((), (turn, prewarm)) = futures_util::future::join(sessions.stop_all(), late).await;
        assert!(matches!(turn, Err(AppError::ProviderUnavailable(_))));
        assert!(matches!(prewarm, Err(AppError::ProviderUnavailable(_))));
        assert_eq!(fixture.launches().len(), 1);
        assert_eq!(sessions.worker_count().await, 0);
        cleanup(&fixture.root);
    }
}
