# Blueprint state — Skills

**Status:** in-progress Q&A session (blueprint skill active)
**Final plan target:** `blueprint/<slug>/plan-<slug>.md` (will be written by `/blueprint-generate` once Q&A concludes)
**Template:** Default — Overview, Expected behavior, Implementation plan, Implementation phases, Testing strategy, Open questions
**Requirements source:** [`docs/skills-requirements.md`](../../docs/skills-requirements.md)

This file captures the in-progress state of the `/blueprint` Q&A session for
the Skills feature. It exists so the session can be resumed cleanly if context
is lost between turns. It is **not** the plan; `/blueprint-generate` will
produce the plan at `blueprint/<slug>/plan-<slug>.md` once Q&A concludes.

---

## Running refined prompt

> Design and implement the Skills feature for Pristine (roadmap item #1),
> aligned with the Agent Skills open standard at <https://agentskills.io>.
> Requirements are captured in `docs/skills-requirements.md`.
>
> * **Engine, not UI.** Pristine surfaces skill-loading outcomes as structured
>   data via the JSON-RPC surface and never decides display.
> * **Source layout.** New top-level `src/skills/` module for discovery,
>   parsing, and the registry; the `ActivateSkill` tool lives under
>   `src/builtins/activate_skill.rs`.
> * **System prompt becomes structured.** `system_prompt: String` is replaced
>   inside the engine by a `SystemPrompt` struct with named-field slots: a
>   fixed `base: String` and a dynamic skills slot holding
>   `Arc<dyn SkillsRegistrySource>`. Rendering into the system `Turn` is
>   owned by `SystemPrompt`, called once per agent iteration. TOML and
>   config types continue to carry `system_prompt: String`; the structured
>   type is constructed at agent-build time inside
>   `build_harness_from_config`. The skills slot is the sole tier-1
>   disclosure surface — the activation tool's description does not
>   enumerate skills.
> * **System-prompt skills rendering.** Markdown section appended after the
>   base prompt: `## Available skills`, one bullet per skill
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
> * **Discovery timing.** Lazy-strict. Registry is constructed empty; first
>   access (first agent turn's system-prompt render) triggers the
>   filesystem scan. Notifications fire immediately after.
> * **TOML configuration.** Single global `[skills]` block in the topology
>   file. Block-present = enabled; `enabled = false` is the kill-switch.
>   `user_paths`/`project_paths` arrays; omitted = the four conventional
>   defaults (`.agents/skills` then `.pristine/skills` under cwd and home
>   respectively). `disabled = [...]` exact-name list. Per-agent skills
>   overrides are explicitly deferred. `activate_skill` is
>   just-another-tool: needs `[tools.activate_skill]` declared and listed in
>   each agent's `tools = [...]`.
> * **Shadowing precedence.** Cross-scope: project shadows user. Intra-scope:
>   last path in the effective `user_paths`/`project_paths` array wins.
>   With the default arrays, this means `.pristine/skills` shadows
>   `.agents/skills` within each scope. Diagnostics record every shadowing
>   event.
> * **Trust gating.** `--trust-project-skills` CLI flag on `pristine run`,
>   forwarded through `just chat` → `client.py` → `1p`. Without the flag,
>   project-scope discovery is skipped; the bypass is recorded in
>   `skills_diagnostics`. Per-project remembered trust is deferred.
> * **Activation tool shape.** Static name `"activate_skill"`, static
>   description ("activate any of the active skills"), input schema
>   `{ name: string }`. Returns `{ body: "..." }` with frontmatter stripped.
>   On unknown name: `ToolError::Execution` carrying
>   `{ kind: "unknown_skill", name, known: [...] }`.
> * **Diagnostics surface.** Two JSON-RPC notifications fired once after
>   discovery: `skills_loaded` with the catalog
>   (`{ skills: [{ name, description }, ...] }`), and `skills_diagnostics`
>   with an array of kind-tagged entries (`shadowed`, `malformed_yaml`,
>   `name_mismatch`, `description_missing`, `bypassed_path`, …).
> * **Testing.** Three-pronged. Tempdir-based integration tests for
>   `FilesystemSkillsRegistry` discovery behavior. A `StubSkillsRegistry`
>   in `src/test_support.rs` for tests that exercise `SystemPrompt` slot
>   rendering or `ActivateSkill` tool lookup without touching the
>   filesystem. A `SkillsFixture` fluent builder in `src/test_support.rs`
>   to make tempdir-based test setup terse.
> * **TOML schema.** `[skills]` block carries `enabled: bool`,
>   `user_paths: [String]`, `project_paths: [String]`,
>   `disabled: [String]`. All fields optional; block-present is sufficient
>   to enable. `~`, relative, and absolute paths permitted in either path
>   array. `deny_unknown_fields` enforced.
> * **Path resolution timing.** `~` expansion and cwd resolution happen at
>   scan time inside `FilesystemSkillsRegistry`, not at config-parse time.
>   Config carries unresolved strings; resolution failures surface as
>   `skills_diagnostics` entries.
> * **Builtin dispatch.** `register_builtin_tools` is refactored into a
>   `HashMap<&'static str, Box<dyn Fn(&BuiltinContext) -> Result<Arc<dyn Tool>, Error>>>`
>   dispatch table. `BuiltinContext` carries dependencies the closures
>   need (`Option<&Arc<SkillsRegistry>>` in v1); future builtins with
>   dependencies plug in here. `activate_skill` registers iff
>   `[tools.activate_skill]` is declared *and* `[skills]` is present.
> * **Implementation phasing.** Vertical slices, five phases:
>   (1) `SystemPrompt` refactor (`String` → struct with `base` slot only);
>   (2) `SkillsRegistry` + trait + empty filesystem impl + slot wiring;
>   (3) filesystem discovery + JSON-RPC notifications;
>   (4) `ActivateSkill` tool + dispatch-table refactor + TOML wiring;
>   (5) `--trust-project-skills` CLI flag plumbed through
>   `pristine`/`1p`/`client.py`/`justfile`.

---

## Locked Q&A pairs

### Round 1

**Q1 — Source layout.** **Locked: (c).** Discovery/parsing/registry under
`src/skills/`; the `ActivateSkill` tool implementation under
`src/builtins/activate_skill.rs` alongside other built-in tools.

**Q2 — Catalog injection mechanism.** **Locked: structured `SystemPrompt`
with named-field slots, shape (i).** Replaces the Agent's
`system_prompt: String` with a struct carrying a fixed `base: String` and a
dynamic skills slot (held as `Arc<dyn SkillsRegistrySource>`). `SystemPrompt`
owns the rendering of the system `Turn`, called once per agent iteration.
The system-prompt slot is the sole tier-1 disclosure surface; the activation
tool's description does not enumerate skills.

**Q3 — TOML configuration shape.** **Locked: (b) with refinements.** Opt-in
`[skills]` block. Block-present = enabled; `enabled = false` is the explicit
kill-switch. Scan paths split into `user_paths` and `project_paths`; omitted
= the four conventional defaults. `disabled = [...]` exact-name list.
`activate_skill` is "just another tool" — requires its own
`[tools.activate_skill]` declaration and explicit listing in each agent's
`tools = [...]`. No implicit registration; no special-casing.

**Q4 — Trust gating.** **Locked.** v1 introduces a CLI flag
`--trust-project-skills` on `pristine run`, forwarded from `just chat`
through `client.py` to `1p`. Without the flag, project-scope paths are
skipped during discovery and the bypass is recorded in the diagnostics
structure. Per-project remembered trust is deferred.

**Q5 — Activation tool shape.** **Locked.** Static name, static description
("activate any of the active skills"), input schema `{ name: string }`
(not an enum). Returns `{ body: "..." }` with frontmatter stripped. On
unknown name: `ToolError::Execution` carrying
`{ kind: "unknown_skill", name, known: Vec<String> }`. The principle is
recorded explicitly: this does *not* establish that tools must have static
schemas — only that this particular tool doesn't need dynamism in v1.

### Round 2

**Q6 — Registry storage shape.** **Locked: (c).** `SkillsRegistry` concrete
struct + `SkillsRegistrySource` trait. The trait is the architectural seam
(filesystem registry in v1; other implementors plug in later); the concrete
type is what `Harness` owns. Naming follows Pristine's existing `Registry`
convention; the spec's "catalog" word survives as user-facing language only
(in rendered system-prompt text).

**Q7 — Discovery timing.** **Locked: (c) lazy-strict.** The registry is
constructed empty in the harness assembly path; first access (first agent
turn's system-prompt render) triggers the filesystem scan. No background
task. The `skills_loaded` notification fires immediately after that scan
completes. Can be made more sophisticated later if observed startup cost
warrants it.

**Q8 — `SkillsRegistrySource` trait surface.** **Locked: (b).** Synchronous.
`fn list(&self) -> Vec<SkillSummary>` returning `(name, description)` for
tier-1; `fn get(&self, name: &str) -> Option<SkillRecord>` returning
`{ name, description, body, directory }` for activation. The `directory`
field is forward-compat for future resource-enumeration without an API
break.

**Q9 — Diagnostics surface.** **Locked: (b).** Notification-only. A single
`skills_loaded` JSON-RPC notification fires after discovery completes,
carrying the catalog and a diagnostics list (shadowed names, malformed
files, bypassed paths). No persistent diagnostics accessor on `Harness`.

> *Superseded by Q14: the diagnostics surface is now two notifications
> rather than one. The catalog still ships in `skills_loaded`; diagnostics
> ship separately in `skills_diagnostics`.*

**Q10 — System-prompt skills-slot rendering.** **Locked: (a) with revision
flag.** Markdown section appended after the base prompt:

```
## Available skills

- **pdf-processing**: Extract PDF text, fill forms, merge files...
- **data-analysis**: Analyze datasets, generate charts...

To activate a skill, call the `activate_skill` tool with its name.
```

Flagged in code as a v1 choice subject to revision. The configurable-
renderer pattern (option d) is deferred until a second format becomes
necessary.

### Round 3 (default-ordering confirmation appended after Q15)

**Q11 — Within-scope shadowing precedence.** **Locked: (d) last path wins.**
Inside an effective `user_paths` / `project_paths` array, iteration order
matters and a later entry's skill of name `foo` overwrites an earlier
entry's `foo`. With the default arrays
(`[.agents/skills, .pristine/skills]`), this means `.pristine/skills`
shadows `.agents/skills` within each scope — equivalent to "Pristine-native
shadows cross-client" but expressed positionally. Cross-scope and
intra-scope rules compose: discover user_paths first (last-wins inside),
then project_paths (last-wins inside), then project-as-a-whole shadows
user-as-a-whole. User-supplied path arrays **replace** the defaults rather
than merging (consistent with how other list-shaped config fields behave).
Every shadowing event is recorded in `skills_diagnostics`.

**Q12 — Scope of the `[skills]` block.** **Locked: (a) global, single
registry for now.** One discovery pass; all agents with `"activate_skill"`
in their `tools = [...]` see the same registry. Per-agent overrides are
explicitly deferred as future work.

**Q13 — `SystemPrompt` construction site.** **Locked: (a).** TOML and
config types continue to carry `system_prompt: String`. The structured
`SystemPrompt` type stays in-engine and is constructed by
`build_harness_from_config` after the `SkillsRegistry` is available.
Public API ripple is limited to `AgentBuilder::system_prompt(...)` taking
a `SystemPrompt` rather than a string; nothing in the config layer changes.

**Q14 — Diagnostics shape.** **Locked: (d) two notifications.**
`skills_loaded` carries the catalog
(`{ skills: [{ name, description }, ...] }`). `skills_diagnostics` carries
an array of kind-tagged entries
(`{ kind, path?, name?, message }`), with kinds including at least
`shadowed`, `malformed_yaml`, `name_mismatch`, `description_missing`,
`bypassed_path`. No severity field; the client distinguishes by kind. Both
notifications fire once, immediately after lazy-strict discovery completes.

**Q15 — Test fixtures.** **Locked: (c) all three.** Tempdir-based
integration tests for `FilesystemSkillsRegistry` discovery behavior.
`StubSkillsRegistry` in `src/test_support.rs` for slot-rendering /
activation-tool tests. `SkillsFixture` fluent builder in
`src/test_support.rs` for ergonomic tempdir setup. Sits alongside the
existing `EchoTool` fixture.

**Default ordering (post-Q11 confirmation).** Locked: the default arrays
are `[.agents/skills, .pristine/skills]` for both `user_paths` and
`project_paths`. Under last-wins, `.pristine/skills` is the effective
winner within each scope — the user-facing rule is "Pristine-native
overrides cross-client by default."

### Round 4

**Q16 — TOML schema for `[skills]`.** **Locked: (a) flat strings.**
`user_paths` and `project_paths` are arrays of strings; scope is implied
by array membership. `~` is permitted; relative paths permitted; absolute
paths permitted. Concrete shape:

```toml
[skills]
enabled = true
user_paths = ["~/.agents/skills", "~/.pristine/skills"]
project_paths = [".agents/skills", ".pristine/skills"]
disabled = ["some-skill-name"]
```

All fields optional; block-present alone is sufficient to enable. Uses
`#[serde(deny_unknown_fields)]` like other config structs.

**Q17 — Path resolution timing.** **Locked: (a) resolve at scan time.**
`~` expansion reuses the existing `HomeSource` trait from
`src/config/discover.rs`. Relative paths resolve against
`std::env::current_dir()` captured once per scan. Resolution happens
inside `FilesystemSkillsRegistry::scan(...)`, not at config-parse time.
The `Config` struct carries unresolved path strings. Failures (no `HOME`,
no cwd) surface as diagnostics in `skills_diagnostics`, not as
`ConfigError`s during `load`.

**Q18 — Activation tool wiring to TOML.** **Locked: (b) with dispatch
table refactor.** `register_builtin_tools` is refactored from a hardcoded
five-call sequence into a registry-driven dispatch:

```rust
struct BuiltinContext<'a> {
    skills_registry: Option<&'a Arc<SkillsRegistry>>,
    // future builtin dependencies go here
}

type BuiltinCtor =
    Box<dyn Fn(&BuiltinContext<'_>) -> Result<Arc<dyn Tool>, Error>>;
```

The dispatch table is `HashMap<&'static str, BuiltinCtor>`, keyed by the
`builtin` string from `[tools.X]` declarations. Closures are `Box<dyn Fn>`
(per-closure concrete types differ; storage shape requires erasure).
Tools themselves stay `Arc<dyn Tool>` because the existing `ToolRegistry`
and `Agent` already use that shape — `Arc` is required for:
sharing across multiple agents, holding tool references across `.await`
points in the agent loop, and `.list()` returning multiple owned handles.
The dispatch table is driven by `[tools.X]` entries in the topology;
`activate_skill` registers iff (a) `[tools.activate_skill]` is declared
and (b) `[skills]` is present (`BuiltinContext::skills_registry` is
`Some`).

**Q19 — Engine signal when project scope is bypassed.** **Locked:
per-path `bypassed_path` diagnostics only.** No aggregate
`project_trust_required` field; that was feature creep. Clients that want
an aggregate signal can compute it from the per-path entries.

**Q20 — Implementation phasing.** **Locked: (a) vertical slices.** Five
phases, each end-to-end and shippable:

1. `SystemPrompt` structural refactor: `system_prompt: String` →
   `SystemPrompt` struct with `base: String` slot only (no skills slot
   yet). `AgentBuilder::system_prompt` signature changes. All call sites
   migrated. Tests rewritten.
2. `SkillsRegistry` + `SkillsRegistrySource` trait + empty filesystem
   implementation + `SystemPrompt::skills` slot wiring. No discovery
   logic yet (registry yields empty `list()` / `get()`). No activation
   tool. End-to-end: an agent built with skills config but no skills on
   disk runs normally; the skills slot renders to nothing.
3. Filesystem discovery: `FilesystemSkillsRegistry::scan(...)` walks the
   four conventional paths (project paths gated on a placeholder
   `trust_project` flag, defaulting `false`); parses `SKILL.md`;
   populates the registry. `skills_loaded` and `skills_diagnostics`
   notifications wired. Tempdir tests pass.
4. `ActivateSkill` tool + dispatch-table refactor of
   `register_builtin_tools` + `[tools.activate_skill]` config
   integration. End-to-end: an agent with `[skills]` configured *and*
   `activate_skill` in its tool list can call the tool and receive
   skill bodies.
5. `--trust-project-skills` CLI flag wired through `pristine` CLI,
   `1p` CLI, `client.py`, and `justfile`. Project-scope discovery
   becomes properly gated. `bypassed_path` diagnostics fire when the
   flag is absent and project paths exist.

### Round 5

**Q21 — Embedded `default.toml` skills wiring.** **Locked: (b).**
`default.toml` stays skills-free. Out-of-the-box behavior is preserved
bit-for-bit. Users who want skills must supply a `-c` config that opts in.
Implication: phase-2-through-phase-4 end-to-end behavior is exercised in
tests via constructed `Config` values or test-only TOML, not via the
embedded default.

**Q22 — `SystemPrompt::render()` return type.** **Locked: (a) `String`.**
The agent loop continues to construct the `Turn { role: System, content:
vec![ContentPart::Text(...)] }` wrapper itself; `SystemPrompt::render()`
only produces the inner text. Minimal blast radius on the agent loop.

**Q23 — Module layout for `src/skills/`.** **Locked: (b) plus no-`mod.rs`
rule.** Module root is `src/skills.rs`; submodules are
`src/skills/types.rs` (`SkillSummary`, `SkillRecord`),
`src/skills/source.rs` (`SkillsRegistrySource` trait),
`src/skills/registry.rs` (`SkillsRegistry` concrete),
`src/skills/filesystem.rs` (`FilesystemSkillsRegistry` implementor),
`src/skills/discover.rs` (path resolution + scan),
`src/skills/parse.rs` (`SKILL.md` parsing). The no-`mod.rs` constraint
is now codified in `DESIGN.md` under "Code style invariants" (the rule
is also stated in `RUST_STYLE_GUIDE.md`, but that file is rust-bucket-
generated; the DESIGN.md home is the durable surface).

**Q24 — `Config` shape for skills.** **Locked: (b).** `Config` always
carries a `SkillsConfig`; absent block deserializes to
`SkillsConfig::default()` with `enabled: false` semantics. Downstream
consumers branch on content, not presence. Composes cleanly with Q25:
every agent's `SystemPrompt` carries the same
`Arc<dyn SkillsRegistrySource>` reference; the registry is either
populated (when `[skills]` is enabled and skills exist) or empty.
Rendering of the slot is empty-output when `list()` is empty. No
conditional wiring required.

**Q25 — Configured-but-unused skills handling.** **Locked: (e), defer to
default behavior.** No special-case detection of "`[skills]` configured
but no agent lists `activate_skill`." Recorded as known v1 behavior in
the plan's Open Questions section: the model may see skills in its
system prompt but be unable to call them, in which case it'll receive a
`tool not found` error from `ToolRegistry::dispatch` and adapt. Not a
defect; not handled specifically.

### Round 5 follow-up — `client.py` default for `--trust-project-skills`

The CLI flag (`--trust-project-skills`) on `pristine run` requires a
forwarding decision in `client.py` (the demo Python JSON-RPC client used
by `just chat`). Three options:

* **(i)** mirror engine default (off; user must pass the flag through
  `just chat`).
* **(ii)** default on in `client.py` only (engine default off, demo
  client opts in).
* **(iii)** no default in `client.py` — `default=None`, only forwarded
  if explicitly passed. Mirrors the existing `--model` pattern.

**Pending user confirmation.** Coordinator recommendation: (iii) for
symmetry with how `--model` is handled today and to minimize per-flag
drift.

---

## Architectural principles surfaced during Q&A

* **Engine, not UI.** Pristine never decides what to display. Skill-loading
  outcomes are structured data on a programmatic surface (JSON-RPC
  notifications); the client layer renders them. No `eprintln!` for
  skill-discovery diagnostics.
* **No special-casing for the activation tool.** It uses the existing
  `Tool` trait and is configured via `[tools.activate_skill]` like any
  other builtin. The tool is registered through the same `add_tool` path
  as `Read`, `Write`, etc.
* **Static schemas are an expedient, not a principle.** The activation
  tool's input schema is `{ name: string }` (not an enum of discovered
  skill names) because v1 doesn't need dynamism; this does *not* preclude
  future tool-trait extensions that allow dynamic schemas.
* **One open question at a time.** When a question is open, no new
  questions are appended. The current question is discussed to resolution
  before the next set is asked. (User-imposed working rule.)
* **User-supplied config lists replace defaults; they don't merge.**
  Applies to `[skills].user_paths` / `[skills].project_paths` and is
  consistent with existing list-shaped config behavior.

---

## In-progress round

Round 5 is fully locked except for the `client.py` default question (the
Round 5 follow-up section above). Once that's confirmed, all Q&A is
complete and `/blueprint-generate` can be invoked to produce
`blueprint/<slug>/plan-<slug>.md`.

No further question rounds are anticipated. If something material
surfaces during plan generation that wasn't covered, it will be recorded
in the plan's "Open questions" section rather than triggering another
round.
