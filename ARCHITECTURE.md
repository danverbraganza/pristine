# ARCHITECTURE

## Goals

The goal for Pristine is to create a robust, simple and watertight "engine" for agent harnesses. Completing this work
and providing most of the plumbing for Agent Harnesses will unlock experimentation with different information
architecture and agent harness designs. The design work of representing the fundamental primitives of agents, therefore, is the most important work for Pristine.

- Integration with multiple language models, including open langauge models and local model providers
- Consistent and extensible modelling of the problem
- Support for multiple parallel agents running within one process
- Agents support forking, creating subagents, speculative task evaluation
- Support for peer message sending between Agents
- Programmable interface for interacting/driving the harness from other programs
- Robustly archictected for maintainability with Single Responsibility Principle, Encapsulation and Information Hiding
- Logging
- Tracing

### Examples
Here are some motivating examples of the kinds of projects I want to build with Pristine:

- A simple coding agent that works with open models, and is able to use tool-calls to edit projects
- A recursive AR-LLM integrated with a diffusion model to continuously tailor its history/context to the task at hand
- A multi-player game with multiple agents, each with their own private reasoning traces, who are able to interact with each other and with the player

### Non-goals
- UI/TUI: Pristine will not ship with a canonical UI, since it is meant to be wrapped by other applications. We will
  have a small helper UI for development purposes, but it will not be intended as the main way to use Pristine.

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

In Phase 1 the Agent constructs `ModelInput` by linearizing its History and skipping variants the model cannot consume
(e.g. `ReasoningTrace`, `ToolCall`, `ToolResult`). In future phases this construction will be extracted into a
`ContextCompiler` trait so that selective summarization, vector-store lookups, and sequence-to-sequence "context
compilation" can be plugged in without changing the Agent or the Model trait. The `ModelInput` shape is the stable
contract between compiler and model.


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

## Key Technological Choices

- Tokio
- Command line parsing with Clap



## Initial Build

To validate this design, we will initially attempt to implement 1p with the following characteristics. Code is indicative only, not proper systax or naming:

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
