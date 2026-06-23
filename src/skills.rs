//! Skills feature: discovery, parsing, and the registry seam.
//!
//! This module hosts the Skills data types ([`SkillSummary`], [`SkillRecord`],
//! [`SkillDiagnostic`]), the [`SkillsRegistrySource`] trait seam, and the
//! concrete [`SkillsRegistry`]. In Phase 2 the registry is constructed empty
//! (no filesystem discovery); the trait is the abstraction the
//! [`crate::agent::SystemPrompt`] skills slot resolves against, and a future
//! filesystem implementor populates the catalog.

pub mod discover;
pub mod filesystem;
pub mod parse;
pub mod registry;
pub mod source;
pub mod types;

pub use registry::SkillsRegistry;
pub use source::SkillsRegistrySource;
pub use types::{SkillDiagnostic, SkillRecord, SkillSummary};
