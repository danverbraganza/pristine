# Technical Design: Pristine Phase 1

## Overview

This plan describes the Phase 1 build of Pristine. It defers to `ARCHITECTURE.md` for the data model and runtime semantics, and documents only the Phase 1-specific implementation choices and step ordering. Information present in `ARCHITECTURE.md` is not restated here.

The Phase 1 deliverable is a working `1p` binary that:

- Constructs a Harness with an `ARModel` implementation backed by the Anthropic Messages API.
- Registers a single Agent under a distinguished `Owner`, with a hard-coded system prompt.
- Starts the Harness, sends two sequential `UserMessage`s from the Owner, streams the Agent's responses to stdout, and shuts down cleanly.

## Phase 1 specifics

### Crate structure

```
pristine/
  Cargo.toml
  src/
    lib.rs              # module declarations, top-level Error
    harness.rs          # Harness, HarnessBuilder, lifecycle
    user.rs             # User, UserId, Owner
    agent.rs            # Agent, AgentId, AgentBuilder, AgentEvent, event loop
    history.rs          # History, HistoryNode, NodeId, Block
    model.rs            # ARModel and DLModel traits, ModelStreamEvent, Usage, ModelRole
    model/
      anthropic.rs      # AnthropicModel: ARModel impl for the Messages API
    messagebus.rs       # MessageBus trait + in-memory implementation
    bin/
      pristine.rs       # Binary; built as `pristine` and `1p`
```

Per `STYLE_GUIDE.md`: non-mod-rs layout, `#![forbid(unsafe_code)]` at the crate root, no `unwrap()` / `expect()` in production code, no `mod.rs`.

### Phase 1 commitments

These pin down implementation choices that `ARCHITECTURE.md` leaves open or that are specific to the first build.

- **`ARModel` and `DLModel`.** Both traits are defined. `ARModel` carries the full streaming completion interface and is the only trait implemented in Phase 1. `DLModel` is declared as a placeholder trait with no methods; its purpose is to make the seam visible to future contributors.
- **`ModelRole` enum.** A single variant `Default` in Phase 1. The enum exists so the `HashMap<ModelRole, Arc<dyn ARModel>>` shape on `Agent` is structurally ready for adaptive-reasoning strategies without later refactoring.
- **`AnthropicModel`.** Implements `ARModel`. Phase 1 uses the Messages endpoint with `stream: true`. Anthropic-specific request/response JSON types are private to `model/anthropic.rs`; only `ModelStreamEvent` crosses the module boundary.
- **`ModelStreamEvent`** (the wire type returned by `ARModel::complete`). API-shape-agnostic enum:
  - `ContentDelta { text }`
  - `ContentComplete { text }`
  - `MessageStart { message_id, model }`
  - `MessageComplete { usage: Usage }`
  - `Error { message }`
  - `Usage { input_tokens, output_tokens }`.
- **`AgentEvent`** (the Agent outbound stream wire type). Unified enum:
  - `TokenDelta { text }`
  - `BlockComplete { block: Arc<HistoryNode> }`
  - `RunComplete { usage: Usage }`.
  - The Agent translates `ModelStreamEvent`s into `AgentEvent`s as the Model stream drains.
- **Outbound channel.** `tokio::sync::broadcast::channel::<AgentEvent>`. Lossy for slow consumers — this is documented on the Agent's outbound field. Token deltas are best-effort; completed Blocks are recoverable from `History` via `NodeId`.
- **Inbound channel.** Each Agent owns an `mpsc::Receiver<Block>`. The MessageBus is the single producer for both Owner-originated `UserMessage`s and routed peer `AgentMessage`s.
- **Agent event loop.** A single `tokio::select!`-driven loop: read one Block from inbound → append to `History` → linearize → call active `ARModel` → drain `ModelStreamEvent`s into `AgentEvent`s on the broadcast channel → append the resulting `AgentMessage` Block → publish `BlockComplete` then `RunComplete` → return to await the next Block. The "at most one pending Model call per Agent" invariant from `ARCHITECTURE.md` falls out structurally; no mutex required.
- **`Agent` configuration.** `AgentBuilder` requires `id: AgentId`, `system_prompt: String`, and at least one entry for `ModelRole::Default`. No skills or tool calls in Phase 1.
- **Harness lifecycle.** `Harness::start() -> JoinHandle<Result<()>>` spawns each Agent's event-loop task and the MessageBus task, and returns a join handle that resolves when all spawned tasks have exited. `Harness::shutdown()` signals cooperative termination (cancellation token + drop senders); awaiting the start-future after `shutdown()` yields a clean exit.
- **`MessageBus`.** Defined as a trait. Phase 1 ships `InMemoryMessageBus`, which routes one Agent's outbound `AgentEvent::BlockComplete` to a recipient Agent's inbound `Block` stream when wired. With a single Agent in the Phase 1 example the routing is trivial; the surface exists so peer messaging can slot in without restructuring Agent or Harness.
- **`User`, `UserId`, `Owner`.** `User` carries `id: UserId` (UUID newtype). `Harness` exposes the `Owner` `UserId` as a distinguished constant accessor after construction. `Harness::send_to_agent(agent_id, from: UserId, content)` constructs a `UserMessage { from, content, timestamp }` Block and pushes it onto the named Agent's inbound stream via the MessageBus.
- **`NodeId`.** Each `HistoryNode` carries a stable `NodeId` (UUID newtype) assigned at creation. Phase 1 does not persist nodes to disk; the stable ID is the addressing scheme for a future `HistoryStore` trait so no structural change to `HistoryNode` will be needed when that lands.
- **`prepare_for_completion`.** Phase 1's Agent linearizes its `History` head into a `Vec<Block>` for `ARModel::complete()`. A future `prepare_for_completion(&History) -> Vec<Block>` step will sit between the Agent and the Model to apply summarization or other context shaping; Phase 1 hard-codes the linearization.

### Error handling

Each module defines its own `Error` enum. The crate root composes them:

```rust
pub enum Error {
    Model(model::Error),
    History(history::Error),
    Agent(agent::Error),
    Harness(harness::Error),
    MessageBus(messagebus::Error),
}
```

With `From` impls so `?` propagates cleanly. Per `STYLE_GUIDE.md`: typed errors throughout; prefer `TryFrom` over `as` casts. The binary uses `anyhow` for top-level reporting.

### Dependencies

```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
reqwest = { version = "0.12", features = ["stream", "json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
futures = "0.3"
tokio-stream = "0.1"
uuid = { version = "1", features = ["v4"] }
eventsource-stream = "0.2"
clap = { version = "4", features = ["derive"] }
anyhow = "1"
```

A subagent may justify substitutions (e.g. hand-rolled SSE in place of `eventsource-stream`) in the relevant Bead.

### CLI surface

Clap with the `derive` feature. One default subcommand `run`. Phase 1 takes the message as a positional argument. The Anthropic API key is read from the `ANTHROPIC_API_KEY` environment variable; there is no `--api-key` flag in Phase 1. The Agent's `system_prompt` is hard-coded. The subcommand surface exists so future commands (e.g. `run-config <path>`) can be added without restructuring the binary.

## Data flow

```
main()
  -> Clap parses `run` (positional message)
  -> HarnessBuilder constructs Harness:
       - registers AnthropicModel as ARModel under ModelRole::Default
       - registers Agent with hard-coded system_prompt and the model handle
  -> harness.start() spawns Agent + MessageBus tasks, returns JoinHandle
  -> harness.subscribe(agent_id) returns broadcast::Receiver<AgentEvent>
  -> harness.send_to_agent(agent_id, Owner, message_text)
       -> MessageBus routes UserMessage Block onto Agent's inbound mpsc
  -> Agent event loop:
       recv Block -> append to History
                  -> linearize History (prepare_for_completion placeholder)
                  -> ARModel::complete -> Stream<ModelStreamEvent>
                  -> translate to AgentEvent, publish on broadcast
                  -> on completion, append AgentMessage Block,
                     publish BlockComplete + RunComplete
  -> main() consumes AgentEvent::TokenDelta and prints to stdout;
     prints usage on AgentEvent::RunComplete
  -> Repeat send_to_agent for the second message; same broadcast Receiver
  -> harness.shutdown(); await JoinHandle
```

## Implementation plan

Each step below maps to one or more Beads created by the Coordinator. Steps may be split or merged at execution time.

### Step 1: Scaffolding
- Create the module file tree per "Crate structure".
- Add `#![forbid(unsafe_code)]` at the crate root.
- Add dependencies to `Cargo.toml`.
- Verify `cargo check` and `cargo clippy --all-targets --all-features -- -D warnings` pass on empty modules.

### Step 2: History and Block
- Implement `NodeId` (UUID newtype) and the `Block` variants per `ARCHITECTURE.md` §History: `UserMessage { from: UserId, content, timestamp }`, `ReasoningTrace`, `ToolCall`, `ToolResult`, `AgentMessage { from: AgentId, content, timestamp }`.
- Implement `HistoryNode { id, timestamp, block, parent: Option<Arc<HistoryNode>> }` and `History { head: Option<Arc<HistoryNode>> }`, with `append` and linearize-to-`Vec<Block>` operations.
- Unit tests: append, linearize, fork (two `History` values sharing a prefix via `Arc`).

### Step 3: AR/DL Model traits
- Define `ARModel` with `complete(&self, messages: &[Block]) -> Pin<Box<dyn Stream<Item = Result<ModelStreamEvent, model::Error>> + Send + '_>>`.
- Define `DLModel` as an empty placeholder trait.
- Define `ModelStreamEvent`, `Usage`, `ModelRole` (sole variant `Default`).
- Define `model::Error`.

### Step 4: Anthropic ARModel
- Implement `AnthropicModel` with `AnthropicModelBuilder` (required: `api_key`, `model_name`).
- Block-to-Anthropic-JSON request serialization (private types).
- SSE response parsing into `Stream<Result<ModelStreamEvent>>` using `eventsource-stream` (unless the Bead justifies otherwise).
- Live API integration test gated behind `#[ignore]` so CI skips it and a developer can run it locally.

### Step 5: MessageBus and AgentEvent
- Define `AgentEvent` enum (`TokenDelta`, `BlockComplete`, `RunComplete`).
- Define the `MessageBus` trait: publish path from an Agent's outbound stream; subscribe path that delivers `Block`s into another Agent's inbound stream.
- Implement `InMemoryMessageBus` for the single-process case. Phase 1 only requires single-agent routing; the trait shape accommodates fan-out.

### Step 6: Harness, User, lifecycle
- Implement `User`, `UserId` (UUID newtype), and the distinguished `Owner` accessor on `Harness`.
- Implement `Harness` with model registry (`HashMap<ModelId, Arc<dyn ARModel>>`), Agent registry, and the `MessageBus` instance.
- Implement `HarnessBuilder` (`add_model`, `add_agent`).
- Implement `Harness::start() -> JoinHandle<Result<()>>` (spawn Agent loops + MessageBus task) and `Harness::shutdown()` (cooperative cancellation token + drop senders; await spawned tasks).
- Implement `Harness::send_to_agent(agent_id, from: UserId, content)` and `Harness::subscribe(agent_id) -> broadcast::Receiver<AgentEvent>`.

### Step 7: Agent event loop
- Implement `Agent` with `id`, `system_prompt`, `models: HashMap<ModelRole, Arc<dyn ARModel>>`, `history: History`, inbound `mpsc::Receiver<Block>`, outbound `broadcast::Sender<AgentEvent>`.
- Implement `AgentBuilder` (required: `id`, `system_prompt`, a `ModelRole::Default` entry).
- Implement the event-loop task: receive inbound Block → append → linearize → call `ARModel` → translate `ModelStreamEvent` → publish `AgentEvent` → append `AgentMessage` Block → publish `BlockComplete` + `RunComplete` → loop.
- The single-task design structurally enforces the "one pending Model call per Agent" invariant.

### Step 8: Binary
- Wire up `src/bin/pristine.rs` (built as both `pristine` and `1p` per existing `Cargo.toml`).
- Clap derive parser with `run` as the default subcommand and a positional message argument.
- Read `ANTHROPIC_API_KEY` from env.
- Construct Harness with one `AnthropicModel` under `ModelRole::Default` and one Agent with a hard-coded `system_prompt`.
- `harness.start()`, subscribe, send the message from the `Owner`, stream `TokenDelta`s to stdout, print usage on `RunComplete`.
- Per `ARCHITECTURE.md`'s "Initial Build" example, send a second message reusing the same subscription before calling `harness.shutdown()` and awaiting the join handle.

## Process

Execution follows the Coordinator / Coding Subagent / Judge / Tidy / Reflection workflow defined in `AGENTS.md`. The Coordinator creates Beads for each step, sequences and gates them; subagents implement; the Judge enforces `STYLE_GUIDE.md` and the Definition of Done. See `AGENTS.md` for the full process.
