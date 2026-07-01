# Forking Requirements

## Why forking?

Forking is a capability provided by many agent harnesses. We want to support that in Pristine. We want to provide both
agents and humans with the ability to initiate a fork of the current agent. Additionally, agents have the ability to
specify how much of their existing context they want to provide to their forked siblings, which enables them to control
the slider from a true fork with full context, to a true subagent.


## Supporting technology: the Tool Call Handle

We want to provide an addressable handle to the model so that it can refer to past points in time. This way, it
can refer to that point when demarcating, e.g. forking a new agent, or seeding an auxiliary History. We have decided
that these addressable positions are going to be **tool-call boundaries**, where in addition to the tool call result,
the harness will inject a short, harness-attributed **checkpoint handle** derived from the immutable `NodeId`. Handles
must be stable and allocated against the append-only node set. Once a block is compacted, its handle can no longer be
used.

The sole exception is the **genesis node**: a synthetic, content-free root node seeded at the head of every history
with the reserved handle `NodeId::nil()`. It is the one always-available handle that is not a tool-call boundary, and
it denotes the empty prefix (see the Fork tool's Handle parameter).


## The Fork Tool

We will provide a built-in Fork tool to the agent. The parameters of this tool are:

* Handle: An **optional** checkpoint handle naming a tool-call boundary in the prior history. When present, the
  forked agent inherits the history prefix up to and including that boundary. Any tool-call boundary can be named,
  so the Handle behaves as a continuous slider between full and partial context. When the Handle is **omitted /
  unspecified, the fork inherits the full prior context** (equivalent to a handle at the current head). Omitting the
  Handle is distinct from supplying the genesis handle: to create a pure subagent with **no** inherited history, the
  initiator explicitly supplies the genesis node's handle — `NodeId::nil()`, the reserved id of a synthetic,
  content-free root node seeded at the head of every history. Forking at the genesis node inherits nothing. The
  genesis node is the one always-available handle that is not a tool-call boundary; every other handle is a
  tool-call boundary.
* Instruction: The immediate next instruction for the forked agent — the *next node in its cycle*, seeded as the
  first message the forked agent processes after inheriting history. This is **not** the forked agent's system
  prompt. (The requirements previously called this parameter "Prompt"; it was never the system prompt.)
* Tools: If present, which subset of the Agent's tools the Forked Agent inherits. If not present, the forked agent inherits all tools.

Aside from the tool subset (when narrowed) and the inherited history prefix (bounded by Handle), the forked agent
inherits **all aspects of its parent** (the super-agent), including the parent's system prompt and model assignment.


## Forked Agents

Forked agents are full agents, running within the harness. The can receive input from users, produce output for users,
and call all the tools that they have access to.

Like the root agent, a forked agent persists until harness shutdown, idling between instructions. In addition, every
agent is provided an `exit()` tool it may invoke to terminate itself, releasing its task so it no longer
processes inbound messages.

## The Exit Tool

The Exit Tool is a regular built-in tool an agent calls to shut itself down. Only if available should an agent have the
ability to call this.
