# DESIGN

Pristine is an SDK-first, configurable agent harness engine, written in Rust. It is not an agent, a UI, or a product; it is the core that agents, UIs, and products are built on. The goal is to make harness design a place where experimentation is cheap, by exposing composable, swappable primitives at every layer of the stack: models, history, context compilation, tool execution, inter-agent routing, and client protocols.

The engine ships as a library crate and a thin binary. The library exposes a `HarnessBuilder` so that any Rust program can construct, configure, and drive a harness programmatically. The binary (`pristine run`) is a reference client that wraps the library behind a JSON-RPC stdio server. There is no canonical UI. Pristine is meant to be embedded and wrapped, not run directly by end users.

Pristine is model-agnostic and model-plural. Agents reference their models through a `ModelRole` indirection, and a single process can host many agents at once, each with its own model assignment, prompt, tools, and skills. The `MessageBus` trait abstracts all inter-agent and agent-to-client routing, so the same agent code runs unchanged whether the bus is in-memory, cross-process, or distributed.

Configuration, not code, is how users assemble a harness. Topology — which agents run, how they are wired together, what each can do — is declarative. The builder API is the escape hatch for the cases configuration cannot express, and is expected to be the minority case.

## Design Principles

These are the rules the engine tries to keep. The mechanisms that implement them live in [ARCHITECTURE.md](ARCHITECTURE.md); this section is the philosophy.

**Portable shape, adapter dialect.** The portable layer — the engine, its traits, and the types agents and clients touch — carries only what every targeted provider, model, or client requires. Provider dialect (wire formats, naming conventions, optional fields, error vocabularies) lives in adapters at the periphery. The portable layer does not grow on speculation; a capability is promoted from adapter-specific to portable only when it appears in a majority of providers, with at least two concrete examples in view. Adding the third provider should not require changing the second. The same shape applies to errors: portable types carry a typed dialect payload (`ToolError::Execution(Value)` wrapping a tool-private error enum), so each component speaks its own vocabulary without leaking it upward.

**Separation of concerns at every boundary.** System prompts carry identity and behavior. Tools carry behavior and self-description. Skills bundle tools with prompt fragments and ship on the filesystem. Adapters carry transport and dialect. Clients carry presentation. Each layer owns what it owns and nothing else, and the seams between them are traits.

**Concrete storage, trait-shaped seam.** Every pluggable subsystem follows the same shape: a concrete type owned by the engine that holds the storage, and a trait that is the read-only abstraction the rest of the engine resolves against. `Tool`/`ToolRegistry`, `ModelProvider`/`ProviderRegistry`, and `SkillsRegistrySource`/`SkillsRegistry` are the three current instances. The engine never names a concrete implementor outside the registry's own module; consumers see only the trait. This is what makes "swap one provider for another" a config change rather than a code change.

**Configuration over code.** A new topology should be a TOML file, not a recompile. Two orthogonal files own two orthogonal concerns: a topology file (agents, tools, prompts) and a per-user auth file (providers, credentials, model aliases). Topology is provider-agnostic and shareable; identity is private and per-user.

**Parse, don't validate.** Configuration is parsed once, all errors are collected into a single aggregate, and the resulting `Config` is an inert, trustworthy data structure. The engine never re-checks invariants the loader has already established. A successful load means the harness can be built.

**Composability is a property of the traits.** Every major subsystem — Model, History, MessageBus, ContextCompiler, Tool, ModelProvider, SkillsRegistry — is a trait with at least one concrete implementation and at least one test double. New implementations plug in at the seam without changing consumer code, and tests do not require live providers, real shells, or a filesystem.

## Roadmap

Items are roughly ordered. Each lands as its own design pass in `ARCHITECTURE.md`.

### 1. Multi-Agent Routing

Enable `MessageBus::route(from, to)` so completed `AgentMessage` blocks from one agent's outbound stream are forwarded to another agent's inbound stream. Receiving agents see these as `AgentMessage { from }` blocks in their history, which is what makes peer-to-peer dialogue work. This is the piece that unlocks the multi-agent scenarios the project was built for. See [ARCHITECTURE.md MessageBus](ARCHITECTURE.md#messagebus).

### 2. ContextCompiler

Extract history-to-`ModelInput` compilation into a pluggable `ContextCompiler` trait. The agent currently linearizes its history directly; moving that behind a trait lets selective summarization, vector-store lookups, and sequence-to-sequence compilation drop in without touching the agent or the model. `ModelInput` remains the stable contract between compiler and model. See [ARCHITECTURE.md Model](ARCHITECTURE.md#model), [History](ARCHITECTURE.md#history).

### 3. Persistence

History is in-memory and does not survive a process restart, which rules out long sessions, crash recovery, and offline inspection of agent traces. Persistence sits behind a trait so storage backends (local file, SQLite, remote store) swap without changing the agent or harness. The wire format for stored history should be the same `ModelInput`-adjacent shape the `ContextCompiler` consumes, so the two roadmap items compose cleanly.

### 4. DLModel

Implement a `DLModel` trait for diffusion language models. Where `ARModel` produces sequential token streams via next-token prediction, `DLModel` denoises masked regions of an input in parallel. The point is hybrid strategies that combine auto-regressive completion with diffusion-based refinement; the engine should be able to host both without either knowing about the other. See [ARCHITECTURE.md Model](ARCHITECTURE.md#model).
