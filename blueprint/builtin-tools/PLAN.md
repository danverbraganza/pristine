# Plan: Built-in tools (Read, Write, Edit, Insert, ExecBash)

## Context

Phase 2 (Tool Calls) landed the engine machinery for tool use: the `Tool`
trait, `ToolRegistry`, `ContentPart::ToolUse`/`ToolResult`, the agent
run-loop tool-call cycle, and a single demo `AddTool`. This inter-phase
gives the agent actual capability — five built-in tools for filesystem
and shell work — before Phase 3 (Configuration File) generalizes the
construction story.

## Guiding principle

**Portable shape, adapter dialect.** Captured in `ARCHITECTURE.md`
(commit `b5da323`). The portable layer carries information common to
every targeted provider/client; dialect lives at the periphery. This
inter-phase upholds the principle: every tool emits errors through the
shared `ToolError::Execution(serde_json::Value)` carrier (portable
shape), but each tool owns its own typed error vocabulary (dialect).
Tool advertisement remains the adapter's responsibility, driven by
`ToolSpec`.

## Decisions

* **Path resolution**: accept both absolute paths and paths relative to
  the process cwd. A future CWD tool may change the root mid-session.
* **Tool naming**: struct names `Read`, `Write`, `Edit`, `Insert`,
  `ExecBash` reflect the operation itself. Each lives in its own
  submodule under `src/builtins/`. The `AddTool` name is grandfathered
  unchanged ("Tool" is part of its name, not a stripped suffix). Wire
  names use snake_case: `read`, `write`, `edit`, `insert`, `exec_bash`,
  `add`.
* **Construction surface**: every tool has an explicit
  `Tool::new() -> Self` constructor. No behavioral arguments today;
  future hooks (audit, path policy, allowlists, metering) land
  additively. The bare `new()` is the stable plugin point.
* **UTF-8 strictness** (revisitable): strict for Read/Write/Edit/Insert
  (non-UTF8 → typed error); lossy for ExecBash with `has_invalid_utf8_*`
  marker fields.
* **Error model**: portable shape is
  `ToolError::Execution(serde_json::Value)`. Each tool owns a typed
  Rust enum (e.g. `EditError`, `ReadError`) deriving `serde::Serialize`
  with `#[serde(tag = "kind", rename_all = "snake_case")]`. No
  cross-tool error vocabulary.
* **Edge cases**: if an edge case has a sensible interpretation, use it
  (e.g. Edit with `old_str == new_str` is a no-op success). If it's
  malformed, flag it as a typed error. Each tool's impl slice codifies
  its own edge-case decisions and documents them.
* **Shell trait**: `Shell` with one method —
  `async fn exec(&self, command: &str, timeout: Duration) -> Result<ShellOutput, ShellError>`.
  `BashShell` is the real impl; `StubShell` is the test fixture. Lives
  in `src/shell.rs` (or a sibling).
* **ExecBash tmp files**: per-process lifecycle at
  `tempdir()/pristine-{pid}/{execution_id}.{stdout,stderr}`. Cleaned at
  shutdown. Forward-compat staging for a future Read-by-id tool that
  is out of scope this cycle.
* **SYSTEM_PROMPT**: stripped at end of cycle to pure identity. Tool
  advertisement is the adapter's responsibility.
* **Tidy and Reflection are NOT the Plan's responsibility.** The
  Coordinator invokes them naturally per the AGENTS.md cadence (Tidy
  every 2-3 coding tasks; Reflection every 6-8). The Plan trusts the
  Coordinator's judgment.

## Module layout

```
src/
  builtins.rs              # aggregator: pub mod ...; re-exports
  builtins/
    add.rs                 # AddTool + AddError (refactored)
    read.rs                # Read + ReadError
    write.rs               # Write + WriteError
    edit.rs                # Edit + EditError
    insert.rs              # Insert + InsertError
    exec_bash.rs           # ExecBash + ExecBashError + ExecStatus
  shell.rs                 # Shell trait + BashShell + ShellOutput + ShellError
  test_support.rs          # gains StubShell alongside StubArModel
  tool.rs                  # ToolError::Execution(Value) (evolved)
  lib.rs                   # registers builtins in run_async
```

`src/builtins.rs` becomes a non-mod-rs aggregator:

```rust
pub mod add;
pub mod read;
pub mod write;
pub mod edit;
pub mod insert;
pub mod exec_bash;

pub use add::AddTool;
pub use read::Read;
pub use write::Write;
pub use edit::Edit;
pub use insert::Insert;
pub use exec_bash::ExecBash;
```

## Type definitions

### `ToolError::Execution(serde_json::Value)` evolution

```rust
pub enum ToolError {
    NotFound(String),
    InvalidInput(String),
    Execution(serde_json::Value),   // changed from String
    AlreadyRegistered(String),
}
```

The dispatch loop in `Agent::run` (currently wrapping `err.to_string()`
into `{"error": ...}`) is reshaped to forward the Value verbatim. The
`Display` impl renders the Value via `serde_json::to_string` for log
messages.

This is a **breaking change** through the call sites:

* `AgentTool::call` return type — unchanged signature
  (`Result<Value, ToolError>`), but constructors of `Execution` now pass
  a `Value`.
* `Agent::run` dispatch arm:
  ```rust
  Err(err) => {
      let value = match err {
          ToolError::Execution(v) => v,
          other => json!({"error": other.to_string()}),
      };
      (value, true)
  }
  ```
* `AddTool::call` — refactored to use `AddError` (see below).

### `Shell` trait

```rust
#[async_trait::async_trait]
pub trait Shell: Send + Sync {
    async fn exec(
        &self,
        command: &str,
        timeout: std::time::Duration,
    ) -> Result<ShellOutput, ShellError>;
}

pub struct ShellOutput {
    pub stdout: Vec<u8>,        // raw bytes, ExecBash converts
    pub stderr: Vec<u8>,
    pub status: ExecStatus,
}

#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ExecStatus {
    Exit { code: i32 },
    Signal { name: String },
    Timeout,
}

pub enum ShellError {
    Spawn(String),
    Io(String),
}
```

`async_trait` is already in scope via `jsonrpsee::core::async_trait`
(per `src/tool.rs` precedent — no new dependency).

### Per-tool input shapes

```rust
// read.rs
#[derive(serde::Deserialize)]
struct ReadInput {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

// write.rs
#[derive(serde::Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

// edit.rs
#[derive(serde::Deserialize)]
struct EditInput {
    path: String,
    old_str: String,
    new_str: String,
}

// insert.rs
#[derive(serde::Deserialize)]
struct InsertInput {
    path: String,
    after_line: usize,
    content: String,
}

// exec_bash.rs
#[derive(serde::Deserialize)]
struct ExecBashInput {
    command: String,
    timeout_seconds: Option<u64>,
}
```

### Per-tool error enums

```rust
// add.rs
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AddError {
    InvalidInput { reason: String },
}

// read.rs
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReadError {
    FileNotFound { path: String },
    FileTooLarge { size_bytes: u64, max_bytes: u64 },
    NotUtf8 { byte_offset: usize },
    InvalidRange { start_line: usize, end_line: usize },
    InvalidPath { reason: String },
    IoError { reason: String },
}

// write.rs
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WriteError {
    InvalidPath { reason: String },
    PermissionDenied { path: String },
    IoError { reason: String },
}

// edit.rs
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum EditError {
    MultipleMatches { count: u32 },
    NoMatches,
    FileNotFound { path: String },
    NotUtf8 { byte_offset: usize },
    InvalidPath { reason: String },
    IoError { reason: String },
}

// insert.rs
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum InsertError {
    FileNotFound { path: String },
    NotUtf8 { byte_offset: usize },
    InvalidAfterLine { after_line: usize, total_lines: usize },
    InvalidPath { reason: String },
    IoError { reason: String },
}

// exec_bash.rs
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExecBashError {
    Spawn { reason: String },
    Io { reason: String },
    TmpFile { reason: String },
}
```

Each enum is private to its module; the serialized JSON Value is what
flows out through `ToolError::Execution(Value)`. A small helper per tool
converts the enum into the Value:

```rust
fn err(e: ReadError) -> ToolError {
    let value = serde_json::to_value(e)
        .unwrap_or_else(|_| serde_json::json!({"kind": "internal_error"}));
    ToolError::Execution(value)
}
```

(Identical helper per tool, intentionally not factored — per-tool error
ownership means small repetition is acceptable.)

### ExecBash output

```rust
#[derive(serde::Serialize)]
struct ExecBashOutput {
    stdout: String,                // up to 64 KiB, tail-truncated
    stderr: String,                // up to 64 KiB, tail-truncated
    status: ExecStatus,
    stdout_truncated: bool,
    stderr_truncated: bool,
    has_invalid_utf8_stdout: bool,
    has_invalid_utf8_stderr: bool,
    execution_id: String,
}
```

## Per-tool behavioral specifications

### Read

Input: `{path, start_line?, end_line?}`. Output: `{content: String}`.

Behavior:

1. Resolve `path` (absolute, or relative to process cwd).
2. `tokio::fs::metadata` to get file size. If > 64 KiB AND no
   `start_line` AND no `end_line`, return `FileTooLarge`. If the file
   is bigger than 64 KiB but a line range is given, proceed (we trust
   the range to bound output).
3. Open file. Read all bytes.
4. Validate UTF-8. If invalid, return `NotUtf8 { byte_offset }`.
5. If `start_line`/`end_line` set:
   * 1-indexed, inclusive.
   * `start_line > end_line` → `InvalidRange`.
   * `start_line == 0` → `InvalidRange` (lines are 1-indexed).
   * `start_line > total_lines` → empty content (sensible: no error).
   * `end_line > total_lines` → clamp to total_lines (sensible).
6. Apply slicing. Return `{content: String}`.

Edge cases:

* Empty file: `{content: ""}`.
* File ends with `\n`: preserved in `content`.
* CRLF line endings: preserved in `content`; line counting uses `\n`.
* Symlinks: followed; the resolved file's content is returned.
* `path` contains non-UTF-8 components: `InvalidPath`.

### Write

Input: `{path, content}`. Output: `{bytes_written: u64}`.

Behavior:

1. Resolve `path`.
2. Compute parent directory; create recursively if missing
   (`tokio::fs::create_dir_all`).
3. Write `content` to `{path}.tmp`. Then rename `{path}.tmp` → `path`
   (atomic on POSIX).
4. Return `{bytes_written}` (length of `content` in bytes).

Edge cases:

* Existing file: silently overwritten (atomic rename).
* `path` is a directory: `IoError` from rename.
* Permission denied on either tmp create or rename: `PermissionDenied`.

### Edit

Input: `{path, old_str, new_str}`. Output: `{replaced: bool}` (always
`true` on success; the structure exists for forward-compat).

Behavior:

1. Resolve `path`.
2. Read file fully. UTF-8 validate; on failure, `NotUtf8`.
3. Count occurrences of `old_str` in the content (byte-substring match).
4. If `0`: `NoMatches`. If `>= 2`: `MultipleMatches { count }`.
5. If `old_str == new_str`: return `{replaced: true}` without rewriting
   (sensible no-op).
6. Otherwise: replace single occurrence; atomic-rename write (same as
   Write) back to `path`. Return `{replaced: true}`.

Edge cases:

* Empty `old_str`: `NoMatches` (we count occurrences; empty matches
  trivially N+1 times — we reject as `NoMatches` for clarity OR as
  `MultipleMatches`. Coding subagent picks the clearer choice; document
  it in the registration paragraph.)
* `old_str` longer than file: `NoMatches`.
* File with trailing newline: preserved exactly through write.

### Insert

Input: `{path, after_line, content}`. Output: `{lines_inserted: usize}`.

Behavior:

1. Resolve `path`.
2. Read file fully. UTF-8 validate; on failure, `NotUtf8`.
3. Split into lines (preserve trailing newline state).
4. If `after_line > total_lines`: `InvalidAfterLine` (wrong).
5. If `after_line == total_lines`: append (sensible).
6. Else: insert between line `after_line` and line `after_line + 1`.
   `after_line == 0` prepends.
7. Atomic-rename write. Return `{lines_inserted}` (count of `\n` in
   `content`, plus 1 if `content` is non-empty and doesn't end in
   `\n`).

Edge cases:

* Empty `content`: `lines_inserted: 0`; file unchanged on disk (treated
  as no-op write to preserve atomicity).
* `content` without trailing newline: a newline is appended to keep the
  file well-formed when inserting mid-file. Documented behavior.
* File without trailing newline: a newline is added before inserted
  content when appending. Documented behavior.

### ExecBash

Input: `{command, timeout_seconds?}`. Output: `ExecBashOutput`.

Behavior:

1. Generate `execution_id` (UUID v4, hyphenless string).
2. Create per-process tmp dir if not already (lazy initialization,
   keyed by process pid).
3. Open `{tmpdir}/{execution_id}.stdout` and `.stderr` for writing.
4. Call `Shell::exec(command, timeout)`. (`BashShell` impl spawns
   `/bin/bash -c <command>` via `tokio::process::Command`, captures
   stdout/stderr to the tmp files AND to in-memory buffers up to
   64 KiB per stream.)
5. Convert `ShellOutput.stdout`/`stderr` to `String` via
   `String::from_utf8_lossy`. Track whether `U+FFFD` was inserted
   (`has_invalid_utf8_*`).
6. Tail-truncate to 64 KiB if needed; set `*_truncated`.
7. Return `ExecBashOutput` including `execution_id`.

Edge cases:

* Timeout fires: process is killed; status = `Timeout`. Whatever was
  written so far is preserved (tail-truncated to 64 KiB).
* Spawn failure: `ExecBashError::Spawn`.
* Tmp file creation failure: `ExecBashError::TmpFile`.
* Process killed by signal: status = `Signal { name }` where name is
  e.g. `"SIGKILL"`.
* Process exits normally: status = `Exit { code }`.

Process-wide tmp dir cleanup: the Harness gains a small `Drop` on
shutdown that recursively removes `{tempdir}/pristine-{pid}/`. Stored
as a static `OnceCell<TmpDirGuard>` to ensure single initialization and
clean teardown.

## SYSTEM_PROMPT

After this cycle:

```rust
const SYSTEM_PROMPT: &str =
    "You are the Pristine agent. You have an identity that is uniquely yours!";
```

The `add`-tool sentence is removed. Tool advertisement is the adapter's
responsibility.

## Documentation strategy

The principle is already in `ARCHITECTURE.md` (committed). One doc-prep
prelude bead lands the following additions BEFORE tool implementation
begins:

1. New `## Built-in Tools` section with placeholder subsections (each
   subsection is a heading and a one-line stub, filled in by the
   per-tool registration slice).
2. A construction-surface paragraph appended to the existing
   `## Tool Calls` section.
3. An ExecBash tmp-file storage paragraph (since this design decision
   has no other home in the codebase).

Each tool's registration slice adds the real paragraph for that tool's
subsection.

## Testing strategy

* Per-tool unit tests live in each `src/builtins/{name}.rs` `mod tests`
  block. Each tool's tests use `tempfile` (already permitted under
  `tokio::fs` — no new dep) or manual tmp paths under `std::env::temp_dir()`.
* `ExecBash` unit tests use `StubShell` (added to `src/test_support.rs`).
  `StubShell` accepts a script of `Result<ShellOutput, ShellError>` and
  emits them in order, mirroring `StubArModel`'s pattern.
* A separate `#[ignore]`-gated real-subprocess test in
  `tests/exec_bash_smoke.rs` exercises actual `/bin/bash` behavior:
  echo a string, exit non-zero, sleep beyond timeout (signal kill).
* Per-tool coverage target: each tool has at least one happy-path test,
  one edge-case test for each typed error variant it defines, and one
  edge-case test for each "sensible interpretation" case it documents.

## End-of-cycle integration test

In `tests/builtin_tools_live.rs`:

* `#[ignore]`-gated, `ANTHROPIC_API_KEY`-guarded.
* Spawns a real Harness with all five tools (plus `AddTool`) registered.
* Creates a tempfile fixture with simple Python source: `print(1+1)`.
* Sends one `UserMessage`:
  *"Read fixture.py, replace `1+1` with `2+2`, then run it with ExecBash
  and tell me the output."*
* Drains until `AgentEvent::Idle`.
* Asserts:
  * The file's on-disk content was modified.
  * An ExecBash `BlockComplete` is in the event stream.
  * The agent's final `AgentMessage` contains "4" (the result of `2+2`).
* Timeout: 60 seconds (generous, since multi-turn).

## Bead plan

~25 beads. Coordinator orders by risk; advised order:
ExecBash → Edit → Read / Write / Insert. Coordinator may compress
slices that land cleanly as one coherent unit.

### Prelude

**P-1. ToolError evolution.** Change `ToolError::Execution(String)` to
`ToolError::Execution(serde_json::Value)`. Update the `Display` impl to
render the Value as a JSON string. Update `Agent::run`'s dispatch arm
to forward the Value verbatim when `Err(ToolError::Execution(v))`, and
wrap other variants as `{"error": v.to_string()}`. Cascade through
`AddTool` (minimal change — wrap existing string errors as
`json!({"error": ...})` for now; `AddError` lands in P-2). Tests:
existing tool tests continue to pass.

**P-2. AddTool error refactor.** Introduce `AddError` in
`src/builtins.rs` (still single-file before submodule split — or
co-located in `src/builtins/add.rs` if the doc-prep bead has already
established the submodule structure). Refactor `AddTool::call` to
return `Err(err(AddError::InvalidInput { reason: ... }))` via a helper.
Update existing `add_tool_rejects_*` tests to assert on the typed JSON
shape.

**P-3. Shell trait + BashShell + StubShell.** Create `src/shell.rs`
with the `Shell` trait, `BashShell` real impl (spawning
`/bin/bash -c`, capturing to in-memory `Vec<u8>` buffers, killing on
timeout, mapping exit/signal/timeout to `ExecStatus`), and `ShellOutput`
/ `ShellError` types. Add `StubShell` to `src/test_support.rs`.
Register `pub mod shell;` in `src/lib.rs`. The trait is unused this
bead — only ExecBash will use it, in a later bead. Tests: a couple of
`StubShell`-driven tests demonstrating the contract; a single
`#[ignore]`-gated `BashShell` smoke test (echo + exit code).

**P-4. ARCHITECTURE doc-prep.** Add three things to `ARCHITECTURE.md`:
the stub `## Built-in Tools` section with placeholder subheadings for
each of the five tools (each has a heading and a one-line "stub —
filled in by registration bead bd-XXX"); a paragraph on the
construction surface appended to `## Tool Calls`; a paragraph on the
ExecBash tmp-file storage model. Doc-only.

### Per-tool slices (Coordinator-ordered)

For each tool **T** in **{ExecBash, Edit, Read, Write, Insert}**:

**T-1. Types.** Create `src/builtins/{t}.rs` with the input
`#[derive(Deserialize)]` struct, the typed error enum (with
`Serialize` + `serde(tag = "kind")`), and an empty struct for the tool
itself with `Tool::new() -> Self`. No `Tool` trait impl yet (an
`#[allow(dead_code)]` may be needed for the not-yet-used types).

**T-2. Impl.** Implement `Tool for <Type>`: deserialize input, perform
the operation, return typed result Value on success or
`Err(ToolError::Execution(value))` on failure. For ExecBash:
introduce the per-process tmp dir initialization (`OnceCell`) and wire
through the Shell trait.

**T-3. Tests.** Add unit tests covering happy path + each typed error
variant + each documented edge case. For ExecBash, tests use
`StubShell` for the dispatch logic; the `#[ignore]`-gated
`/tests/exec_bash_smoke.rs` separately exercises real bash.

**T-4. Registration.** Add `.add_tool(Arc::new(<Type>::new())?)` to the
HarnessBuilder chain in `src/lib.rs::run_async`. Update
`ARCHITECTURE.md`'s placeholder subsection for that tool with the real
paragraph (input shape, output shape, error variants, edge cases).

20 slices total. Coordinator may compress T-1+T-2 into a single bead
per tool if a particular tool's types and impl land naturally as one
coherent diff. The plan does not enforce a hard split.

### End-of-cycle

**EOC-1. SYSTEM_PROMPT strip.** Remove the `add`-tool sentence from
`SYSTEM_PROMPT` in `src/lib.rs`. The prompt becomes pure identity.
One-line change.

**EOC-2. DESIGN.md Completed bullet.** Add a bullet to DESIGN.md's
`## Completed` section describing this inter-phase.

**EOC-3. Chained live integration test.** Add
`tests/builtin_tools_live.rs` as described in the Testing section.
`#[ignore]`-gated.

Coordinator may merge EOC-1 + EOC-2 + EOC-3 into a single end-of-cycle
bead or keep them separate; per the granularity preference, separate
preferred.

## Definition of Done (per bead, mirrors AGENTS.md)

* `cargo fmt --check`
* `cargo clippy --all-targets --all-features -- -D warnings`
* `cargo nextest run` (within 120s; live tests `#[ignore]`-gated)
* No style violations
* Each bead is one atomic commit with the project's standard trailer

## Definition of Done (inter-phase)

* All 25 beads closed.
* All listed unit tests pass; live tests skip cleanly without
  `ANTHROPIC_API_KEY`.
* `pristine run` registers six tools (`AddTool` + the five new ones)
  and the chained live test exercises Read → Edit → ExecBash end to
  end against a real model.
* `ARCHITECTURE.md` reflects the implemented tools.
* `DESIGN.md` Completed section lists this inter-phase.

## Open items / deferred

* **Read-by-id tool** — uses `execution_id` to fetch full ExecBash
  output from tmp files. Separate inter-phase or part of Phase 3.
* **CWD tool** — changes path-resolution root mid-session. Separate
  inter-phase or part of Phase 3.
* **Other shells** (DashShell, FishShell, sandboxed shells) — separate
  inter-phase. Shell trait is the leverage point.
* **Path traversal / workspace enforcement** — separate inter-phase.
  Construction-surface hooks are the leverage point.
* **UTF-8 policy variations** — strict-everywhere or base64-fallback
  options can flip later without changing tool signatures.
* **Tidy and Reflection** — invoked by the Coordinator naturally
  during the cycle, not scheduled by this plan.
