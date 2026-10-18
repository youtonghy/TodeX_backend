//! Server-Sent Events for the API: a decrypting [`EventPump`] (replay
//! from a cursor, then the live hub) written as an SSE response.

use std::{convert::Infallible, time::Duration};

use axum::response::sse::{Event, KeepAlive, Sse};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::agent_calls::{ends_turn, EventPump};
use crate::conversation::ConversationEvent;
use crate::error::AppError;

const KEEP_ALIVE: Duration = Duration::from_secs(15);
/// How often an open stream rechecks that its key is still active.
const KEY_RECHECK: Duration = Duration::from_secs(5);
const STREAM_BUFFER: usize = 64;

fn sse_event(event: &ConversationEvent) -> Event {
    Event::default()
        .id(event.sequence.to_string())
        .event(event.event_type.clone())
        .json_data(event)
        .unwrap_or_else(|_| Event::default().comment("unserializable event"))
}

fn sse_error(error: &AppError) -> Event {
    Event::default()
        .event("error")
        .json_data(serde_json::json!({ "code": error.code(), "message": error.to_string() }))
        .unwrap_or_else(|_| Event::default().comment("error"))
}

/// Streams `pump` as SSE until the client leaves, the key stops being
/// active, or — with `until_turn` — that turn ends.
pub(super) fn sse(
    mut pump: EventPump,
    until_turn: Option<Option<String>>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let (sender, receiver) = mpsc::channel(STREAM_BUFFER);
    tokio::spawn(async move {
        let mut recheck = tokio::time::interval(KEY_RECHECK);
        recheck.tick().await;
        loop {
            tokio::select! {
                batch = pump.next_batch() => {
                    let events = match batch {
                        Ok(events) => events,
                        Err(error) => {
                            let _ = sender.send(Ok(sse_error(&error))).await;
                            return;
                        }
                    };
                    for event in &events {
                        if sender.send(Ok(sse_event(event))).await.is_err() {
                            return;
                        }
                        if let Some(turn_id) = &until_turn {
                            if ends_turn(event, turn_id.as_deref()) {
                                return;
                            }
                        }
                    }
                }
                _ = recheck.tick() => {
                    if !pump.key_active() {
                        let _ = sender.send(Ok(sse_error(&AppError::Unauthenticated))).await;
                        return;
                    }
                }
                _ = sender.closed() => return,
            }
        }
    });
    Sse::new(ReceiverStream::new(receiver)).keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
}
