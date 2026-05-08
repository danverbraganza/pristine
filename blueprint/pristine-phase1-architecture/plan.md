# Technical Design: Pristine Phase 1 Architecture

## Overview

Phase 1 establishes the core type system and runtime for the Pristine agentic harness engine. The deliverable is a Rust workspace with a library crate (`pristine`) containing core types and traits, and a binary crate (`pristine`/`1p`) that demonstrates the end-to-end workflow: construct a Harness, register a Model, add an Agent, send a message, and stream the response.

## Architecture

### Crate structure

```
pristine/
  Cargo.toml            # workspace root (single crate, lib + two bin targets)
  src/
    lib.rs              # pub mod declarations, top-level Error enum
    harness.rs          # Harness struct, builder, agent/model registry
    agent.rs            # Agent struct, AgentId type
    history.rs          # HistoryNode, Block enum, node IDs
    model.rs            # Model trait, StreamEvent, common types
    model/
      anthropic.rs      # Anthropic Messages API implementation
    bin/
      pristine.rs       # Binary entry point (also aliased as `1p`)
```

`lib.rs` is minimal: module declarations, the composed `Error` enum, and re-exports of key public types.

### Core types

#### Harness

```rust
pub struct Harness {
    models: HashMap<ModelId, Arc<dyn Model>>,
    agents: HashMap<AgentId, Agent>,
}
```

- Created via `HarnessBuilder` in `main()`.
- Owns all Models (behind `Arc<dyn Model>` for shared borrowing by Agents).
- Owns all Agents (acts as a registry).
- Not a global static. Created as a local in `main()`, passed by reference or owned directly.
- Phase 1 exposes `add_model()` and `add_agent()` methods. No removal or dynamic reconfiguration yet.

**Why `Arc<dyn Model>` instead of `&Model`:** Agents need a reference to their Model, but the Harness also owns the Agents. Storing `&'a Model` inside an Agent that's stored inside the Harness creates a self-referential struct. `Arc` sidesteps this cleanly — the Harness holds one `Arc`, the Agent holds a clone.

#### Agent

```rust
pub struct Agent {
    id: AgentId,
    model: Arc<dyn Model>,
    history: History,
}
```

- `AgentId` is a newtype over a `String` (or compact ID type). Assigned at construction, unique within the Harness.
- Holds a cloned `Arc<dyn Model>` — cheap, no lifetime entanglement with Harness.
- Owns its `History`.
- Primary method: `send_message(&mut self, input: &str) -> Result<impl Stream<Item = Result<StreamEvent>>>` — appends a `UserMessage` to history, calls the Model, streams back the response, and appends the `AgentMessage` to history once the stream completes.

#### History

```rust
pub struct History {
    head: Option<Arc<HistoryNode>>,
}

pub struct HistoryNode {
    id: NodeId,
    block: Block,
    parent: Option<Arc<HistoryNode>>,
}
```

- Immutable persistent linked list. Each node points to its parent via `Arc`.
- `History` is a cursor — it holds an `Arc` to the current head node. Appending creates a new node pointing to the old head; it does not mutate.
- Multiple Agents can share a prefix: clone the `History` (cheap — just cloning an `Arc`), then each appends independently.
- `NodeId` is a newtype (UUID or similar) assigned at node creation. Exists for future disk persistence — each node is addressable by its stable ID.
- `Block` is an enum:

```rust
pub enum Block {
    UserMessage { content: String },
    AgentMessage { content: String },
    ToolCall { /* deferred — fields TBD */ },
    ToolResult { /* deferred — fields TBD */ },
    PeerMessage { from: AgentId, content: String },
}
```

`ToolCall` and `ToolResult` variants are defined but not used in Phase 1. They exist so the enum is forward-compatible without breaking changes.

**Future persistence:** The stable `NodeId` on each node enables a future `HistoryStore` trait that can write nodes to disk and load them lazily. The `Arc`-based in-memory representation becomes a cache layer. This requires no structural changes to `HistoryNode` — only adding a store behind `History` that intercepts parent traversal.

**Future history transformation:** A function `fn prepare_for_completion(&self, history: &History) -> Vec<Block>` (or similar) will sit between the Agent and the Model, transforming the raw history into whatever the Model needs. Phase 1 just linearizes the linked list into a `Vec<Block>`.

#### Model trait

```rust
pub trait Model: Send + Sync {
    fn complete(
        &self,
        messages: &[Block],
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent, model::Error>> + Send + '_>>;
}
```

- Returns a boxed `Stream` of `StreamEvent`s. Boxed because the trait is object-safe (`dyn Model`).
- `StreamEvent` covers the SSE event types from the Anthropic API:

```rust
pub enum StreamEvent {
    ContentDelta { text: String },
    ContentComplete { text: String },
    MessageStart { message_id: String, model: String },
    MessageComplete { usage: Usage },
    Error { message: String },
}
```

`Usage` captures token counts (`input_tokens`, `output_tokens`).

- The trait takes `&[Block]` — the caller (Agent) is responsible for linearizing its History into this slice. This keeps the Model stateless and decoupled from History's linked-list representation.

#### Anthropic implementation

```rust
pub struct AnthropicModel {
    client: reqwest::Client,
    api_key: String,
    model_name: String,  // e.g., "claude-sonnet-4-20250514"
}
```

- Constructed via `AnthropicModelBuilder` with required `api_key` and `model_name`.
- Implements `Model` by:
  1. Converting `&[Block]` into the Anthropic Messages API JSON request format.
  2. Sending a POST to `https://api.anthropic.com/v1/messages` with `stream: true`.
  3. Parsing the SSE response into a `Stream<Item = Result<StreamEvent>>`.
- Request/response types (Anthropic-specific JSON structures) are private to `model/anthropic.rs`. Only `StreamEvent` crosses the module boundary.
- SSE parsing: use `reqwest`'s byte streaming + manual SSE line parsing, or `eventsource-stream` crate (lightweight, well-maintained). The Anthropic SSE format follows the standard `data:` / `event:` protocol.

### Error handling

Each module defines its own error enum:

```rust
// model.rs
pub enum Error {
    Request(reqwest::Error),
    Serialization(serde_json::Error),
    Api { status: u16, message: String },
    Stream(String),
}

// history.rs
pub enum Error {
    // Phase 1: minimal. Grows as persistence is added.
}
```

Top-level composition in `lib.rs`:

```rust
pub enum Error {
    Model(model::Error),
    History(history::Error),
    Agent(agent::Error),
}
```

With `From` impls for each, so `?` propagates cleanly across module boundaries. The binary wraps everything in `anyhow` for ergonomic error reporting.

### Dependencies

```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
reqwest = { version = "0.12", features = ["stream", "json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
futures-core = "0.3"        # Stream trait
tokio-stream = "0.1"        # Stream utilities
uuid = { version = "1", features = ["v4"] }  # NodeId, AgentId
eventsource-stream = "0.2"  # SSE parsing (evaluate vs hand-rolling)

[dev-dependencies]
anyhow = "1"

[[bin]]
# anyhow used in binary via direct dependency or dev-dep trick
```

Note: `anyhow` is only needed by the binary. Depending on Cargo layout, it may go in `[dependencies]` but only be imported in `bin/pristine.rs`.

### Binary workflow

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let model = AnthropicModelBuilder::new()
        .api_key("sk-...".into())  // or read from arg/env in practice
        .model_name("claude-sonnet-4-20250514".into())
        .build()?;

    let mut harness = HarnessBuilder::new()
        .add_model("claude", model)
        .build()?;

    harness.add_agent("agent-1", "claude")?;

    let stream = harness.agent_mut("agent-1")?.send_message("Hello!").await?;
    tokio::pin!(stream);

    let start = std::time::Instant::now();
    let mut full_response = String::new();

    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::ContentDelta { text } => {
                print!("{}", text);
                full_response.push_str(&text);
            }
            StreamEvent::MessageComplete { usage } => {
                println!("\n---");
                println!("Input tokens: {}", usage.input_tokens);
                println!("Output tokens: {}", usage.output_tokens);
                println!("Duration: {:?}", start.elapsed());
            }
            _ => {}
        }
    }

    Ok(())
}
```

## Data flow

```
main()
  |
  v
HarnessBuilder -- constructs --> Harness (owns Models, Agents)
  |
  v
harness.add_agent("agent-1", "claude")
  |  Agent created with:
  |    - AgentId("agent-1")
  |    - Arc<dyn Model> cloned from Harness's model registry
  |    - Empty History
  v
agent.send_message("Hello!")
  |
  |  1. Append UserMessage { content: "Hello!" } to History
  |  2. Linearize History into Vec<Block>
  |  3. Call model.complete(&blocks)
  |  4. Return the Stream to caller
  |  5. As stream completes, append AgentMessage to History
  v
Stream<StreamEvent> --> caller prints tokens, collects metadata
```

## Implementation plan

### Step 1: Project scaffolding
- Create `src/lib.rs` with module declarations
- Create empty module files: `harness.rs`, `agent.rs`, `history.rs`, `model.rs`, `model/anthropic.rs`
- Add dependencies to `Cargo.toml`
- Verify `cargo check` passes

### Step 2: History and Block types
- Implement `NodeId` (UUID newtype), `AgentId` (String newtype)
- Implement `Block` enum with all five variants
- Implement `HistoryNode` and `History` (append, linearize to `Vec<Block>`)
- Implement `history::Error`
- Unit tests: append, linearize, fork (two histories sharing a prefix)

### Step 3: Model trait and StreamEvent
- Define `Model` trait with `complete` method
- Define `StreamEvent` enum and `Usage` struct
- Define `model::Error`

### Step 4: Anthropic implementation
- Implement request serialization (Block -> Anthropic JSON)
- Implement SSE response parsing into `Stream<StreamEvent>`
- Implement `AnthropicModelBuilder`
- Implement `Model` for `AnthropicModel`
- Integration test (may require API key; mark `#[ignore]` if so)

### Step 5: Harness and Agent
- Implement `Harness` struct with model and agent registries
- Implement `HarnessBuilder`
- Implement `Agent` struct with `send_message`
- Implement `agent::Error`
- Wire up history append on send and on stream completion

### Step 6: Top-level error composition
- Define `pristine::Error` in `lib.rs` with `From` impls
- Verify `?` propagation works across module boundaries

### Step 7: Binary
- Wire up `main()` in `src/bin/pristine.rs` with the full workflow
- Stream tokens to stdout, print metadata on completion
- Manual end-to-end test against live Anthropic API

### Execution model

This plan is executed using the Coordinator/Coding Subagent/Judge workflow defined in this repository:

- The **Coordinator** creates beads for each step and delegates them to **Coding Subagents**.
- Each **Coding Subagent** must satisfy the Definition of Done on every commit:
  - `cargo fmt --check` passes
  - `cargo clippy --all-targets --all-features` passes (zero warnings)
  - `cargo nextest run` passes within the 120s global timeout
  - No policy violations in `STYLE_GUIDE.md`
- The **Judge** reviews each completed bead for correctness, style, and policy compliance before the Coordinator accepts it.
- Steps 1-7 above map to beads. The Coordinator may split or combine them as appropriate during execution.

## Open questions (deferred past Phase 1)

- **IPC protocol:** Client isolation via socket/protocol TBD. Binary will gain run modes based on config.
- **Tool use / SkillSets:** Agent capabilities beyond text-in/text-out.
- **History persistence:** Disk-backed `HistoryStore` trait, lazy loading, eviction.
- **History transformation:** Summarization or other preprocessing before completion.
- **Multi-model Agents:** Agents with multiple Models for different purposes.
- **Config files:** TOML deserialization into builders.
- **Agent lifecycle:** Dynamic add/remove, agent-to-agent communication via PeerMessage.
