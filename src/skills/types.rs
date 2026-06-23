//! Core data types for the Skills feature.
//!
//! These are introduced ahead of the registry and discovery machinery so the
//! [`SkillsRegistrySource`](crate::skills::SkillsRegistrySource) trait has a
//! stable surface to reference. Later phases populate these via filesystem
//! discovery.

use std::path::PathBuf;

/// Tier-1 disclosure payload: the name and description surfaced in the system
/// prompt's `## Available skills` section.
#[derive(Debug, Clone, PartialEq, Eq)]
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
