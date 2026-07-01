# Agent Forking

Support forking of the current agent, for both agents and humans, via a built-in Fork tool. Agents (and humans)
control how much prior context a fork inherits, sliding from a full-context fork to a true subagent.

## Overview

- **Forks are full peer agents.** A fork is a full agent running in the harness — user-addressable, independently
  running, and fire-and-forget from the parent's perspective (no value flows back into the parent's loop). It inherits
  every aspect of its parent (system prompt, model, tool set) except where explicitly narrowed.
- **Context is controlled by a single `Handle` parameter — a continuous slider.** A handle names a point in the
  parent's history; the fork inherits the prefix up to and including it. There is no side-channel "amount of context"
  flag: the handle *is* the control.
- **Handles are checkpoint identifiers derived from the immutable `NodeId`.** The harness makes each tool-call boundary
  addressable by rendering its `NodeId` into the model-visible stream, so an agent can name a past point in time. This
  checkpoint-handle mechanism is built as part of this work.
- **The empty end of the slider is the reserved genesis handle.** `NodeId::nil()` is an always-available checkpoint
  handle that resolves to the empty prefix (a virtual sentinel — no physical root node is stored). Forking there
  inherits nothing (a pure subagent); omitting the handle inherits everything.
- **Forking reuses the engine's established seam pattern, not a new mechanism.** Runtime agent spawning is exposed
  behind an `AgentSpawner` trait (concrete type owned by the Harness, trait as the read-only abstraction) and threaded
  to the Fork tool through `BuiltinContext` — mirroring `ToolRegistry`/`Tool`, `SkillsRegistry`/`SkillsRegistrySource`,
  and how `activate_skill` receives its registry.

## Expected behavior

### Checkpoint handles
- Every tool result the model sees now carries a short, harness-attributed checkpoint handle for that tool-call
  boundary, derived from the boundary's `NodeId`.
- The handle is stable for the life of the (uncompacted) node; once a block is compacted its handle is no longer usable.
- The genesis handle (`NodeId::nil()`) is always available and always denotes the empty prefix; it is the only
  handle that is not a tool-call boundary.

### The Fork tool (agent-initiated)
- An agent calls Fork with an `Instruction`, an optional `Handle`, and an optional `Tools` subset.
- **Handle behavior:**
  - Omitted → the fork inherits the **full** prior context (equivalent to the current head).
  - `NodeId::nil()` (genesis) → the fork inherits **nothing** (a pure subagent).
  - A tool-call boundary handle → the fork inherits the prefix **up to and including** that boundary.
  - An invalid handle (unknown, non-boundary, or compacted-away) → the call **fails with an error**; no agent is spawned.
- `Instruction` is the fork's immediate next node in its cycle — seeded as the first message it processes — **not** its
  system prompt.
- `Tools`, if present, narrows the inherited tool set; if absent, the fork inherits all of the parent's tools.
- The fork otherwise inherits all aspects of its parent: system prompt and model assignment.
- Fork returns to the caller the new peer agent's `AgentId` and the handle it forked from.

### Forked agents
- A fork is a full peer agent: it can receive user input, produce user output, and call any tool it holds.
- Like the root agent, a fork persists until harness shutdown, idling between instructions.
- A fork that holds the `exit()` tool can terminate itself; after exit it no longer processes inbound messages.
- Because a fork inherits its parent's tools by default, it can itself fork again and (if it holds `exit()`) exit.

### Human-initiated forking
- A human (the owner) can initiate a fork over JSON-RPC with the same parameters as the tool: target agent,
  instruction, optional handle, optional tools subset.
- When any fork is created, the harness announces it so clients learn the new agent's identity and can address it like
  any other agent.

### Interaction with existing behavior
- Fork and `exit()` are opt-in per agent via topology, exactly like the existing built-in tools — an agent without them
  declared cannot fork or exit, and default behavior is unchanged.
- Handle rendering is additive on tool results; agents without any forking configured simply see the extra
  harness-attributed handle line and are otherwise unaffected.
- Per-agent fault isolation still holds: a fork failing does not cascade to its parent or siblings.

## Changes

### History & checkpoint handles
- Introduce a reserved **genesis handle** `NodeId::nil()`: an always-available checkpoint handle that resolves to the
  empty prefix (a virtual sentinel; no physical root node is stored).
- Make `NodeId` addressable as a **handle**: give it a stable string form and a reserved nil/genesis value, and a way
  to resolve a handle back to its history node.
- Render the checkpoint handle for each tool-call boundary into the model-visible content of the corresponding tool
  result, as harness-attributed text (the same class of injection as the system prompt).

### The Fork tool
- Add a config-gated **Fork** built-in tool (declared per-agent in topology, like the existing five and
  `activate_skill`), with parameters: `instruction`, optional `handle`, optional `tools`.
- Resolve the handle to a history prefix (omitted → full, genesis → empty, boundary → that prefix), rejecting invalid
  handles as an error.
- Build the forked agent from the resolved prefix plus the parent's system prompt, model, and tool set (narrowed by
  `tools`), seeding `instruction` as the fork's first inbound message.
- Return the new `AgentId` and the forked-from handle.

### The Exit tool
- Add a config-gated **Exit** built-in tool (no arguments) that terminates the calling agent so it stops processing
  inbound messages.

### Runtime-spawn seam
- Factor the per-agent spawn logic currently inside `Harness::start()` into a reusable spawn method usable after
  startup.
- Expose runtime spawning behind an **`AgentSpawner`** (Nursery) trait — concrete type owned by the Harness, trait as
  the read-only abstraction — matching the engine's concrete-storage/trait-seam pattern.
- Thread `Arc<dyn AgentSpawner>` to the Fork tool via `BuiltinContext`, as the skills source is threaded to
  `activate_skill`. Spawned forks register with the shared `MessageBus` and `ToolRegistry` like any agent.

### Human path & notifications
- Add a JSON-RPC method to initiate a fork with the same parameters as the tool (target agent, instruction, optional
  handle, optional tools), driven by the owner.
- Add a JSON-RPC notification announcing a newly forked agent (new agent id, and its origin), mirroring the existing
  one-shot session notifications, so clients can address the fork.

### Configuration
- Register Fork and Exit as config-gated builtins in the builtin dispatch table, available when declared in topology.
- Extend `BuiltinContext` to carry the `AgentSpawner` handle so Fork can be constructed only when spawning is wired,
  matching how `activate_skill` is gated on the skills registry.
