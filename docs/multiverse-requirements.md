# Multiverse Histories — Requirements

This document captures the requirements for the **multiverse** capability in Pristine: the ability for a single agent to
own and manipulate more than one History, to derive nodes via auxiliary sub-computations, and to hold and choose between
alternative continuations at a given position. It is the substrate intended to support Recursive Language Models (RLM)
and task-aware compaction.

Requirements are kept separate from design so they can be validated on their own terms. Design follow-up (trait shapes,
storage model, scheduling, the reduction interface) will be tracked in sibling documents under `docs/` and ultimately
landed in `ARCHITECTURE.md`.

## 1. Motivation

These concrete capabilities drive this feature and currently have no home in a model where an agent owns exactly one
linear History.

- Task-aware compaction or **Focus** We're introducing a new concept called Focus. Once context becomes super long,
  attention begins to drop off, and agent performance degrades. If a coding agent is switching between two unrelated
  domains such as frontend and backend development, a linear thread suffers. Now, a single compaction would necessarily
  have to summarize the entire history at a given point, producing an artifact that's not great for either purpose.
  "Focus" allows us to produce a tightly-scoped view of the History which is more than a summary. Parts that are
  irrelevant to the Task at hand can be summarized aggressively, while parts that are more relevant are injected
  verbatim into the model.

- **Recursive Language Models (RLM).** We can provide agents with Tools to enable them to speculatively work with the
  future. For example, an agent might be able to read a complex, large file, decide that it was a mistake, and instead
  "jump back" in time to before it read the file with just a summary of the key insights, or a warning that it was not
  necessary.


The unifying observation is that both are the same operation: run an auxiliary History, reduce it to a single Block, and
graft that Block into another History.

## 2. Vocabulary

The model an agent operates over must be understood as three distinct views. Conflating them is the primary source of
confusion and must be avoided in design discussion.

- **The process (generation).** A single Agent is always sequential. At any instant there is exactly one cursor, one
  active line, and one "what comes next." Concurrency, if any, is an implementation detail of Agents that is layered on
  top, not part of the logical model. So, when an agent forks, it generates another agent with shared context up to a
  point. The second agent is now a different (logical) process, with its own active line.

- **The structure (what is stored).** A tree/DAG. A node on one line may have been produced by an entire sub-line, so
  the stored structure branches even though the process does not. The structure is immutable. Multiple agents co-exist
  simultaneously upon the structure, since they might share some of the same component blocks. However, once agents
  generate their independent new blocks, these will be different.

- **A resolution (what a model reads).** A single linearization — one path through the structure. A model always
  consumes a flat sequence; resolution is the act of producing that sequence from the structure. A model may have
  options of how it selects between different linearizations. E.g. we should give our models the ability to "grep" or
  otherwise read their own full linearizations.

All three are simultaneously true. "It is linear" (process/resolution) and "it is not linear" (structure) are statements
about different views.

## 3. The agent as histories + models + tools

R3.1. An agent is defined by a **set of Histories**, a set of **Models**, and a set of **Tools**. (Today an agent owns a
single History; this requirement generalizes that.)

R3.2. At each generation step, the engine must **resolve a single linear history** from the agent's set of histories,
feed it to a chosen model, and turn the produced Block into a new node appended to the active line.

R3.3. The set of histories owned by an agent forms a **tree linked by derivation** (see §4), together with a **cursor**
naming the line currently being extended. Exactly one line is active at a time.

R3.4. Model selection is per-step, that is, it explicitly can vary on every node. For a limited example, the model used
to extend an auxiliary line need not be the model used on the main line (this is what lets a cheap model do compaction
work for an expensive main line, or vice versa).

## 4. Nodes, lines, and derivation

R4.1. A **Block** is immutable (already true today). A **Node** wraps a Block and carries two relations:

- `prev` — the previous node on the same line. This is the existing sequence relation and defines read order.
- `derivation` — an optional reference to the auxiliary History whose reduction produced this node's Block.

R4.2. **Linearization (resolution) follows `prev` only.** A node's `derivation` is invisible to the linear read unless
deliberately inlined. This is what keeps resolution linear regardless of how much sub-structure exists.

R4.3. A node with a `derivation` is a **derived node**; a node without one is **directly generated**. The two must be
indistinguishable to a reader of the linear history — a derived node presents only its Block.

R4.4. `derivation` is **provenance**. It justifies the node without being part of the line the model reads. The engine
must be able to traverse it for auditing, re-collapse, and re-expansion.

R4.5. We want to provide an addressable handle to the model so that it can refer to past points in time. This way, it
can refer to that point when demarcating, e.g. forking a new agent, or seeding an auxiliary History. We have decided
that these addressable positions are going to be **tool-call boundaries**, where in addition to the tool call result,
the harness will inject a short, harness-attributed **checkpoint handle** derived from the immutable `NodeId`. Handles
must be stable and allocated against the append-only node set. Once a block is compacted, its handle can no longer be
used.

## 8. Invariants

I8.1. **Blocks are immutable; the composite is not.** Individual Blocks (and the Nodes wrapping them) never change. The
*line* — which alternative occupies each slot, in what order, with what elided — is a mutable projection. Insert,
reorder, elide, and summarize all operate on the projection, never on a Block.
