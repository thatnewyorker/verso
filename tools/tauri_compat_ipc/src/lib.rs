/*!
Optional Tauri-compatible IPC adapter for Verso.

This crate provides a small, dependency-light surface inspired by Tauri's IPC:
- protocol: typed invoke request/response and event message shapes
- error: adapter error types
- command_registry: registration and dispatch of synchronous (and optionally async) handlers
- event_bus: publish/subscribe event hub
- channel: a simple adapter that wires invoke dispatch + event emission together
- js_shim: optional tiny JS helper snippet (string) for webview-side convenience
- compat: placeholder module for mapping to Verso protocol types (kept void for now)

All functionality is opt-in and transport-agnostic. Consumers integrate it with their
own transport (e.g., Verso envelopes).

Feature flags:
- `tracing`: enable tracing instrumentation points (no-ops otherwise)
- `async-handlers`: enable async handler registration and dispatch helpers
*/

#![deny(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(feature = "tracing")]
use tracing as _;

/// Logical protocol elements for invoke/command and event flows.
pub mod protocol {
    use serde::{Deserialize, Serialize};

    /// A uniquely identifying message ID used to correlate requests and responses.
    pub type MessageId = u64;

    /// Typed request representing a host → adapter invoke.
    ///
    /// The payload is serde-serializable to allow flexible command parameters.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct InvokeRequest {
        /// Command name registered in the command registry.
        pub command: String,
        /// Payload encoded as a JSON value. Binary transports may re-encode this.
        pub payload: serde_json::Value,
        /// Correlation identifier chosen by the requester.
        pub id: MessageId,
    }

    /// Typed response representing an adapter → host reply.
    ///
    /// The payload must remain serde-serializable for transport interop.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct InvokeResponse {
        /// ID of the request this response references.
        pub in_reply_to: MessageId,
        /// Successful result payload (if any).
        pub ok: Option<serde_json::Value>,
        /// Error representation (if any).
        pub err: Option<String>,
    }

    /// Asynchronous event that can be published by the host or adapter.
    ///
    /// Events are identified by a string name and carry an optional payload.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct EventMessage {
        /// Event name; consumers subscribe to names of interest.
        pub name: String,
        /// Optional serde-serializable payload.
        pub payload: Option<serde_json::Value>,
        /// Optional correlation or sequencing token if the transport needs it.
        pub tag: Option<String>,
    }
}

/// Error types surfaced by the adapter.
pub mod error {
    use thiserror::Error;

    /// Top-level error type for the adapter.
    #[derive(Debug, Error)]
    pub enum AdapterError {
        /// A command was not found in the registry.
        #[error("unknown command: {0}")]
        UnknownCommand(String),
        /// A handler failed with a message.
        #[error("handler error: {0}")]
        Handler(String),
        /// The message could not be serialized or deserialized.
        #[error("serialization error: {0}")]
        Serialization(String),
        /// A generic error placeholder for future expansion.
        #[error("adapter error: {0}")]
        Other(String),
    }
}

/// Command registry: maps command names to handlers.
pub mod command_registry {
    use super::error::AdapterError;
    use serde_json::Value as JsonValue;
    use std::collections::HashMap;

    /// Opaque type representing a registered command name.
    pub type CommandName = String;

    /// Opaque type representing a handler input payload.
    pub type CommandPayload = JsonValue;

    /// Opaque type representing a handler output payload.
    pub type CommandResult = Result<JsonValue, AdapterError>;

    /// Synchronous handler function signature for registered commands.
    pub type CommandHandler = Box<dyn Fn(CommandPayload) -> CommandResult + Send + Sync + 'static>;

    #[cfg(feature = "async-handlers")]
    mod async_fn {
        use super::{CommandPayload, CommandResult};
        use std::future::Future;
        use std::pin::Pin;

        /// Type-erased async handler function signature.
        pub type AsyncHandler = Box<
            dyn Fn(CommandPayload) -> Pin<Box<dyn Future<Output = CommandResult> + Send>>
                + Send
                + Sync
                + 'static,
        >;

        /// Helper to box an async closure into the `AsyncHandler` type.
        pub fn boxed_async<F, Fut>(f: F) -> AsyncHandler
        where
            F: Fn(CommandPayload) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = CommandResult> + Send + 'static,
        {
            Box::new(move |payload| Box::pin(f(payload)))
        }
    }
    #[cfg(feature = "async-handlers")]
    pub use async_fn::{AsyncHandler, boxed_async as box_async};

    /// Optional metadata describing a command's capabilities/ACL (placeholder).
    #[derive(Debug, Clone, Default)]
    pub struct CommandMeta {
        /// Optional list of named capabilities required to invoke this command.
        pub capabilities: Vec<String>,
    }

    /// Minimal command registry holding handlers identified by name.
    #[derive(Default)]
    pub struct CommandRegistry {
        sync_commands: HashMap<CommandName, (CommandHandler, CommandMeta)>,
        #[cfg(feature = "async-handlers")]
        async_commands: HashMap<CommandName, (AsyncHandler, CommandMeta)>,
    }

    impl CommandRegistry {
        /// Create a new, empty command registry.
        pub fn new() -> Self {
            Self {
                sync_commands: HashMap::new(),
                #[cfg(feature = "async-handlers")]
                async_commands: HashMap::new(),
            }
        }

        /// Register a synchronous handler under the given command name with optional metadata.
        /// Returns the previous handler if one existed.
        pub fn register(
            &mut self,
            name: impl Into<CommandName>,
            handler: CommandHandler,
        ) -> Option<CommandHandler> {
            self.register_with_meta(name, handler, CommandMeta::default())
        }

        /// Register a synchronous handler with metadata.
        pub fn register_with_meta(
            &mut self,
            name: impl Into<CommandName>,
            handler: CommandHandler,
            meta: CommandMeta,
        ) -> Option<CommandHandler> {
            let n = name.into();
            self.sync_commands.insert(n, (handler, meta)).map(|p| p.0)
        }

        /// Unregister a synchronous handler by command name.
        /// Returns true if a handler was present.
        pub fn unregister(&mut self, name: &str) -> bool {
            self.sync_commands.remove(name).is_some()
        }

        /// Invoke a synchronous command with a JSON payload and return the handler result.
        pub fn invoke(&self, name: &str, payload: CommandPayload) -> CommandResult {
            match self.sync_commands.get(name) {
                Some((handler, _meta)) => handler(payload),
                None => Err(AdapterError::UnknownCommand(name.to_string())),
            }
        }

        /// Returns true if any handler is registered for the given name.
        pub fn contains(&self, name: &str) -> bool {
            self.sync_commands.contains_key(name) || {
                #[cfg(feature = "async-handlers")]
                {
                    self.async_commands.contains_key(name)
                }
                #[cfg(not(feature = "async-handlers"))]
                {
                    false
                }
            }
        }

        /// Register an async handler (only available with the `async-handlers` feature).
        #[cfg(feature = "async-handlers")]
        pub fn register_async(
            &mut self,
            name: impl Into<CommandName>,
            handler: AsyncHandler,
        ) -> Option<AsyncHandler> {
            self.register_async_with_meta(name, handler, CommandMeta::default())
        }

        /// Register an async handler with metadata.
        #[cfg(feature = "async-handlers")]
        pub fn register_async_with_meta(
            &mut self,
            name: impl Into<CommandName>,
            handler: AsyncHandler,
            meta: CommandMeta,
        ) -> Option<AsyncHandler> {
            let n = name.into();
            self.async_commands.insert(n, (handler, meta)).map(|p| p.0)
        }

        /// Unregister an async handler by command name (feature-gated).
        #[cfg(feature = "async-handlers")]
        pub fn unregister_async(&mut self, name: &str) -> bool {
            self.async_commands.remove(name).is_some()
        }

        /// Invoke a command, preferring synchronous handlers when both exist.
        /// Async handlers require using `invoke_async` (feature-gated).
        #[cfg(feature = "async-handlers")]
        pub async fn invoke_async(&self, name: &str, payload: CommandPayload) -> CommandResult {
            if let Some((handler, _meta)) = self.sync_commands.get(name) {
                return handler(payload);
            }
            if let Some((ah, _meta)) = self.async_commands.get(name) {
                return (ah)(payload).await;
            }
            Err(AdapterError::UnknownCommand(name.to_string()))
        }
    }
}

/// Publish/subscribe event bus.
///
/// Note: callbacks are executed synchronously on the emitter's thread; they must
/// not block. If your handlers may block, offload within the callback.
pub mod event_bus {
    use serde_json::Value as JsonValue;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Type alias for event names.
    pub type EventName = String;

    /// Type alias for listener identifiers.
    pub type ListenerId = u64;

    /// Listener callback type.
    pub type Listener = Arc<dyn Fn(&JsonValue) + Send + Sync + 'static>;

    #[derive(Default)]
    struct Inner {
        listeners: HashMap<EventName, Vec<(ListenerId, Listener)>>,
        next_id: ListenerId,
    }

    /// Minimal pub/sub event bus (thread-safe).
    #[derive(Default)]
    pub struct EventBus {
        inner: Mutex<Inner>,
    }

    impl EventBus {
        /// Create a new, empty event bus.
        pub fn new() -> Self {
            Self {
                inner: Mutex::new(Inner::default()),
            }
        }

        /// Subscribe a listener to a named event. Returns the listener id.
        pub fn subscribe(&self, name: impl Into<EventName>, listener: Listener) -> ListenerId {
            let mut g = self.inner.lock().expect("event bus poisoned");
            let id = if g.next_id == 0 { 1 } else { g.next_id };
            g.next_id = id.saturating_add(1);
            let entry = g.listeners.entry(name.into()).or_default();
            entry.push((id, listener));
            id
        }

        /// Unsubscribe a listener by id. Returns true if removed.
        pub fn unsubscribe(&self, id: ListenerId) -> bool {
            let mut g = self.inner.lock().expect("event bus poisoned");
            let mut removed = false;
            for (_, vec) in g.listeners.iter_mut() {
                let before = vec.len();
                vec.retain(|(lid, _)| *lid != id);
                if vec.len() != before {
                    removed = true;
                    break;
                }
            }
            removed
        }

        /// Emit an event with a payload to all current subscribers.
        ///
        /// Copies the current listener list to avoid holding the lock during callbacks.
        pub fn emit(&self, name: &str, payload: &JsonValue) {
            let callbacks: Vec<Listener> = {
                let g = self.inner.lock().expect("event bus poisoned");
                g.listeners
                    .get(name)
                    .map(|v| v.iter().map(|(_, cb)| Arc::clone(cb)).collect())
                    .unwrap_or_default()
            };
            for cb in callbacks {
                cb(payload);
            }
        }
    }
}

/// Transport channel adapter: invoke dispatch + embedded event bus.
///
/// The adapter embeds an event bus and provides subscribe/unsubscribe/emit APIs.
/// It also exposes an optional `on_emit` hook so a host can be notified whenever
/// an event is emitted locally (e.g., to forward over a transport).
pub mod channel {
    use super::command_registry::CommandRegistry;
    use super::error::AdapterError;
    use super::event_bus::{EventBus, Listener, ListenerId};
    use super::protocol::{EventMessage, InvokeRequest, InvokeResponse};
    use serde_json::Value as JsonValue;
    use std::sync::Mutex;

    /// Adapter to dispatch invokes and handle pub/sub events.
    #[derive(Default)]
    pub struct ChannelAdapter {
        bus: EventBus,
        on_emit: Mutex<Option<Box<dyn Fn(&EventMessage) + Send + Sync + 'static>>>,
    }

    impl ChannelAdapter {
        /// Create a new channel adapter.
        pub fn new() -> Self {
            Self {
                bus: EventBus::new(),
                on_emit: Mutex::new(None),
            }
        }

        /// Dispatch a typed invoke request to the command registry and produce an invoke response.
        pub fn invoke(&self, registry: &CommandRegistry, req: InvokeRequest) -> InvokeResponse {
            match registry.invoke(&req.command, req.payload) {
                Ok(ok) => InvokeResponse {
                    in_reply_to: req.id,
                    ok: Some(ok),
                    err: None,
                },
                Err(e) => InvokeResponse {
                    in_reply_to: req.id,
                    ok: None,
                    err: Some(self.err_to_string(e)),
                },
            }
        }

        /// Async dispatch (only available with `async-handlers`).
        #[cfg(feature = "async-handlers")]
        pub async fn invoke_async(
            &self,
            registry: &CommandRegistry,
            req: InvokeRequest,
        ) -> InvokeResponse {
            match registry.invoke_async(&req.command, req.payload).await {
                Ok(ok) => InvokeResponse {
                    in_reply_to: req.id,
                    ok: Some(ok),
                    err: None,
                },
                Err(e) => InvokeResponse {
                    in_reply_to: req.id,
                    ok: None,
                    err: Some(self.err_to_string(e)),
                },
            }
        }

        fn err_to_string(&self, e: AdapterError) -> String {
            match e {
                AdapterError::UnknownCommand(s) => format!("unknown command: {s}"),
                AdapterError::Handler(s) => s,
                AdapterError::Serialization(s) => format!("serialization error: {s}"),
                AdapterError::Other(s) => s,
            }
        }

        /// Subscribe to a named event on the embedded event bus.
        pub fn subscribe(&self, name: impl Into<String>, listener: Listener) -> ListenerId {
            self.bus.subscribe(name, listener)
        }

        /// Unsubscribe a listener by id from the embedded event bus.
        pub fn unsubscribe(&self, id: ListenerId) -> bool {
            self.bus.unsubscribe(id)
        }

        /// Emit an event to local subscribers. Also triggers the `on_emit` hook if set.
        pub fn emit_event(&self, name: &str, payload: &JsonValue) {
            // Notify local subscribers.
            self.bus.emit(name, payload);

            // Notify external hook for transport forwarding.
            let hook = self.on_emit.lock().expect("on_emit poisoned");
            if let Some(cb) = &*hook {
                let msg = EventMessage {
                    name: name.to_owned(),
                    payload: Some(payload.clone()),
                    tag: None,
                };
                cb(&msg);
            }
        }

        /// Install or clear the on-emit hook.
        pub fn set_on_emit(&self, f: Option<Box<dyn Fn(&EventMessage) + Send + Sync + 'static>>) {
            let mut hook = self.on_emit.lock().expect("on_emit poisoned");
            *hook = f;
        }
    }
}

/// Optional tiny webview-side JS shim.
/// This does not talk to any specific transport. It exposes a minimal surface
/// that applications can adapt as desired.
pub mod js_shim {
    /// A tiny, self-invoking snippet that ensures `window.__verso.invoke` exists.
    ///
    /// Default behavior: posts a message with a simple envelope; applications can
    /// install a listener to bridge this to their actual transport. Returns a
    /// resolved Promise with `null` to keep the surface Promise-based.
    pub const SHIM_MIN: &str = r#"(() => {
if (typeof window !== 'undefined') {
  window.__verso = window.__verso || {};
  if (!window.__verso.invoke) {
    window.__verso.invoke = async (cmd, payload) => {
      try {
        window.postMessage({ __verso_invoke: { cmd, payload } }, '*');
        return null;
      } catch (e) {
        return Promise.reject(String(e));
      }
    };
  }
}
})();"#;

    /// Return the minified shim contents.
    pub fn script() -> &'static str {
        SHIM_MIN
    }
}

/// Compatibility helpers with Verso protocol types (placeholder).
///
/// Intentionally left empty to avoid coupling this crate to Verso at this time.
/// Downstream crates (e.g., `versoview`) can implement mapping functions using
/// these logical types, or a future feature flag may expose helpers here.
pub mod compat {}

/// Public re-exports (stable import points for downstream crates).
pub use channel::ChannelAdapter;
#[cfg(feature = "async-handlers")]
pub use command_registry::{AsyncHandler, box_async};
pub use command_registry::{
    CommandMeta, CommandName, CommandPayload, CommandRegistry, CommandResult,
};
pub use error::AdapterError;
pub use event_bus::{EventBus, EventName, ListenerId};
pub use protocol::{EventMessage, InvokeRequest, InvokeResponse, MessageId};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_sync_invoke_roundtrip() {
        let mut reg = CommandRegistry::new();
        reg.register(
            "echo",
            Box::new(|payload| Ok(payload.get("msg").cloned().unwrap_or_default())),
        );

        let adapter = ChannelAdapter::new();
        let req = InvokeRequest {
            command: "echo".to_string(),
            payload: serde_json::json!({"msg": "hi"}),
            id: 42,
        };
        let resp = adapter.invoke(&reg, req);
        assert_eq!(resp.in_reply_to, 42);
        assert!(resp.err.is_none());
        assert_eq!(resp.ok, Some(serde_json::json!("hi")));
    }

    #[test]
    fn registry_unknown_command() {
        let reg = CommandRegistry::new();
        let adapter = ChannelAdapter::new();
        let req = InvokeRequest {
            command: "nope".to_string(),
            payload: serde_json::Value::Null,
            id: 1,
        };
        let resp = adapter.invoke(&reg, req);
        assert!(resp.ok.is_none());
        assert!(resp.err.unwrap().contains("unknown command"));
    }

    #[test]
    fn event_bus_subscribe_emit_unsubscribe() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let adapter = ChannelAdapter::new();
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits_cloned = std::sync::Arc::clone(&hits);
        let id = adapter.subscribe(
            "tick",
            std::sync::Arc::new(move |v| {
                if v == &serde_json::json!(1) {
                    // count only payload = 1
                    hits_cloned.fetch_add(1, Ordering::SeqCst);
                }
            }),
        );
        adapter.emit_event("tick", &serde_json::json!(1));
        adapter.emit_event("tick", &serde_json::json!(2));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(adapter.unsubscribe(id));
        adapter.emit_event("tick", &serde_json::json!(1));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
