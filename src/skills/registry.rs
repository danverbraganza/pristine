//! The engine-owned [`SkillsRegistry`].
//!
//! The concrete storage type for discovered skills, mirroring `ToolRegistry`
//! and `ProviderRegistry`. In Phase 2 it carries no discovery logic: `list()`
//! returns an empty catalog and `get()` returns `None`. The constructor records
//! the resolved [`SkillsConfig`] and the `trust_project` flag so a later phase
//! can trigger the filesystem scan on first access via the `OnceLock` seam
//! without a constructor signature change.

use std::sync::OnceLock;

use crate::config::SkillsConfig;
use crate::skills::source::SkillsRegistrySource;
use crate::skills::types::{SkillRecord, SkillSummary};

/// Owned storage for the skills catalog.
///
/// Constructed empty. The `OnceLock` is the lazy-discovery seam reserved for a
/// later phase; it stays unused in Phase 2 (no scan runs, so it is never
/// initialized).
pub struct SkillsRegistry {
    /// Resolved configuration driving discovery in a later phase.
    config: SkillsConfig,
    /// Whether project-scope discovery is permitted. Hardcoded `false` by the
    /// caller in Phase 2 until the `--trust-project-skills` flag lands.
    trust_project: bool,
    /// Lazy-discovery seam. Holds the scanned catalog after first access in a
    /// later phase; never initialized in Phase 2.
    catalog: OnceLock<Vec<SkillRecord>>,
}

impl SkillsRegistry {
    /// Construct an empty registry from resolved config and the trust flag. No
    /// discovery is performed.
    pub fn new(config: SkillsConfig, trust_project: bool) -> Self {
        Self {
            config,
            trust_project,
            catalog: OnceLock::new(),
        }
    }
}

impl SkillsRegistrySource for SkillsRegistry {
    fn list(&self) -> Vec<SkillSummary> {
        // Phase 2: no discovery. The config and trust flag are retained for the
        // later filesystem scan; reference them so they are not flagged as dead
        // until that phase wires them in.
        let _ = (&self.config, self.trust_project, &self.catalog);
        Vec::new()
    }

    fn get(&self, _name: &str) -> Option<SkillRecord> {
        None
    }
}
