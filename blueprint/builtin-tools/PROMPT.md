# Prompt: Built-in tools (Read, Write, Edit, Insert, ExecBash)

Inter-phase implementation of five built-in tools that give the agent
filesystem and shell capabilities. Sits between Phase 2 (Tool Calls) and
Phase 3 (Configuration File) per DESIGN.md.

## Guiding principle (foundational)

**Portable shape, adapter dialect.** Landed in ARCHITECTURE.md at commit
`b5da323`. Pristine's portable layer carries information common to every
targeted provider, model, or client. Provider/tool dialect lives at the
periphery. The portable layer never grows speculatively; capabilities are
promoted only when a pattern is shared across multiple consumers.

This inter-phase upholds the principle: each tool defines its own typed
error vocabulary (dialect), but every tool emits errors through the
shared `ToolError::Execution(serde_json::Value)` carrier (portable shape).
Tool advertisement remains the adapter's responsibility, driven by
`ToolSpec`.

## Final refined prompt

* **Path resolution**: absolute paths or paths relative to the process
  cwd. A future CWD tool may change the root mid-session.
* **Five tools** under `src/builtins/{name}.rs` (non-mod-rs submodules),
  tests co-located. Struct names: `Read`, `Write`, `Edit`, `Insert`,
  `ExecBash`. Wire names (snake_case): `read`, `write`, `edit`, `insert`,
  `exec_bash`.
  * **Read** — `{path, start_line?, end_line?}` → `{content: String}`.
    Hard cap 64 KiB. Anything larger → typed error. Strict UTF-8.
  * **Write** — atomic write (`.tmp` + rename) + auto-create parent
    directories. Strict UTF-8.
  * **Edit** — `{path, old_str, new_str}` with str_replace
    match-exactly-once safety. Anthropic-style. Strict UTF-8.
  * **Insert** — `{path, after_line: usize, content}`. `after_line: 0`
    means prepend. Strict UTF-8.
  * **ExecBash** — `{command, timeout_seconds?}`. `/bin/bash -c
    <command>`. cwd and env inherit from `pristine run`. Lossy UTF-8.
  * **ExecBash output**: `{stdout, stderr, status: ExecStatus,
    stdout_truncated, stderr_truncated, has_invalid_utf8_stdout,
    has_invalid_utf8_stderr, execution_id}`. `ExecStatus` is a tagged
    enum (`exit{code}` / `signal{name}` / `timeout`). Last 64 KiB of
    each stream returned. Full output staged in tmp files at
    `tempdir()/pristine-{pid}/{execution_id}.{stdout,stderr}` with
    per-process lifecycle (cleaned at shutdown). Forward-compat staging
    for a future Read-by-id tool that is out of scope this cycle.
* **Construction surface**: every tool has an explicit
  `Tool::new() -> Self` constructor. No arguments today; future hooks
  (audit, path policy, allowlists, metering) land additively. The bare
  `new()` is the stable plugin point.
* **UTF-8 strictness** (revisitable): strict for Read/Write/Edit/Insert
  (non-UTF8 → typed error); lossy for ExecBash (`String::from_utf8_lossy`
  + `has_invalid_utf8_*` markers). Decoupled enough that later cycles
  can flip the policy without changing tool signatures.
* **Error model**: portable shape is
  `ToolError::Execution(serde_json::Value)`. Each tool owns its own
  typed Rust enum (e.g. `EditError`, `ReadError`, `ExecBashError`), each
  deriving `serde::Serialize` with
  `#[serde(tag = "kind", rename_all = "snake_case")]`. No cross-tool
  shared error vocabulary; per-tool autonomy.
* **`AddTool`** stays as `AddTool` (the name is its own identifier; the
  "Tool" suffix is part of the name, not a stripped type-suffix
  convention). Adopts the per-tool error pattern: introduces
  `AddError`.
* **Shell trait**: `Shell` with one method:
  `async fn exec(&self, command: &str, timeout: Duration) -> Result<ShellOutput, ShellError>`.
  `BashShell` is the real impl; `StubShell` (test fixture) makes
  ExecBash unit-testable. Pre-stages future shell variants (`DashShell`,
  `FishShell`, sandboxed runners).
* **SYSTEM_PROMPT**: identity-only. The `add`-tool reference is stripped
  at end-of-cycle. Tool advertisement is the adapter's responsibility,
  driven by `ToolSpec`.
* **Edge-case principle**: if an edge case has a sensible interpretation,
  use it (e.g. Edit with `old_str == new_str` is a no-op success). If
  it's malformed, flag it as a typed error (e.g. Read with
  `start_line > end_line` → `InvalidRange`).
* **Bead plan** (~25 beads):
  * **Prelude** (3 code + 1 doc): ToolError evolution; `AddTool` error
    refactor; Shell trait introduction; ARCHITECTURE doc-prep.
  * **Per-tool** (20 = 5 × 4 slices: types, impl, tests, registration).
    Coordinator orders by risk — advised: ExecBash → Edit → Read /
    Write / Insert. Coordinator may compress slices that land cleanly
    as a single coherent unit.
  * **End-of-cycle**: SYSTEM_PROMPT strip + DESIGN.md Completed bullet
    + one `#[ignore]`-gated chained live-API integration test
    (Read → Edit → ExecBash on a fixture, multi-turn agent loop).
* **Tidy and Reflection are NOT the Plan's responsibility.** The
  Coordinator invokes them naturally along the way per the AGENTS.md
  cadence (Tidy every 2-3 coding tasks; Reflection every 6-8). The Plan
  trusts the Coordinator's judgment.

## Q&A log highlights

The Q&A established the principle, the per-tool shapes, the structured
error model (and pivoted from cross-tool shared kinds to per-tool typed
enums after recognizing the latter was more principle-consistent), the
Shell trait surface, the persistent-id ExecBash tmp-file model, and the
bead phasing. Several pieces of complexity were flagged and explicitly
accepted:

* Persistent ExecBash tmp files stage a future Read-by-id tool that
  doesn't yet exist (forward-compat dead weight, accepted).
* Per-tool error enums duplicate cross-cutting concepts like
  `NotUtf8` across tools (accepted as the cost of dialect ownership).
* `Shell` trait has one user this cycle, justified by future shell
  experimentation (accepted).
* Construction surface is `fn new() -> Self` with no behavioral args
  today; the plugin point exists in principle but is empty in practice
  (accepted).
* End-of-cycle chained integration test is substantially more ambitious
  than Phase 2's `bd-2ms` smoke (accepted).
* 20 per-tool slices may compress to 15 or fewer if a tool's types and
  impl land cleanly as one unit; Coordinator's call.
