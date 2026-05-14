Adapt `pristine run` from a hardcoded two-message demo into a JSON-RPC 2.0 server reading newline-delimited JSON-RPC from stdin and writing responses/notifications to stdout.

* Use `jsonrpsee-core`'s `RpcModule` for method registration and dispatch (proc macros for defining methods), with a thin (~30-50 line) stdio adapter that calls `raw_json_request` and forwards subscription notifications
* Initial method surface: `send_message` (wraps `send_to_agent`), `shutdown`, `initialize` (returns agent/owner IDs, LSP-style handshake)
* Streaming agent events delivered as JSON-RPC notifications via jsonrpsee's subscription mechanism, using a single `agent.event` notification method with a `type` discriminator
* Auto-push: `send_message` implicitly subscribes the caller to that agent's events
* All four AgentEvent variants (TokenDelta, BlockComplete, RunComplete, Error) are delivered; client filters as needed
* Hardcoded single-agent/single-model setup retained for now; method dispatch via proc macros makes adding CRUD methods cheap later
* Graceful shutdown on both `shutdown` method and EOF on stdin
* Stderr stays informal (eprintln!) for now
* `initialized` notification NOT auto-fired; client sends `initialize` request to get IDs
* Stdout writes serialized with an async mutex (simple, fine for single-client stdio)
* Extract the current two-message demo into `client.py` with inline `# /// script` metadata, runnable via `uv run`, using `jsonrpcclient`
* Don't bolt the door on explicit `subscribe`/`unsubscribe` methods later
* ARCHITECTURE.md: keep Phase 1 Initial Build pseudocode as historical context, add new Phase 2 section describing the server
