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


## The Fork Tool

We will provide a built-in Fork tool to the agent. The parameters of this tool are:

* Handle: A signifiier of much of the prior history the forked agent inherits
* Prompt: A prompt for the forked agent to inherit
* Tools: If present, which subset of the Agent's tools the Forked Agent inherits. If not present, the forked agent inherits all tools.


## Forked Agents

Forked agents are full agents, running within the harness. The can receive input from users, produce output for users,
and call all the tools that they have access to.
