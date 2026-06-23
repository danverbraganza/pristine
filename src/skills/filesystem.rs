//! Filesystem skill discovery.
//!
//! [`FilesystemSkillsRegistry::scan`] is the discovery entry point: it resolves
//! the configured scan paths via [`discover::resolve_paths`], walks each
//! resolved directory one level deep for skill directories (an immediate
//! subdirectory containing a `SKILL.md`), parses each candidate with
//! [`parse::parse_skill_md`], applies shadowing precedence, and filters out
//! [`SkillsConfig::disabled`] names. It is pure modulo filesystem I/O: no shared
//! state, no logging.
//!
//! Shadowing precedence (requirements doc §5): within a scope, a later path in
//! the effective array wins (last-wins); the project scope wins over the user
//! scope. [`discover::resolve_paths`] returns user paths then project paths and
//! already excludes project paths when project trust is not granted, so simply
//! consuming user paths before project paths and replacing earlier records with
//! later ones of the same name yields the correct precedence. Every shadowing
//! emits a [`SkillDiagnostic::Shadowed`] naming the winning and losing paths.

use std::path::Path;

use crate::config::SkillsConfig;
use crate::config::discover::HomeSource;
use crate::skills::discover::resolve_paths;
use crate::skills::parse::parse_skill_md;
use crate::skills::types::{SkillDiagnostic, SkillRecord};

/// Upper bound on the number of immediate entries inspected per scan path. A
/// guard against runaway traversal on pathological trees; well past any
/// realistic skills directory.
const MAX_ENTRIES_PER_PATH: usize = 4096;

/// Directory names that never hold skills and are skipped during the one-level
/// walk of each scan path.
const NOISE_DIRS: &[&str] = &[".git", "node_modules"];

/// Filesystem-backed skill discovery. A zero-field handle whose sole associated
/// function, [`FilesystemSkillsRegistry::scan`], performs the discovery walk.
pub struct FilesystemSkillsRegistry;

impl FilesystemSkillsRegistry {
    /// Discover skills across the configured scan paths.
    ///
    /// Resolves paths via [`resolve_paths`], walks user paths then project
    /// paths, parses each `SKILL.md`, applies last-wins shadowing (project over
    /// user across scopes; later path over earlier within a scope), and excludes
    /// any name listed in [`SkillsConfig::disabled`]. Returns the final catalog
    /// plus every diagnostic accumulated along the way (path-resolution,
    /// parse-time warnings/skips, and shadowing).
    pub fn scan(
        config: &SkillsConfig,
        trust_project: bool,
        env: &dyn HomeSource,
    ) -> (Vec<SkillRecord>, Vec<SkillDiagnostic>) {
        let (user_paths, project_paths, mut diagnostics) =
            resolve_paths(config, trust_project, env);

        // Catalog accumulated in discovery order. Later insertions of the same
        // name shadow earlier ones (last-wins), emitting a `Shadowed`
        // diagnostic. User paths are consumed before project paths so the
        // project scope wins across scopes.
        let mut catalog: Vec<SkillRecord> = Vec::new();

        for dir in user_paths.iter().chain(project_paths.iter()) {
            for record in scan_one_path(dir, &mut diagnostics) {
                insert_with_shadowing(&mut catalog, record, &mut diagnostics);
            }
        }

        // Exact-name disabled filtering: a disabled skill is excluded from the
        // catalog entirely, not parsed-then-rejected at activation.
        if !config.disabled.is_empty() {
            catalog.retain(|record| !config.disabled.iter().any(|d| d == &record.name));
        }

        (catalog, diagnostics)
    }
}

/// Walk a single resolved scan path one level deep, returning the parsed skill
/// records found in its immediate subdirectories. A missing or non-directory
/// path yields no skills (not an error). Parse-time diagnostics — warnings on
/// loaded skills and the skip diagnostic for rejected ones — are pushed onto
/// `diagnostics`.
fn scan_one_path(dir: &Path, diagnostics: &mut Vec<SkillDiagnostic>) -> Vec<SkillRecord> {
    let mut records = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // A missing/unreadable path is not an error: it simply yields no skills.
        Err(_) => return records,
    };

    for (count, entry) in entries.enumerate() {
        if count >= MAX_ENTRIES_PER_PATH {
            break;
        }
        let Ok(entry) = entry else { continue };
        let candidate = entry.path();
        if !candidate.is_dir() {
            continue;
        }
        if is_noise_dir(&candidate) {
            continue;
        }
        let skill_md = candidate.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        match parse_skill_md(&skill_md) {
            Ok((record, mut warns)) => {
                diagnostics.append(&mut warns);
                records.push(record);
            }
            Err(diag) => diagnostics.push(diag),
        }
    }

    records
}

/// Whether `dir`'s final component is a known noise directory to skip.
fn is_noise_dir(dir: &Path) -> bool {
    dir.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| NOISE_DIRS.contains(&name))
}

/// Insert `record` into `catalog`, applying last-wins shadowing by name. When a
/// record with the same name already exists, it is replaced and a
/// [`SkillDiagnostic::Shadowed`] is emitted naming the losing (replaced) and
/// winning (new) directories.
fn insert_with_shadowing(
    catalog: &mut Vec<SkillRecord>,
    record: SkillRecord,
    diagnostics: &mut Vec<SkillDiagnostic>,
) {
    if let Some(existing) = catalog.iter_mut().find(|r| r.name == record.name) {
        diagnostics.push(SkillDiagnostic::Shadowed {
            name: record.name.clone(),
            shadowed_path: existing.directory.clone(),
            winning_path: record.directory.clone(),
        });
        *existing = record;
    } else {
        catalog.push(record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SkillsConfig;
    use crate::test_support::{MockHome, SkillsFixture};
    use std::path::PathBuf;

    /// Build a `SkillsConfig` whose user/project arrays are exactly `user` /
    /// `project`, bypassing the conventional defaults.
    fn config_with(user: Vec<String>, project: Vec<String>, disabled: Vec<String>) -> SkillsConfig {
        SkillsConfig {
            enabled: Some(true),
            user_paths: Some(user),
            project_paths: Some(project),
            disabled,
        }
    }

    fn path_str(p: &std::path::Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn discovers_a_single_skill() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = SkillsFixture::new()?.add_skill("alpha", "First skill.", "Body A.")?;
        let dir = fixture.path().to_path_buf();
        let config = config_with(vec![path_str(&dir)], vec![], vec![]);

        let (catalog, diags) = FilesystemSkillsRegistry::scan(&config, false, &MockHome::none());

        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "alpha");
        assert_eq!(catalog[0].description, "First skill.");
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        Ok(())
    }

    #[test]
    fn discovers_multiple_skills() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = SkillsFixture::new()?
            .add_skill("alpha", "First.", "A")?
            .add_skill("beta", "Second.", "B")?;
        let dir = fixture.path().to_path_buf();
        let config = config_with(vec![path_str(&dir)], vec![], vec![]);

        let (catalog, _diags) = FilesystemSkillsRegistry::scan(&config, false, &MockHome::none());

        let mut names: Vec<&str> = catalog.iter().map(|r| r.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["alpha", "beta"]);
        Ok(())
    }

    #[test]
    fn later_path_within_scope_shadows_earlier() -> Result<(), Box<dyn std::error::Error>> {
        let early = SkillsFixture::new()?.add_skill("dup", "Early version.", "early")?;
        let late = SkillsFixture::new()?.add_skill("dup", "Late version.", "late")?;
        let config = config_with(
            vec![path_str(early.path()), path_str(late.path())],
            vec![],
            vec![],
        );

        let (catalog, diags) = FilesystemSkillsRegistry::scan(&config, false, &MockHome::none());

        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].description, "Late version.");
        let shadowed: Vec<&SkillDiagnostic> = diags
            .iter()
            .filter(|d| matches!(d, SkillDiagnostic::Shadowed { .. }))
            .collect();
        assert_eq!(shadowed.len(), 1);
        assert!(matches!(
            shadowed[0],
            SkillDiagnostic::Shadowed { name, .. } if name == "dup"
        ));
        Ok(())
    }

    #[test]
    fn project_scope_shadows_user_scope() -> Result<(), Box<dyn std::error::Error>> {
        let user = SkillsFixture::new()?.add_skill("dup", "User version.", "user")?;
        let project = SkillsFixture::new()?.add_skill("dup", "Project version.", "project")?;
        let config = config_with(
            vec![path_str(user.path())],
            vec![path_str(project.path())],
            vec![],
        );

        // Project paths require trust to be visible; test scan directly with
        // trust granted for the cross-scope case.
        let (catalog, diags) = FilesystemSkillsRegistry::scan(&config, true, &MockHome::none());

        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].description, "Project version.");
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, SkillDiagnostic::Shadowed { name, .. } if name == "dup"))
        );
        Ok(())
    }

    #[test]
    fn malformed_yaml_is_skipped_with_diagnostic() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = SkillsFixture::new()?
            .add_raw_skill(
                "broken",
                "---\nname: broken\ndescription:\n  - a\n - b\n---\nBody.\n",
            )?
            .add_skill("good", "Fine skill.", "ok")?;
        let dir = fixture.path().to_path_buf();
        let config = config_with(vec![path_str(&dir)], vec![], vec![]);

        let (catalog, diags) = FilesystemSkillsRegistry::scan(&config, false, &MockHome::none());

        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "good");
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, SkillDiagnostic::MalformedYaml { .. }))
        );
        Ok(())
    }

    #[test]
    fn name_mismatch_loads_with_warning() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = SkillsFixture::new()?.add_raw_skill(
            "actual-dir",
            "---\nname: declared\ndescription: Mismatched.\n---\nBody.\n",
        )?;
        let dir = fixture.path().to_path_buf();
        let config = config_with(vec![path_str(&dir)], vec![], vec![]);

        let (catalog, diags) = FilesystemSkillsRegistry::scan(&config, false, &MockHome::none());

        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "declared");
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, SkillDiagnostic::NameMismatch { .. }))
        );
        Ok(())
    }

    #[test]
    fn missing_description_is_skipped_with_diagnostic() -> Result<(), Box<dyn std::error::Error>> {
        let fixture =
            SkillsFixture::new()?.add_raw_skill("no-desc", "---\nname: no-desc\n---\nBody.\n")?;
        let dir = fixture.path().to_path_buf();
        let config = config_with(vec![path_str(&dir)], vec![], vec![]);

        let (catalog, diags) = FilesystemSkillsRegistry::scan(&config, false, &MockHome::none());

        assert!(catalog.is_empty());
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, SkillDiagnostic::DescriptionMissing { .. }))
        );
        Ok(())
    }

    #[test]
    fn tilde_path_is_expanded() -> Result<(), Box<dyn std::error::Error>> {
        // The fixture lives under a tempdir; treat that tempdir as the home dir
        // and configure a `~/<leaf>` user path to exercise expansion.
        let fixture = SkillsFixture::new()?.add_skill("alpha", "Tilde skill.", "A")?;
        let skill_dir = fixture.path().to_path_buf();
        let home = skill_dir
            .parent()
            .ok_or("fixture path has no parent")?
            .to_path_buf();
        let leaf = skill_dir
            .file_name()
            .ok_or("fixture path has no leaf")?
            .to_string_lossy()
            .into_owned();
        let config = config_with(vec![format!("~/{leaf}")], vec![], vec![]);

        let (catalog, diags) =
            FilesystemSkillsRegistry::scan(&config, false, &MockHome::some(home));

        assert_eq!(catalog.len(), 1, "diags: {diags:?}");
        assert_eq!(catalog[0].name, "alpha");
        Ok(())
    }

    #[test]
    fn relative_path_resolves_against_cwd() -> Result<(), Box<dyn std::error::Error>> {
        // A non-existent relative path resolves against cwd and yields no skills
        // without erroring. (Creating a real relative dir would race with other
        // tests sharing the process cwd.)
        let config = config_with(vec!["does-not-exist/skills".to_string()], vec![], vec![]);

        let (catalog, diags) = FilesystemSkillsRegistry::scan(&config, false, &MockHome::none());

        assert!(catalog.is_empty());
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        Ok(())
    }

    #[test]
    fn untrusted_project_paths_are_bypassed() -> Result<(), Box<dyn std::error::Error>> {
        let project = SkillsFixture::new()?.add_skill("proj", "Project skill.", "P")?;
        let config = config_with(vec![], vec![path_str(project.path())], vec![]);

        let (catalog, diags) = FilesystemSkillsRegistry::scan(&config, false, &MockHome::none());

        assert!(catalog.is_empty(), "project skills must be invisible");
        let bypassed: Vec<&PathBuf> = diags
            .iter()
            .filter_map(|d| match d {
                SkillDiagnostic::BypassedPath { path } => Some(path),
                _ => None,
            })
            .collect();
        assert_eq!(bypassed, vec![project.path()]);
        Ok(())
    }

    #[test]
    fn disabled_name_is_excluded() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = SkillsFixture::new()?
            .add_skill("keep", "Kept.", "K")?
            .add_skill("drop", "Dropped.", "D")?;
        let dir = fixture.path().to_path_buf();
        let config = config_with(vec![path_str(&dir)], vec![], vec!["drop".to_string()]);

        let (catalog, _diags) = FilesystemSkillsRegistry::scan(&config, false, &MockHome::none());

        let names: Vec<&str> = catalog.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["keep"]);
        Ok(())
    }
}
