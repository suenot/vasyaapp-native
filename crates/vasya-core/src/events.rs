//! Event abstraction decoupling the engine from any particular transport.
//!
//! Design choice: a string-topic + JSON-payload sink (`EventSink`) rather
//! than a typed event enum. The frontend contract is exactly Tauri's
//! `emit(name, payload)` — event names like `telegram:new-message` and
//! camelCase JSON payloads must not change — and several payloads are
//! ad-hoc `serde_json::json!` objects, so a closed enum would force
//! inventing types now and risk payload drift. The typed payload structs
//! that do exist (`NewMessageEvent`, …) stay public in
//! [`crate::telegram::updates`] for consumers that want them.
//!
//! Implementations:
//! * Tauri app: an adapter forwarding to `AppHandle::emit` (lives in the
//!   Tauri crate, not here).
//! * Server: [`BroadcastEventSink`] — a `tokio::sync::broadcast` channel;
//!   each WebSocket/GraphQL subscriber calls [`BroadcastEventSink::subscribe`]
//!   and filters by event name / payload `accountId`.

use serde_json::Value;

/// Sink for engine events. `emit` must be cheap and non-blocking — it is
/// called from the per-account update pump hot path.
pub trait EventSink: Send + Sync + 'static {
    fn emit(&self, event: &str, payload: Value);
}

/// An emitted event: the frontend-visible name (e.g. `telegram:new-message`)
/// and its JSON payload. Account scoping lives inside the payload
/// (`accountId` field), same as the existing frontend contract.
#[derive(Debug, Clone)]
pub struct Event {
    pub name: String,
    pub payload: Value,
}

/// Fan-out sink for servers: every subscriber gets every event.
/// Slow subscribers lag (broadcast semantics) rather than block the pump.
pub struct BroadcastEventSink {
    tx: tokio::sync::broadcast::Sender<Event>,
}

impl BroadcastEventSink {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(capacity);
        Self { tx }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

impl EventSink for BroadcastEventSink {
    fn emit(&self, event: &str, payload: Value) {
        // send only fails when there are no subscribers — fine to drop.
        let _ = self.tx.send(Event {
            name: event.to_string(),
            payload,
        });
    }
}

/// Duplicates every event to several sinks — e.g. the desktop app running
/// an embedded API server emits to both the webview and the broadcast bus.
pub struct MultiEventSink {
    sinks: Vec<std::sync::Arc<dyn EventSink>>,
}

impl MultiEventSink {
    pub fn new(sinks: Vec<std::sync::Arc<dyn EventSink>>) -> Self {
        Self { sinks }
    }
}

impl EventSink for MultiEventSink {
    fn emit(&self, event: &str, payload: Value) {
        for sink in &self.sinks {
            sink.emit(event, payload.clone());
        }
    }
}
