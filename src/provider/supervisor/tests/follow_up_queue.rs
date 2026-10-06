//! Backend follow-up queue: ordering, pausing, idempotency and restart.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex as StdMutex;

use tokio::sync::Semaphore;

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
