#![cfg(all(feature = "tauri_compat_ipc", feature = "async-handlers"))]

//! Async-handler integration tests for the Tauri-compatible IPC adapter.
//!
//! These tests are only compiled and run when both features are enabled:
//! - versoview feature: `tauri_compat_ipc`
//! - adapter feature (forwarded by versoview): `async-handlers`
//!
//! To run (after forwarding the `async-handlers` feature in versoview to the dependency):
//!   cargo test -p versoview --features "tauri_compat_ipc,async-handlers" --tests
//!
//! The tests exercise:
//! 1) async invoke dispatch with a simple command that awaits and returns a value.
//! 2) sync-vs-async precedence: `invoke_async` prefers sync handlers if both are present.
//! 3) error mapping for async handlers that return `Err`.

use serde_json::{Value as JsonValue, json};
use tauri_compat_ipc::{ChannelAdapter, CommandRegistry, InvokeRequest, box_async};
use tokio::time::{Duration, sleep};

#[tokio::test(flavor = "multi_thread")]
async fn async_invoke_roundtrip_delayed_sum() {
    // Arrange: registry with an async handler that sums two integers after a short delay.
    let mut reg = CommandRegistry::new();
    reg.register_async(
        "delayed_sum",
        box_async(|payload: JsonValue| async move {
            // Extract two numbers from payload
            let a = payload.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
            let b = payload.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
            // Simulate async work
            sleep(Duration::from_millis(25)).await;
            Ok(json!(a + b))
        }),
    );

    let adapter = ChannelAdapter::new();

    // Act: send an InvokeRequest using the async dispatch API.
    let req = InvokeRequest {
        command: "delayed_sum".to_string(),
        payload: json!({"a": 2, "b": 40}),
        id: 100,
    };
    let resp = adapter.invoke_async(&reg, req).await;

    // Assert: the result is present and equals 42.
    assert_eq!(resp.in_reply_to, 100);
    assert!(resp.err.is_none(), "unexpected error: {:?}", resp.err);
    assert_eq!(resp.ok, Some(json!(42)));
}

#[tokio::test(flavor = "multi_thread")]
async fn async_invoke_prefers_sync_when_both_exist() {
    // Arrange: register both a sync and an async handler for the same command.
    // The registry's invoke_async prefers the sync handler when both exist.
    let mut reg = CommandRegistry::new();

    // Sync handler returns a fixed string immediately.
    reg.register(
        "echo_mode",
        Box::new(|payload: JsonValue| {
            let mode = payload
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("none");
            Ok(json!(format!("sync:{mode}")))
        }),
    );

    // Async handler would return "async:<mode>" after a delay, but should not be used.
    reg.register_async(
        "echo_mode",
        box_async(|payload: JsonValue| async move {
            let mode = payload
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("none");
            sleep(Duration::from_millis(10)).await;
            Ok(json!(format!("async:{mode}")))
        }),
    );

    let adapter = ChannelAdapter::new();

    // Act: call invoke_async - registry prefers sync when both registered.
    let req = InvokeRequest {
        command: "echo_mode".to_string(),
        payload: json!({"mode": "fast"}),
        id: 7,
    };
    let resp = adapter.invoke_async(&reg, req).await;

    // Assert: sync result takes precedence.
    assert_eq!(resp.in_reply_to, 7);
    assert!(resp.err.is_none(), "unexpected error: {:?}", resp.err);
    assert_eq!(resp.ok, Some(json!("sync:fast")));
}

#[tokio::test(flavor = "multi_thread")]
async fn async_invoke_propagates_handler_error() {
    // Arrange: async handler that returns an error after a short delay.
    let mut reg = CommandRegistry::new();
    reg.register_async(
        "fail_after_delay",
        box_async(|_payload: JsonValue| async move {
            sleep(Duration::from_millis(5)).await;
            Err(tauri_compat_ipc::AdapterError::Handler(
                "boom: test error".to_string(),
            ))
        }),
    );

    let adapter = ChannelAdapter::new();

    // Act
    let req = InvokeRequest {
        command: "fail_after_delay".to_string(),
        payload: JsonValue::Null,
        id: 999,
    };
    let resp = adapter.invoke_async(&reg, req).await;

    // Assert: error propagated in string form, no ok payload.
    assert_eq!(resp.in_reply_to, 999);
    assert!(resp.ok.is_none());
    let err = resp.err.unwrap_or_default();
    assert!(
        err.contains("boom: test error"),
        "missing error message: {err}"
    );
}
