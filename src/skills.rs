//! Skills feature: discovery, parsing, and the registry seam.
//!
//! This module is introduced as a forward declaration in Phase 1 of the Skills
//! plan (`blueprint/skills/plan-skills.md`). It currently exposes only the
//! [`SkillsRegistrySource`] trait and its supporting data types
//! ([`SkillSummary`], [`SkillRecord`]); there are no implementors yet. The
//! trait is the architectural seam the [`crate::agent::SystemPrompt`] skills
//! slot resolves against once a concrete registry lands in a later phase.

pub mod types;

pub use types::{SkillRecord, SkillSummary};

/// Read-only source of skills surfaced to an agent.
///
/// Mirrors the `Tool`/`ToolRegistry` and `ModelProvider`/`ProviderRegistry`
/// seams: the trait is the abstraction, concrete registries are the
/// implementors. The surface is synchronous and side-effect free from the
/// caller's perspective — `list` powers tier-1 disclosure in the system
/// prompt, `get` resolves a single skill for activation.
///
/// No implementors exist in Phase 1; this is a forward declaration.
pub trait SkillsRegistrySource: Send + Sync {
    /// Catalog of skills for tier-1 system-prompt disclosure.
    fn list(&self) -> Vec<SkillSummary>;

    /// Resolve a single skill by name for activation. Returns `None` when the
    /// name is unknown.
    fn get(&self, name: &str) -> Option<SkillRecord>;
}
