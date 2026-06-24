//! Skills feature: discovery, parsing, and the registry seam.
//!
//! This module hosts the Skills data types ([`SkillSummary`], [`SkillRecord`],
//! [`SkillDiagnostic`]), the [`SkillsRegistrySource`] trait seam, and the
//! concrete [`SkillsRegistry`]. The registry is constructed empty and runs the
//! filesystem scan lazily on first access; the trait is the abstraction the
//! [`crate::agent::SystemPrompt`] skills slot resolves against, with the
//! filesystem implementor populating the catalog from the configured scan
//! paths.

pub mod discover;
pub mod filesystem;
pub mod parse;
pub mod registry;
pub mod source;
pub mod types;

pub use registry::SkillsRegistry;
pub use source::SkillsRegistrySource;
pub use types::{SkillDiagnostic, SkillRecord, SkillSummary};
