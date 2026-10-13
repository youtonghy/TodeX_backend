//! Streaming text fragments are merged before they reach the journal so a
//! reply is stored as a handful of events instead of one event per token.
//!
//! A merged fragment is the newest payload with every text field replaced by
//! the concatenation of the merged fragments' text. Clients append a delta's
//! text to its block, so the merged event renders exactly like the sequence it
//! replaces.

use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;

/// Merged fragments are held back at most this long; it bounds the extra live
/// latency coalescing adds.
pub const DELTA_COALESCE_WINDOW: Duration = Duration::from_millis(100);
/// Bounds the text of one merged event. Compaction replaces old stream
/// fragments with markers wholesale, so its string limit does not apply; a
/// larger window means fewer per-event envelopes in the journal.
pub const DELTA_COALESCE_MAX_BYTES: usize = 16 * 1024;

/// Identity and text location of one streaming fragment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeltaFragment {
    key: String,
    text_pointers: &'static [&'static str],
}

impl DeltaFragment {
    /// Fragments merge only when the event type and `key` match. The key must
    /// identify the message/block (and turn) the text belongs to.
    pub fn keyed(key: impl Into<String>, text_pointers: &'static [&'static str]) -> Self {
        Self {
            key: key.into(),
            text_pointers,
        }
    }

    /// Identity derived from every non-text field of `payload` plus `extra`,
    /// for provider state that is not part of the payload (for example a
    /// native content-block index). Returns `None` when a text field is not a
    /// string, so such payloads are journalled unmerged.
    pub fn from_payload(
        payload: &Value,
        text_pointers: &'static [&'static str],
        extra: Option<&str>,
    ) -> Option<Self> {
        let mut identity = payload.clone();
        for pointer in text_pointers {
            let text = identity.pointer_mut(pointer)?;
            if !text.is_string() {
                return None;
            }
            *text = Value::Null;
        }
        let mut key = serde_json::to_string(&identity).ok()?;
        if let Some(extra) = extra {
            key.push('\0');
            key.push_str(extra);
        }
        Some(Self::keyed(key, text_pointers))
    }
}

/// The open merge window of one stream. `target` is whatever the owner needs
/// to write the merged event later (a sink or a publish hub).
pub struct PendingDelta<T> {
    target: T,
    event_type: String,
    fragment: DeltaFragment,
    payload: Value,
    started: Instant,
}

impl<T> PendingDelta<T> {
    pub fn new(
        target: T,
        event_type: impl Into<String>,
        fragment: DeltaFragment,
        payload: Value,
    ) -> Self {
        Self {
            target,
            event_type: event_type.into(),
            fragment,
            payload,
            started: Instant::now(),
        }
    }

    /// The window closes here; the owner must flush by then.
    pub fn deadline(&self) -> Instant {
        self.started + DELTA_COALESCE_WINDOW
    }

    /// Appends `payload` to the open window. Hands the payload back when it
    /// belongs to another stream, the window expired, a text field is missing
    /// or the merged text would exceed [`DELTA_COALESCE_MAX_BYTES`].
    pub fn try_append(
        &mut self,
        event_type: &str,
        fragment: &DeltaFragment,
        mut payload: Value,
    ) -> Result<(), Value> {
        if self.event_type != event_type
            || self.fragment != *fragment
            || Instant::now() >= self.deadline()
        {
            return Err(payload);
        }
        // Check every field before touching either payload, so a refused
        // fragment leaves the window as it was.
        for pointer in self.fragment.text_pointers {
            let (Some(text), Some(next)) = (
                self.payload.pointer(pointer).and_then(Value::as_str),
                payload.pointer(pointer).and_then(Value::as_str),
            ) else {
                return Err(payload);
            };
            if text.len() + next.len() > DELTA_COALESCE_MAX_BYTES {
                return Err(payload);
            }
        }
        // The merged text grows in place: the window's buffer moves into the
        // newest payload instead of being copied into a new string.
        for pointer in self.fragment.text_pointers {
            if let (Some(Value::String(text)), Some(Value::String(next))) = (
                self.payload.pointer_mut(pointer),
                payload.pointer_mut(pointer),
            ) {
                let mut merged = std::mem::take(text);
                merged.push_str(next);
                *next = merged;
            }
        }
        self.payload = payload;
        Ok(())
    }

    pub fn into_parts(self) -> (T, String, Value) {
        (self.target, self.event_type, self.payload)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const DELTA: &[&str] = &["/delta"];

    fn fragment(payload: &Value) -> DeltaFragment {
        DeltaFragment::from_payload(payload, DELTA, None).unwrap()
    }

    #[tokio::test]
    async fn merges_fragments_of_one_block_into_the_concatenated_text() {
        let first = json!({"delta": "Hel", "block": {"id": "item-1"}, "turnId": "t1"});
        let mut pending = PendingDelta::new((), "message.delta", fragment(&first), first);
        let next = json!({"delta": "lo", "block": {"id": "item-1"}, "turnId": "t1"});
        pending
            .try_append("message.delta", &fragment(&next), next)
            .unwrap();
        let (_, event_type, payload) = pending.into_parts();
        assert_eq!(event_type, "message.delta");
        assert_eq!(
            payload,
            json!({"delta": "Hello", "block": {"id": "item-1"}, "turnId": "t1"})
        );
    }

    #[tokio::test]
    async fn keeps_other_blocks_turns_and_event_types_apart() {
        let first = json!({"delta": "a", "block": {"id": "item-1"}, "turnId": "t1"});
        let mut pending = PendingDelta::new((), "message.delta", fragment(&first), first);
        for (event_type, other) in [
            (
                "message.delta",
                json!({"delta": "b", "block": {"id": "item-2"}, "turnId": "t1"}),
            ),
            (
                "message.delta",
                json!({"delta": "b", "block": {"id": "item-1"}, "turnId": "t2"}),
            ),
            (
                "thought.delta",
                json!({"delta": "b", "block": {"id": "item-1"}, "turnId": "t1"}),
            ),
        ] {
            let key = fragment(&other);
            assert_eq!(
                pending.try_append(event_type, &key, other.clone()),
                Err(other)
            );
        }
        let extra = DeltaFragment::from_payload(&json!({"delta": "b"}), DELTA, Some("1"));
        let plain = DeltaFragment::from_payload(&json!({"delta": "b"}), DELTA, None);
        assert_ne!(extra, plain);
    }

    #[tokio::test]
    async fn non_string_text_is_never_merged() {
        assert!(DeltaFragment::from_payload(&json!({"delta": null}), DELTA, None).is_none());
        assert!(DeltaFragment::from_payload(&json!({"other": "x"}), DELTA, None).is_none());
    }

    #[tokio::test]
    async fn every_text_field_is_concatenated() {
        const BOTH: &[&str] = &["/delta", "/thought"];
        let first = json!({"delta": "a", "thought": "a"});
        let key = DeltaFragment::from_payload(&first, BOTH, None).unwrap();
        let mut pending = PendingDelta::new((), "thought.delta", key.clone(), first);
        pending
            .try_append("thought.delta", &key, json!({"delta": "b", "thought": "b"}))
            .unwrap();
        let (_, _, payload) = pending.into_parts();
        assert_eq!(payload, json!({"delta": "ab", "thought": "ab"}));
    }

    #[tokio::test]
    async fn size_limit_starts_a_new_window() {
        let first = json!({"delta": "x".repeat(DELTA_COALESCE_MAX_BYTES - 1)});
        let key = fragment(&first);
        let mut pending = PendingDelta::new((), "message.delta", key.clone(), first);
        pending
            .try_append("message.delta", &key, json!({"delta": "y"}))
            .unwrap();
        let overflow = json!({"delta": "z"});
        assert_eq!(
            pending.try_append("message.delta", &key, overflow.clone()),
            Err(overflow)
        );
    }

    #[tokio::test]
    async fn one_window_holds_up_to_sixteen_kib_of_text() {
        assert_eq!(DELTA_COALESCE_MAX_BYTES, 16 * 1024);
        let first = json!({"delta": "x".repeat(1024)});
        let key = fragment(&first);
        let mut pending = PendingDelta::new((), "thought.delta", key.clone(), first);
        for _ in 1..16 {
            pending
                .try_append("thought.delta", &key, json!({"delta": "x".repeat(1024)}))
                .unwrap();
        }
        let (_, _, payload) = pending.into_parts();
        assert_eq!(payload["delta"].as_str().unwrap().len(), 16 * 1024);
    }

    /// Feeds `fragments` through merge windows the way the store does and
    /// returns the events it would journal. Windows never expire here, so
    /// the split depends only on keys and sizes, not on how fast this runs.
    fn merged(fragments: &[(DeltaFragment, Value)]) -> Vec<Value> {
        let mut out = Vec::new();
        let mut pending: Option<PendingDelta<()>> = None;
        for (fragment, payload) in fragments {
            let payload = match pending.as_mut() {
                Some(window) => match window.try_append("message.delta", fragment, payload.clone())
                {
                    Ok(()) => continue,
                    Err(payload) => payload,
                },
                None => payload.clone(),
            };
            if let Some(window) = pending.take() {
                out.push(window.into_parts().2);
            }
            let mut window = PendingDelta::new((), "message.delta", fragment.clone(), payload);
            window.started += Duration::from_secs(3600);
            pending = Some(window);
        }
        out.extend(pending.map(|window| window.into_parts().2));
        out
    }

    #[tokio::test]
    async fn precomputed_keys_merge_a_thousand_fragments_like_payload_keys() {
        let fragments: Vec<Value> = (0..1000)
            .map(|index| {
                json!({"role": "assistant", "delta": format!("{index:04}é…{}", "x".repeat(40)),
                       "block": {"id": if index < 600 { "item-1" } else { "item-2" }}, "turnId": "t1"})
            })
            .collect();
        let by_payload: Vec<_> = fragments
            .iter()
            .map(|payload| (fragment(payload), payload.clone()))
            .collect();
        let by_key: Vec<_> = fragments
            .iter()
            .map(|payload| {
                let key = payload["block"]["id"].as_str().unwrap().to_owned();
                (DeltaFragment::keyed(key, DELTA), payload.clone())
            })
            .collect();
        let expected = merged(&by_payload);
        let actual = merged(&by_key);
        assert_eq!(
            serde_json::to_vec(&actual).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        // Windows fill to the size limit and never cross a block.
        let text: String = fragments
            .iter()
            .map(|payload| payload["delta"].as_str().unwrap())
            .collect();
        assert_eq!(
            actual
                .iter()
                .map(|payload| payload["delta"].as_str().unwrap())
                .collect::<String>(),
            text
        );
        assert!(actual.iter().all(|payload| {
            payload["delta"].as_str().unwrap().len() <= DELTA_COALESCE_MAX_BYTES
        }));
        assert_eq!(
            actual
                .iter()
                .filter(|payload| payload["block"]["id"] == "item-1")
                .count(),
            2
        );
        assert_eq!(actual.len(), 4);
    }

    #[tokio::test]
    async fn a_refused_fragment_leaves_the_window_untouched() {
        const BOTH: &[&str] = &["/delta", "/thought"];
        let first = json!({"delta": "a", "thought": "b"});
        let key = DeltaFragment::from_payload(&first, BOTH, None).unwrap();
        let mut pending = PendingDelta::new((), "thought.delta", key.clone(), first.clone());
        // The second field is missing: nothing of the first may move.
        let partial = json!({"delta": "c"});
        assert_eq!(
            pending.try_append("thought.delta", &key, partial.clone()),
            Err(partial)
        );
        assert_eq!(pending.into_parts().2, first);
    }

    #[tokio::test]
    async fn window_expiry_starts_a_new_window() {
        let first = json!({"delta": "a"});
        let key = fragment(&first);
        let mut pending = PendingDelta::new((), "message.delta", key.clone(), first);
        tokio::time::sleep_until(pending.deadline()).await;
        let late = json!({"delta": "b"});
        assert_eq!(
            pending.try_append("message.delta", &key, late.clone()),
            Err(late)
        );
    }
}
