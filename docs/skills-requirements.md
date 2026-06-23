# Skills — Requirements

Status: Draft (requirements only — no design or implementation yet)
Owner: roadmap item #1 in [DESIGN.md](../DESIGN.md)
Last updated: 2026-06-22

This document captures the requirements for the **Skills** feature in Pristine.
Design follow-up (trait shape, config integration, activation tool schema,
discovery scopes, etc.) will be tracked in sibling documents under `docs/` and
ultimately landed in `ARCHITECTURE.md`. Requirements are kept separate so they
can be validated against an external spec without being entangled with
implementation choices.

## 1. Background: the Agent Skills open standard

The `Skills` concept Pristine intends to implement matches an established
external pattern. Anthropic introduced "Agent Skills" on 2025-10-16 and
republished it as a portable cross-client standard on 2025-12-18 at
<https://agentskills.io>. The relevant primary sources are:

- Specification: <https://agentskills.io/specification>
- Client implementation guide: <https://agentskills.io/client-implementation/adding-skills-support>
- Engineering deep-dive: <https://www.anthropic.com/engineering/equipping-agents-for-the-real-world-with-agent-skills>
- Reference skills repo: <https://github.com/anthropics/skills>

Pristine will track this spec as the canonical reference for skill file
format, discovery conventions, and progressive-disclosure semantics.
Deliberate deviations are listed in [§7](#7-deliberate-deviations-from-the-canonical-spec).

## 2. What a skill is

A skill is a **directory** containing, at minimum, a file named exactly
`SKILL.md`. The directory name must match the skill's `name` frontmatter
field. The skill directory may contain arbitrary additional files and
subdirectories; conventional optional subdirectories are `scripts/`,
`references/`, and `assets/`.

```
skill-name/
├── SKILL.md          # required
├── scripts/          # optional: executable resources
├── references/       # optional: additional documentation
├── assets/           # optional: templates, static data
└── ...               # any additional files
```

## 3. `SKILL.md` file format

`SKILL.md` is a Markdown file that begins with YAML frontmatter delimited by
`---` lines, followed by a Markdown body.

### 3.1 Frontmatter — required fields

| Field         | Required | Constraints                                                                                                                 |
| ------------- | -------- | --------------------------------------------------------------------------------------------------------------------------- |
| `name`        | yes      | 1–64 chars; lowercase letters, digits, and hyphens only; no leading/trailing/consecutive hyphens; must match parent dir name. |
| `description` | yes      | 1–1024 chars; non-empty; describes what the skill does and when it should be used.                                          |

### 3.2 Frontmatter — optional fields

| Field           | Constraints                                                                                              |
| --------------- | -------------------------------------------------------------------------------------------------------- |
| `license`       | Short string: license name or reference to a bundled license file.                                       |
| `compatibility` | ≤500 chars. Environment requirements (intended host product, required system packages, network, etc.).  |
| `metadata`      | Arbitrary string→string map for client-specific extensions. Keys SHOULD be namespaced to avoid collisions. |
| `allowed-tools` | Experimental. Space-separated string of pre-approved tool names this skill may invoke.                   |

### 3.3 Body

The Markdown content after the closing `---` is the skill's instructions.
There are no format restrictions on the body. The full body is what gets
loaded into the model's context when the skill is activated.

## 4. Progressive disclosure

Skills use a three-tier loading strategy. Pristine MUST implement all three
tiers:

| Tier | Content                                       | When loaded                                | Typical cost          |
| ---- | --------------------------------------------- | ------------------------------------------ | --------------------- |
| 1    | `name` + `description` (the "catalog")        | Always present in the agent's context     | ~50–100 tok/skill    |
| 2    | Full `SKILL.md` body                          | When the skill is activated                | <5000 tok (target)    |
| 3    | Bundled resources (scripts, references, …)    | When the activated skill references them   | varies                |

Tier 1 is the contract that makes skills cheap: an agent with N installed
skills pays only ~`N * 100` tokens up front, not `N * full-body`.

Tier 3 is the agent's responsibility — once a skill is activated, its body
may reference other files within the skill directory, which the agent loads
using its existing file-read tools (e.g. Pristine's built-in `Read`).
Pristine does **not** eagerly read tier-3 resources at activation time.

## 5. Discovery

### 5.1 Scopes

Pristine MUST scan at least two scopes for skill directories:

- **Project scope**: rooted at the agent's current working directory.
- **User scope**: rooted at the user's home directory.

Other scopes (organization-wide, bundled-with-binary, configured paths) MAY
be supported. The set of enabled scopes SHOULD be configurable via
`pristine-config`.

### 5.2 Search paths within a scope

Within each scope Pristine MUST scan a cross-client path and MAY scan a
Pristine-native path:

| Scope   | Cross-client path (required)         | Pristine-native path (optional)         |
| ------- | ------------------------------------ | --------------------------------------- |
| Project | `<cwd>/.agents/skills/`              | `<cwd>/.pristine/skills/`               |
| User    | `~/.agents/skills/`                  | `~/.pristine/skills/`                   |

`.agents/skills/` is the cross-client convention adopted by compliant
implementations of the Agent Skills standard. Honoring it lets skills
authored for any compliant agent be visible to Pristine and vice versa.

Pristine MAY additionally scan `.claude/skills/` (project and user) for
pragmatic compatibility with skills authored for Claude Code, since this
location is widespread in the wild.

### 5.3 Discovery semantics

- Within a skills directory, a "skill" is any **immediate subdirectory
  containing a file named exactly `SKILL.md`**. Other files are ignored.
- Scanning MUST set sane bounds (e.g., maximum recursion depth, maximum
  directory count) to avoid runaway traversal on pathological trees.
- Common noise directories (`.git/`, `node_modules/`, etc.) SHOULD be
  skipped.

### 5.4 Shadowing / precedence

When two discovered skills share the same `name`, Pristine MUST resolve the
collision deterministically using the following precedence (highest wins):

1. Project scope shadows user scope. **(Local overrides remote.)**
2. Within the same scope, the Pristine-native path shadows the cross-client
   path. (Subject to revision in design; the requirement is that a
   deterministic rule exists and is documented.)
3. Configured paths interleave per their declared order in config.

When a collision causes a skill to be shadowed, Pristine MUST emit a
diagnostic (warning-level) identifying the winning and losing paths so the
user is not silently surprised.

### 5.5 Trust

Project-scope skills come from arbitrary repositories and MAY contain
adversarial instructions. Pristine SHOULD gate project-scope skill loading
behind a trust mechanism (e.g., an explicit "trust this project" decision
recorded in config or a per-project marker). The exact mechanism is a
design question; the requirement is that loading untrusted project-scope
skills is not the default behavior.

## 6. Activation

### 6.1 Activation tool

Pristine MUST provide a built-in tool that activates a skill by name.
Working name: **`activate_skill`** (final name is a design decision).

Requirements:

- Input: a single string argument naming the skill to activate. The schema
  SHOULD constrain this argument to the set of currently-known skill names
  (e.g. as a JSON Schema `enum`) so the model cannot hallucinate a skill
  name. If no skills are discovered, the tool MUST NOT be registered at
  all.
- Output: the skill body (Markdown), optionally wrapped in identifying
  tags, optionally accompanied by an enumeration of files present in the
  skill directory (NOT their contents).
- Side effect: the activation MUST be recorded so subsequent context-
  management logic (see §6.4) can identify the result as protected skill
  content and deduplicate repeated activations.

### 6.2 Catalog injection

At session startup Pristine MUST inject a catalog of all discovered (and
non-filtered) skills into the agent's context. The catalog MUST contain
the `name` and `description` of each skill and MAY contain its on-disk
path. Catalog placement (system prompt vs. activation-tool description)
is a design choice; both are spec-blessed.

A short behavioral preamble alongside the catalog SHOULD tell the model
how to activate skills (e.g., "call `activate_skill` with the name of a
matching skill before proceeding").

If no skills are discovered, the catalog and preamble MUST be omitted
entirely (no empty list rendered).

### 6.3 Filtering

Skills MAY be filtered out of the catalog by user configuration (e.g.,
disabled by name) or by future permission mechanisms. Filtered skills
MUST be excluded from the catalog rather than listed and rejected at
activation time, to avoid wasting model turns on unreachable skills.

### 6.4 Context-window management

Skill body content, once activated, MUST be treated as durable behavioral
guidance:

- If/when Pristine grows a history-compaction or summarization layer
  (roadmap item #3, `ContextCompiler`), activated-skill content MUST be
  exempt from elision. Silent loss of skill instructions would degrade
  behavior with no visible failure mode.
- Pristine SHOULD deduplicate redundant activations: if the model calls
  `activate_skill` for a skill already present in context, the tool MAY
  return a short "already active" acknowledgement rather than re-injecting
  the body.

### 6.5 Explicit user activation (optional, future)

A future extension MAY allow users to activate skills directly (e.g., via
a `/skill-name` slash command at the client layer). This is out of scope
for the initial Skills delivery but should not be foreclosed by the
design.

## 7. Deliberate deviations from the canonical spec

This section records intentional differences from <https://agentskills.io>.
Each deviation MUST be justified.

*(None yet. Populate during design review.)*

Candidate areas where Pristine may diverge — to be decided during design:

- Whether `allowed-tools` (experimental in the spec) is honored at all
  initially. Pristine's tool model is richer than a single bash-style
  pattern string, so a deeper-integrated alternative may be preferable.
- Whether activation strips YAML frontmatter from the body returned to
  the model. The spec describes both options as valid.
- Whether subagent-delegated skill execution (an advanced spec pattern)
  is supported in v1.

## 8. Parsing robustness

- Lenient YAML parsing: skills authored for other clients sometimes
  contain technically-invalid YAML that other parsers tolerate (notably
  unquoted values containing colons). Pristine SHOULD attempt a
  fallback parse (e.g., quoting suspicious values) before discarding.
- Validation severity:
  - Missing or empty `description` → **skip** the skill, log error.
  - Completely unparseable frontmatter → **skip** the skill, log error.
  - `name` doesn't match parent directory, or exceeds 64 chars →
    **warn** and load anyway.
- All diagnostics MUST be recordable for surfacing to the user (debug
  command, log, or future UI), not silently swallowed.

## 9. Non-requirements (out of scope for v1)

- Remote skill registries / package managers.
- A skill-authoring assistant (Anthropic's `skill-creator`).
- Built-in skills shipped inside the Pristine binary.
- Cross-skill dependency declarations.
- Versioning / upgrade flows for installed skills.

These are explicitly deferred. Nothing in v1 should foreclose adding them.

## 10. References

- Agent Skills specification: <https://agentskills.io/specification>
- Client implementation guide: <https://agentskills.io/client-implementation/adding-skills-support>
- "Introducing Agent Skills" (Anthropic, Oct 16 2025):
  <https://www.anthropic.com/news/skills>
- "Equipping agents for the real world with Agent Skills" (Anthropic
  engineering blog):
  <https://www.anthropic.com/engineering/equipping-agents-for-the-real-world-with-agent-skills>
- Reference skills repository: <https://github.com/anthropics/skills>
