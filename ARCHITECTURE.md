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

`ModelStreamEvent` carries a reasoning stream surface
(`ReasoningDelta`/`ReasoningComplete`) alongside content; the Agent publishes
`AgentEvent::ReasoningDelta` and appends each completed trace as a
`Block::ReasoningTrace` retained in History but never sent back to the model.


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

An Agent references Models through a ModelRole indirection: it owns a Models newtype holding a lifted `default` model
plus a `by_role` map of the remaining ModelRole bindings (and, eventually, an analogous structure for DLModel). ModelRole is an enum naming each model's purpose within the Agent (initially
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

A distinct `agent_forked` notification announces a newly forked peer Agent,
carrying `{agent_id, origin, handle}`. Unlike `agent.event`, it is not keyed to
a single subscribed Agent: forks originate from any Agent on the bus (whether a
tool-initiated fork or a human `fork_agent` request), so the transport
subscribes to the bus-wide fork broadcast and surfaces every one. See §Forking.

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
- `fork_agent(agent_id, instruction, handle?, tools?)` — the human fork path.
  Routes a `Control::Fork` request THROUGH the named Agent rather than spawning
  directly; the new peer's id is not returned synchronously but surfaces on the
  `agent_forked` notification. See §Forking.
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

### DeepSeek adapter

The DeepSeek adapter (`src/model/deepseek.rs`) speaks the OpenAI-compatible
ChatCompletions dialect against `https://api.deepseek.com` (base URL
overridable via `ModelInstanceConfig::extras`), posting to
`/v1/chat/completions` with `Authorization: Bearer` auth. The configured model
is one of `deepseek-v4-pro` / `deepseek-v4-flash`. DeepSeek V4 thinks by
default and surfaces a `reasoning_content` field in each streamed delta; no
thinking toggle is sent. `stream_options.include_usage` is set so the stream
ends with a usage-bearing chunk.

Unlike Anthropic, the system prompt is not a top-level field. Each
`Role::System` turn is emitted as a `system`-role **message** at the head of
the flat `messages` array; consecutive system turns concatenate.

Tool exchanges use the OpenAI shape. An assistant `ToolUse` becomes a
`tool_calls` entry whose `function.arguments` is the **stringified** JSON of
the call input (not a nested object). Tools are advertised as a top-level
`tools` array (omitted when empty), each entry `{type: "function", function:
{name, description, parameters}}` — the portable `ToolSpec.input_schema` nests
under `function.parameters`, where Anthropic uses a flat top-level
`input_schema`. A tool result becomes its own `role: "tool"` message keyed by
`tool_call_id`, separate from the assistant message that issued the call.

The streaming parser accumulates `reasoning_content`, `content`, and
indexed `tool_calls` deltas, emitting the shared `ModelStreamEvent` surface:
`ReasoningDelta`/`ReasoningComplete` for thinking, `ContentDelta`/`Complete`
for text, and `ToolUseStart`/`Delta`/`Complete` for calls. The reasoning
stream flows `ModelStreamEvent::ReasoningDelta`/`ReasoningComplete` →
`AgentEvent::ReasoningDelta` → `Block::ReasoningTrace`, retained in History but
never sent back to the model on a subsequent turn.

### Findings: the provider seam held

Adding DeepSeek as the second provider required **zero core changes**. The
provider seam (`ModelProvider` / `ProviderRegistry` / `ModelInstanceConfig`)
absorbed an entirely different wire dialect at the periphery; the engine,
`ModelInput`, and the `ModelStreamEvent` surface were sufficient as-is. Two
observations about the portable layer surfaced, neither of which justified a
change at one cross-provider example:

- `ContentPart::ToolResult.is_error` is an Anthropic-ism. OpenAI `tool`
  messages have no error field, so the DeepSeek adapter folds the flag into the
  result content text (`[tool error] …`). This is a candidate for
  promotion/demotion in the portable layer; revisit at a third provider per the
  two-example promotion rule.
- The one-Turn-per-role grouping does not match OpenAI, which needs tool
  results as separate `role: "tool"` messages. The adapter explodes a single
  `Turn` into N wire messages (assistant text + tool_calls, then one `tool`
  message per result). This is a mapping cost paid inside the adapter, not a
  core change.

### OpenRouter adapter

The OpenRouter adapter (`src/model/openrouter.rs`) targets the OpenRouter
aggregator, which exposes the same OpenAI-compatible ChatCompletions dialect as
DeepSeek. It is the second same-dialect provider, and that second example is
what motivated extracting the shared `src/model/openai_dialect.rs` module
(bd-jsr): request/response shaping, the streaming parser, and the `StreamDelta`
type are now factored out and back both adapters rather than being duplicated.

The adapter posts to `/v1/chat/completions` against a default base URL of
`https://openrouter.ai/api` (overridable via `ModelInstanceConfig::extras`),
with `Authorization: Bearer` auth. Model names are namespaced by upstream
provider (e.g. `anthropic/claude-3.5-sonnet`) and passed through verbatim.
Reasoning is surfaced via the `reasoning` delta field; the shared `StreamDelta`
reads both `reasoning` and `reasoning_content`, so one parser covers OpenRouter
and DeepSeek alike. OpenRouter's optional attribution headers (`HTTP-Referer` /
`X-Title`) are omitted for now; making them configurable is deferred.

Like DeepSeek, OpenRouter plugged in at the `ProviderRegistry` seam with **zero
core changes** — the dialect work lives entirely in `openai_dialect.rs` and the
thin per-provider adapter on top of it.

### Built-in tools

`src/builtins.rs` retains an `AddTool` example — takes
`{a: number, b: number}` and returns `{sum: number}` — exported as
`pristine::builtins::AddTool` for SDK consumers writing their own `Tool`
implementations. `pristine run` does not register it; the binary's
tool-call surface is the five real built-ins (Read, Write, Edit, Insert,
ExecBash), registered by `src/lib.rs::register_builtin_tools` and listed
in the embedded `default.toml`.

### Construction surface

Built-in tools follow a stable construction surface: each tool is a
struct in `src/builtins/{name}.rs` with an explicit
`Tool::new() -> Self` constructor. No behavioral arguments today;
future hooks (audit, path policy, allowlists, metering) land additively
on `new`. The bare `new()` is the stable plugin point.

## Built-in Tools

The harness registers five built-in tools — Read, Write, Edit, Insert,
and ExecBash — that give the agent direct filesystem and shell
capabilities. Each tool lives in its own non-mod-rs submodule under
`src/builtins/`, with co-located tests. Each tool owns its own typed
error enum (the dialect) and emits errors through the shared
`ToolError::Execution(serde_json::Value)` carrier (the portable shape).

### Read

Reads a UTF-8 text file with an optional 1-indexed inclusive line range.
Input: `{path, start_line?, end_line?}`. Output: `{content: String}`. The
file is resolved (absolute or cwd-relative); if it does not exist, the
tool returns `FileNotFound { path }`. If the file exceeds 64 KiB and no
line range is provided, the tool returns `FileTooLarge { size_bytes,
max_bytes }` (a line range bypasses this cap). The file is read in full
and UTF-8 validated; non-UTF-8 returns `NotUtf8 { byte_offset }`. Range
semantics: `start_line` of 0 or `start > end` returns `InvalidRange`; a
start beyond the file is empty (not an error); an `end_line` past the
file is clamped. Trailing newlines and CRLF are preserved. Other errors:
`InvalidPath { reason }`, `IoError { reason }`.

### Write

Atomic file write with auto-created parent directories. Input:
`{path, content}` (content is UTF-8; the `String` type guarantees it).
Output: `{bytes_written: u64}`. The target path is resolved (absolute
or cwd-relative); the parent directory is created recursively if it
does not exist. The write itself uses the shared atomic-rename helper:
content is written to `{path}.tmp` then renamed atop `{path}`,
guaranteeing readers see either the old file or the new file but
never a partial write. Existing files are silently overwritten. Error
variants: `InvalidPath { reason }`, `PermissionDenied { path }`,
`IoError { reason }`. Permission errors during parent-dir creation or
the atomic-rename steps surface as `PermissionDenied`.

### Edit

In-place text replacement with str_replace match-once safety. Input:
`{path, old_str, new_str}`. Output: `{replaced: bool}`. Behavior:
resolve the path (absolute or cwd-relative), read the file, validate
strict UTF-8, count exact-byte occurrences of `old_str`. If 0 → typed
`NoMatches` error; if ≥2 → typed `MultipleMatches { count }` error; if
exactly 1 → atomic-rename replace and return `{replaced: true}`. If
`old_str == new_str`, return `{replaced: true}` without writing
(documented no-op success). Empty `old_str` short-circuits to
`NoMatches`. Error variants (in `ToolError::Execution(Value)`):
`MultipleMatches { count }`, `NoMatches`, `FileNotFound { path }`,
`NotUtf8 { byte_offset }`, `InvalidPath { reason }`, `IoError {
reason }`. Malformed input surfaces as `ToolError::InvalidInput`. The
atomic-rename pattern (write `{path}.tmp`, then `rename` to `{path}`)
ensures Edit never leaves a partial write on disk.

### Insert

Inserts text at a specified line position in a UTF-8 text file. Input:
`{path, after_line: usize, content}`. Output: `{lines_inserted: usize}`.
The path is resolved (absolute or cwd-relative); the file is read in
full and UTF-8 validated. `after_line` is 1-indexed in the
intuitive sense: `after_line == 0` prepends, `after_line == total_lines`
appends, and any value in between inserts between line `after_line` and
line `after_line + 1`. `after_line` past the end of the file returns
`InvalidAfterLine { after_line, total_lines }`. Empty `content` is a
no-op success returning `lines_inserted: 0`. Content without a trailing
newline gets one appended to keep the file well-formed when inserting
mid-file; if the original file lacked a trailing newline, one is added
before the inserted content during append. The file write goes through
the shared atomic-rename helper. Errors: `FileNotFound { path }`,
`NotUtf8 { byte_offset }`, `InvalidAfterLine { after_line, total_lines }`,
`InvalidPath { reason }`, `IoError { reason }`.

### ExecBash

Executes a single bash command via the `Shell` trait (real impl: `BashShell`,
which spawns `/bin/bash -c <command>`). Input: `{command: String,
timeout_seconds?: u64}`; the default timeout is 30 seconds. Output:
`{stdout, stderr, status, stdout_truncated, stderr_truncated,
has_invalid_utf8_stdout, has_invalid_utf8_stderr, execution_id}`. The
returned stdout/stderr are the **last 64 KiB** of each stream
(tail-truncated), converted via `String::from_utf8_lossy`; the
`has_invalid_utf8_*` flags are `true` iff lossy conversion replaced any
bytes. `status` is one of `{status:"exit", code}`, `{status:"signal",
name}`, or `{status:"timeout"}`. `execution_id` is a hyphenless UUID v4
(32 hex chars) that names the full-output tmp files staged at
`tempdir()/pristine-{pid}/{execution_id}.{stdout,stderr}` (best-effort
write; failures are logged but do not fail the call). Error variants
(serialized as `ToolError::Execution(Value)`): `Spawn { reason }`,
`Io { reason }`, `TmpFile { reason }`. Malformed input (missing
`command`, non-string, etc.) surfaces as the engine-level
`ToolError::InvalidInput`, not an ExecBash-dialect error.

### ExecBash tmp-file storage model

ExecBash stages full stdout and stderr for each execution to per-process
tmp files at `tempdir()/pristine-{pid}/{execution_id}.{stdout,stderr}`,
with `execution_id` a UUID v4 (hyphenless). The 64 KiB tail returned
in the tool's JSON response is the live capture buffer; the full
stream lives on disk. The tmp directory is created lazily on first
ExecBash invocation and removed at harness shutdown. This staging is
forward-compat for a future Read-by-id tool that fetches full output
by execution_id (out of scope this cycle).

## Forking

Forking lets an initiator — an agent via the Fork tool, or a human via the
`fork_agent` JSON-RPC method — spawn a peer Agent seeded from the initiator's
live runtime state. The peer inherits all aspects of its parent (system prompt,
model assignment, tool set) except where explicitly narrowed, plus a bounded
slice of the parent's history. How much history the fork inherits is a slider
the initiator controls, addressed through checkpoint handles.

### Checkpoint handles

A `CheckpointHandle` (`src/history.rs`) is a stable, model-safe reference to a
point in a `History`, derived from a node's immutable `NodeId`. Its string form
is `ckpt-<uuid>` via `Display`/`FromStr`. Handles name **tool-call
boundaries**: when History is compiled into a `ModelInput`, the Agent appends a
harness-attributed `[pristine checkpoint] ckpt-<uuid>` line to each rendered
tool-result content (`annotate_tool_result_content` in `src/agent.rs`). The
line is injected at compile time only — the stored `Block::ToolResult` is never
mutated — so the model can name a past tool-call boundary when forking without
the handle polluting the durable log.

The reserved **genesis handle**, `CheckpointHandle::genesis()` wrapping
`NodeId::nil()`, is the one always-available handle that is not a tool-call
boundary. It is a **virtual sentinel**: `History::resolve` short-circuits it to
`Ok(None)` — the empty prefix — regardless of history state, so no physical
root node is ever stored. Every other handle resolves by walking the head's
parent chain; an unmatched one yields `HandleError::Unknown`, and a
non-`ckpt-` string yields `HandleError::Malformed` on parse.

### The Handle slider

The Fork tool's optional `handle` parameter is a continuous slider between full
and partial context:

- **Omitted** → the fork inherits the full prior context (the caller's live
  history head).
- **Genesis** (`nil`) → the fork inherits nothing — a pure subagent.
- **A boundary handle** → the fork inherits the prefix up to and including that
  node, dropping later blocks. Because prefixes are shared via `Arc`
  (`History::from_prefix`), the inherited slice costs no copy.
- **Invalid** (malformed or unknown) → an error, and nothing is spawned.

Omitting the handle is deliberately distinct from supplying genesis: the former
is a full fork, the latter a context-free subagent.

### The `AgentSpawner` / `Nursery` seam

Runtime agent spawning is factored out of `Harness::start` behind the
`AgentSpawner` trait (`src/harness.rs`), following the engine's
concrete-type-plus-trait shape (`Tool`/`ToolRegistry`,
`ModelProvider`/`ProviderRegistry`). `Nursery` is the Harness-owned concrete
implementor; its `build_and_track` routine is the single spawn path shared by
build-time draining of `PendingAgent`s and runtime `spawn` calls. Every spawned
Agent registers with the shared bus, observes a **per-agent child cancellation
token** derived from the Harness's root token, and has its `JoinHandle`
tracked. `Nursery::stop(agent_id)` cancels one child token, terminating a single
Agent while siblings run; the root token (fired by `Harness::shutdown`) cancels
all children at once.

An `AgentSpec` carries everything the Nursery needs to spawn a peer: the system
prompt, model, tool set, optional `history_prefix` head, optional initial
`instruction` block, and — when the spec is a fork — a `fork` provenance pairing
the origin Agent with the inherited handle. On a fork spawn the Nursery emits an
`AgentForked` event on the bus's fork broadcast; a plain runtime spawn leaves
`fork` `None` and emits nothing.

### The `ToolCallContext` seam

Agent-aware tools reach the calling Agent's live state through an additive
`Tool::call_with_context(input, ctx)` path (`src/tool.rs`), which defaults to
forwarding to `call`, so context-free tools need no changes. The Agent
assembles a `ToolCallContext` at each dispatch, exposing the caller's identity,
current `History` head, `SystemPrompt`, model assignment, tool set, an
`AgentSpawner` for creating peers, and a self-stop child token (`stop()`).
Agents built outside a running Harness receive a `DisconnectedSpawner` whose
`spawn` returns a clear error rather than panicking.

### Fork and Exit built-in tools

Both are config-gated: registered only when the topology declares `[tools.fork]`
/ `[tools.exit]`, through the builtin dispatch table in `src/lib.rs`.

`Fork` (`src/builtins/fork.rs`) takes `{instruction, handle?, tools?}`. Its
`call` rejects invocation without a context; `call_with_context` delegates to
the shared `fork_from_context` helper, which resolves the handle against the
caller's head, narrows the tool set to the named subset (unknown names error and
spawn nothing), inherits the parent's system prompt and `Default` model, seeds
`instruction` as a `Block::UserMessage`, and spawns via the context's spawner.
It returns `{agent_id, handle}`, where `handle` is the boundary the fork
inherited up to.

`Exit` (`src/builtins/exit.rs`) takes no parameters. `call_with_context` calls
`ctx.stop()`, cancelling the calling Agent's per-agent child token so only that
Agent terminates — siblings and the harness keep running — and returns
`{status: "exiting"}`.

### The human path

Humans do not spawn peers directly; a fork is routed **through** the target
Agent so it resolves against that Agent's own live context, exactly as a
tool-initiated fork does. The `fork_agent` JSON-RPC method (`src/rpc.rs`)
translates its params into a `Control::Fork(ForkRequest)` and delivers it on the
target Agent's out-of-band control stream (`send_control`,
`src/messagebus.rs`) — separate from the `Block` inbound stream, so it drives no
model turn for the request itself. The Agent's run loop merges control and block
inbounds into one `Inbound` stream and dispatches `Control::Fork` to
`handle_control`, which builds a `ToolCallContext` from its live fields and calls
the same `fork_from_context` helper; an invalid request surfaces as an
`AgentEvent::Error`.

Whichever path forks, the `Nursery` publishes a single `AgentForked` on the
bus-wide fork broadcast. The stdio transport subscribes to that broadcast
(`subscribe_forks`) and surfaces each as an `agent_forked` notification
(`AgentForkedNotification`, `{agent_id, origin, handle}`), so the human path's
`fork_agent` call — which returns only an acknowledgement — learns the new
peer's id asynchronously.

## Configuration

The Harness setup that drives `pristine run` is produced from declarative
configuration rather than hardcoded in the binary. The configuration layer
lives in `src/config/` (with the `src/config.rs` module root holding the
`Config` value, `assemble_config`, and the `load` / `load_with` orchestrators)
and is the single boundary between on-disk artefacts and the engine. The
engine itself — `Harness`, `HarnessBuilder`, `Agent`, `ARModel`, `Tool`,
`MessageBus` — never touches TOML, file paths, or environment variables.

### Two-file model

Configuration is split across two orthogonal files that own two orthogonal
concerns:

- **Topology** — an embedded `default.toml` (overridable per invocation via
  `-c/--config`) describing the agents to run, the tools each agent holds,
  the system prompts, and the model alias each agent uses. Topology is
  provider-agnostic; nothing in a topology file names Anthropic or any other
  specific provider.
- **Identity** — a user-global `pristine-auth.toml` (default
  `~/pristine-auth.toml`, overridable via `--auth`) declaring the available
  providers, model aliases, and credentials. The auth file is the only place
  API keys live.

The split is load-bearing. A user can keep many topologies — a coding-assistant
shape, a game-simulator shape, a critic/actor pair — and share a single auth
file across all of them. One identity, many topologies. Topology files can be
checked into version control; the auth file cannot. The embedded
`default.toml` is documented under "Embedded default topology" in §Key
Technological Choices, including the `include_str!` build-time coupling.

### Parse-don't-validate boundary

`pristine_config::load(args)` returns either `Ok(Config)` or
`Err(ConfigErrors)`. A successful `Config` is an inert, fully-resolved data
struct: agents carry pre-resolved `ResolvedModel` values, tool declarations
have been validated against their references, and credentials have already
been substituted in via `{{ENV_VAR}}` templating. Once `load` succeeds, no
further validation is required — every consumer can trust the contents.

The binary's `run_async` walks the returned `Config` and issues
`HarnessBuilder` calls: `add_provider` for each provider, `add_tool` for each
declared tool, `add_model` for each resolved alias, and `add_agent` for each
agent. Nothing else translates the file into the engine. This keeps the engine
free of any configuration concerns and makes the contract between layers
exactly one type wide.

### `ModelProvider` and `ProviderRegistry`

`ModelProvider` (in `src/provider.rs`) is the runtime analogue of `Tool`. It
exposes one method, `build_model(ModelInstanceConfig) -> Result<Arc<dyn
ARModel>, ProviderError>`. The `ModelInstanceConfig` carries a typed
`model_name` plus an opaque `extras: serde_json::Value` carrier so the trait
never names provider-specific fields. Provider-specific dialect — Anthropic's
`base_url`, future providers' organisation IDs, project tags, region hints —
lives only inside the provider impl and the matching `[providers.X]` section
in the auth file. The trait surface remains provider-agnostic.

`ProviderRegistry` is a name-keyed `HashMap<String, Arc<dyn ModelProvider>>`,
parallel to `ToolRegistry`. `HarnessBuilder::add_provider` rejects duplicate
names with `ProviderError::DuplicateProvider`. The binary registers
`AnthropicProvider` (`anthropic`) and `DeepSeekProvider` (`deepseek`); both
plugged in at this seam without changes elsewhere. See "DeepSeek adapter" under
§Model.

### `{{ENV_VAR}}` templating

Credentials and other environment-sourced values are referenced inside the
auth file as `{{NAME}}` placeholders. Templating walks the parsed
`toml::Value` tree after the initial TOML lex but before final
deserialization into the typed structs: every string node is scanned, every
`{{NAME}}` placeholder is substituted with the corresponding env-var value,
and the resulting tree is deserialized into `AuthConfig` /
`TopologyConfig`. Missing variables do not abort the walk — each becomes a
`ConfigError::UnknownEnvVar { name, location }` recorded against the TOML
key path so the eventual diagnostic points at the right line.

The env lookup is abstracted behind the `EnvSource` trait
(`src/config/template.rs`). The production impl, `ProcessEnv`, defers to
`std::env::var`. Tests use an in-memory map. This is the only place in the
crate that touches the process environment for configuration; the binary no
longer reads `ANTHROPIC_API_KEY` directly.

### Error model

`ConfigErrors` is a `Vec`-backed aggregate of `ConfigError` values. Every
parse, templating, resolution, and validation pass that runs during one call
to `assemble_config` or `load_with` pushes its failures into this aggregate;
the call returns `Err(ConfigErrors)` only after every recoverable pass has had
a chance to run. Hard short-circuits are limited to cases where no input is
available at all (e.g. an unreadable auth file path).

The `Display` impl renders every contained error in registration order, with
file path plus line/column information when the underlying error carries it
(notably for `TomlParse`). The full list of failure modes lives on the
`ConfigError` enum (`src/config/error.rs`): `TomlParse`, `UnknownEnvVar`,
`DanglingAlias`, `UndeclaredTool`, `DuplicateToolRef`,
`UnknownProvider`, `IoError`, and `MissingHome`. Unknown TOML keys are
rejected by `#[serde(deny_unknown_fields)]` on every typed struct and surface
through `TomlParse`. No configuration failure is silent.

## Skills

Skills are filesystem-authored capabilities the agent discovers and the model
activates on demand. A skill is a directory containing a `SKILL.md` with YAML
frontmatter (`name`, `description`, plus optional fields) and a Markdown body.
The engine surfaces discovery outcomes as structured JSON-RPC notifications and
never decides display; the model reads the catalog from its system prompt and
pulls a skill's body in by calling a tool. The feature tracks the Agent Skills
open standard at <https://agentskills.io>.

### `SystemPrompt` and the slot model

The agent's system prompt is not a flat `String` inside the engine. `Agent`
holds a `SystemPrompt` struct (`src/agent.rs`) with named-field slots: a fixed
`base: String` and a dynamic skills slot, `skills: Option<Arc<dyn
SkillsRegistrySource>>`. `SystemPrompt::render() -> String` concatenates the
base text with a `## Available skills` Markdown section — one `**name**:
description` bullet per discovered skill, closing with a pointer at
`activate_skill` — but only when the slot is `Some` and its `list()` is
non-empty. With no skills configured the slot is `None` and `render()` returns
`base` unchanged, so out-of-the-box behavior is preserved bit-for-bit.

The agent loop calls `render()` once per iteration when it builds the system
`Turn`, so any catalog growth between turns is picked up without rebuilding the
`Agent`. TOML and the `Config` types continue to carry `system_prompt: String`;
the structured `SystemPrompt` is assembled at agent-build time inside
`build_harness_from_config`, which wires the shared registry into every agent's
`skills` slot.

### `SkillsRegistry` / `SkillsRegistrySource` seam

The skills catalog follows the same concrete-type-plus-trait shape as
`ToolRegistry`/`Tool` and `ProviderRegistry`/`ModelProvider`. The trait
`SkillsRegistrySource` (`src/skills/source.rs`) is the read-only abstraction the
`SystemPrompt` slot and the `activate_skill` tool resolve against; it is
synchronous and side-effect-free from the caller's perspective:

- `list() -> Vec<SkillSummary>` powers tier-1 disclosure in the system prompt.
- `get(name) -> Option<SkillRecord>` resolves a single skill for activation.
- `summarize() -> Vec<SkillSummary>` and `diagnostics() -> Vec<SkillDiagnostic>`
  are trait-default methods feeding the two notifications (below).

`SkillsRegistry` (`src/skills/registry.rs`) is the engine-owned concrete
implementor. The filesystem registry ships in v1; the trait is the seam for
future implementors. A `StubSkillsRegistry` in `src/test_support.rs` implements
the trait over an in-memory `Vec<SkillRecord>` for tests that exercise rendering
or activation without touching the disk.

### Lazy-strict discovery contract

Discovery is lazy-strict. `SkillsRegistry` is constructed empty; the first call
to `list()` or `get()` triggers `FilesystemSkillsRegistry::scan` exactly once,
guarded by an `OnceLock<ScanResult>`. The scan resolves the configured paths
(`~` expansion and cwd-relative joins happen at scan time, not config-parse
time), walks each one level deep for skill directories, parses each `SKILL.md`,
applies shadowing precedence (later path within a scope wins; project shadows
user across scopes), and filters out `disabled` names. The `ScanResult` caches
both the surviving catalog and every diagnostic accumulated during the scan, so
the harness can drain diagnostics for the notification surface without a second
scan or a constructor change.

The `[skills]` TOML block is the kill-switch: block-present means enabled,
`enabled = false` disables. With no block, `Config::skills` is the default
(disabled) value, no registry is constructed, the slot stays `None`, and a
declared `activate_skill` fails to construct. The embedded `default.toml` stays
skills-free, so default behavior is unchanged.

### JSON-RPC notifications

Two session-level notifications fire once, immediately after the first agent
turn triggers discovery (`src/rpc.rs`, emitted via `SkillsAnnouncer` in
`src/harness.rs`):

- `skills_loaded` — the catalog `{ skills: [{ name, description }, ...] }`.
  Emitted only when the catalog is non-empty.
- `skills_diagnostics` — an array of kind-tagged entries (`shadowed`,
  `malformed_yaml`, `name_mismatch`, `description_missing`, `bypassed_path`,
  `resolution_failure`). Emitted only when at least one diagnostic exists.

Both are one-shot: a client attaching after startup misses them, which is
acceptable for the single-client stdio transport.

### `activate_skill` tool

`ActivateSkill` (`src/builtins/activate_skill.rs`) is just-another builtin with a
static name (`"activate_skill"`), a static description (it does not enumerate
skills — the system-prompt section is the sole tier-1 disclosure surface), and a
static input schema `{ name: string }`. On a hit it returns `{ body: "..." }`
with frontmatter stripped; on an unknown name it returns a `ToolError::Execution`
carrying `{ kind: "unknown_skill", name, known: [...] }` so the model can retry.

Registration is config-gated through the builtin dispatch table in `src/lib.rs`.
`BuiltinContext` carries `Option<Arc<dyn SkillsRegistrySource>>`; the
`activate_skill` constructor closure registers the tool iff `[tools.activate_skill]`
is declared *and* the registry is present. A `[tools.activate_skill]` declared
without a `[skills]` block surfaces as a harness-assembly error rather than a
silent no-op.

### `--trust-project-skills` CLI plumbing

Project-scope discovery is gated behind a per-invocation `--trust-project-skills`
flag; persistent per-project trust is deferred. The flag threads end to end:

- `src/lib.rs`: a global boolean `--trust-project-skills` on `Cli` (defaults
  false). `run_async` passes `cli.trust_project_skills` into
  `build_harness_from_config`, which forwards it as `SkillsRegistry::new(config,
  trust_project)`.
- Without trust, `FilesystemSkillsRegistry::scan` skips every project-scope path
  and records each as a `bypassed_path` diagnostic; user-scope discovery is
  unaffected.
- `client.py`: an argparse `--trust-project-skills` (`action="store_true"`,
  defaulting false) is forwarded to the binary only when passed, gated on a
  plain truthiness check.
- `justfile`: `chat *args` already forwards arbitrary arguments via `{{args}}`,
  so the flag passes through `just chat` with no justfile change.

## Key Technological Choices

- Tokio
- Command line parsing with Clap
- `jsonrpsee` 0.26 (`server-core`, `macros`) for JSON-RPC dispatch

### Embedded default topology

The canonical fallback topology lives at `default.toml` in the repository root
and is embedded into the binary at compile time via
`include_str!("../default.toml")` in `src/config.rs`. This is the topology
served when `pristine run` is invoked without a `-c/--config` override. Moving
or renaming `default.toml` will break the build unless the `include_str!` path
in `src/config.rs` is updated in lockstep.

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
