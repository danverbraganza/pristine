# Prompt: Configuration File

Closes the Configuration File item from DESIGN.md's roadmap.

## Final refined prompt

Replace the hardcoded `HarnessBuilder` setup in `pristine`/`1p` with file-driven configuration.

* `pristine-config` is a separate library; engine code (`Harness`/`HarnessBuilder`) stays unaware of TOML, files, env vars, or templating
* Rust structs are the schema source of truth; TOML is one serde front-end
* One TOML file, top-level sections: `[models]`, `[agents]`, `[tools]`; no file-level sharding at v1
* Two-mode discovery: no flag → shipped `default.toml` (single-agent vanilla coding agent); `-c <file>` → that file used entirely, no merging, no user-tier overlay
* Unknown keys are a hard error (`#[serde(deny_unknown_fields)]`)
* Secrets templated as `{{ENV_VAR}}`, resolved at config-load time; missing env vars are a hard error
* `--model` CLI flag is removed; model name lives in the config
* No reserved forward-looking slots for skills, routes, profiles, or hot-reload
* `[tools]` table declares tools by name with values of `"built-in"` (referring to a binary-registered tool) or a structured plugin-loader shape (TBD); agents select tools by name from this declared set
* Agent → model wiring supports both string shorthand (`model = "x"`) and table form (`models = { default = "x" }`)
* Stable IDs vs names: TBD; names act as identifiers for now
* `default.toml` is embedded via `include_str!` at compile time; the binary stays self-contained; the embedded file serves as a customization template users copy out and pass via `-c`
* `default.toml` ships with a generic coding-assistant system prompt and drops `AddTool` from the registered set (its source remains as a code example)
* `[tools]` entries accept only the literal string `"built-in"` in v1; any other shape (including the future plugin-loader table) is a deserialization error
* Provider-specific dialect (e.g. Anthropic's `base_url`) lives only inside `pristine-auth.toml`, never in the topology file
* `pristine` and `1p` are defined as aliases (always identical behavior); `-c` / `--config` is a global flag accepted at any position (`1p -c file run` and `1p run -c file` both work)
* Tool reference validation: an agent's tool list referencing a name not declared in `[tools]` is a hard error at config load; runtime tool-call mismatches surface to the LLM via existing `ToolError::NotFound`
* Identity is separated from topology: a user-global `pristine-auth.toml` (default location in `$HOME`, overrideable via a global `--auth` flag) owns provider identity, model bindings, and credentials
* Topology files reference models by alias name only (e.g. `model = "default"`); the auth file resolves each alias → `(provider, model_name, credentials)` at config-load time
* Multiple topology configurations (coding agent, game simulator, etc.) share a single auth file, so users authenticate once across all uses of Pristine
* `ModelProvider` is introduced as a trait (parallel to `Tool`), held in a `ProviderRegistry` (parallel to `ToolRegistry`); the auth file's `provider = "..."` string is a name key into that registry, and each `ModelProvider` impl owns its own credentials schema and dialect fields
* Topology has no `[models]` section; agents reference model aliases directly (`model = "default"`); missing alias bindings in the auth file error at config load
* `pristine-auth.toml` accepts both `{{ENV_VAR}}` templating and inline plaintext for credentials; the auto-written template uses templating
* On first run when `pristine-auth.toml` is missing, `pristine-config` auto-writes an `AnthropicProvider`-shaped template (no interactive flow in v1, since only one provider exists)
* v1 ships `AnthropicProvider` as the only entry in `ProviderRegistry`; the registry is open-shaped so additional providers land in roadmap item 2 without schema changes
* The `ModelProvider` "built-in vs plugin-shape" parallel to `[tools]` is deferred; v1 auth file accepts only built-in provider names as bare strings
* Default location for `pristine-auth.toml` is `~/pristine-auth.toml`
* Auto-written `pristine-auth.toml` is created with `chmod 600`
* Config loading collects all errors before exit; the loader follows parse-don't-validate — its result is an inert `Config` struct (no function calls baked in, no `Arc<dyn ModelProvider>` held); `main` consumes the value and makes the `HarnessBuilder` calls itself; no Levenshtein hint in v1
* `pristine-config` is a top-level module (`src/config/` non-mod-rs layout) inside the existing single crate; workspace conversion is deferred
* Auto-write template behavior: on first run with no auth file, `pristine-config` writes the template and continues — the run proceeds if `ANTHROPIC_API_KEY` is set, and errors via the standard missing-env-var path if not
* `main` stops reading `ANTHROPIC_API_KEY` directly from env; only `pristine-config`'s templating layer touches env vars
* `pristine-config` exposes `load(...) -> Result<Config, ConfigErrors>`; `main` walks the returned inert `Config` to call `HarnessBuilder`, `add_model`, `add_tool`, `add_agent`
* `ModelProvider` trait and `ProviderRegistry` both live in a new top-level module `src/provider.rs` (mirrors `src/tool.rs`)
* Plan prescribes a detailed bead list with rough scope per bead; Coordinator decides intra-bead sequencing and any compression
* DESIGN.md: Configuration File moves to Completed, with a note flagging Multi-Model (roadmap item 2) as the next adjacent piece; ARCHITECTURE.md: new Configuration section covering the two-file model, parse-don't-validate boundary, `ModelProvider`/`ProviderRegistry`, templating, error model
* Test plan: unit tests inside `src/config/*.rs` (parse-don't-validate, templating, alias resolution, error collection, auto-write file permissions); integration test in `tests/` that loads the embedded `default.toml` plus a fixture `pristine-auth.toml` and produces a built `Harness` (no live API calls)
* `{{ENV_VAR}}` templating implementation: post-parse `toml::Value` walk that visits every string node and substitutes placeholders before final deserialization (safer than raw-text substitution against env-var contents containing quotes/backslashes)
* `AnthropicProvider` replaces `AnthropicModelBuilder` — the builder's role of constructing `AnthropicModel` is folded into the provider impl

## Concrete file shapes

```toml
# default.toml (embedded via include_str!)
[agents.default]
system_prompt = """
You are a coding assistant with access to filesystem and shell tools.
Read files before editing them. Keep changes small and focused.
"""
model = "default"
tools = ["read", "write", "edit", "insert", "exec_bash"]

[tools]
read = "built-in"
write = "built-in"
edit = "built-in"
insert = "built-in"
exec_bash = "built-in"
```

```toml
# pristine-auth.toml (auto-written on first run; chmod 600)
[models.default]
provider = "anthropic"
model_name = "claude-sonnet-4-6"
api_key = "{{ANTHROPIC_API_KEY}}"
```
