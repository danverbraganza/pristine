# ARCHITECTURE

## Design Principles

### Portable shape, adapter dialect

Pristine's portable layer -- the engine, traits, and types that agents and clients touch -- carries only the information
that every targeted provider, model, or client requires. Provider dialect (wire formats, naming conventions, optional
fields, error vocabularies, advertisement mechanisms) lives in adapters at the periphery. The portable layer never grows
speculatively; a capability is promoted from "adapter-specific" into the portable layer only when it surfaces as a
common pattern across a majority of providers.

The principle has four consequences:

1. **Information vs. dialect.** Portable types capture *what* a turn,
   tool, or stream event is. Adapters capture *how* one provider
   encodes it on the wire. The split is load-bearing.
2. **No leaks downward.** When a feature is unique to one provider,
   the rest of the engine never learns about it. Substituting providers
   does not propagate changes through the core.
3. **Additive evolution.** The portable layer grows when a pattern is
   shared, not when one provider gains a feature. Promotion happens
   with at least two cross-provider examples in view -- never on
   speculation about one.
4. **Separation of concerns at every boundary.** System prompts carry
   identity and behavior. Tools carry behavior and self-description.
   Adapters carry transport and dialect. Clients carry presentation.
   Each layer owns what it owns and nothing else.

The Goals listed below -- composability, multi-agent isolation,
transport neutrality -- are consequences of this principle, not peer
principles.

## Goals

This document describes the architecture of Pristine's core engine. See DESIGN.md for the project's design philosophy, goals, and roadmap.

The architecture is driven by three constraints:

1. **Composability** -- every major subsystem (Model, History, MessageBus, ContextCompiler) is a trait, so implementations can be swapped without changing consumer code.
2. **Multi-agent isolation** -- multiple Agents run independently within one process; a failure in one does not cascade to others.
3. **Transport neutrality** -- the JSON-RPC method surface is identical regardless of whether the transport is stdio, a Unix socket, or something else entirely.

## Core Data Model Objects

### Harness

The Harness struct represents an instance of the Pristine application itself. There will only be one Harness running per
1p process. Once constructed and allocated, the Harness owns all the Models and Agents.

On start-up, Pristine will construct the Harness, start its main loop, and then yield control to the Harness. Initially,
the Harness will be constructed via a builder function pattern, but the eventual goal is going to be for the Harness to
be customized via a configuration file, which will control how many agents to run, any custom prompts, what tools they
have and how they can interact with each other.

The Harness is responsible for accepting input from external users and passing them on to Agents within its registry.

The Harness also owns the MessageBus, and connects that to agents as necessary.

The Harness has a concept of multiple users, stored in Vec<User>; the main user is the Owner, representing the person
who launched this process.

The Harness has an explicit lifecycle:

- `start(&mut self) -> Result<(), Error>` synchronously spawns the per-Agent tasks and the MessageBus routing tasks. Returns once spawning is complete.
- `shutdown(&mut self)` signals cooperative termination by firing a cancellation token that every spawned task observes. Idempotent.
- `join(&mut self) -> impl Future<Output = Result<(), Error>>` is async; it awaits every spawned task to completion and returns the first error encountered, or `Ok(())` if all tasks exited cleanly. Idempotent: after `join` returns, the Agent registry is empty and a subsequent `join` returns `Ok(())` immediately.
- `send_to_agent(&self, ...)` and `subscribe(&self, ...)` interact with the running Harness via `&self`, so they can be called freely between `start` and `join`. They cannot be called concurrently with `join` itself because `join` holds `&mut self`.

**Per-Agent fault isolation.** When an Agent's `run` task exits with an `Err` (e.g., an underlying Model returns an error), the Harness emits `AgentEvent::Error { message }` on that Agent's outbound stream so subscribers can detect the failure and react. Other Agents continue running. The Harness does NOT cascade-cancel on a single Agent's failure; the policy of when to call `shutdown()` is the caller's responsibility. This isolation is required to support the architectural goal of "multiple parallel agents running within one process."


### Model

Model is a trait that abstracts how the rest of the system may interact with LLM systems to get completions.

The Model trait itself will specify functions common to the life-cycle of all models, whether Open or Closed.

The purpose of the Model trait is provide a watertight abstraction away from the details of any one Model
implementation. When designing functions that belong in these traits, therefore, it is worth thinking forward and
considering future candidate models to ensure we build a generic enough interface.

There are two variants of the Model trait:

- ARModel: Auto-regressive Language Models, with next-token-style completion-oriented structures.
- DLModel: Diffusion Langauge Models, which are able to denoise masked text in chunks

Initially, the DLModel variant will be empty, and we will focus implementation on the ARModel. The ARModel will support
a streaming interface.

We will begin by implementing the ARModel for Anthropic, but in order to avoid biasing us to their tool-call interface,
we will limit ourselves to their completion interface.

ARModel will support a streaming interface. Upon receiving a request, ARModel will return (a result of) a Stream that
will asynchronously yield the tokens.

The Model trait does not see `Block`. The Agent compiles its History into a structured `ModelInput` (a sequence of
role-tagged turns, each carrying content parts). The Model — and its provider adapters — operate exclusively on
`ModelInput`; the mapping from `Block` to `Turn` is the Agent's responsibility. The system prompt is not a separate
parameter on the Model trait; it is represented as one or more `Role::System` turns at the head of `ModelInput`, which
provider adapters concatenate or place into their provider-native system field as appropriate.

The Agent constructs `ModelInput` by linearizing its History and translating each
`Block` variant into the appropriate `Turn` / `ContentPart` shape: `UserMessage` and
`AgentMessage` become `Text` content parts on `User` / `Assistant` turns, `ToolCall`
becomes `ContentPart::ToolUse` on an `Assistant` turn, and `ToolResult` becomes
`ContentPart::ToolResult` on a `User` turn. `ReasoningTrace` is retained in History
but not routed to the model; ARMs vary on whether they accept it. In future phases
this construction will be extracted into a `ContextCompiler` trait so that selective
summarization, vector-store lookups, and sequence-to-sequence "context compilation"
can be plugged in without changing the Agent or the Model trait. The `ModelInput`
shape is the stable contract between compiler and model.


### History

History will be represented as a linked-list and a stack of immutable Block objects. Each Block object will have a timestamp of when
it was generated, the contents within it, and the prior block in the series. Blocks will have a type, depending on if they are:

 - UserMessage, ReasoningTrace, ToolCall, ToolResult, AgentMessage

History is the Agent's durable event log: an append-only, immutable record of
every Block the Agent has observed or produced. It is the lossless ground
truth from which any model input must be derived. Higher-level constructs —
context compilation, summarization, selective retrieval, vector-store
augmentation — are operations OVER History, not replacements for it. The
Block enum lists the engine's primitive event kinds; domain-specific agent
implementations may layer their own event types alongside History without
extending Pristine's core Block vocabulary.


### Agent

An Agent will own its History, and will have a shared reference to one or more Models. In the future of this design the
Agent will also be configurable, allowing us to alter the prompt, the skills and the Tool Calls available to it. We will
also be able to tune the strategy to enable adaptive reasoning, utilizing different models for different purposes, etc.
For now, it is sufficient if these configuration points are hard-coded properties that are directly accessed.

An Agent references Models through a ModelRole indirection: it owns HashMap<ModelRole, Arc<dyn ARModel>> (and,
eventually, an analogous map for DLModel). ModelRole is an enum naming each model's purpose within the Agent (initially
just Default; later e.g. Summarizer, Critic). The indirection allows adaptive-reasoning strategies to dispatch to the
appropriate model without restructuring the Agent.

An Agent owns an asynchonous Stream that it monitors for incoming information. This information will be a subset of the
various kinds of Block types, including but not limited to: UserMessage, ToolResult, AgentMessage. AgentMessages in the
incoming stream represent _another_ agent in the Harness communicating with our agent.

An Agent's inbound stream is provided by the MessageBus (via `register`). The
Agent reads inbound `Block`s one at a time and serializes their processing:
each Block is appended to History; the History is then compiled into a
`ModelInput` and dispatched to the Agent's `Default` Model. Streaming model
output is published to the Agent's outbound stream as `AgentEvent`s, the final
assistant response is appended to History as an `AgentMessage` Block, and the
Agent awaits the next inbound Block. Agents are limited by design to one
pending Model call at a time; this invariant falls out of the single-task event
loop and requires no explicit synchronization.

An Agent owns a logical Stream that any clients (or other agents) may subscribe to, to represent any blocks added to its
History, plus any partial streaming Tokens. The Harness might be able to follow this Stream, for instance to stream
tokens to the user as they are generated by the Model.

The Agent's outbound stream carries `AgentEvent`s of four kinds:

- `TokenDelta { text }` — a partial chunk of the active Model call's streaming
  output. Best-effort delivery; slow consumers may miss intermediate deltas.
- `BlockComplete { block }` — a `HistoryNode` has been appended to the Agent's
  History. Subscribers can rebuild a faithful view of the Agent's History by
  subscribing before the run starts and accumulating these events.
- `RunComplete { usage }` — the current model call has finished. Carries the
  reported token usage.
- `Error { message }` — the Agent's task exited with an error. Emitted by the
  Harness on behalf of the failed Agent; other Agents continue running.

The stream is multi-subscriber; subscribers filter to the kinds they care about.

When another Agent's outbound stream is routed to this Agent via the MessageBus, completed-Block events arrive on this
Agent's incoming stream and are appended to History as AgentMessage { from } Blocks like any other. The from field
enables the Agent to distinguish between Blocks from itself and peer agent messages.



### MessageBus

The MessageBus is owned by the Harness. It is the routing fabric between Agents,
and between Agents and the Harness. Each Agent publishes its outbound events
through the MessageBus; subscribers (the Harness, peer Agents, external clients)
consume them through the same surface.

The MessageBus is defined as a trait. Its surface deliberately does NOT expose
concrete channel types (no `broadcast::Sender`, no `mpsc::Receiver`). Instead the
trait exposes:

- `register(agent_id)` — produces the single-consumer inbound stream for an Agent.
- `publish(agent_id, event)` — broadcast an outbound `AgentEvent` for an Agent.
- `subscribe(agent_id)` — obtain a fresh multi-consumer stream of an Agent's
  outbound `AgentEvent`s.
- `send_inbound(agent_id, block)` — push a `Block` onto an Agent's inbound stream.
- `route(from, to)` — forward AgentMessage Blocks from one Agent's outbound to
  another Agent's inbound; the bridge between independent Agents.

The trait returns abstract `Stream` types throughout. Phase 1 ships
`InMemoryMessageBus`, a single-process implementation backed by Tokio channels.
The trait surface is intentionally shaped so that future implementations spanning
process boundaries — for client isolation, distributed agents, or persistence —
can be added without changing the `Agent`, `Harness`, or any consumer code. The
Tokio-channel mechanics are an implementation detail of `InMemoryMessageBus`,
not a contract of the trait.

### Client Protocol

Pristine exposes its capabilities to external clients via JSON-RPC 2.0. The
protocol layer sits between external callers and the Harness: clients send
JSON-RPC requests (e.g. "send a message to an agent", "subscribe to events"),
and the server translates them into Harness operations and returns results.

JSON-RPC was chosen because it provides structured request/response framing
(method dispatch, request IDs, typed errors) with minimal ceremony, and is a
proven pattern for engine-wrapping harnesses (LSP, MCP). The protocol is
transport-agnostic by design; the same JSON-RPC methods work identically
regardless of the underlying carrier.

#### Transports

The default transport is **stdio** (newline-delimited JSON-RPC over
stdin/stdout). When `pristine run` starts, it reads JSON-RPC requests from stdin
and writes JSON-RPC responses and notifications to stdout. This transport is
inherently single-client and is reserved for the Owner — the user who launched
the process.

In future work, Pristine will support a **Unix domain socket** transport,
allowing multiple concurrent clients to connect to a running server. Socket
clients will authenticate as distinct Users within the Harness. The JSON-RPC
method surface will be identical across transports; only the connection
establishment and User identity resolution differ.

#### Notifications (server → client)

Streaming data (e.g. token deltas from an Agent's model call) will be delivered
as JSON-RPC **notifications** — messages from server to client with no request
ID and no expectation of a response. Notifications use a single `agent.event`
method with a `type` discriminator field identifying the AgentEvent variant
(`token_delta`, `block_complete`, `run_complete`, `error`). All four variants
are delivered; the client filters as needed.

Calling `send_message` implicitly subscribes the caller to that Agent's event
stream for the duration of the resulting model run. Explicit `subscribe` /
`unsubscribe` methods may be added in the future for clients that want to
observe Agents they did not message directly.

#### Methods

The initial method surface is deliberately small. Adding methods is cheap: each
is a function on a `#[rpc(server)]` trait, and the `jsonrpsee` proc macro
generates all dispatch, deserialization, and error-handling code.

- `initialize` — client calls this first; server responds with `agent_id` and
  `owner_id` so the client knows how to address subsequent requests. No
  `initialized` notification is auto-fired; the client must request.
- `send_message(agent_id, content)` — wraps `Harness::send_to_agent`. Returns
  acknowledgement. Implicitly subscribes the caller to `agent.event`
  notifications for that Agent.
- `shutdown` — triggers graceful Harness shutdown.

#### Implementation

The server is built on `jsonrpsee` (v0.26, `server-core` + `macros` features
only — no HTTP/WS server dependency). Method dispatch uses
`RpcModule::raw_json_request`, which accepts a raw JSON-RPC string and returns
the response plus a `Receiver` for subscription notifications.

A thin stdio adapter (~30–50 lines) provides the read/dispatch/write loop:
read a line from `tokio::io::stdin`, dispatch via `raw_json_request`, write the
response to stdout, and drain subscription notifications to stdout as they
arrive. Stdout writes are serialized via an async mutex — simple and correct for
the single-client stdio transport.

Graceful shutdown occurs on either a `shutdown` method call or EOF on stdin.

## Tool Calls

Agents can invoke registered tools during the run loop. The subsystem
splits cleanly across the existing trait boundaries: tools are runtime
behaviors held by the Harness, their schemas appear on the Model
contract via `ModelInput.tools`, and their exchanges flow through
History as new `Block` variants.

### Tool trait and registry

`Tool` (in `src/tool.rs`) is `Send + Sync` and exposes three synchronous
getters — `name`, `description`, `input_schema` — plus an async
`call(input) -> Result<Value, ToolError>`. Implementors are stored as
`Arc<dyn Tool>` so they can be shared across Agent tasks.

`ToolRegistry` is a name-keyed `HashMap<String, Arc<dyn Tool>>` owned by
the Harness and shared by `Arc` with every Agent it spawns.
`HarnessBuilder::add_tool` rejects duplicate names with
`Error::DuplicateTool`. `ToolRegistry::dispatch(name, input)` returns
either the tool's `Value` or a typed `ToolError` (`NotFound`,
`InvalidInput`, `Execution`).

### ToolSpec on ModelInput

A `ToolSpec { name, description, input_schema }` is the provider-agnostic
descriptor carried as `tools: Vec<ToolSpec>` alongside `turns` on each
`ModelInput`. The Agent snapshots `self.tools.list()` into ToolSpecs per
model call. Provider adapters translate ToolSpec into their native wire
format; ToolSpec itself carries no behavior.

### Block and ContentPart correlation

`ContentPart::ToolUse { id, name, input }` and
`ContentPart::ToolResult { tool_use_id, content, is_error }` carry tool
exchanges at the Model boundary. `Block::ToolCall` and `Block::ToolResult`
carry matching correlation keys (`id` and `tool_use_id`) so History →
ContentPart compilation round-trips. `Block::ToolResult.name` is retained
for human inspection but dropped during compilation — Anthropic
correlates by id alone.

### The tool-call cycle

Per inbound Block, `Agent::run` enters an inner loop. Each iteration:

1. Builds a `ModelInput { turns, tools }` snapshot from current History
   and registry.
2. Streams the model. `ModelStreamEvent::ToolUseComplete` events are
   accumulated into a pending list. An `AgentMessage` Block is appended
   only if text was produced; `RunComplete` is then emitted.
3. If the pending list is empty, the inner loop exits.
4. Otherwise each pending tool is dispatched sequentially. A
   `Block::ToolCall` is appended, the registry is invoked, and a
   `Block::ToolResult` is appended with `is_error = false` on success or
   `is_error = true` carrying the rendered error. The loop returns to
   step 1.

After the inner loop exits, the Agent emits `AgentEvent::Idle`.

### AgentEvent and JSON-RPC additions

`AgentEvent::Idle` (no fields) fires once per outer inbound-block
iteration. Subscribers use it as the "agent ready for next input"
signal — distinct from `RunComplete`, which fires per model call and
may occur multiple times within one tool-call cycle.

The JSON-RPC `block_complete` notification's `data` payload now carries
a `block_type` discriminator — one of `user_message`, `agent_message`,
`tool_call`, `tool_result`, `reasoning_trace` — with variant-appropriate
fields (e.g. `id`/`name`/`arguments` for tool_call,
`tool_use_id`/`name`/`result`/`is_error` for tool_result). `timestamp`
is intentionally not serialized yet; it requires an explicit format
choice deferred to future observability work.

### Error handling

A `ToolError` returned from `ToolRegistry::dispatch` is NOT propagated
as `Agent::Error`. The error is rendered as
`{"error": "<message>"}` and stored in
`Block::ToolResult { is_error: true, result, ... }`. The model sees the
error on its next iteration and may recover or terminate the cycle.

### Anthropic adapter

The outgoing request body uses typed content blocks (`text`, `tool_use`,
`tool_result`) instead of a flat string, and emits a top-level `tools`
array when `input.tools` is non-empty (omitted otherwise, preserving the
Phase 1 request shape for tool-free calls). The streaming parser
recognises `content_block_start` with `type: tool_use` and
`content_block_delta` with `type: input_json_delta`, accumulating partial
JSON per content-block index and emitting
`ModelStreamEvent::ToolUseStart` / `Delta` / `Complete` events.

### Built-in tools

`src/builtins.rs` ships an `AddTool` example — takes
`{a: number, b: number}` and returns `{sum: number}`. `pristine run`
registers it with the `HarnessBuilder` so the binary demonstrates the
tool-call loop end-to-end. Future built-in tools live in the same
module.

## Key Technological Choices

- Tokio
- Command line parsing with Clap
- `jsonrpsee` 0.26 (`server-core`, `macros`) for JSON-RPC dispatch

## Phase 1: Initial Build

To validate the core engine design, Phase 1 implemented `pristine run` as a
hardcoded two-message demo. Code is indicative only, not proper syntax or
naming:

```
fn main() {
    let mut harness = HarnessBuilder::new()
        .add_model(ModelId("anthropic-default"), AnthropicModel::build(...))
        .add_agent(PendingAgent { id, system_prompt: "You are the Pristine agent. ...", model_id })
        .build()?;

    harness.start()?;

    let mut events = harness.subscribe(agent_id)?;

    harness.send_to_agent(agent_id, owner_id, "Introduce yourself to me, Pristine");

    // Stream tokens to stdout until the first RunComplete arrives.

    harness.send_to_agent(agent_id, owner_id, "Write me a poem of what it is like to be you, Pristine");

    // Stream tokens to stdout until the second RunComplete arrives.

    harness.shutdown();
    harness.join().await?;
}
```

## Phase 2: JSON-RPC Server

Phase 2 replaces the hardcoded demo with the JSON-RPC stdio server described in
§Client Protocol above. `pristine run` starts the server, which reads
newline-delimited JSON-RPC requests from stdin and writes responses and
notifications to stdout. The Harness setup (single Agent, single Anthropic
Model) remains hardcoded for now.

The Phase 1 demo flow is preserved as a Python client script (`client.py`),
runnable via `uv run`, which spawns `pristine run` as a subprocess and drives
the same two-message conversation over JSON-RPC:

```python
# Pseudocode — client.py (uv run, jsonrpcclient)
proc = subprocess.Popen(["pristine", "run"], stdin=PIPE, stdout=PIPE)

# Handshake
send(proc, "initialize") -> { agent_id, owner_id }

# First message
send(proc, "send_message", { agent_id, content: "Introduce yourself..." })
# Read agent.event notifications, print token deltas until run_complete

# Second message
send(proc, "send_message", { agent_id, content: "Write me a poem..." })
# Read agent.event notifications, print token deltas until run_complete

send(proc, "shutdown")
```
