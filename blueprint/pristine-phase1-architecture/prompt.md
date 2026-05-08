# Refined Prompt

Design and implement Phase 1 of the Pristine agentic harness engine.

* Phase 1 delivers a library crate (core types, traits) and a binary crate (`pristine`/`1p`)
* The binary demonstrates the core workflow: create a Harness, add one Agent, send a message, stream tokens to stdout, print metadata (token count, timing) at end
* The Harness is a long-lived owned struct created in `main()`, acting as a registry for Models and Agents
* An Agent holds a unique ID, borrows a Model from the Harness, and owns a History
* History is an immutable persistent linked list (`Arc<HistoryNode>`) with stable node IDs for future disk persistence. Block types: UserMessage, ToolCall, ToolResult, AgentMessage, PeerMessage
* The Model trait returns `impl Stream<Item = Result<StreamEvent>>`; the first implementation targets the Anthropic Messages API via direct reqwest + SSE (no third-party SDK crate)
* Request/response types should be derived from the Anthropic OpenAPI 3.1.0 spec
* Streaming from day one using Tokio as the async runtime
* Error handling: per-module error types (`model::Error`, `history::Error`, etc.) composed into a top-level `pristine::Error` enum with `From` impls; the binary uses `anyhow`
* API key is passed explicitly to the builder (no env var fallback)
* Builder pattern for object construction in Phase 1; no config file modeling yet (future: TOML -> deserialize into Builder -> run)
* IPC/client isolation is deferred past Phase 1; the binary runs the workflow in-process
* No tool use / SkillSet in Phase 1 — text-in, text-out only
* A configuration point for History transformation (e.g., summarization before completion) is anticipated but not implemented in Phase 1
