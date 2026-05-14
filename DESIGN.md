# DESIGN

Pristine is an SDK-first, configurable agent harness engine written in Rust. It is not an agent, a UI, or a product -- it is the core that agents, UIs, and products are built on. The goal is to enable experimentation and exploration in harness design by providing composable, swappable primitives for every layer of the agent stack: models, history, context compilation, tool execution, inter-agent routing, and client protocols.

Pristine ships as a library crate and a thin binary. The library exposes a `HarnessBuilder` API so that any Rust program can construct, configure, and drive an agent harness programmatically. The binary (`pristine run`) is a reference client that wraps the library behind a JSON-RPC stdio server. This SDK-first posture means there is no canonical UI; Pristine is designed to be embedded and wrapped, not run directly by end users.

The engine is deliberately model-agnostic and model-plural. An Agent references its models through a `ModelRole` indirection, and the harness can host multiple agents -- each with distinct model assignments, prompts, tools, and skills -- within a single process. The `MessageBus` trait abstracts all inter-agent and agent-to-client routing so that the same agent code runs unchanged whether the bus is in-memory, cross-process, or distributed.

Configuration is the primary user interface. Rather than hard-coding agent topologies, Pristine will support declarative configuration files that describe which agents to run, how they are wired together, and what capabilities each has. The builder API remains the programmatic escape hatch for cases configuration cannot express.

## Completed

**Initial Build** -- Validated the core engine design with a hardcoded two-message demo exercising the Harness, Agent, History, Model, and MessageBus primitives. See ARCHITECTURE.md Phase 1.

**JSON-RPC Server** -- Replaced the hardcoded demo with a JSON-RPC stdio server. `pristine run` reads newline-delimited JSON-RPC from stdin and writes responses and notifications to stdout. The Phase 1 demo flow is preserved as a Python client script. See ARCHITECTURE.md Phase 2 and Client Protocol.

## Roadmap

### 1. Tool Calls

Enable Agents to make tool calls during their run loop. Currently the Agent skips `ToolCall` and `ToolResult` blocks when compiling History into `ModelInput` (see ARCHITECTURE.md History, Agent). This phase adds a tool registry to the Harness, wires tool execution into the Agent's event loop so that model-requested tool calls are dispatched and their results appended to History, and extends the `ModelInput`/`ARModel` contract to carry tool-call requests and results.

### 2. Configuration File

Replace the hardcoded `HarnessBuilder` setup with file-driven configuration (see ARCHITECTURE.md Harness). A configuration file will control how many Agents to run, their system prompts, which tools are available to each Agent, model assignments via `ModelRole`, and inter-agent routing rules. The builder remains available for programmatic use; the config file is sugar that produces the same builder calls.

### 3. Multi-Model Support

Implement the `ARModel` trait for at least one provider beyond Anthropic (see ARCHITECTURE.md Model). This validates that the trait abstraction is genuinely provider-agnostic and that `ModelInput`/`Turn`/`ContentPart` are sufficient to represent another provider's request format. The `ModelRole` indirection on Agent (see ARCHITECTURE.md Agent) should require no changes.

### 4. Skills

Support Skills as a higher-level abstraction over tool calls and prompt injection. A Skill is a composable capability that can be attached to an Agent, bundling one or more tools with associated prompt fragments and lifecycle hooks. Skills allow reusable agent capabilities to be packaged, shared, and composed without requiring callers to manually wire individual tools and prompts.

### 5. Multi-Agent Routing

Enable the MessageBus `route(from, to)` method for inter-agent communication (see ARCHITECTURE.md MessageBus). When a route is established, completed `AgentMessage` blocks from one Agent's outbound stream are forwarded to another Agent's inbound stream. Receiving Agents see these as `AgentMessage { from }` blocks in their History, enabling peer-to-peer dialogue. This unlocks the multi-agent scenarios described in the project goals.

### 6. ContextCompiler

Extract History-to-`ModelInput` compilation into a pluggable `ContextCompiler` trait (see ARCHITECTURE.md Model, History). The Agent currently linearizes its History directly; this phase moves that logic behind a trait so that selective summarization, vector-store lookups, and sequence-to-sequence context compilation can be plugged in without changing Agent or Model. The `ModelInput` shape remains the stable contract between compiler and model.

### 7. DLModel

Implement the `DLModel` trait for diffusion language models (see ARCHITECTURE.md Model). Where `ARModel` produces sequential token streams via next-token prediction, `DLModel` enables masked-text denoising -- filling in blanked regions of an input in parallel. This opens the door to hybrid agent strategies that combine auto-regressive completion with diffusion-based refinement.

## Error Handling and Observability

## Persistence

History is currently in-memory and does not survive process restarts. A persistence layer will allow History to be durably stored and recovered, enabling long-lived agent sessions, crash recovery, and offline inspection of agent traces. The persistence boundary should sit behind a trait so that storage backends (local file, SQLite, remote store) can be swapped without changing the Agent or Harness.
