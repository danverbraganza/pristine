//! Core data types for the Skills feature.
//!
//! These define the stable surface the
//! [`SkillsRegistrySource`](crate::skills::SkillsRegistrySource) trait
//! references. Filesystem discovery populates them by scanning the configured
//! skill paths.

use std::path::PathBuf;

use serde::Serialize;

/// Tier-1 disclosure payload: the name and description surfaced in the system
/// prompt's `## Available skills` section.
///
/// Serializes as `{ "name", "description" }` for inclusion in the
/// `skills_loaded` JSON-RPC notification's `skills` array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
}

/// Activation payload returned when a skill is resolved by name. Carries the
/// on-disk `directory` for future bundled-resource enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRecord {
    pub name: String,
    pub description: String,
    pub body: String,
    pub directory: PathBuf,
}

/// Kind-tagged outcomes recorded during discovery and surfaced to the client
/// via the `skills_diagnostics` JSON-RPC notification.
///
/// Serializes as an internally-tagged enum with `snake_case` kind tags so the
/// notification payload reads `{ "kind": "shadowed", ... }`. The variants are
/// the closed set the requirements doc enumerates. Filesystem discovery
/// produces these during the scan; the registry caches them for the
/// `skills_diagnostics` notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkillDiagnostic {
    /// A skill was shadowed by another of the same name with higher
    /// precedence (later path within a scope, or project over user).
    Shadowed {
        name: String,
        shadowed_path: PathBuf,
        winning_path: PathBuf,
    },
    /// The frontmatter YAML in a `SKILL.md` could not be parsed.
    MalformedYaml { path: PathBuf, reason: String },
    /// The frontmatter `name` did not match the skill's directory name.
    NameMismatch {
        path: PathBuf,
        frontmatter_name: String,
        directory_name: String,
    },
    /// The frontmatter `name` exceeded the maximum permitted length.
    NameTooLong {
        path: PathBuf,
        name: String,
        max: usize,
    },
    /// The frontmatter omitted the required `description` field.
    DescriptionMissing { path: PathBuf },
    /// A project-scope path was skipped because project trust was not granted.
    BypassedPath { path: PathBuf },
    /// A configured scan path could not be resolved (e.g. `~` with no `HOME`).
    ResolutionFailure { path: String, reason: String },
}
