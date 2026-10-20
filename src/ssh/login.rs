//! Logins an agent waits on.
//!
//! An agent's `ssh_exec` runs in BatchMode: it reuses the shared master a
//! terminal, SFTP session or earlier login left behind, or authenticates
//! with keys and ssh-agent. When neither works (typically a password-only
//! host whose master closed after its idle time), the conversation asks for
//! a login instead of failing: a client answers with a password, the daemon
//! opens a new shared master with it ([`SshService::connect_shared`]) and
//! the command runs again on that connection. Passwords only pass through;
//! nothing is stored.
//!
//! One request is open per conversation and host; concurrent commands wait
//! on it together. A declined or unanswered request is not asked again for
//! [`LOGIN_BACKOFF`], so a looping agent cannot flood anyone with dialogs.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use serde::Serialize;
use tokio::sync::watch;
use uuid::Uuid;

use super::{ConnectionTestResult, SshService};
use crate::error::AppError;

/// How long a login request waits for an answer.
pub(crate) const LOGIN_TIMEOUT: Duration = Duration::from_secs(120);
/// After a declined or unanswered request, the same conversation fails fast
/// for that host this long.
pub(crate) const LOGIN_BACKOFF: Duration = Duration::from_secs(30);
/// How often a waiting request checks whether someone logged in elsewhere
/// (for example in the TodeX Terminal), which makes the answer unnecessary.
const MASTER_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How a login request ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum LoginOutcome {
    /// A shared master is up.
    Connected,
    /// Someone dismissed the request.
    Declined,
    /// Nobody answered within [`LOGIN_TIMEOUT`].
    TimedOut,
    /// Every command waiting on it gave up (the tool call was cancelled).
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LoginResolution {
    pub outcome: LoginOutcome,
    /// The answer came from a password the client had saved.
    pub saved_password: bool,
}

impl LoginResolution {
    fn of(outcome: LoginOutcome) -> Self {
        Self {
            outcome,
            saved_password: false,
        }
    }
}

/// The reply to an answer.
#[derive(Debug)]
pub(crate) enum LoginAnswer {
    Connected,
    Declined,
    /// The password did not work; the request stays open for another try.
    Failed(ConnectionTestResult),
}

struct Pending {
    conversation_id: String,
    alias: String,
    resolution: watch::Sender<Option<LoginResolution>>,
}

#[derive(Default)]
struct State {
    /// Request id → open request.
    pending: HashMap<String, Pending>,
    /// (conversation, alias) → when a request was last declined or
    /// unanswered.
    backoff: HashMap<(String, String), Instant>,
}

pub(crate) struct LoginBroker {
    state: Mutex<State>,
    timeout: Duration,
}

impl Default for LoginBroker {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            timeout: LOGIN_TIMEOUT,
        }
    }
}

/// What asking for a login gave.
pub(crate) enum LoginAsk {
    /// Declined or unanswered moments ago; not asked again yet.
    BackedOff,
    /// A new request: the caller announces it and waits with
    /// [`LoginRequest::wait`].
    New(LoginRequest),
    /// The conversation already asks for this host; wait for its answer.
    Joined(LoginWaiter),
}

impl LoginBroker {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn resolve(&self, request_id: &str, resolution: LoginResolution) {
        let mut state = self.state();
        let Some(pending) = state.pending.remove(request_id) else {
            return;
        };
        if matches!(
            resolution.outcome,
            LoginOutcome::Declined | LoginOutcome::TimedOut
        ) {
            state
                .backoff
                .insert((pending.conversation_id, pending.alias), Instant::now());
        }
        let _ = pending.resolution.send(Some(resolution));
    }

    /// Ends every open request for `alias`: a master is up for all of them.
    fn resolve_host(&self, alias: &str, saved_password: bool) {
        let mut state = self.state();
        let ids: Vec<String> = state
            .pending
            .iter()
            .filter(|(_, pending)| pending.alias == alias)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(pending) = state.pending.remove(&id) {
                let _ = pending.resolution.send(Some(LoginResolution {
                    outcome: LoginOutcome::Connected,
                    saved_password,
                }));
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn clear_backoff_for_tests(&self, conversation_id: &str, alias: &str) {
        self.state()
            .backoff
            .remove(&(conversation_id.to_owned(), alias.to_owned()));
    }

    /// The open request `request_id`, if it is for `alias`.
    fn is_pending(&self, alias: &str, request_id: &str) -> bool {
        self.state()
            .pending
            .get(request_id)
            .is_some_and(|pending| pending.alias == alias)
    }
}

impl SshService {
    /// Asks the clients of `conversation_id` for a login to `alias`.
    pub(crate) fn ask_login(&self, conversation_id: &str, alias: &str) -> LoginAsk {
        let broker = self.logins();
        let mut state = broker.state();
        let key = (conversation_id.to_owned(), alias.to_owned());
        if let Some(at) = state.backoff.get(&key) {
            if at.elapsed() < LOGIN_BACKOFF {
                return LoginAsk::BackedOff;
            }
            state.backoff.remove(&key);
        }
        if let Some(pending) = state
            .pending
            .values()
            .find(|pending| pending.conversation_id == conversation_id && pending.alias == alias)
        {
            return LoginAsk::Joined(LoginWaiter(pending.resolution.subscribe()));
        }
        let id = format!("sshl_{}", Uuid::new_v4().simple());
        let (sender, receiver) = watch::channel(None);
        state.pending.insert(
            id.clone(),
            Pending {
                conversation_id: conversation_id.to_owned(),
                alias: alias.to_owned(),
                resolution: sender,
            },
        );
        LoginAsk::New(LoginRequest {
            service: self.clone(),
            id,
            alias: alias.to_owned(),
            waiter: LoginWaiter(receiver),
            finished: false,
        })
    }

    /// A client's answer to request `request_id`: `None` declines it, a
    /// password logs in with it. A password that does not work leaves the
    /// request open, so the user can try again until it times out.
    pub(crate) async fn answer_login(
        &self,
        alias: &str,
        request_id: &str,
        password: Option<&str>,
        saved_password: bool,
    ) -> Result<LoginAnswer, AppError> {
        let broker = self.logins();
        if !broker.is_pending(alias, request_id) {
            return Err(AppError::NotFound(
                "this SSH login request is no longer pending".to_owned(),
            ));
        }
        let Some(password) = password else {
            broker.resolve(request_id, LoginResolution::of(LoginOutcome::Declined));
            return Ok(LoginAnswer::Declined);
        };
        let result = self.connect_shared(alias, password).await?;
        if !result.ok {
            return Ok(LoginAnswer::Failed(result));
        }
        // Whoever else waits for this host can use the new master too.
        broker.resolve_host(alias, saved_password);
        Ok(LoginAnswer::Connected)
    }
}

/// Waits for the answer to a request someone else opened.
pub(crate) struct LoginWaiter(watch::Receiver<Option<LoginResolution>>);

impl LoginWaiter {
    pub(crate) async fn wait(mut self) -> LoginResolution {
        loop {
            if let Some(resolution) = *self.0.borrow_and_update() {
                return resolution;
            }
            if self.0.changed().await.is_err() {
                // Dropped without an answer: its waiter was cancelled.
                return LoginResolution::of(LoginOutcome::Cancelled);
            }
        }
    }
}

/// An open request this caller drives to its end. Dropping it unanswered
/// cancels it for every waiter.
pub(crate) struct LoginRequest {
    service: SshService,
    id: String,
    alias: String,
    waiter: LoginWaiter,
    finished: bool,
}

impl LoginRequest {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// Waits for an answer, a login made elsewhere (a master appears), or
    /// [`LOGIN_TIMEOUT`].
    pub(crate) async fn wait(mut self) -> LoginResolution {
        let deadline = tokio::time::Instant::now() + self.service.logins().timeout;
        let mut poll = tokio::time::interval(MASTER_POLL_INTERVAL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        poll.tick().await;
        let resolution = loop {
            tokio::select! {
                changed = self.waiter.0.changed() => {
                    let resolution = *self.waiter.0.borrow_and_update();
                    match (changed, resolution) {
                        (_, Some(resolution)) => break resolution,
                        (Err(_), None) => break LoginResolution::of(LoginOutcome::Cancelled),
                        (Ok(()), None) => {}
                    }
                }
                _ = poll.tick() => {
                    if self.service.master_alive(&self.alias).await {
                        self.service.logins().resolve_host(&self.alias, false);
                    }
                }
                () = tokio::time::sleep_until(deadline) => {
                    self.service
                        .logins()
                        .resolve(&self.id, LoginResolution::of(LoginOutcome::TimedOut));
                }
            }
        };
        self.finished = true;
        resolution
    }
}

impl Drop for LoginRequest {
    fn drop(&mut self) {
        if !self.finished {
            self.service
                .logins()
                .resolve(&self.id, LoginResolution::of(LoginOutcome::Cancelled));
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ssh::tests::fixture;

    #[tokio::test]
    async fn one_request_per_conversation_and_host() {
        let fixture = fixture("Host web\n").await;
        let service = &fixture.service;
        let LoginAsk::New(first) = service.ask_login("c1", "web") else {
            panic!("expected a new request");
        };
        assert!(matches!(
            service.ask_login("c1", "web"),
            LoginAsk::Joined(_)
        ));
        let LoginAsk::New(other) = service.ask_login("c2", "web") else {
            panic!("another conversation asks on its own");
        };
        drop(other);
        let LoginAsk::Joined(joined) = service.ask_login("c1", "web") else {
            panic!("expected to join");
        };
        let id = first.id().to_owned();
        let waiting = tokio::spawn(first.wait());
        let answer = service
            .answer_login("web", &id, Some("hunter2"), true)
            .await
            .unwrap();
        assert!(matches!(answer, LoginAnswer::Connected));
        let expected = LoginResolution {
            outcome: LoginOutcome::Connected,
            saved_password: true,
        };
        assert_eq!(waiting.await.unwrap(), expected);
        assert_eq!(joined.wait().await, expected);
        // Answered requests are gone.
        assert!(matches!(
            service.answer_login("web", &id, None, false).await,
            Err(AppError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn decline_backs_off_and_drop_cancels() {
        let fixture = fixture("Host web\n").await;
        let service = &fixture.service;
        let LoginAsk::New(request) = service.ask_login("c1", "web") else {
            panic!("expected a new request");
        };
        let id = request.id().to_owned();
        let LoginAsk::Joined(joined) = service.ask_login("c1", "web") else {
            panic!("expected to join");
        };
        // An answer for another host does not touch it.
        assert!(matches!(
            service.answer_login("db", &id, None, false).await,
            Err(AppError::NotFound(_))
        ));
        assert!(matches!(
            service.answer_login("web", &id, None, false).await.unwrap(),
            LoginAnswer::Declined
        ));
        assert_eq!(request.wait().await.outcome, LoginOutcome::Declined);
        assert_eq!(joined.wait().await.outcome, LoginOutcome::Declined);
        assert!(matches!(
            service.ask_login("c1", "web"),
            LoginAsk::BackedOff
        ));

        let LoginAsk::New(request) = service.ask_login("c1", "db") else {
            panic!("expected a new request");
        };
        let LoginAsk::Joined(joined) = service.ask_login("c1", "db") else {
            panic!("expected to join");
        };
        drop(request);
        assert_eq!(joined.wait().await.outcome, LoginOutcome::Cancelled);
        // Cancelling does not back off.
        assert!(matches!(service.ask_login("c1", "db"), LoginAsk::New(_)));
    }

    #[tokio::test]
    async fn a_wrong_password_keeps_the_request_open() {
        let fixture = fixture("Host bad\n").await;
        let service = &fixture.service;
        let LoginAsk::New(request) = service.ask_login("c1", "bad") else {
            panic!("expected a new request");
        };
        let id = request.id().to_owned();
        let LoginAnswer::Failed(result) = service
            .answer_login("bad", &id, Some("wrong"), false)
            .await
            .unwrap()
        else {
            panic!("expected a failed login");
        };
        assert_eq!(
            result.failure,
            Some(super::super::SshFailureKind::AuthenticationFailed)
        );
        assert!(matches!(
            service.answer_login("bad", &id, None, false).await.unwrap(),
            LoginAnswer::Declined
        ));
        assert_eq!(request.wait().await.outcome, LoginOutcome::Declined);
    }

    #[tokio::test]
    async fn unanswered_requests_time_out_unless_a_master_appears() {
        let mut fixture = fixture("Host bad\nHost web\n").await;
        std::sync::Arc::get_mut(&mut fixture.service.inner)
            .unwrap()
            .logins
            .timeout = Duration::from_millis(50);
        let service = &fixture.service;
        let LoginAsk::New(request) = service.ask_login("c1", "bad") else {
            panic!("expected a new request");
        };
        assert_eq!(request.wait().await.outcome, LoginOutcome::TimedOut);
        assert!(matches!(
            service.ask_login("c1", "bad"),
            LoginAsk::BackedOff
        ));

        if service.inner.multiplex {
            std::sync::Arc::get_mut(&mut fixture.service.inner)
                .unwrap()
                .logins
                .timeout = Duration::from_secs(30);
            let service = &fixture.service;
            let LoginAsk::New(request) = service.ask_login("c1", "web") else {
                panic!("expected a new request");
            };
            // Someone logs in from the terminal meanwhile.
            std::fs::write(fixture.root.join("master-web"), "").unwrap();
            let resolution = tokio::time::timeout(Duration::from_secs(10), request.wait())
                .await
                .expect("the new master ends the wait");
            assert_eq!(resolution.outcome, LoginOutcome::Connected);
            assert!(!resolution.saved_password);
        }
    }
}
