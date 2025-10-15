# versoview

An out-of-process server that implements the Verso IPC protocol over framed stdio (and optionally a Unix-domain socket on Unix). It hosts a pluggable rendering “Engine” used by a Photon-like shell for standalone windowing. Engines can be selected at runtime via an env var, and compiled in via Cargo features.

Highlights:
- Runs on Linux and macOS locally (Windows planned).
- Rust toolchain: 1.90 (MSRV aligned with your Servo workspace requirement).
- Engines:
  - servo (default placeholder adapter in this branch)
  - demo (deterministic gradient generator for protocol bring-up)
  - dioxus (feature-gated; SSR in-process → rasterized PNG placeholder)

## Engine selection at runtime

Select the engine with the `VERSOVIEW_ENGINE` environment variable:

- `servo` (default): placeholder that currently forwards to the demo engine; intended for future Servo embedding.
- `demo`: deterministic gradient frames, useful for transport testing.
- `dioxus`: in-process Dioxus SSR → HTML → rasterized PNG. Requires building with the `dioxus_engine` feature.

Examples:
- Run demo engine:
    
    VERSOVIEW_ENGINE=demo ./target/debug/versoview

- Run dioxus engine (build with feature first):
    
    VERSOVIEW_ENGINE=dioxus ./target/debug/versoview


## Enabling the Dioxus engine (feature: `dioxus_engine`)

The Dioxus-based engine is feature-gated to keep builds lean unless you opt in.

- Build (debug):

    cargo build -p versoview --features dioxus_engine

- Build with optional zero-copy transport support on Unix:

    cargo build -p versoview --features "dioxus_engine zero_copy"

What the Dioxus engine does today:
- Renders a small, compiled-in Dioxus component using SSR (no JS), producing HTML.
- Rasterizes a deterministic placeholder image (PNG) derived from the HTML’s content for POC purposes.
- Returns frames to the host as `CompressedImage { format: Png, payload: Inline }` via the Verso protocol.
- Load semantics:
  - Passing `about:blank` will synthesize an empty HTML page.
  - Passing `dioxus://default` (or `about:dioxus`) uses the compiled-in Dioxus SSR sample.
  - Passing a string starting with `<` is treated as raw HTML.
  - Passing `data:text/html,...` is accepted verbatim (percent-decoding TBD).

Using your own Dioxus app:
- The current POC engine compiles and renders a small built-in component.
- To render your own Dioxus crate, depend on it in the workspace and replace the SSR call in `src/dioxus_engine.rs` with your app’s root component rendering.
- Future work can expose a stable integration point (e.g., a trait or function pointer) to avoid editing the engine source.

## Building

- Default (no special engine features):

    cargo build -p versoview

- With Dioxus engine:

    cargo build -p versoview --features dioxus_engine

- With Dioxus + zero-copy (Unix):

    cargo build -p versoview --features "dioxus_engine zero_copy"


## Running

- StdIO transport (default):

    ./target/debug/versoview

- Select engine:

    VERSOVIEW_ENGINE=demo ./target/debug/versoview
    VERSOVIEW_ENGINE=dioxus ./target/debug/versoview

- Unix socket transport (feature: `zero_copy`, Unix only):

    ./target/debug/versoview --unix-socket /tmp/versoview.sock

Logging (tracing):
- Set RUST_LOG to control verbosity, e.g.:

    RUST_LOG=info ./target/debug/versoview


## Protocol and frames

- Versoview speaks the Verso IPC protocol over framed stdio by default.
- Frames emitted by the engines are mapped to protocol descriptors:
  - Demo engine and Dioxus engine return `CompressedImage` (PNG) inline payloads for portability.
  - Zero-copy paths (memfd, dmabuf, IOSurface, DXGI shared handles) are planned; gated by `zero_copy` feature and engine support.

See the integration tests for a full round-trip example: `tests/ipc_roundtrip.rs` (Init → BindSurface → Load → Events → Shutdown).


## Testing

Follow the Nextest rule provided for the workspace:

    RUST_BACKTRACE=full cargo nextest run --workspace --all-targets --no-fail-fast

To run only versoview tests:

    RUST_BACKTRACE=full cargo nextest run -p versoview --all-targets --no-fail-fast


## Transport notes

- StdIO is always available.
- Unix socket transport requires building with `zero_copy` on Unix.
- Handle passing is only used when both the transport and engine advertise support; the Dioxus engine currently returns compressed PNG frames inline (no handle passing needed).


## Roadmap

- Replace the Dioxus engine’s placeholder rasterizer with a real HTML/CSS renderer (Servo subsystems with WeBrender for GPU acceleration).
- Engine capability negotiation for zero-copy paths (memfd/dmabuf on Linux, IOSurface on macOS, DXGI shared handles on Windows).
- Pluggable Dioxus app integration (trait-based) without modifying the engine source.
- DevTools and diagnostics plumbing exposed via IPC events.


## Troubleshooting

- “VERSOVIEW_ENGINE=dioxus” has no effect:
  - Ensure you built with `--features dioxus_engine`.
- No frames arrive:
  - Check logs with `RUST_LOG=debug`.
  - Ensure you sent `BindSurface` and `RequestDraw`/`DrawNow` after `Load`.
- Build errors on older Rust:
  - Use Rust 1.90 toolchain as required.