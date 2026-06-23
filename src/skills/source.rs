//! The [`SkillsRegistrySource`] read-only trait seam.
//!
//! Mirrors the `Tool`/`ToolRegistry` and `ModelProvider`/`ProviderRegistry`
//! seams: the trait is the abstraction, concrete registries are the
//! implementors. Moved here from the Phase-1 forward declaration in
//! `skills.rs`; re-exported from the module root so existing `use` paths keep
//! working.

use crate::skills::types::{SkillRecord, SkillSummary};

/// Read-only source of skills surfaced to an agent.
///
/// The surface is synchronous and side-effect free from the caller's
/// perspective — `list` powers tier-1 disclosure in the system prompt, `get`
/// resolves a single skill for activation.
pub trait SkillsRegistrySource: Send + Sync {
    /// Catalog of skills for tier-1 system-prompt disclosure.
    fn list(&self) -> Vec<SkillSummary>;

    /// Resolve a single skill by name for activation. Returns `None` when the
    /// name is unknown.
    fn get(&self, name: &str) -> Option<SkillRecord>;
}
