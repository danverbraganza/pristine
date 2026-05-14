# Plan: JSON-RPC stdio server

## Context

Phase 1 delivered a working Harness with Agent, Model, MessageBus, and History. The binary (`pristine run`) hardcodes two Owner messages and streams replies to stdout. This plan replaces that hardcoded flow with a JSON-RPC 2.0 server over stdin/stdout, and extracts the demo into a Python client script.

## Decisions

- **Protocol**: JSON-RPC 2.0, newline-delimited on stdio.
- **Framework**: `jsonrpsee` 0.26.0 with `server-core` + `macros` features. Gives us `RpcModule` for dispatch and `#[rpc(server)]` proc macros for method registration, without the HTTP/WS server dependency.
- **Stdio adapter**: ~30-50 lines of glue. Reads newline-delimited JSON from `tokio::io::stdin`, dispatches via `RpcModule::raw_json_request`, writes responses to stdout. Subscription notifications from the returned `Receiver` are interleaved on stdout via an async mutex.
- **Stdout serialization**: Async mutex on stdout writer. Simple, correct for single-client stdio.
- **Shutdown**: Both a `shutdown` JSON-RPC method and EOF on stdin trigger graceful Harness shutdown.
- **Harness config**: Hardcoded single-agent/single-model (same as today). Adding agents/models later is cheap because the proc macro approach means each new method is one trait function.
- **Logging**: Stderr stays informal (`eprintln!`) for now.

## Method surface

### `initialize` (request → response)

Client sends this first to learn the server's state. Returns:

```json
{
  "agent_id": "<uuid>",
  "owner_id": "<uuid>"
}
```

No `initialized` notification is auto-fired; the client must request.

### `send_message` (request → response + notifications)

Params:

```json
{
  "agent_id": "<uuid>",
  "content": "Hello, Pristine"
}
```

Response: `{ "ok": true }`

Side effect: the server implicitly subscribes the caller to that agent's event stream. Notifications flow as `agent.event`:

```json
{
  "jsonrpc": "2.0",
  "method": "agent.event",
  "params": {
    "agent_id": "<uuid>",
    "type": "token_delta",
    "data": { "text": "Hello" }
  }
}
```

Event types: `token_delta`, `block_complete`, `run_complete`, `error`. All four AgentEvent variants are delivered; the client filters as needed.

### `shutdown` (request → response)

Params: none. Response: `{ "ok": true }`. Triggers graceful Harness shutdown.

## Implementation steps

### Step 1: Add jsonrpsee dependency

Add to `Cargo.toml`:

```toml
jsonrpsee = { version = "0.26.0", features = ["server-core", "macros"] }
```

Files: `Cargo.toml`

### Step 2: Define the RPC trait

Create `src/rpc.rs` with a `#[rpc(server)]` trait defining the three methods. The trait impl holds an `Arc` to the running Harness state (bus, agent ID, owner ID, shutdown signal).

The `send_message` method calls `bus.send_inbound(...)` and spawns a task that subscribes to the agent's event stream and forwards AgentEvents as JSON-RPC notifications through jsonrpsee's subscription/pending-subscription sink.

Files: `src/rpc.rs` (new), `src/lib.rs` (add `pub mod rpc`)

### Step 3: Write the stdio adapter

Create `src/stdio.rs` with the read/dispatch/write loop:

1. Build the Harness (same hardcoded setup as today).
2. Call `harness.start()`.
3. Register RPC methods on an `RpcModule`, passing in shared Harness state.
4. Loop: read a line from stdin → `rpc_module.raw_json_request(line)` → write response to stdout. Spawn a task to drain the subscription notification `Receiver` and write those to stdout too.
5. On EOF or `shutdown` method: call `harness.shutdown()`, `harness.join()`, exit.

Stdout writes go through `Arc<tokio::sync::Mutex<tokio::io::Stdout>>`.

Files: `src/stdio.rs` (new), `src/lib.rs` (add `pub mod stdio`)

### Step 4: Rewire `pristine run`

Replace the current `run_async()` body in `src/lib.rs` with a call into the stdio adapter. The constants `SYSTEM_PROMPT`, `DEFAULT_MODEL`, `ANTHROPIC_MODEL_KEY` stay; the `FIRST_MESSAGE`/`SECOND_MESSAGE` constants and the hardcoded event loop are removed.

Files: `src/lib.rs`

### Step 5: Write the Python client

Create `client.py` at the repo root with inline `# /// script` metadata declaring `jsonrpcclient` as a dependency. The script:

1. Spawns `cargo run -- run` (or the built binary) as a subprocess with piped stdin/stdout.
2. Sends `initialize`, prints agent/owner IDs.
3. Sends `send_message` with "Introduce yourself to me, Pristine".
4. Reads notifications, prints token deltas to the terminal.
5. On `run_complete`, sends `send_message` with "Write me a poem of what it is like to be you, Pristine".
6. On second `run_complete`, sends `shutdown`.

Runnable as `uv run client.py`.

Files: `client.py` (new)

### Step 6: Update ARCHITECTURE.md

Add a "Phase 2: JSON-RPC Server" section after the existing "Initial Build" section. Describes the server architecture, method surface, stdio adapter, and notification model. Keep the Phase 1 pseudocode as historical context.

Files: `ARCHITECTURE.md`

### Step 7: Tests

- Unit tests for the RPC method implementations using a stub Harness/MessageBus.
- Integration test: spawn the stdio adapter in-process, feed it JSON-RPC requests via an in-memory pipe, assert correct responses and notification sequences.
- Verify existing 31 tests still pass (no regressions).

Files: `src/rpc.rs` (inline tests), `tests/stdio_integration.rs` (new)

## Risks

- **`raw_json_request` subscription semantics**: The `Receiver` returned by `raw_json_request` may have edge cases around subscription lifecycle (buffering, backpressure). If it doesn't map cleanly to "auto-push on send_message", we may need to manage subscription sinks manually via `register_subscription` instead of the proc macro.
- **jsonrpsee version pinning**: 0.26.0 is latest. The `raw_json_request` API is not marked as stable in their changelog — a future version could change it. Pin the version.

## Future work (not in scope)

- Explicit `subscribe`/`unsubscribe` methods for multi-agent observation.
- Unix domain socket transport for multi-client access.
- Agent/model CRUD methods (`create_agent`, `list_agents`, `register_model`).
- Structured logging on stderr.
- Configuration file for Harness setup.
