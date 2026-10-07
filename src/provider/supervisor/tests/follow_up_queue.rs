//! Backend follow-up queue: ordering, pausing, idempotency and restart.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex as StdMutex;

use tokio::sync::Semaphore;

use super::super::follow_ups::rate_limit_resume_at;

use super::*;

/// Each turn records its prompt text, then waits for a permit (or its
/// cancellation). `fail_next` makes the next released turn fail;
/// `limit_next` makes it fail on a plan window exhausted until that instant.
struct GatedDriver {
    real: Arc<dyn ProviderDriver>,
    permits: Arc<Semaphore>,
    prompts: Arc<StdMutex<Vec<String>>>,
    fail_next: Arc<AtomicBool>,
    limit_next: Arc<StdMutex<Option<chrono::DateTime<chrono::Utc>>>>,
}

#[async_trait::async_trait]
impl ProviderDriver for GatedDriver {
    fn descriptor(&self) -> ProviderDescriptor {
        self.real.descriptor()
    }

    async fn run_turn(
        &self,
        _context: DriverContext,
        prompt: DriverPrompt,
        sink: DriverEventSink,
        mut cancel: watch::Receiver<bool>,
        _launch_permit: crate::workspace_trust::WorkspaceTrustPermit,
    ) -> Result<DriverTurnResult, AppError> {
        self.prompts.lock().unwrap().push(prompt.text);
        tokio::select! {
            permit = self.permits.acquire() => permit.unwrap().forget(),
            _ = cancel.wait_for(|cancelled| *cancelled) => return Err(AppError::TurnCancelled),
        }
        let limit = self.limit_next.lock().unwrap().take();
        if let Some(reset) = limit {
            sink.emit(
                "quota.updated",
                json!({
                    "provider": "claude-code",
                    "scope": "account",
                    "status": "rejected",
                    "windows": [{ "id": "five_hour", "usedPercent": 101.0, "resetsAt": reset.timestamp() }],
                }),
            )
            .await?;
            return Err(AppError::ProviderUnavailable(
                "You've hit your session limit".to_owned(),
            ));
        }
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(AppError::ProviderUnavailable("fixture failure".to_owned()));
        }
        Ok(DriverTurnResult {
            native_session_id: None,
            stop_reason: "end_turn".to_owned(),
            cancelled: false,
        })
    }
}

struct Gate {
    permits: Arc<Semaphore>,
    prompts: Arc<StdMutex<Vec<String>>>,
    fail_next: Arc<AtomicBool>,
    limit_next: Arc<StdMutex<Option<chrono::DateTime<chrono::Utc>>>>,
}

impl Gate {
    /// The next released turn fails on a plan window resetting `after` from
    /// now (whole seconds, as providers report it).
    fn limit_next(&self, after: chrono::Duration) {
        let reset = chrono::Utc::now() + after;
        *self.limit_next.lock().unwrap() =
            chrono::DateTime::from_timestamp(reset.timestamp() + 1, 0);
    }

    fn release(&self) {
        self.permits.add_permits(1);
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }

    async fn wait_for_prompts(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.prompts().len() < count {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("expected {count} prompts, saw {:?}", self.prompts()));
    }
}

async fn gated_fixture(
    label: &str,
) -> (
    PathBuf,
    ConversationStore,
    ConversationSupervisor,
    PathBuf,
    Gate,
) {
    let (root, store, mut supervisor, workspace) = control_fixture(label).await;
    let gate = Gate {
        permits: Arc::new(Semaphore::new(0)),
        prompts: Arc::default(),
        fail_next: Arc::default(),
        limit_next: Arc::default(),
    };
    let (permits, prompts, fail_next, limit_next) = (
        gate.permits.clone(),
        gate.prompts.clone(),
        gate.fail_next.clone(),
        gate.limit_next.clone(),
    );
    replace_driver(&mut supervisor, ProviderKind::ClaudeCode, move |real| {
        Arc::new(GatedDriver {
            real,
            permits,
            prompts,
            fail_next,
            limit_next,
        })
    });
    // The fixture's resets are a second away; the production minute floor
    // would only slow the tests down.
    supervisor.rate_limit_retry_floor = Duration::from_millis(50);
    (root, store, supervisor, workspace, gate)
}

fn queued_prompt(text: &str) -> ConversationPrompt {
    ConversationPrompt {
        client_request_id: None,
        text: text.to_owned(),
        model: None,
        reasoning_effort: None,
        skills: Vec::new(),
        content: Vec::new(),
        permission_mode: None,
        work_mode: None,
        permission_profile: None,
        sandbox_mode: None,
        approval_policy: None,
    }
}

async fn add(
    supervisor: &ConversationSupervisor,
    id: &str,
    item: &str,
    prompt: ConversationPrompt,
) -> FollowUpAddOutcome {
    supervisor
        .queue_add_owned("local", id, item, prompt, false)
        .await
        .unwrap()
}

async fn queue_ids(supervisor: &ConversationSupervisor, id: &str) -> (Vec<String>, Value) {
    let snapshot = supervisor.queue_list_owned("local", id).await.unwrap();
    let ids = snapshot["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect();
    (ids, snapshot)
}

async fn wait_for_queue(
    supervisor: &ConversationSupervisor,
    id: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = supervisor.queue_list_owned("local", id).await.unwrap();
            if predicate(&snapshot) {
                break snapshot;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("queue should reach the expected state")
}

#[tokio::test]
async fn queued_follow_up_with_content_starts_after_the_turn_completes() {
    let (root, store, supervisor, workspace, gate) = gated_fixture("todex-queue-order").await;
    let manifest = supervisor
        .create(ProviderKind::ClaudeCode, workspace, None, None)
        .await
        .unwrap();
    supervisor
        .prompt(&manifest.id, "first".to_owned(), None)
        .await
        .unwrap();
    gate.wait_for_prompts(1).await;

    let mut second = queued_prompt("second");
    second.content = vec![PromptContentRef::Text {
        text: "attached notes".to_owned(),
    }];
    assert_eq!(
        add(&supervisor, &manifest.id, "item-2", second).await,
        FollowUpAddOutcome::Queued
    );
    assert_eq!(
        add(&supervisor, &manifest.id, "item-3", queued_prompt("third")).await,
        FollowUpAddOutcome::Queued
    );
    let (ids, snapshot) = queue_ids(&supervisor, &manifest.id).await;
    assert_eq!(ids, ["item-2", "item-3"]);
    assert_eq!(snapshot["items"][0]["contentCount"], 1);
    assert_eq!(snapshot["paused"], false);

    gate.release();
    gate.wait_for_prompts(2).await;
    assert!(
        gate.prompts()[1].starts_with("second"),
        "{:?}",
        gate.prompts()
    );
    assert!(gate.prompts()[1].contains("attached notes"));
    let (ids, _) = queue_ids(&supervisor, &manifest.id).await;
    assert_eq!(ids, ["item-3"]);

    gate.release();
    gate.wait_for_prompts(3).await;
    assert_eq!(gate.prompts()[2], "third");
    gate.release();
    wait_for_queue(&supervisor, &manifest.id, |q| {
        q["items"].as_array().unwrap().is_empty()
    })
    .await;
    wait_until_idle(&supervisor).await;

    let history = store.complete_history(&manifest.id).await.unwrap();
    let delivered = history
        .iter()
        .filter(|event| event.event_type == "message.created")
        .filter_map(|event| event.payload["clientRequestId"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(delivered, ["item-2", "item-3"]);
    assert!(history
        .iter()
        .any(|event| event.event_type == "followups.updated"));
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn adding_to_an_idle_conversation_starts_immediately() {
    let (root, store, supervisor, workspace, gate) = gated_fixture("todex-queue-idle").await;
    let manifest = supervisor
        .create(ProviderKind::ClaudeCode, workspace, None, None)
        .await
        .unwrap();
    let FollowUpAddOutcome::Started(turn_id) =
        add(&supervisor, &manifest.id, "item-1", queued_prompt("now")).await
    else {
        panic!("an idle conversation should start the prompt");
    };
    gate.wait_for_prompts(1).await;
    assert!(queue_ids(&supervisor, &manifest.id).await.0.is_empty());
    // Re-adding a delivered item reports its turn instead of running again.
    assert_eq!(
        add(&supervisor, &manifest.id, "item-1", queued_prompt("now")).await,
        FollowUpAddOutcome::Started(turn_id)
    );
    gate.release();
    wait_until_idle(&supervisor).await;
    assert_eq!(gate.prompts().len(), 1);
    assert!(store.follow_up_queue(&manifest.id).await.unwrap().is_none());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn re_adding_a_queued_item_keeps_one_copy() {
    let (root, _store, supervisor, workspace, gate) = gated_fixture("todex-queue-dedupe").await;
    let manifest = supervisor
        .create(ProviderKind::ClaudeCode, workspace, None, None)
        .await
        .unwrap();
    supervisor
        .prompt(&manifest.id, "first".to_owned(), None)
        .await
        .unwrap();
    gate.wait_for_prompts(1).await;
    for _ in 0..2 {
        assert_eq!(
            add(&supervisor, &manifest.id, "item-2", queued_prompt("second")).await,
            FollowUpAddOutcome::Queued
        );
    }
    assert_eq!(queue_ids(&supervisor, &manifest.id).await.0, ["item-2"]);
    gate.release();
    gate.wait_for_prompts(2).await;
    gate.release();
    wait_for_queue(&supervisor, &manifest.id, |q| {
        q["items"].as_array().unwrap().is_empty()
    })
    .await;
    wait_until_idle(&supervisor).await;
    assert_eq!(gate.prompts(), ["first", "second"]);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn failed_turn_pauses_the_queue_until_resumed() {
    let (root, _store, supervisor, workspace, gate) = gated_fixture("todex-queue-pause").await;
    let manifest = supervisor
        .create(ProviderKind::ClaudeCode, workspace, None, None)
        .await
        .unwrap();
    supervisor
        .prompt(&manifest.id, "first".to_owned(), None)
        .await
        .unwrap();
    gate.wait_for_prompts(1).await;
    add(&supervisor, &manifest.id, "item-2", queued_prompt("second")).await;
    gate.fail_next.store(true, Ordering::SeqCst);
    gate.release();
    let snapshot = wait_for_queue(&supervisor, &manifest.id, |q| q["paused"] == true).await;
    assert_eq!(snapshot["pauseReason"], "turn_failed");
    wait_until_idle(&supervisor).await;
    sleep(Duration::from_millis(50)).await;
    assert_eq!(
        gate.prompts().len(),
        1,
        "a failed turn must not advance the queue"
    );

    // While paused, an idle add waits behind the paused head.
    assert_eq!(
        add(&supervisor, &manifest.id, "item-3", queued_prompt("third")).await,
        FollowUpAddOutcome::Queued
    );
    supervisor
        .queue_resume_owned("local", &manifest.id)
        .await
        .unwrap();
    gate.wait_for_prompts(2).await;
    assert_eq!(gate.prompts()[1], "second");
    gate.release();
    gate.wait_for_prompts(3).await;
    gate.release();
    wait_until_idle(&supervisor).await;
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn cancelled_turn_pauses_and_remove_and_clear_edit_the_queue() {
    let (root, _store, supervisor, workspace, gate) = gated_fixture("todex-queue-cancel").await;
    let manifest = supervisor
        .create(ProviderKind::ClaudeCode, workspace, None, None)
        .await
        .unwrap();
    supervisor
        .prompt(&manifest.id, "first".to_owned(), None)
        .await
        .unwrap();
    gate.wait_for_prompts(1).await;
    for item in ["a", "b", "c"] {
        add(&supervisor, &manifest.id, item, queued_prompt(item)).await;
    }
    supervisor
        .queue_add_owned("local", &manifest.id, "front", queued_prompt("front"), true)
        .await
        .unwrap();
    assert_eq!(
        queue_ids(&supervisor, &manifest.id).await.0,
        ["front", "a", "b", "c"]
    );
    supervisor
        .queue_remove_owned("local", &manifest.id, "b")
        .await
        .unwrap();
    assert!(matches!(
        supervisor
            .queue_remove_owned("local", &manifest.id, "missing")
            .await,
        Err(AppError::NotFound(_))
    ));
    supervisor.cancel(&manifest.id).await.unwrap();
    let snapshot = wait_for_queue(&supervisor, &manifest.id, |q| q["paused"] == true).await;
    assert_eq!(snapshot["pauseReason"], "turn_cancelled");
    let cleared = supervisor
        .queue_clear_owned("local", &manifest.id)
        .await
        .unwrap();
    assert_eq!(cleared["items"], json!([]));
    assert_eq!(cleared["paused"], false);
    wait_until_idle(&supervisor).await;
    assert_eq!(gate.prompts().len(), 1);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn a_queued_item_that_cannot_start_stays_at_the_head_and_pauses() {
    let (root, _store, supervisor, workspace, gate) =
        gated_fixture("todex-queue-start-failed").await;
    let manifest = supervisor
        .create(ProviderKind::ClaudeCode, workspace.clone(), None, None)
        .await
        .unwrap();
    let attachment = workspace.join("notes.txt");
    fs::write(&attachment, "notes").unwrap();
    supervisor
        .prompt(&manifest.id, "first".to_owned(), None)
        .await
        .unwrap();
    gate.wait_for_prompts(1).await;
    let mut prompt = queued_prompt("with file");
    prompt.content = vec![PromptContentRef::File {
        path: attachment.clone(),
        name: None,
    }];
    add(&supervisor, &manifest.id, "item-2", prompt).await;
    fs::remove_file(&attachment).unwrap();
    gate.release();
    let snapshot = wait_for_queue(&supervisor, &manifest.id, |q| q["paused"] == true).await;
    assert_eq!(snapshot["pauseReason"], "start_failed");
    assert!(snapshot["pauseMessage"]
        .as_str()
        .is_some_and(|message| !message.is_empty()));
    assert_eq!(queue_ids(&supervisor, &manifest.id).await.0, ["item-2"]);
    wait_until_idle(&supervisor).await;
    assert_eq!(gate.prompts().len(), 1);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn enqueue_rejects_invalid_requests_and_a_full_queue() {
    let (root, _store, supervisor, workspace, gate) = gated_fixture("todex-queue-limits").await;
    let manifest = supervisor
        .create(ProviderKind::ClaudeCode, workspace.clone(), None, None)
        .await
        .unwrap();
    supervisor
        .prompt(&manifest.id, "first".to_owned(), None)
        .await
        .unwrap();
    gate.wait_for_prompts(1).await;
    let mut outside = queued_prompt("outside");
    outside.content = vec![PromptContentRef::File {
        path: root.join("outside.txt"),
        name: None,
    }];
    fs::write(root.join("outside.txt"), "x").unwrap();
    assert!(supervisor
        .queue_add_owned("local", &manifest.id, "bad", outside, false)
        .await
        .is_err());
    assert!(matches!(
        supervisor
            .queue_add_owned("local", &manifest.id, "", queued_prompt("x"), false)
            .await,
        Err(AppError::InvalidRequest(_))
    ));
    for index in 0..crate::provider::supervisor::follow_ups::MAX_FOLLOW_UP_ITEMS {
        add(
            &supervisor,
            &manifest.id,
            &format!("item-{index}"),
            queued_prompt("x"),
        )
        .await;
    }
    assert!(matches!(
        supervisor
            .queue_add_owned("local", &manifest.id, "overflow", queued_prompt("x"), false)
            .await,
        Err(AppError::ResourceExhausted(_))
    ));
    supervisor
        .queue_clear_owned("local", &manifest.id)
        .await
        .unwrap();
    gate.release();
    wait_until_idle(&supervisor).await;
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn daemon_restart_pauses_a_waiting_queue() {
    let (root, store, supervisor, workspace, gate) = gated_fixture("todex-queue-restart").await;
    let manifest = supervisor
        .create(ProviderKind::ClaudeCode, workspace, None, None)
        .await
        .unwrap();
    supervisor
        .prompt(&manifest.id, "first".to_owned(), None)
        .await
        .unwrap();
    gate.wait_for_prompts(1).await;
    add(&supervisor, &manifest.id, "item-2", queued_prompt("second")).await;

    // A fresh daemon over the same data directory.
    let restarted = ConversationSupervisor::new(
        supervisor.config.clone(),
        store.clone(),
        ConversationEventHub::default(),
        supervisor.workspace_trust.clone(),
    );
    restarted.recover_all().await.unwrap();
    let snapshot = restarted
        .queue_list_owned("local", &manifest.id)
        .await
        .unwrap();
    assert_eq!(snapshot["paused"], true);
    assert_eq!(snapshot["pauseReason"], "daemon_restarted");
    assert_eq!(snapshot["items"][0]["id"], "item-2");
    supervisor.cancel(&manifest.id).await.unwrap();
    wait_until_idle(&supervisor).await;
    assert_eq!(gate.prompts().len(), 1);
    fs::remove_dir_all(root).unwrap();
}

const CONTINUE_TEXT: &str = "The previous request was interrupted by a provider usage limit before it could finish. Continue where it left off and complete the task.";

#[tokio::test]
async fn rate_limited_turn_continues_by_itself_after_the_reset() {
    let (root, store, supervisor, workspace, gate) =
        gated_fixture("todex-queue-rate-limit-empty").await;
    let manifest = supervisor
        .create(ProviderKind::ClaudeCode, workspace, None, None)
        .await
        .unwrap();
    let mut first = queued_prompt("long task");
    first.client_request_id = Some("first".to_owned());
    first.model = Some("claude-opus-5-5".to_owned());
    let failed_turn = supervisor
        .prompt_owned("local", &manifest.id, first)
        .await
        .unwrap();
    gate.wait_for_prompts(1).await;
    gate.limit_next(chrono::Duration::zero());
    gate.release();

    // An empty queue still gets the continuation, waiting for the reset.
    let snapshot = wait_for_queue(&supervisor, &manifest.id, |q| q["paused"] == true).await;
    assert_eq!(snapshot["pauseReason"], "rate_limited");
    assert!(snapshot["resumeAt"].is_string(), "{snapshot}");
    let continue_id = format!("rate-limit-continue-{failed_turn}");
    assert_eq!(
        queue_ids(&supervisor, &manifest.id).await.0,
        std::slice::from_ref(&continue_id)
    );
    assert_eq!(gate.prompts().len(), 1, "nothing starts before the reset");

    gate.wait_for_prompts(2).await;
    assert_eq!(gate.prompts()[1], CONTINUE_TEXT);
    gate.release();
    let snapshot = wait_for_queue(&supervisor, &manifest.id, |q| {
        q["items"].as_array().unwrap().is_empty()
    })
    .await;
    assert_eq!(snapshot["paused"], false);
    assert!(snapshot["resumeAt"].is_null());
    wait_until_idle(&supervisor).await;
    // The continuation keeps the failed turn's settings.
    let request = store.last_request(&manifest.id).await.unwrap().unwrap();
    assert_eq!(request["request"]["clientRequestId"], continue_id);
    assert_eq!(request["request"]["model"], "claude-opus-5-5");
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn rate_limit_continuation_goes_ahead_of_queued_items_once() {
    let (root, _store, supervisor, workspace, gate) =
        gated_fixture("todex-queue-rate-limit-front").await;
    let manifest = supervisor
        .create(ProviderKind::ClaudeCode, workspace, None, None)
        .await
        .unwrap();
    supervisor
        .prompt(&manifest.id, "first".to_owned(), None)
        .await
        .unwrap();
    gate.wait_for_prompts(1).await;
    add(&supervisor, &manifest.id, "item-2", queued_prompt("second")).await;
    gate.limit_next(chrono::Duration::zero());
    gate.release();

    gate.wait_for_prompts(2).await;
    assert_eq!(gate.prompts()[1], CONTINUE_TEXT);
    assert_eq!(queue_ids(&supervisor, &manifest.id).await.0, ["item-2"]);
    // The continuation hits the limit again: one new continuation, still
    // ahead of the queued item.
    gate.limit_next(chrono::Duration::zero());
    gate.release();
    gate.wait_for_prompts(3).await;
    assert_eq!(gate.prompts()[2], CONTINUE_TEXT);
    gate.release();
    gate.wait_for_prompts(4).await;
    assert_eq!(gate.prompts()[3], "second");
    gate.release();
    wait_until_idle(&supervisor).await;
    assert!(queue_ids(&supervisor, &manifest.id).await.0.is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn a_turn_completed_during_the_wait_drops_the_continuation() {
    let (root, _store, supervisor, workspace, gate) =
        gated_fixture("todex-queue-rate-limit-manual").await;
    let manifest = supervisor
        .create(ProviderKind::ClaudeCode, workspace, None, None)
        .await
        .unwrap();
    supervisor
        .prompt(&manifest.id, "first".to_owned(), None)
        .await
        .unwrap();
    gate.wait_for_prompts(1).await;
    add(&supervisor, &manifest.id, "item-2", queued_prompt("second")).await;
    gate.limit_next(chrono::Duration::hours(1));
    gate.release();
    let snapshot = wait_for_queue(&supervisor, &manifest.id, |q| q["paused"] == true).await;
    assert_eq!(snapshot["pauseReason"], "rate_limited");
    wait_until_idle(&supervisor).await;

    // The user continues by hand once the window reopened.
    supervisor
        .prompt(&manifest.id, "manual".to_owned(), None)
        .await
        .unwrap();
    gate.wait_for_prompts(2).await;
    gate.release();
    gate.wait_for_prompts(3).await;
    assert_eq!(gate.prompts()[1..], ["manual", "second"]);
    gate.release();
    wait_until_idle(&supervisor).await;
    let (ids, snapshot) = queue_ids(&supervisor, &manifest.id).await;
    assert!(ids.is_empty());
    assert_eq!(snapshot["paused"], false);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn daemon_restart_keeps_a_rate_limit_wait() {
    let (root, store, supervisor, workspace, gate) =
        gated_fixture("todex-queue-rate-limit-restart").await;
    let manifest = supervisor
        .create(ProviderKind::ClaudeCode, workspace, None, None)
        .await
        .unwrap();
    supervisor
        .prompt(&manifest.id, "first".to_owned(), None)
        .await
        .unwrap();
    gate.wait_for_prompts(1).await;
    gate.limit_next(chrono::Duration::hours(1));
    gate.release();
    let armed = wait_for_queue(&supervisor, &manifest.id, |q| q["paused"] == true).await;
    wait_until_idle(&supervisor).await;

    let restarted = ConversationSupervisor::new(
        supervisor.config.clone(),
        store.clone(),
        ConversationEventHub::default(),
        supervisor.workspace_trust.clone(),
    );
    restarted.recover_all().await.unwrap();
    let snapshot = restarted
        .queue_list_owned("local", &manifest.id)
        .await
        .unwrap();
    assert_eq!(snapshot["pauseReason"], "rate_limited");
    assert_eq!(snapshot["resumeAt"], armed["resumeAt"]);
    assert_eq!(snapshot["items"], armed["items"]);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn continuations_wait_at_least_the_floor_and_back_off() {
    let now = chrono::Utc::now();
    let floor = Duration::from_secs(60);
    let minutes = |n: i64| chrono::Duration::minutes(n);
    // A reset already in the past still waits a minute.
    assert_eq!(
        rate_limit_resume_at(now - minutes(5), now, 0, floor),
        now + minutes(1)
    );
    // A later reset wins over the floor.
    assert_eq!(
        rate_limit_resume_at(now + minutes(90), now, 2, floor),
        now + minutes(90)
    );
    // The nth consecutive failed continuation waits 60 s × 2^(n−1).
    for (failures, wait) in [(1, 1), (2, 2), (3, 4)] {
        assert_eq!(
            rate_limit_resume_at(now, now, failures, floor),
            now + minutes(wait),
            "{failures}"
        );
    }
}

#[tokio::test]
async fn continuations_stop_after_three_consecutive_limits() {
    let (root, store, supervisor, workspace, gate) =
        gated_fixture("todex-queue-rate-limit-cap").await;
    let manifest = supervisor
        .create(ProviderKind::ClaudeCode, workspace, None, None)
        .await
        .unwrap();
    supervisor
        .prompt(&manifest.id, "first".to_owned(), None)
        .await
        .unwrap();
    gate.wait_for_prompts(1).await;
    // The original turn, then three continuations, all hit the limit.
    for started in 2..=4 {
        gate.limit_next(chrono::Duration::zero());
        gate.release();
        gate.wait_for_prompts(started).await;
        assert_eq!(gate.prompts()[started - 1], CONTINUE_TEXT);
    }
    gate.limit_next(chrono::Duration::zero());
    gate.release();
    let snapshot = wait_for_queue(&supervisor, &manifest.id, |q| {
        q["pauseReason"] == "turn_failed"
    })
    .await;
    assert_eq!(snapshot["paused"], true);
    assert_eq!(snapshot["items"], json!([]), "no fourth continuation");
    assert!(snapshot["resumeAt"].is_null());
    assert!(
        snapshot["pauseMessage"]
            .as_str()
            .unwrap()
            .contains("3 automatic continuations"),
        "{snapshot}"
    );
    wait_until_idle(&supervisor).await;
    // The count survives a restart, since it lives in queue.json.
    let saved = store.follow_up_queue(&manifest.id).await.unwrap().unwrap();
    assert_eq!(saved["rateLimitFailures"], 3);
    sleep(Duration::from_millis(200)).await;
    assert_eq!(gate.prompts().len(), 4);

    // A completed turn starts the count over and lifts the stale pause.
    supervisor
        .prompt(&manifest.id, "manual".to_owned(), None)
        .await
        .unwrap();
    gate.wait_for_prompts(5).await;
    gate.release();
    wait_until_idle(&supervisor).await;
    let snapshot = wait_for_queue(&supervisor, &manifest.id, |q| q["paused"] == false).await;
    assert!(snapshot["pauseReason"].is_null());
    let saved = store.follow_up_queue(&manifest.id).await.unwrap().unwrap();
    assert!(saved.get("rateLimitFailures").is_none(), "{saved}");
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn a_rate_limit_keeps_a_pause_the_user_holds() {
    let (root, _store, supervisor, workspace, gate) =
        gated_fixture("todex-queue-rate-limit-held").await;
    let manifest = supervisor
        .create(ProviderKind::ClaudeCode, workspace, None, None)
        .await
        .unwrap();
    supervisor
        .prompt(&manifest.id, "first".to_owned(), None)
        .await
        .unwrap();
    gate.wait_for_prompts(1).await;
    add(&supervisor, &manifest.id, "item-2", queued_prompt("second")).await;
    supervisor.cancel(&manifest.id).await.unwrap();
    wait_for_queue(&supervisor, &manifest.id, |q| {
        q["pauseReason"] == "turn_cancelled"
    })
    .await;
    wait_until_idle(&supervisor).await;

    // A manual prompt under the pause hits the limit.
    let failed_turn = supervisor
        .prompt(&manifest.id, "manual".to_owned(), None)
        .await
        .unwrap();
    gate.wait_for_prompts(2).await;
    gate.limit_next(chrono::Duration::zero());
    gate.release();
    let continue_id = format!("rate-limit-continue-{failed_turn}");
    let snapshot = wait_for_queue(&supervisor, &manifest.id, |q| {
        q["items"][0]["id"] == continue_id.as_str()
    })
    .await;
    assert_eq!(snapshot["pauseReason"], "turn_cancelled");
    assert!(snapshot["resumeAt"].is_string(), "{snapshot}");
    assert_eq!(snapshot["items"][1]["id"], "item-2");
    wait_until_idle(&supervisor).await;

    // No timer: the reset passes and nothing starts until the user resumes.
    let resume_at =
        chrono::DateTime::parse_from_rfc3339(snapshot["resumeAt"].as_str().unwrap()).unwrap();
    let remaining = resume_at.signed_duration_since(chrono::Utc::now());
    sleep(remaining.to_std().unwrap_or_default() + Duration::from_millis(300)).await;
    assert_eq!(gate.prompts().len(), 2);
    assert_eq!(
        queue_ids(&supervisor, &manifest.id).await.1["pauseReason"],
        "turn_cancelled"
    );

    supervisor
        .queue_resume_owned("local", &manifest.id)
        .await
        .unwrap();
    gate.wait_for_prompts(3).await;
    assert_eq!(gate.prompts()[2], CONTINUE_TEXT);
    gate.release();
    gate.wait_for_prompts(4).await;
    assert_eq!(gate.prompts()[3], "second");
    gate.release();
    wait_until_idle(&supervisor).await;
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn nothing_starts_from_the_queue_once_shutdown_begins() {
    let (root, store, supervisor, workspace, gate) = gated_fixture("todex-queue-shutdown").await;
    let manifest = supervisor
        .create(ProviderKind::ClaudeCode, workspace, None, None)
        .await
        .unwrap();
    supervisor
        .prompt(&manifest.id, "first".to_owned(), None)
        .await
        .unwrap();
    gate.wait_for_prompts(1).await;
    assert!(!supervisor.has_pending_follow_ups());
    add(&supervisor, &manifest.id, "item-2", queued_prompt("second")).await;
    // The updater must not count this conversation as idle.
    assert!(supervisor.has_pending_follow_ups());

    // Shutdown has begun when the running turn completes.
    supervisor
        .shutting_down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    gate.release();
    wait_until_idle(&supervisor).await;
    sleep(Duration::from_millis(200)).await;
    assert_eq!(gate.prompts().len(), 1, "the queued item did not start");
    assert_eq!(queue_ids(&supervisor, &manifest.id).await.0, ["item-2"]);
    supervisor
        .queue_resume_owned("local", &manifest.id)
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;
    assert_eq!(gate.prompts().len(), 1, "resume does not start it either");

    // The next daemon finds it waiting and holds it for the user.
    let restarted = ConversationSupervisor::new(
        supervisor.config.clone(),
        store.clone(),
        ConversationEventHub::default(),
        supervisor.workspace_trust.clone(),
    );
    restarted.recover_all().await.unwrap();
    let snapshot = restarted
        .queue_list_owned("local", &manifest.id)
        .await
        .unwrap();
    assert_eq!(snapshot["pauseReason"], "daemon_restarted");
    assert!(!restarted.has_pending_follow_ups());
    fs::remove_dir_all(root).unwrap();
}

/// Real Claude driver, scripted CLI. The fixture speaks stream-json the way
/// Claude Code 2.1.288 does on a usage limit: an assistant `rate_limit` frame
/// with no `quotaLimits`, then `result.is_error` whose text is
/// "You've hit your session limit · resets <time> (UTC)". No `rate_limit_event`.
/// The reset is the next UTC minute. After that instant the queued continuation
/// must run by itself.
#[tokio::test]
async fn claude_session_limit_retries_after_the_reset() {
    let (root, store, mut supervisor, workspace) = control_fixture("todex-claude-limit-e2e").await;
    supervisor.rate_limit_retry_floor = Duration::from_millis(50);
    fs::write(root.join("provider-fixture.sh"), CLAUDE_LIMIT_FIXTURE).unwrap();
    let manifest = supervisor
        .create(ProviderKind::ClaudeCode, workspace, None, None)
        .await
        .unwrap();
    supervisor
        .prompt(&manifest.id, "finish the migration".to_owned(), None)
        .await
        .unwrap();

    let armed = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let snapshot = supervisor
                .queue_list_owned("local", &manifest.id)
                .await
                .unwrap();
            if snapshot["pauseReason"] == "rate_limited" && snapshot["resumeAt"].is_string() {
                break snapshot;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a session-limit failure should arm the rate-limit wait");
    let resume_at = chrono::DateTime::parse_from_rfc3339(armed["resumeAt"].as_str().unwrap())
        .expect("resumeAt");
    let remaining = resume_at.signed_duration_since(chrono::Utc::now());
    assert!(
        remaining.num_seconds() > 0 && remaining.num_seconds() < 120,
        "reset should be the upcoming minute, got {resume_at}"
    );
    assert_eq!(
        armed["items"][0]["text"], CONTINUE_TEXT,
        "continuation is queued ahead of a manual retry"
    );

    let prompts = tokio::time::timeout(Duration::from_secs(90), async {
        let log_path = root.join("prompts.log");
        loop {
            let log = fs::read_to_string(&log_path).unwrap_or_default();
            let lines = log
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if lines.len() >= 2 {
                break lines;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the continuation should start once the reset time has passed");
    assert_eq!(prompts[0], "finish the migration");
    assert!(
        prompts[1].contains("provider usage limit"),
        "second turn should be the auto continuation, got {:?}",
        prompts
    );

    tokio::time::timeout(Duration::from_secs(10), async {
        while supervisor.has_active_turns() {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("continuation turn should finish");
    let snapshot = wait_for_queue(&supervisor, &manifest.id, |queue| {
        queue["items"].as_array().unwrap().is_empty() && queue["paused"] == false
    })
    .await;
    assert!(snapshot["resumeAt"].is_null());
    let request = store.last_request(&manifest.id).await.unwrap().unwrap();
    assert_eq!(request["request"]["text"], CONTINUE_TEXT);
    fs::remove_dir_all(root).unwrap();
}

const CLAUDE_LIMIT_FIXTURE: &str = r#"#!/usr/bin/env python3
import json, os, sys
from datetime import datetime, timedelta, timezone

log_path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "prompts.log")

def emit(obj):
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def limit_text():
    now = datetime.now(timezone.utc)
    target = (now + timedelta(minutes=1)).replace(second=0, microsecond=0)
    if (target - now).total_seconds() < 5:
        target += timedelta(minutes=1)
    hour = target.hour
    suffix = "am" if hour < 12 else "pm"
    hour12 = hour % 12 or 12
    return "You've hit your session limit · resets %d:%02d%s (UTC)" % (hour12, target.minute, suffix)

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    try:
        msg = json.loads(raw)
    except json.JSONDecodeError:
        continue
    if msg.get("type") == "control_request":
        emit({"type": "control_response", "response": {"subtype": "success", "request_id": msg.get("request_id"), "response": {}}})
        continue
    if msg.get("type") != "user":
        continue
    content = msg.get("message", {}).get("content", "")
    if isinstance(content, list):
        content = "\n".join(part.get("text", "") for part in content if isinstance(part, dict))
    with open(log_path, "a", encoding="utf-8") as log:
        log.write(content.replace("\n", " ") + "\n")
    if "provider usage limit" in content:
        emit({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "continued"}}})
        emit({"type": "result", "subtype": "success", "is_error": False, "session_id": "claude-native", "result": "continued"})
        continue
    notice = limit_text()
    emit({"type": "assistant", "error": "rate_limit", "is_api_error_message": True, "message": {"role": "assistant", "content": [{"type": "text", "text": notice}]}})
    emit({"type": "result", "subtype": "success", "is_error": True, "session_id": "claude-native", "result": notice})
"#;

/// Regression: a 64 KiB read buffer held inline across `.await` in
/// `fingerprint_file` grew `prompt_inner` and every future above it (queue
/// resume/drain, the WebSocket dispatcher) to ~68 KiB. Unoptimized builds keep
/// several stack copies of such futures per poll frame, and
/// `conversation.queue.resume` overflowed the 2 MiB tokio worker stack. The
/// futures are created but never polled; only their sizes are checked.
#[tokio::test]
async fn prompt_path_futures_stay_small() {
    const LIMIT: usize = 16 * 1024;
    let (root, _store, supervisor, workspace) = control_fixture("todex-future-sizes").await;
    let file = workspace.join("fingerprint.txt");
    let fingerprint = fingerprint_file(&file);
    assert!(
        std::mem::size_of_val(&fingerprint) < 1024,
        "fingerprint_file future is {} bytes",
        std::mem::size_of_val(&fingerprint)
    );
    drop(fingerprint);
    let sizes = [
        (
            "prompt_inner",
            std::mem::size_of_val(&supervisor.prompt_inner("local", "c", queued_prompt("x"))),
        ),
        (
            "prompt_owned",
            std::mem::size_of_val(&supervisor.prompt_owned("local", "c", queued_prompt("x"))),
        ),
        (
            "drain_follow_ups",
            std::mem::size_of_val(&supervisor.drain_follow_ups("c")),
        ),
        (
            "queue_resume_owned",
            std::mem::size_of_val(&supervisor.queue_resume_owned("local", "c")),
        ),
    ];
    for (name, size) in sizes {
        assert!(
            size < LIMIT,
            "{name} future is {size} bytes (limit {LIMIT})"
        );
    }
    fs::remove_dir_all(root).unwrap();
}
