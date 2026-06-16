# DESIGN

Pristine is an SDK-first, configurable agent harness engine written in Rust. It is not an agent, a UI, or a product -- it is the core that agents, UIs, and products are built on. The goal is to enable experimentation and exploration in harness design by providing composable, swappable primitives for every layer of the agent stack: models, history, context compilation, tool execution, inter-agent routing, and client protocols.

Pristine ships as a library crate and a thin binary. The library exposes a `HarnessBuilder` API so that any Rust program can construct, configure, and drive an agent harness programmatically. The binary (`pristine run`) is a reference client that wraps the library behind a JSON-RPC stdio server. This SDK-first posture means there is no canonical UI; Pristine is designed to be embedded and wrapped, not run directly by end users.

The engine is deliberately model-agnostic and model-plural. An Agent references its models through a `ModelRole` indirection, and the harness can host multiple agents -- each with distinct model assignments, prompts, tools, and skills -- within a single process. The `MessageBus` trait abstracts all inter-agent and agent-to-client routing so that the same agent code runs unchanged whether the bus is in-memory, cross-process, or distributed.

Configuration is the primary user interface. Rather than hard-coding agent topologies, Pristine will support declarative configuration files that describe which agents to run, how they are wired together, and what capabilities each has. The builder API remains the programmatic escape hatch for cases configuration cannot express.

## Completed

**Initial Build** -- Validated the core engine design with a hardcoded two-message demo exercising the Harness, Agent, History, Model, and MessageBus primitives. See ARCHITECTURE.md Phase 1.

**JSON-RPC Server** -- Replaced the hardcoded demo with a JSON-RPC stdio server. `pristine run` reads newline-delimited JSON-RPC from stdin and writes responses and notifications to stdout. The Phase 1 demo flow is preserved as a Python client script. See ARCHITECTURE.md Phase 2 and Client Protocol.

**Tool Calls** -- Agents can invoke tools registered with the Harness. Defined the `Tool` trait and a `ToolRegistry`; extended the `ModelInput` contract with `tools: Vec<ToolSpec>` and `ContentPart::ToolUse`/`ToolResult`; extended `ModelStreamEvent` with streaming `ToolUseStart`/`Delta`/`Complete`; reconciled `Block::ToolCall`/`ToolResult` with correlation IDs; refactored the Anthropic adapter to serialize tools + tool content blocks and parse tool_use streaming events; wired the registry from `HarnessBuilder` through `Harness` to each `Agent`; replaced the History-to-ContentPart skip with real emission; added a tool-call cycle to `Agent::run` that dispatches tools and re-enters the model loop until the model returns no tool calls; surfaced an `AgentEvent::Idle` signal once per inbound block; expanded the JSON-RPC `block_complete` notification payload; shipped a built-in `add` tool. See ARCHITECTURE.md Tool Calls.

**Built-in Tools** -- The harness ships five built-in tools that give the agent direct filesystem and shell capabilities: `Read`, `Write`, `Edit`, `Insert`, and `ExecBash`. Each lives in its own submodule under `src/builtins/` with co-located tests, owns its own typed error enum (the dialect), and emits errors through the shared `ToolError::Execution(serde_json::Value)` carrier (the portable shape). ExecBash uses a `Shell` trait for testability (`BashShell` real impl plus `StubShell` test fixture) and stages full stdout/stderr to per-process tmp files. The tools are registered in `HarnessBuilder` alongside the existing `AddTool` demo. See ARCHITECTURE.md "Built-in Tools".

**Configuration File** -- The hardcoded `HarnessBuilder` setup is replaced with file-driven configuration produced by a new `pristine-config` module. Two orthogonal files own two orthogonal concerns: an embedded `default.toml` carries topology (agents, tools, system prompts) and a user-global `~/pristine-auth.toml` carries identity (providers, model aliases, credentials). Topology files are provider-agnostic; a single auth file is shared across many topologies. `pristine-config` exposes `assemble_config` plus `load`/`load_with` orchestrators that read both files, apply `{{ENV_VAR}}` templating, deserialize into typed structs with `deny_unknown_fields`, resolve model aliases, and validate tool references. `ModelProvider` is now a noun in the type system -- a trait plus `ProviderRegistry` parallel to `Tool`/`ToolRegistry` -- with `AnthropicProvider` as the v1 impl. Loading follows parse-don't-validate: every parse, templating, and resolution failure is collected into a `ConfigErrors` aggregate and surfaced before exit, so a successful `Config` is an inert, trustworthy data struct. When `~/pristine-auth.toml` is missing the binary auto-writes a template (chmod `0o600`) and continues. The CLI gains global `-c/--config` and `--auth` flags; `--model` is removed. Engine code (`Harness`, `Agent`, `ARModel`, `Tool`, `MessageBus`) is untouched -- the split lives entirely at the `pristine-config` boundary. Multi-Model support (roadmap item 1) is the natural next piece, since `ProviderRegistry` is now shaped for it. See ARCHITECTURE.md Configuration.

**Multi-Model Support** -- Implemented the `ARModel` trait for DeepSeek V4 (`deepseek-v4-pro` / `deepseek-v4-flash`), the first provider beyond Anthropic, validating the `ARModel`/`ModelProvider` abstraction against a non-Anthropic dialect. DeepSeek speaks the OpenAI-compatible ChatCompletions wire format -- a genuinely different dialect from Anthropic's: the system prompt is a `system`-role message rather than a top-level field, tool calls carry stringified `function.arguments` with the schema nested under `function.parameters`, and tool results are separate `role: "tool"` messages. The provider plugged in at the `ProviderRegistry` seam with zero core changes; `ModelInput`/`Turn`/`ContentPart` and the `ModelStreamEvent` reasoning surface proved sufficient as-is, and the `ModelRole` indirection on Agent was untouched. Two portable-layer observations were surfaced for later (a third provider) rather than acted on at one example: `ContentPart::ToolResult.is_error` is an Anthropic-ism the adapter folds into content text, and OpenAI's separate-message tool results require the adapter to explode one `Turn` into N wire messages. See ARCHITECTURE.md "DeepSeek adapter" and "Findings: the provider seam held". A follow-on added OpenRouter as a second OpenAI-dialect provider; the shared `openai_dialect` module now backs both DeepSeek and OpenRouter, validating the `ProviderRegistry` seam a second time with zero core changes (see ARCHITECTURE.md "OpenRouter adapter").

## Roadmap

### 1. Skills

Support Skills as a higher-level abstraction over tool calls and prompt injection. A Skill is a composable capability that can be attached to an Agent, bundling one or more tools with associated prompt fragments and lifecycle hooks. Skills allow reusable agent capabilities to be packaged, shared, and composed without requiring callers to manually wire individual tools and prompts.

### 2. Multi-Agent Routing

Enable the MessageBus `route(from, to)` method for inter-agent communication (see ARCHITECTURE.md MessageBus). When a route is established, completed `AgentMessage` blocks from one Agent's outbound stream are forwarded to another Agent's inbound stream. Receiving Agents see these as `AgentMessage { from }` blocks in their History, enabling peer-to-peer dialogue. This unlocks the multi-agent scenarios described in the project goals.

### 3. ContextCompiler

Extract History-to-`ModelInput` compilation into a pluggable `ContextCompiler` trait (see ARCHITECTURE.md Model, History). The Agent currently linearizes its History directly; this phase moves that logic behind a trait so that selective summarization, vector-store lookups, and sequence-to-sequence context compilation can be plugged in without changing Agent or Model. The `ModelInput` shape remains the stable contract between compiler and model.

### 4. DLModel

Implement the `DLModel` trait for diffusion language models (see ARCHITECTURE.md Model). Where `ARModel` produces sequential token streams via next-token prediction, `DLModel` enables masked-text denoising -- filling in blanked regions of an input in parallel. This opens the door to hybrid agent strategies that combine auto-regressive completion with diffusion-based refinement.

## Error Handling and Observability

## Persistence

History is currently in-memory and does not survive process restarts. A persistence layer will allow History to be durably stored and recovered, enabling long-lived agent sessions, crash recovery, and offline inspection of agent traces. The persistence boundary should sit behind a trait so that storage backends (local file, SQLite, remote store) can be swapped without changing the Agent or Harness.
