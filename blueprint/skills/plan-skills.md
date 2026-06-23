# Plan — Skills

> Implement the Skills feature for Pristine (DESIGN.md roadmap item #1),
> aligned with the Agent Skills open standard at <https://agentskills.io>.
> Requirements are captured in `docs/skills-requirements.md`.
>
> * **Engine, not UI.** Pristine surfaces skill-loading outcomes as
>   structured data via the JSON-RPC surface and never decides display.
> * **Source layout.** New top-level `src/skills.rs` module (no `mod.rs`)
>   for discovery, parsing, and the registry; the `ActivateSkill` tool
>   lives under `src/builtins/activate_skill.rs`.
> * **System prompt becomes structured.** `system_prompt: String` is
>   replaced inside the engine by a `SystemPrompt` struct with
>   named-field slots: a fixed `base: String` and a dynamic skills slot
>   holding `Arc<dyn SkillsRegistrySource>`. Rendering into the system
>   `Turn` is owned by `SystemPrompt`, called once per agent iteration.
>   TOML and config types continue to carry `system_prompt: String`; the
>   structured type is constructed at agent-build time inside
>   `build_harness_from_config`. The skills slot is the sole tier-1
>   disclosure surface — the activation tool's description does not
>   enumerate skills.
>   * `SystemPrompt::render()` returns `String`; the agent loop wraps it
>     in a `Turn { role: System, content: vec![ContentPart::Text(...)] }`
>     exactly as it does today.
> * **System-prompt skills rendering.** Markdown section appended after
>   the base prompt: `## Available skills`, one bullet per skill
>   (`**name**: description`), closing line pointing at `activate_skill`.
>   v1 choice, marked as subject to revision.
> * **Registry storage shape.** `SkillsRegistry` concrete struct +
>   `SkillsRegistrySource` trait, mirroring `ToolRegistry`/`Tool` and
>   `ProviderRegistry`/`ModelProvider`. Filesystem registry ships in v1;
>   the trait is the architectural seam for future implementors.
> * **`SkillsRegistrySource` trait surface.** Synchronous.
>   `fn list(&self) -> Vec<SkillSummary>` for tier-1;
>   `fn get(&self, name: &str) -> Option<SkillRecord>` for activation.
>   `SkillRecord` carries `{ name, description, body, directory }`.
> * **Discovery timing.** Lazy-strict. Registry is constructed empty;
>   first access (first agent turn's system-prompt render) triggers the
>   filesystem scan. Notifications fire immediately after.
> * **TOML configuration.** Single global `[skills]` block in the
>   topology file. Block-present = enabled; `enabled = false` is the
>   kill-switch. `user_paths`/`project_paths` arrays; omitted = the four
>   conventional defaults (`.agents/skills` then `.pristine/skills`
>   under cwd and home respectively). `disabled = [...]` exact-name
>   list. Per-agent skills overrides are explicitly deferred.
>   `activate_skill` is just-another-tool: needs
>   `[tools.activate_skill]` declared and listed in each agent's
>   `tools = [...]`.
>   * `Config` always carries a `SkillsConfig`; absent block
>     deserializes to `SkillsConfig::default()` with `enabled: false`
>     semantics.
>   * Embedded `default.toml` stays skills-free; out-of-the-box behavior
>     is preserved bit-for-bit.
>   * Skill module layout follows the no-`mod.rs` rule: `src/skills.rs`
>     plus `src/skills/types.rs`, `source.rs`, `registry.rs`,
>     `filesystem.rs`, `discover.rs`, `parse.rs`.
> * **Shadowing precedence.** Cross-scope: project shadows user.
>   Intra-scope: last path in the effective `user_paths`/`project_paths`
>   array wins. With the default arrays, this means `.pristine/skills`
>   shadows `.agents/skills` within each scope. Diagnostics record every
>   shadowing event.
>   * Configured-but-unused skills (`[skills]` present but no agent
>     lists `activate_skill`) is silent / valid by default; no special-
>     case detection.
> * **Trust gating.** `--trust-project-skills` CLI flag on `pristine
>   run`, forwarded through `just chat` → `client.py` → `1p`. Without
>   the flag, project-scope discovery is skipped; the bypass is recorded
>   in `skills_diagnostics`. Per-project remembered trust is deferred.
>   * `client.py` mirrors the existing `--model` pattern: `default=None`,
>     flag is only forwarded if explicitly passed.
> * **Activation tool shape.** Static name `"activate_skill"`, static
>   description ("activate any of the active skills"), input schema
>   `{ name: string }`. Returns `{ body: "..." }` with frontmatter
>   stripped. On unknown name: `ToolError::Execution` carrying
>   `{ kind: "unknown_skill", name, known: [...] }`.
> * **Diagnostics surface.** Two JSON-RPC notifications fired once after
>   discovery: `skills_loaded` with the catalog
>   (`{ skills: [{ name, description }, ...] }`), and
>   `skills_diagnostics` with an array of kind-tagged entries
>   (`shadowed`, `malformed_yaml`, `name_mismatch`,
>   `description_missing`, `bypassed_path`, …).
> * **Testing.** Three-pronged. Tempdir-based integration tests for
>   `FilesystemSkillsRegistry` discovery behavior. A
>   `StubSkillsRegistry` in `src/test_support.rs` for tests that
>   exercise `SystemPrompt` slot rendering or `ActivateSkill` tool
>   lookup without touching the filesystem. A `SkillsFixture` fluent
>   builder in `src/test_support.rs` to make tempdir-based test setup
>   terse.
> * **TOML schema.** `[skills]` block carries `enabled: bool`,
>   `user_paths: [String]`, `project_paths: [String]`,
>   `disabled: [String]`. All fields optional; block-present is
>   sufficient to enable. `~`, relative, and absolute paths permitted in
>   either path array. `deny_unknown_fields` enforced.
> * **Path resolution timing.** `~` expansion and cwd resolution happen
>   at scan time inside `FilesystemSkillsRegistry`, not at config-parse
>   time. Config carries unresolved strings; resolution failures surface
>   as `skills_diagnostics` entries.
> * **Builtin dispatch.** `register_builtin_tools` is refactored into a
>   `HashMap<&'static str, Box<dyn Fn(&BuiltinContext) -> Result<Arc<dyn Tool>, Error>>>`
>   dispatch table. `BuiltinContext` carries dependencies the closures
>   need (`Option<&Arc<SkillsRegistry>>` in v1); future builtins with
>   dependencies plug in here. `activate_skill` registers iff
>   `[tools.activate_skill]` is declared *and* `[skills]` is present.
> * **Implementation phasing.** Vertical slices, five phases:
>   (1) `SystemPrompt` refactor (`String` → struct with `base` slot
>   only); (2) `SkillsRegistry` + trait + empty filesystem impl + slot
>   wiring; (3) filesystem discovery + JSON-RPC notifications;
>   (4) `ActivateSkill` tool + dispatch-table refactor + TOML wiring;
>   (5) `--trust-project-skills` CLI flag plumbed through
>   `pristine`/`1p`/`client.py`/`justfile`.

---

## Overview

- Skills is roadmap item #1 (DESIGN.md). It is the highest-priority post-
  configuration capability.
- The implementation tracks the Agent Skills open standard at
  <https://agentskills.io> for file format, discovery conventions, and
  progressive disclosure.
- The engine surfaces skill-loading outcomes as structured JSON-RPC
  notifications; UI decisions remain entirely with the client layer.
- The Agent's single `system_prompt: String` is generalized to a
  `SystemPrompt` struct with named-field slots so the skills catalog can
  resolve per turn without rebuilding the Agent; the change keeps TOML and
  config types as plain strings.
- A new `SkillsRegistry` concrete type plus a `SkillsRegistrySource` trait
  mirrors the existing `ToolRegistry`/`Tool` and
  `ProviderRegistry`/`ModelProvider` patterns; the filesystem implementor
  ships in v1; the trait is the seam for future implementors.
- A new built-in tool `ActivateSkill` returns skill bodies on demand; its
  description and input schema are static (input is a plain string) to
  avoid coupling tool dynamism to the Skills feature.
- Discovery is lazy-strict: the registry is constructed empty and the
  filesystem scan runs on first access (first agent turn's system-prompt
  render). Resulting catalog and diagnostics fire as two one-shot JSON-RPC
  notifications.
- Project-scope discovery is gated behind a per-invocation
  `--trust-project-skills` CLI flag forwarded through `client.py` to the
  engine. Persistent per-project trust is deferred.
- Module layout follows the project-wide no-`mod.rs` rule: `src/skills.rs`
  is the module root; submodules are files under `src/skills/`.

## Expected behavior

- A user can author a skill as a directory containing `SKILL.md` with YAML
  frontmatter (`name`, `description`, optional `license`, `compatibility`,
  `metadata`, `allowed-tools`) and a markdown body, in any of the
  conventional scan paths.
- With no `[skills]` block in the topology, behavior is unchanged from
  today: no discovery, no notifications, no slot rendering.
- With a `[skills]` block present and skills authored on disk, the
  default agent's system prompt grows a `## Available skills` section
  listing each discovered skill's name and description.
- The system prompt's skills section is the model's sole tier-1
  disclosure of available skills; the activation tool's description does
  not enumerate skills.
- The model activates a skill by calling `activate_skill` with
  `{ name: "skill-name" }`. The tool returns `{ body: "..." }` carrying
  the SKILL.md body with frontmatter stripped.
- If the model calls `activate_skill` with an unknown name, the tool
  returns a `ToolError::Execution` carrying
  `{ kind: "unknown_skill", name, known: [...] }`; the model can retry
  with a corrected name.
- Project-scope paths are scanned only when `--trust-project-skills` is
  passed. Without the flag, their contents are invisible to the agent
  and each skipped path appears as a `bypassed_path` entry in
  `skills_diagnostics`.
- The client receives two JSON-RPC notifications shortly after the first
  agent turn begins: `skills_loaded` (the catalog) and
  `skills_diagnostics` (shadowing, malformed files, bypassed paths,
  resolution failures).
- Within a scope, later paths in the effective array shadow earlier ones
  by name; with the default ordering this means `.pristine/skills`
  shadows `.agents/skills`. Project-scope wins over user-scope when both
  are present. Every shadowing is reported as a diagnostic.
- If `[skills]` is configured but no agent lists `activate_skill` in its
  tools, the system prompt's skills section is still rendered (the model
  sees skills), but `activate_skill` calls fail with the existing
  `tool not found` error. Not specially detected; treated as user
  configuration to fix.
- Existing engine behavior with no Skills config is preserved bit-for-
  bit. No new diagnostics, no new notifications, no system-prompt
  changes for agents whose `SystemPrompt::skills` slot points at an
  empty registry.

## Implementation plan

### `src/skills.rs` (new module root)

- Declares submodules `types`, `source`, `registry`, `filesystem`,
  `discover`, `parse`.
- Re-exports the public surface needed by the rest of the engine.
- No `mod.rs`; this file *is* the module root.

### `src/skills/types.rs` (new)

- `SkillSummary { name: String, description: String }`. Tier-1
  disclosure payload.
- `SkillRecord { name: String, description: String, body: String, directory: PathBuf }`.
  Activation payload; carries the on-disk location for future resource
  enumeration.
- `SkillDiagnostic` enum carrying the kind-tagged entries reported via
  the `skills_diagnostics` notification (`Shadowed`, `MalformedYaml`,
  `NameMismatch`, `DescriptionMissing`, `BypassedPath`,
  `ResolutionFailure`, …). Implements `serde::Serialize` for direct
  inclusion in the notification payload.

### `src/skills/source.rs` (new)

- `SkillsRegistrySource: Send + Sync` trait with two synchronous
  methods:
  - `fn list(&self) -> Vec<SkillSummary>` — tier-1.
  - `fn get(&self, name: &str) -> Option<SkillRecord>` — activation.
- Pure read surface. No mutation, no async, no I/O lifecycle methods on
  the trait itself.

### `src/skills/registry.rs` (new)

- `SkillsRegistry` concrete struct: the engine's owned storage type for
  discovered skills. Implements `SkillsRegistrySource`.
- Internally holds an `OnceLock` (or equivalent) so first access
  triggers the underlying `FilesystemSkillsRegistry::scan` exactly once.
- Constructor takes the `SkillsConfig` (resolved values) plus a
  `trust_project: bool` flag (true iff `--trust-project-skills` was
  passed).
- Holds the discovered catalog plus the collected diagnostics for later
  emission on the notification surface.

### `src/skills/filesystem.rs` (new)

- `FilesystemSkillsRegistry::scan(config: &SkillsConfig, trust_project: bool, env: &dyn HomeSource) -> (Vec<SkillRecord>, Vec<SkillDiagnostic>)`.
- Pure function (modulo filesystem I/O): no shared state, no logging.
  Returns the catalog plus the diagnostics list.
- Calls `discover::resolve_paths` for path expansion, then iterates
  through user_paths first, then project_paths (if `trust_project`),
  applying last-wins shadowing within each scope and project-shadows-
  user across.

### `src/skills/discover.rs` (new)

- `resolve_paths(config: &SkillsConfig, trust_project: bool, env: &dyn HomeSource) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<SkillDiagnostic>)`.
- Resolves `~` and cwd-relative paths into absolute `PathBuf`s.
- Returns the resolved user_paths, project_paths, and any path-level
  diagnostics (e.g., a `~/foo` when `HOME` is unset).
- When `trust_project = false` and `project_paths` is non-empty, emits a
  `BypassedPath` diagnostic per path and excludes them from the second
  returned vector.

### `src/skills/parse.rs` (new)

- `parse_skill_md(path: &Path) -> Result<SkillRecord, SkillDiagnostic>`.
- Locates the YAML frontmatter between `---` lines; parses the YAML;
  extracts the body; returns a `SkillRecord` (with `name`, `description`,
  `body`, `directory`).
- Lenient parsing per the requirements doc: attempt a YAML-quoting
  fallback for the unquoted-colon case; emit warn-tagged diagnostics
  (returned as part of a separate diagnostics channel for the caller to
  collect, or carried inline if the parse succeeds) for non-fatal issues
  like name-not-matching-directory or >64-char names.
- Skips (returns `Err(SkillDiagnostic)`) on missing description or
  unparseable YAML.

### `src/agent.rs` (modify)

- Replace `system_prompt: String` with `system_prompt: SystemPrompt`.
- Add a new `SystemPrompt` type with named-field slots:
  ```rust
  pub struct SystemPrompt {
      pub base: String,
      pub skills: Option<Arc<dyn SkillsRegistrySource>>,
  }
  ```
- `impl SystemPrompt { pub fn render(&self) -> String { ... } }`. Concatenates
  the `base` text and (if `skills` is `Some` and `list()` is non-empty)
  the markdown skills section.
- `AgentBuilder::system_prompt` changes signature from `impl Into<String>` to
  `SystemPrompt`. All existing call sites pass `SystemPrompt { base: <prompt>, skills: None }`.
- The agent loop changes one line: `ContentPart::Text(self.system_prompt.clone())` → `ContentPart::Text(self.system_prompt.render())`.
  Per-iteration rendering picks up any catalog growth between turns.

### `src/config/topology.rs` (modify)

- Add `SkillsConfig`:
  ```rust
  #[derive(Debug, Clone, Deserialize, Default)]
  #[serde(deny_unknown_fields)]
  pub struct SkillsConfig {
      #[serde(default)]
      pub enabled: Option<bool>,         // present-means-enabled; explicit false kills
      #[serde(default)]
      pub user_paths: Option<Vec<String>>,
      #[serde(default)]
      pub project_paths: Option<Vec<String>>,
      #[serde(default)]
      pub disabled: Vec<String>,
  }
  ```
- Add `skills: Option<SkillsConfig>` to `TopologyConfig`.
- `SkillsConfig::default()` yields `{ enabled: None, user_paths: None, project_paths: None, disabled: vec![] }`,
  with `enabled.unwrap_or(false)` interpreted at consumer time.
- `SkillsConfig::is_enabled(&self) -> bool` helper that returns `false` when
  the block is absent (`Option::None` upstream) or when `enabled = false`
  is explicit.
- `SkillsConfig::effective_user_paths()` / `effective_project_paths()`
  helpers that return the user-supplied list when present, otherwise the
  four conventional defaults.

### `src/config.rs` (modify)

- `Config` gains `pub skills: SkillsConfig`.
- `assemble_config` populates `skills` from `topology.skills.unwrap_or_default()`.
- Re-export `SkillsConfig` alongside the other config types.

### `src/builtins/activate_skill.rs` (new)

- `pub struct ActivateSkill { schema: Value, registry: Arc<dyn SkillsRegistrySource> }`.
- `ActivateSkill::new(registry: Arc<dyn SkillsRegistrySource>) -> Self`.
- Schema is built once at construction: `{ "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"] }`.
- Local typed error enum (per-builtin convention):
  ```rust
  #[derive(serde::Serialize)]
  #[serde(tag = "kind", rename_all = "snake_case")]
  enum ActivateSkillError {
      InvalidInput { reason: String },
      UnknownSkill { name: String, known: Vec<String> },
  }
  ```
- `impl Tool for ActivateSkill` with:
  - `name() -> "activate_skill"`,
  - `description() -> "activate any of the active skills..."`,
  - `input_schema() -> &Value`,
  - `call(input) -> Result<Value, ToolError>` — parses the JSON for
    `{ name: String }`, calls `registry.get(&name)`, returns
    `{ "body": <stripped markdown> }` on hit, or
    `ToolError::Execution(serde_json::to_value(ActivateSkillError::UnknownSkill { name, known })?)`
    on miss.

### `src/lib.rs` (modify)

- Add `--trust-project-skills` to the `Cli` struct as a global bool flag.
- In `build_harness_from_config`, after the existing provider and tool
  setup, construct a `SkillsRegistry` from `config.skills` and the
  `trust_project` flag, wrap as `Arc<dyn SkillsRegistrySource>`, and:
  - pass it into each `SystemPrompt::skills` slot when assembling
    `PendingAgent` system prompts;
  - make it available via `BuiltinContext` for the dispatch table.
- Refactor `register_builtin_tools` into a dispatch table driven by the
  `[tools.X]` declarations in `config.tools`:
  ```rust
  struct BuiltinContext<'a> {
      skills_registry: Option<&'a Arc<SkillsRegistry>>,
  }
  type BuiltinCtor =
      Box<dyn Fn(&BuiltinContext<'_>) -> Result<Arc<dyn Tool>, Error>>;
  fn builtin_constructors() -> HashMap<&'static str, BuiltinCtor> { ... }
  ```
- The five existing builtins register through this table.
  `activate_skill` registers iff `[tools.activate_skill]` is declared in
  `config.tools` *and* the `SkillsRegistry` is present in the context.

### `src/harness.rs` (modify)

- `PendingAgent.system_prompt: String` → `PendingAgent.system_prompt: SystemPrompt`.
- `Harness::start` forwards `spec.system_prompt` into `AgentBuilder::system_prompt`
  unchanged (signature already swung in agent.rs).
- Add a one-shot post-discovery hook surfacing the two notifications
  through the existing `MessageBus`/JSON-RPC pipeline. Implementation
  detail: the `SkillsRegistry` exposes a `take_diagnostics()` or
  equivalent the harness drains and emits as `skills_diagnostics`, plus a
  `summarize()` returning the `Vec<SkillSummary>` for `skills_loaded`.

### `src/rpc.rs` / `src/stdio.rs` (modify)

- Add the two new notification kinds (`skills_loaded`,
  `skills_diagnostics`) to whatever the existing notification dispatcher
  surface looks like. Both fire once, immediately after the first call
  to `SystemPrompt::render()` triggers discovery on the registry.

### `src/test_support.rs` (modify)

- Add `StubSkillsRegistry` implementing `SkillsRegistrySource` with
  user-provided in-memory `Vec<SkillRecord>`.
- Add `SkillsFixture` fluent builder:
  ```rust
  pub struct SkillsFixture { dir: TempDir, ... }
  impl SkillsFixture {
      pub fn new() -> Self { ... }
      pub fn add_skill(self, name, description, body) -> Self { ... }
      pub fn build(self) -> (FilesystemSkillsRegistry, TempDir) { ... }
  }
  ```
- Sits alongside the existing `EchoTool`.

### `client.py` (modify)

- Add `parser.add_argument("--trust-project-skills", action="store_true", default=None)`.
- After the existing `--model` forwarding block, add the analogous
  `if args.trust_project_skills: command += ["--trust-project-skills"]`.
- Default is `None`, mirroring the existing `--model` pattern; the flag
  is only forwarded if the user explicitly passes it.

### `justfile` (modify)

- `chat *args` already forwards arbitrary positional arguments via the
  `{{args}}` interpolation. No changes required; `--trust-project-skills`
  passes through automatically.

### `docs/skills-requirements.md` (no change)

- The requirements doc is the contract; the plan is the implementation
  of that contract. No edits to the requirements doc as part of this
  plan.

### `DESIGN.md` (no change beyond the no-`mod.rs` invariant already added)

- The Skills entry under "Roadmap" already references
  `docs/skills-requirements.md`. The "Code style invariants" section
  with the no-`mod.rs` rule was added before plan generation.

### `ARCHITECTURE.md` (modify, end-of-implementation)

- Once Skills lands, add a "Skills" section to ARCHITECTURE.md
  documenting:
  - The `SystemPrompt` struct and its slot model.
  - The `SkillsRegistry` / `SkillsRegistrySource` seam.
  - The two JSON-RPC notifications.
  - The CLI flag and its plumbing.
  - The lazy-strict discovery contract.
- This is a documentation deliverable that ships with the final
  vertical-slice phase.

## Implementation phases

Each phase is end-to-end and ships in a clean state. Each phase must
satisfy the project's Definition of Done (cargo fmt, clippy, nextest,
ratchets check) before the next begins.

### Phase 1 — `SystemPrompt` structural refactor

- Introduce `SystemPrompt { base: String, skills: Option<Arc<dyn SkillsRegistrySource>> }`
  in `src/agent.rs`. The `skills` field is declared but no type for it
  exists yet (or stub it as `Option<()>` and bump in Phase 2; preferred:
  introduce the trait as a forward declaration in `src/skills.rs` with
  no implementors).
- Implement `SystemPrompt::render(&self) -> String` returning only
  `self.base.clone()` when `skills` is `None` or empty.
- Change `AgentBuilder::system_prompt` to take `SystemPrompt` instead of
  `impl Into<String>`.
- Update `PendingAgent.system_prompt`, `lib.rs::build_harness_from_config`,
  and every test constructing `PendingAgent` or calling
  `AgentBuilder::system_prompt`.
- Agent loop changes one line: `ContentPart::Text(self.system_prompt.clone())`
  → `ContentPart::Text(self.system_prompt.render())`.
- All existing tests pass unchanged in behavior. Wire `skills: None` at
  every call site.

### Phase 2 — `SkillsRegistry` + trait + empty filesystem impl + slot wiring

- Create `src/skills.rs` plus `src/skills/types.rs`, `source.rs`,
  `registry.rs`, `filesystem.rs`.
- Define `SkillsRegistrySource`, `SkillSummary`, `SkillRecord`,
  `SkillDiagnostic`.
- `SkillsRegistry` implements `SkillsRegistrySource`. `list()` returns
  empty; `get(_)` returns `None`. No discovery logic yet.
- Add `SkillsConfig` to `src/config/topology.rs` and `Config` to
  `src/config.rs`.
- `build_harness_from_config` constructs a `SkillsRegistry` (empty) when
  `config.skills.is_enabled()` and wires `Arc<dyn SkillsRegistrySource>`
  into each agent's `SystemPrompt::skills` slot.
- Add `StubSkillsRegistry` to `src/test_support.rs`.
- Tests: SystemPrompt rendering with and without the skills slot
  (using `StubSkillsRegistry`); `Config` parsing of the `[skills]`
  block (default values, `enabled = false`, custom paths, malformed).

### Phase 3 — Filesystem discovery + JSON-RPC notifications

- Implement `src/skills/discover.rs` (path resolution) and
  `src/skills/parse.rs` (SKILL.md parsing, lenient YAML).
- Implement `FilesystemSkillsRegistry::scan(...)` in
  `src/skills/filesystem.rs`. Returns the catalog plus diagnostics.
- Wire it into `SkillsRegistry` so first access (first call to
  `list()` or `get()`) triggers `scan` exactly once via `OnceLock`.
- Add the two JSON-RPC notifications (`skills_loaded`,
  `skills_diagnostics`) to the notification surface.
- The harness emits both notifications immediately after the first
  `SystemPrompt::render()` call returns.
- Add `SkillsFixture` to `src/test_support.rs`.
- Hardcode `trust_project = false` for this phase; project paths are
  still bypassed (and the bypass diagnostic fires) since Phase 5
  hasn't introduced the flag yet.
- Tests: tempdir-based discovery (one skill, multiple skills,
  shadowing within scope, shadowing cross-scope, malformed YAML, name
  mismatch, missing description, `~` expansion, relative path
  resolution, bypassed paths when `trust_project = false`).

### Phase 4 — `ActivateSkill` tool + dispatch-table refactor + TOML wiring

- Create `src/builtins/activate_skill.rs` with the tool implementation.
- Refactor `register_builtin_tools` in `lib.rs` into the dispatch table.
  All five existing builtins register through the table; behavior
  unchanged.
- Add the `activate_skill` constructor closure to the table. It
  registers iff `[tools.activate_skill]` is declared in `config.tools`
  *and* `BuiltinContext::skills_registry` is `Some`.
- Tests: tool dispatch using `StubSkillsRegistry` (known skill returns
  body; unknown name returns `UnknownSkill` with the known list;
  malformed input returns `InvalidInput`); integration test that
  end-to-end activates a skill from a `Config` containing both
  `[skills]` and `[tools.activate_skill]`.

### Phase 5 — `--trust-project-skills` CLI flag

- Add `trust_project_skills: bool` to `Cli` in `lib.rs`; thread it
  through to `build_harness_from_config` and from there into
  `SkillsRegistry::new(..., trust_project)`.
- Add the corresponding `--trust-project-skills` argparse entry to
  `client.py`, defaulted to `None`, forwarded only if explicitly
  passed. Mirrors the `--model` pattern.
- Verify `justfile` `chat *args` already forwards (it does); no
  justfile change required.
- Tests: CLI parsing accepts the flag at any position; with-flag and
  without-flag tempdir tests confirm project-scope inclusion /
  exclusion and the corresponding `bypassed_path` diagnostics.
- Document the Skills feature end-to-end in ARCHITECTURE.md as the
  final step.

## Testing strategy

- **Unit tests** (cargo nextest): co-located with each module. Cover:
  - `SkillsConfig` parsing (default, explicit, malformed,
    `deny_unknown_fields`).
  - `SystemPrompt::render` (no slot, empty slot, populated slot,
    rendering format).
  - `parse_skill_md` (valid, missing description, unparseable YAML,
    YAML-quoting fallback, name mismatch, oversized name).
  - `resolve_paths` (`~` expansion, cwd resolution, missing `HOME`,
    `trust_project = false` bypass behavior).
  - `FilesystemSkillsRegistry::scan` (one skill, many, shadowing
    within scope, shadowing cross-scope, malformed files, default
    arrays).
  - `ActivateSkill::call` (happy path, unknown name with `known` list,
    invalid input).
  - `register_builtin_tools` dispatch table (all five existing
    builtins register; activate_skill registers iff both prerequisites
    hold).
- **Integration tests** (under `tests/`): exercise the full pipeline
  from a `Config` value (or a test TOML) through the harness assembly,
  agent loop, and tool dispatch. Use `SkillsFixture` for tempdir setup.
- **JSON-RPC notification tests**: verify `skills_loaded` and
  `skills_diagnostics` are emitted, with correct payloads, after the
  first agent turn begins. Use the existing JSON-RPC test
  infrastructure.
- **CLI tests**: extend the existing `cli_accepts_*_flag` patterns in
  `lib.rs` to cover `--trust-project-skills`.
- **Edge cases to exercise explicitly**:
  - Multiple skills with the same name across scopes (project should
    shadow user when project trust is granted; otherwise user wins).
  - Multiple skills with the same name within one scope (last path
    wins).
  - User-supplied `user_paths`/`project_paths` arrays replace the
    defaults rather than merging.
  - Empty registry produces empty system-prompt section, no
    notifications-with-empty-payload weirdness.
  - `[skills] enabled = false` short-circuits discovery entirely.
  - `[skills]` present but no `[tools.activate_skill]` declared:
    catalog renders but tool isn't registered.
  - `[tools.activate_skill]` declared but `[skills]` absent: tool
    construction fails at config-load time (the closure needs
    `BuiltinContext::skills_registry`, which is `None`); covered as a
    config error.
- **Pristine conventions** apply to every test: no `unwrap()` /
  `expect()` outside tests-where-it-improves-readability; typed errors;
  no panic budget regressions on `ratchets check`.

## Open questions

- **Persistent per-project trust.** A `--trust-project-skills` CLI flag
  must be passed each invocation. Long-term we likely want a
  `.pristine/trusted` marker file (or similar) recorded per project.
  Deferred to a future bead.
- **Per-agent skills overrides.** v1 has a single global `[skills]`
  block; all agents see the same registry. A topology where one agent
  has skills and another does not, or where two agents have disjoint
  skill sets, is not expressible. Deferred to a future bead.
- **Tool-schema dynamism.** `ActivateSkill` deliberately ships with a
  static input schema (`{ name: string }`) rather than a JSON Schema
  `enum` of discovered names. The `Tool` trait's `input_schema(&self) -> &Value`
  signature returns a borrowed value, so dynamism would require either
  interior mutability (rejected by RUST_STYLE_GUIDE.md without
  authorization) or a trait change. Deferred; not blocking v1.
- **Configured-but-unused skills.** If `[skills]` is present but no
  agent lists `activate_skill`, the model sees skills in its system
  prompt but can't call them. Treated as a user-authored config
  mistake the engine does not detect. Acceptable for v1; revisit if
  observed to confuse users.
- **`compatibility` frontmatter field.** The Agent Skills spec defines
  `compatibility` as a free-text environment-requirements string. v1
  parses and stores it but does not act on it. Future work could gate
  activation on compatibility checks (e.g., refuse to activate a skill
  whose `compatibility` says "requires git" if git is absent).
- **`allowed-tools` frontmatter field.** Experimental in the spec; v1
  parses it but does not enforce it. Honoring it would require
  integrating with the agent's tool-permission model, which Pristine
  doesn't currently have.
- **Bundled-resource enumeration.** The spec describes an optional
  pattern where activation returns a listing of bundled
  `scripts/`/`references/`/`assets/` files alongside the body. v1
  omits this; the model uses `Read` or `ExecBash` (`ls`) to discover
  resources within the skill directory if needed. Forward-compat is
  preserved by `SkillRecord.directory`.
- **Subagent-delegated skill execution.** An advanced spec pattern
  where the skill runs in a separate subagent session. Not in v1;
  blocked on roadmap item #2 (Multi-Agent Routing).
- **Late-attaching client diagnostic visibility.** `skills_loaded` and
  `skills_diagnostics` fire once at startup. Clients that connect
  after this point miss them. Acceptable for the v1 JSON-RPC stdio
  server (one-client, attached at startup); revisit when multi-client
  scenarios appear.
- **Activation-content protection from future compaction.** Once
  `ContextCompiler` (roadmap item #3) lands, activated skill content
  must be exempt from elision. v1 does not implement compaction at
  all, so this is a forward-compat note rather than a v1 task.
