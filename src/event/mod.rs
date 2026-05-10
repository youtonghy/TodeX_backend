pub mod bus;

pub use bus::EventBus;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventRecord {
    pub time: DateTime<Utc>,
    pub event_id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub workspace_id: Option<String>,
    pub window_id: Option<String>,
    pub pane_id: Option<String>,
    pub payload: Value,
}

impl EventRecord {
    pub fn new(
        event_type: impl Into<String>,
        workspace_id: Option<String>,
        window_id: Option<String>,
        pane_id: Option<String>,
        payload: Value,
    ) -> Self {
        Self {
            time: Utc::now(),
            event_id: format!("evt_{}", Uuid::new_v4().simple()),
            event_type: event_type.into(),
            workspace_id,
            window_id,
            pane_id,
            payload,
        }
    }
}
