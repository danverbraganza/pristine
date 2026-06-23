//! The engine-owned [`SkillsRegistry`].
//!
//! The concrete storage type for discovered skills, mirroring `ToolRegistry`
//! and `ProviderRegistry`. Discovery is lazy-strict: the registry is
//! constructed empty, and the first call to `list()` or `get()` triggers
//! [`FilesystemSkillsRegistry::scan`] exactly once via the `OnceLock` seam,
//! caching both the discovered catalog and the collected diagnostics in a single
//! [`ScanResult`]. Storing the diagnostics alongside the catalog lets a later
//! phase drain them for the `skills_diagnostics` notification without a second
//! scan or a constructor change.

use std::sync::OnceLock;

use crate::config::SkillsConfig;
use crate::config::discover::ProcessHome;
use crate::skills::filesystem::FilesystemSkillsRegistry;
use crate::skills::source::SkillsRegistrySource;
use crate::skills::types::{SkillDiagnostic, SkillRecord, SkillSummary};

/// Cached outcome of the one-shot filesystem scan: the discovered catalog plus
/// every diagnostic accumulated during discovery. Held behind the registry's
/// `OnceLock` so a single `get_or_init` write populates both fields together.
pub struct ScanResult {
    /// Skills surviving shadowing and disabled filtering, in discovery order.
    pub catalog: Vec<SkillRecord>,
    /// Diagnostics accumulated during discovery (resolution, parse, shadowing).
    pub diagnostics: Vec<SkillDiagnostic>,
}

/// Owned storage for the skills catalog.
///
/// Constructed empty. The `OnceLock` is the lazy-discovery seam: the first
/// access triggers [`FilesystemSkillsRegistry::scan`] exactly once and caches
/// its [`ScanResult`].
pub struct SkillsRegistry {
    /// Resolved configuration driving discovery.
    config: SkillsConfig,
    /// Whether project-scope discovery is permitted. Hardcoded `false` by the
    /// caller until the `--trust-project-skills` flag lands.
    trust_project: bool,
    /// Lazy-discovery seam. Populated on first access with the scanned catalog
    /// and diagnostics.
    scan: OnceLock<ScanResult>,
}

impl SkillsRegistry {
    /// Construct an empty registry from resolved config and the trust flag. No
    /// discovery is performed until first access.
    pub fn new(config: SkillsConfig, trust_project: bool) -> Self {
        Self {
            config,
            trust_project,
            scan: OnceLock::new(),
        }
    }

    /// Return the cached [`ScanResult`], running the filesystem scan exactly
    /// once on first access.
    fn scan_result(&self) -> &ScanResult {
        self.scan.get_or_init(|| {
            let (catalog, diagnostics) =
                FilesystemSkillsRegistry::scan(&self.config, self.trust_project, &ProcessHome);
            ScanResult {
                catalog,
                diagnostics,
            }
        })
    }
}

impl SkillsRegistrySource for SkillsRegistry {
    fn list(&self) -> Vec<SkillSummary> {
        self.scan_result()
            .catalog
            .iter()
            .map(|record| SkillSummary {
                name: record.name.clone(),
                description: record.description.clone(),
            })
            .collect()
    }

    fn get(&self, name: &str) -> Option<SkillRecord> {
        self.scan_result()
            .catalog
            .iter()
            .find(|record| record.name == name)
            .cloned()
    }

    fn summarize(&self) -> Vec<SkillSummary> {
        self.list()
    }

    fn diagnostics(&self) -> Vec<SkillDiagnostic> {
        self.scan_result().diagnostics.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `SkillsConfig` with explicitly empty path arrays so discovery has
    /// nothing to find regardless of the dev machine's home directory. Using
    /// `Some(vec![])` (not `default()`, which resolves to conventional paths)
    /// keeps the empty-contract assertions valid.
    fn empty_config() -> SkillsConfig {
        SkillsConfig {
            enabled: Some(true),
            user_paths: Some(vec![]),
            project_paths: Some(vec![]),
            disabled: vec![],
        }
    }

    #[test]
    fn list_is_empty_over_empty_paths() {
        let registry = SkillsRegistry::new(empty_config(), false);
        assert!(registry.list().is_empty());
    }

    #[test]
    fn get_returns_none_over_empty_paths() {
        let registry = SkillsRegistry::new(empty_config(), false);
        assert!(registry.get("anything").is_none());
    }
}
