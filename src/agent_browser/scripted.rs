//! A scripted Chromium for tests: it answers the few CDP commands tab
//! handling uses, records every command, and can stall a command, delay a
//! launch, send events, or exit.

use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};

use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::{cdp::Cdp, session::Running, AgentBrowser, BrowserError, Shared};

#[derive(Default)]
struct State {
    launches: AtomicUsize,
    active: AtomicUsize,
    peak_active: AtomicUsize,
    targets: AtomicUsize,
    /// Every command sent: (method, params, session).
    log: Mutex<Vec<(String, Value, Option<String>)>>,
    /// Commands that are never answered.
    held: Mutex<HashSet<String>>,
    launch_delay: Mutex<Duration>,
    /// Profile → its connection, in launch order.
    connections: Mutex<Vec<(String, Cdp)>>,
}

#[derive(Clone, Default)]
pub(crate) struct ScriptedChromium {
    state: Arc<State>,
}

/// Counts a launch in progress until dropped (also when cancelled).
struct Active(Arc<State>);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ScriptedChromium {
    pub(crate) fn launches(&self) -> usize {
        self.state.launches.load(Ordering::SeqCst)
    }

    /// The most launches that were in progress at once.
    pub(crate) fn peak_launches(&self) -> usize {
        self.state.peak_active.load(Ordering::SeqCst)
    }

    pub(crate) fn set_launch_delay(&self, delay: Duration) {
        *self
            .state
            .launch_delay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = delay;
    }

    /// `method` is no longer answered.
    pub(crate) fn hold(&self, method: &str) {
        self.state
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(method.to_owned());
    }

    /// `method` is answered again.
    pub(crate) fn release(&self, method: &str) {
        self.state
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(method);
    }

    /// The params of every `method` command sent so far.
    pub(crate) fn calls(&self, method: &str) -> Vec<Value> {
        self.state
            .log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(sent, _, _)| sent == method)
            .map(|(_, params, _)| params.clone())
            .collect()
    }

    fn connection(&self, index: usize) -> Cdp {
        self.state
            .connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())[index]
            .1
            .clone()
    }

    /// The browser sends an event on the session of its `index`th launch.
    pub(crate) fn emit(&self, index: usize, event: &Value) {
        self.connection(index).inject(event);
    }

    /// The `index`th launched Chromium exits.
    pub(crate) fn crash(&self, index: usize) {
        self.connection(index).disconnect();
    }

    async fn launch(
        &self,
        profile: String,
        shared: Weak<Shared>,
    ) -> Result<Arc<Running>, BrowserError> {
        let state = &self.state;
        state.launches.fetch_add(1, Ordering::SeqCst);
        let now = state.active.fetch_add(1, Ordering::SeqCst) + 1;
        state.peak_active.fetch_max(now, Ordering::SeqCst);
        let _active = Active(state.clone());
        let delay = *state
            .launch_delay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tokio::time::sleep(delay).await;
        let (cdp, outgoing) = Cdp::scripted();
        state
            .connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((profile.clone(), cdp.clone()));
        self.respond(cdp.clone(), outgoing);
        Running::start_scripted(cdp, shared, profile).await
    }

    fn respond(&self, cdp: Cdp, mut outgoing: mpsc::UnboundedReceiver<String>) {
        let state = self.state.clone();
        tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                let message: Value = serde_json::from_str(&message).expect("a CDP command");
                let method = message["method"].as_str().unwrap_or_default().to_owned();
                let params = message["params"].clone();
                state
                    .log
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push((
                        method.clone(),
                        params.clone(),
                        message["sessionId"].as_str().map(str::to_owned),
                    ));
                if state
                    .held
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .contains(&method)
                {
                    continue;
                }
                let result = match method.as_str() {
                    "Target.createTarget" => {
                        let n = state.targets.fetch_add(1, Ordering::SeqCst) + 1;
                        json!({ "targetId": format!("T{n}") })
                    }
                    "Target.attachToTarget" => {
                        json!({ "sessionId": format!("S-{}", params["targetId"].as_str().unwrap_or_default()) })
                    }
                    "Page.getFrameTree" => json!({ "frameTree": { "frame": { "id": "F" } } }),
                    "Runtime.evaluate" => json!({ "result": { "value": "complete" } }),
                    "Target.getTargetInfo" => {
                        json!({ "targetInfo": { "url": "http://localhost:5173/", "title": "App" } })
                    }
                    _ => json!({}),
                };
                // Commands sent without waiting have no waiter; the answer
                // is dropped like a real one.
                cdp.inject(&json!({ "id": message["id"], "result": result }));
            }
        });
    }
}

impl AgentBrowser {
    /// Chromium launches are scripted from now on (no executable needed).
    pub(crate) fn script_chromium(&self) -> ScriptedChromium {
        let chromium = ScriptedChromium::default();
        let launcher = chromium.clone();
        *self
            .inner
            .launcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(Box::new(move |profile, shared| {
                let launcher = launcher.clone();
                let profile = profile.to_owned();
                Box::pin(async move { launcher.launch(profile, shared).await })
            }));
        chromium
    }
}
