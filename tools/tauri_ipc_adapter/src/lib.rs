/*!
Lightweight Tauri-like IPC and event adapter for Verso/versoview.

Overview
- This crate provides a minimal, dependency-light façade to build a Tauri-inspired
  invoke/command and event model on top of Verso's existing transport and protocol.
- It focuses on:
  - A typed logical protocol (InvokeRequest/InvokeResponse/EventMessage).
  - A command registry (register command names to handlers).
  - An event bus (publish/subscribe).
  - A channel adapter (translate between Verso envelopes and the logical protocol).
  - An optional tiny JS shim for webview-side convenience.

Design notes
- Keep the adapter transport-agnostic and serde-first, while mapping cleanly to Verso's
  bincode-encoded envelopes.
- The command registry and event bus are decoupled from any specific webview backend.
- When adapting concepts inspired by Tauri, preserve attribution and follow licensing
  requirements (MIT/Apache-2.0). This crate is not a drop-in replacement for Tauri.

Feature flags
- `tracing`: enables instrumentation points (no-ops by default in this skeleton).

License
- Dual-licensed under MIT or Apache-2.0, at your option.
*/

#![deny(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(feature = "tracing")]
use tracing as _;

/// Logical protocol elements for invoke/command and event flows.
///
/// This module defines strongly typed messages that represent the "intent" level
/// of the adapter. Concrete transports (e.g., Verso envelopes) should map to/from
/// these types without leaking transport details into command or event runtimes.
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

    /// Handler function signature for registered commands.
    pub type CommandHandler = Box<dyn Fn(CommandPayload) -> CommandResult + Send + Sync + 'static>;

    /// Minimal command registry holding synchronous handlers identified by name.
    #[derive(Default)]
    pub struct CommandRegistry {
        commands: HashMap<CommandName, CommandHandler>,
    }

    impl CommandRegistry {
        /// Create a new, empty command registry.
        pub fn new() -> Self {
            Self {
                commands: HashMap::new(),
            }
        }

        /// Register a handler under the given command name.
        /// Returns the previous handler if one existed.
        pub fn register(
            &mut self,
            name: impl Into<CommandName>,
            handler: CommandHandler,
        ) -> Option<CommandHandler> {
            self.commands.insert(name.into(), handler)
        }

        /// Unregister a handler by command name.
        /// Returns true if a handler was present.
        pub fn unregister(&mut self, name: &str) -> bool {
            self.commands.remove(name).is_some()
        }

        /// Invoke a command with a JSON payload and return the handler result.
        pub fn invoke(&self, name: &str, payload: CommandPayload) -> CommandResult {
            match self.commands.get(name) {
                Some(handler) => handler(payload),
                None => Err(AdapterError::UnknownCommand(name.to_string())),
            }
        }

        /// Returns true if a handler is registered for the given name.
        pub fn contains(&self, name: &str) -> bool {
            self.commands.contains_key(name)
        }
    }
}

/// Publish/subscribe event bus.
pub mod event_bus {
    use serde_json::Value as JsonValue;
    use std::collections::HashMap;

    /// Type alias for event names.
    pub type EventName = String;

    /// Type alias for listener identifiers.
    pub type ListenerId = u64;

    /// Listener callback type.
    pub type Listener = Box<dyn Fn(&JsonValue) + Send + Sync + 'static>;

    /// Minimal pub/sub event bus.
    #[derive(Default)]
    pub struct EventBus {
        listeners: HashMap<EventName, Vec<(ListenerId, Listener)>>,
        next_id: ListenerId,
    }

    impl EventBus {
        /// Create a new, empty event bus.
        pub fn new() -> Self {
            Self {
                listeners: HashMap::new(),
                next_id: 1,
            }
        }

        /// Subscribe a listener to a named event. Returns the listener id.
        pub fn subscribe(&mut self, name: impl Into<EventName>, listener: Listener) -> ListenerId {
            let id = self.next_id;
            self.next_id = self.next_id.saturating_add(1);
            let entry = self.listeners.entry(name.into()).or_default();
            entry.push((id, listener));
            id
        }

        /// Unsubscribe a listener by id. Returns true if removed.
        pub fn unsubscribe(&mut self, id: ListenerId) -> bool {
            let mut removed = false;
            for (_, vec) in self.listeners.iter_mut() {
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
        pub fn emit(&self, name: &str, payload: &JsonValue) {
            if let Some(vec) = self.listeners.get(name) {
                for (_, cb) in vec {
                    cb(payload);
                }
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
    use super::event_bus::{EventBus, ListenerId};
    use super::protocol::{EventMessage, InvokeRequest, InvokeResponse};
    use serde_json::Value as JsonValue;
    use std::sync::Mutex;

    /// Adapter to dispatch invokes and handle pub/sub events.
    #[derive(Default)]
    pub struct ChannelAdapter {
        bus: Mutex<EventBus>,
        on_emit: Mutex<Option<Box<dyn Fn(&EventMessage) + Send + Sync + 'static>>>,
    }

    impl ChannelAdapter {
        /// Create a new channel adapter.
        pub fn new() -> Self {
            Self {
                bus: Mutex::new(EventBus::new()),
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
                Err(e) => {
                    let msg = match e {
                        AdapterError::UnknownCommand(s) => format!("unknown command: {s}"),
                        AdapterError::Handler(s) => s,
                        AdapterError::Serialization(s) => format!("serialization error: {s}"),
                        AdapterError::Other(s) => s,
                    };
                    InvokeResponse {
                        in_reply_to: req.id,
                        ok: None,
                        err: Some(msg),
                    }
                }
            }
        }

        /// Subscribe to a named event on the embedded event bus.
        pub fn subscribe(
            &self,
            name: impl Into<String>,
            listener: super::event_bus::Listener,
        ) -> ListenerId {
            let mut bus = self.bus.lock().expect("event bus poisoned");
            bus.subscribe(name, listener)
        }

        /// Unsubscribe a listener by id from the embedded event bus.
        pub fn unsubscribe(&self, id: ListenerId) -> bool {
            let mut bus = self.bus.lock().expect("event bus poisoned");
            bus.unsubscribe(id)
        }

        /// Emit an event to local subscribers. Also triggers the `on_emit` hook if set.
        pub fn emit_event(&self, name: &str, payload: &JsonValue) {
            // Notify local subscribers.
            {
                let bus = self.bus.lock().expect("event bus poisoned");
                // `emit` takes &self; borrowing scoped so we can call the hook below.
                bus.emit(name, payload);
            }
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

// -- Public re-exports (stable import points for downstream crates) -------------------------------

pub use channel::ChannelAdapter;
pub use command_registry::{CommandName, CommandPayload, CommandRegistry, CommandResult};
pub use error::AdapterError;
pub use event_bus::{EventBus, EventName, ListenerId};
pub use protocol::{EventMessage, InvokeRequest, InvokeResponse, MessageId};
