# Plan: Configuration File

## Overview

- Closes the Configuration File item from DESIGN.md's roadmap. The hardcoded `HarnessBuilder` setup in `src/lib.rs::run_async` is replaced with file-driven configuration produced by a new `pristine-config` module.
- Two orthogonal files own two orthogonal concerns: `default.toml` (embedded; topology — agents, tools, prompts) and `pristine-auth.toml` (user-global at `~/pristine-auth.toml`; identity — providers, models, credentials). Topology files are provider-agnostic; identity is shared across topologies.
- `ModelProvider` becomes a noun in the type system — a trait + `ProviderRegistry` parallel to `Tool` / `ToolRegistry`. Provider-specific dialect (e.g. Anthropic's `base_url`) lives only inside the provider impl and the auth file's provider section.
- Loading follows "parse don't validate": every parse / templating / resolution failure is collected into a `ConfigErrors` value before exit; success means the resulting `Config` value is an inert, trustworthy data struct. `main` walks `Config` and makes `HarnessBuilder` calls itself.
- Engine code stays untouched. `Harness`, `HarnessBuilder`, `Agent`, `ARModel`, `Tool`, `MessageBus` learn nothing about TOML, file paths, env vars, or templating. The split lives at the `pristine-config` boundary.

## Expected behavior

- A first-time user runs `pristine run` or `1p run` with no setup. The binary auto-writes `~/pristine-auth.toml` (chmod 600) containing an `AnthropicProvider`-shaped template that references `{{ANTHROPIC_API_KEY}}`, then continues.
- If `ANTHROPIC_API_KEY` is set in the environment, the run succeeds end-to-end. If unset, the run exits with the standard missing-env-var error pointing at the auth file.
- `pristine` and `1p` are defined as aliases — always identical behavior. Two new global flags are accepted at any position:
  - `-c <path>` / `--config <path>` — replace the embedded `default.toml` entirely with the supplied file. No merging.
  - `--auth <path>` — replace `~/pristine-auth.toml` as the auth file path.
- The `--model` CLI flag is removed. Model name now lives in `pristine-auth.toml`.
- The default behavior (no flags, env var set) is functionally equivalent to today's hardcoded behavior except: a real coding-assistant system prompt replaces the identity-only prompt; `AddTool` is no longer registered.
- Topology references model aliases by name (e.g. `model = "default"` on an agent). The auth file resolves each alias to `(provider, model_name, credentials)`. A topology can reference multiple aliases; v1 ships `AnthropicProvider` as the only valid `provider` value.
- A user can keep multiple topology files (coding agent, game simulator, etc.) and share a single auth file across all of them — one identity file, many shapes.
- Configuration errors collect everything that could be parsed and report all problems together before exit, with TOML line/column info where available. Unknown keys, missing env vars, dangling aliases, undeclared tool references, and unknown provider names are all hard errors. No silent fallthrough.
- The binary stops reading `ANTHROPIC_API_KEY` from env directly — env vars are touched only by the templating layer inside `pristine-config`.
- JSON-RPC stdio behavior (request/response, `agent.event` notifications, `initialize` / `send_message` / `shutdown`) is unaffected.

## Changes

### Source code

- New module `src/provider.rs` — `ModelProvider` trait, `ProviderError`, `ProviderRegistry`. Mirrors `src/tool.rs`.
- `AnthropicProvider` replaces `AnthropicModelBuilder`. The builder's responsibilities (constructing `AnthropicModel` from `api_key` / `model_name` / optional `base_url`) are folded into the provider impl. Existing builder tests migrate to the provider.
- New module `src/config/` (non-mod-rs layout). Submodules cover topology types, auth types, `{{ENV_VAR}}` templating, alias resolution, error collection, and the top-level `load(...) -> Result<Config, ConfigErrors>` entry point. `Config` is the inert returned value; `ConfigErrors` is a `Vec`-backed aggregate.
- `HarnessBuilder` gains an `add_provider(Arc<dyn ModelProvider>)` registration path, parallel to `add_tool`. `add_model` retains its current shape for tests and direct programmatic use.
- `src/lib.rs::run_async` is rewritten to: call `pristine_config::load(...)`, walk the resulting `Config`, drive `HarnessBuilder`. The direct env read of `ANTHROPIC_API_KEY` is removed. `--model` is removed from the clap CLI; `-c/--config` and `--auth` are added as global flags (`#[arg(global = true)]`).
- `AddTool` is dropped from binary registration. Its source and tests in `src/builtins/add.rs` are retained as an example.

### Assets

- New embedded asset `default.toml` at the repo root (placement decided by Coordinator), pulled in via `include_str!`. Ships with the coding-assistant system prompt and the five-tool registration (`read`, `write`, `edit`, `insert`, `exec_bash`).

### Documentation

- DESIGN.md: Configuration File moves from Roadmap to Completed. A note in the new entry flags Multi-Model (roadmap item 2) as the next adjacent piece, since `ProviderRegistry` is now shaped for it.
- ARCHITECTURE.md: new Configuration section. Covers the two-file model, the parse-don't-validate boundary between `pristine-config` and the engine, the `ModelProvider` / `ProviderRegistry` parallel to `Tool` / `ToolRegistry`, the `{{ENV_VAR}}` templating mechanism, and the error model.

### Tests

- Unit tests inside `src/config/*.rs`:
  - TOML deserialization and `deny_unknown_fields` enforcement
  - `{{ENV_VAR}}` templating (set, unset, mid-string, multi-occurrence, env-var contents containing quotes/backslashes)
  - Alias resolution (resolved, dangling, extra)
  - Tool reference validation (declared, undeclared, empty list)
  - Error collection (multiple errors across topology + auth in one load)
  - Auto-write existence and `chmod 600` (using tempdir)
- Integration test in `tests/`: loads the embedded `default.toml` plus a fixture `pristine-auth.toml`, asserts that the resulting `Harness` has the expected agent, model, and tool registrations. No live API calls.
- Existing `src/harness.rs` unit tests using `HarnessBuilder` directly with stub models remain unchanged — they exercise the engine, not the config layer.

### Bead phasing

Plan is prescriptive on bead scope and dependencies. Coordinator decides intra-phase sequencing and may compress beads that land cleanly as one unit. Phase dependencies are strict.

**Phase A — Provider infrastructure**

- A1: `ModelProvider` trait + `ProviderRegistry` types in new `src/provider.rs`.
- A2: `AnthropicProvider` impl, replacing `AnthropicModelBuilder`. Folds the builder's `api_key` / `model_name` / `base_url` responsibilities into the provider; updates references in `src/lib.rs` and migrates existing Anthropic tests.
- A3: Wire `ProviderRegistry` into `HarnessBuilder` (`add_provider` registration path).

**Phase B — Config schema**

- B1: `src/config/` module skeleton; typed structs (`TopologyConfig`, `AuthConfig`, `Config`, `ConfigError`, `ConfigErrors`) with serde derives and `#[serde(deny_unknown_fields)]`.
- B2: TOML deserialization into the typed structs (no resolution, no templating yet).
- B3: `{{ENV_VAR}}` templating layer — post-parse `toml::Value` walk that visits every string node and substitutes placeholders before final deserialization.

**Phase C — Config semantics**

- C1: Model-alias resolution (topology `model = "X"` against auth `[models.X]`).
- C2: Tool-reference validation at config-load time (an agent's `tools = [...]` must all appear as keys in the topology's `[tools]`).
- C3: Parse-don't-validate error collection — accumulate every parse / templating / resolution error across both files before returning `Err(ConfigErrors)`.

**Phase D — Auth file lifecycle**

- D1: Discovery rules (`~/pristine-auth.toml` default; `--auth` override; expand `~`).
- D2: Auto-write template + `chmod 600` when the auth file is missing.
- D3: Top-level `pristine_config::load(...)` orchestration: read topology, read or auto-write auth, template, deserialize, resolve, validate, return `Config` or `ConfigErrors`.

**Phase E — Binary integration**

- E1: Author `default.toml`; embed via `include_str!`.
- E2: Replace `src/lib.rs::run_async` to load `Config` and walk it into `HarnessBuilder` calls. Remove direct env-var read of `ANTHROPIC_API_KEY`.
- E3: CLI changes — add global `-c/--config` and `--auth`; remove `--model`; preserve identical behavior between `pristine` and `1p` binaries.
- E4: Drop `AddTool` from binary registration (source and tests retained).

**Phase F — Documentation + tests**

- F1: DESIGN.md update (Completed bullet for Configuration File + adjacent-piece note for Multi-Model).
- F2: ARCHITECTURE.md new Configuration section.
- F3: Integration test in `tests/` loading `default.toml` + fixture auth, producing a built `Harness`.
