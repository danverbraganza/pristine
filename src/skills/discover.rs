//! Skill scan-path resolution.
//!
//! [`resolve_paths`] turns the unresolved string paths carried by
//! [`ResolvedSkillsConfig`] into absolute [`PathBuf`]s at scan time: a leading `~` is
//! expanded against the injected [`HomeSource`] and cwd-relative paths are
//! joined onto the current working directory. Resolution is pure path math
//! modulo reading the home dir and the cwd — no directory walking, no file
//! reads, no logging. Directory scanning is handled by
//! [`filesystem::scan`](crate::skills::filesystem::scan).
//!
//! Path-level failures surface as [`SkillDiagnostic`] entries rather than hard
//! errors: a `~/...` path with no home dir becomes a
//! [`SkillDiagnostic::ResolutionFailure`] and is excluded from the returned
//! vector, and — when project trust is not granted — each effective project
//! path becomes a [`SkillDiagnostic::BypassedPath`] and the project vector is
//! returned empty.

use std::path::PathBuf;

use crate::config::discover::HomeSource;
use crate::config::topology::ResolvedSkillsConfig;
use crate::skills::SkillDiagnostic;

/// Resolve the configured/default skill scan paths to absolute [`PathBuf`]s.
///
/// Returns `(user_paths, project_paths, diagnostics)`:
/// - User paths come from [`ResolvedSkillsConfig::effective_user_paths`], each expanded
///   and resolved to absolute. A `~/...` path with no home dir is dropped and
///   recorded as [`SkillDiagnostic::ResolutionFailure`].
/// - Project paths come from [`ResolvedSkillsConfig::effective_project_paths`]. When
///   `trust_project` is `false` and that list is non-empty, every path is
///   recorded as [`SkillDiagnostic::BypassedPath`] and the returned project
///   vector is empty. When `trust_project` is `true`, each path is resolved the
///   same way as user paths.
///
/// Pure modulo reading the home dir (`env`) and the process cwd. No directory
/// walking, no file reads, no logging.
pub fn resolve_paths(
    config: &ResolvedSkillsConfig,
    trust_project: bool,
    env: &dyn HomeSource,
) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<SkillDiagnostic>) {
    let mut diagnostics = Vec::new();

    let mut user_paths = Vec::new();
    for raw in config.effective_user_paths() {
        match resolve_one(&raw, env) {
            Ok(path) => user_paths.push(path),
            Err(diag) => diagnostics.push(diag),
        }
    }

    let mut project_paths = Vec::new();
    let effective_project = config.effective_project_paths();
    if !trust_project {
        for raw in effective_project {
            diagnostics.push(SkillDiagnostic::BypassedPath {
                path: PathBuf::from(raw),
            });
        }
    } else {
        for raw in effective_project {
            match resolve_one(&raw, env) {
                Ok(path) => project_paths.push(path),
                Err(diag) => diagnostics.push(diag),
            }
        }
    }

    (user_paths, project_paths, diagnostics)
}

/// Resolve a single raw path string to an absolute [`PathBuf`].
///
/// - `~` / `~/...`: expanded against `env`; a missing home dir yields a
///   [`SkillDiagnostic::ResolutionFailure`].
/// - Absolute paths: returned verbatim.
/// - Relative paths: joined onto the current working directory; a cwd read
///   failure yields a [`SkillDiagnostic::ResolutionFailure`].
fn resolve_one(raw: &str, env: &dyn HomeSource) -> Result<PathBuf, SkillDiagnostic> {
    if raw == "~" {
        return env.home_dir().ok_or_else(|| missing_home(raw));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        let home = env.home_dir().ok_or_else(|| missing_home(raw))?;
        return Ok(home.join(rest));
    }

    let path = PathBuf::from(raw);
    if path.is_absolute() {
        return Ok(path);
    }

    let cwd = std::env::current_dir().map_err(|e| SkillDiagnostic::ResolutionFailure {
        path: raw.to_string(),
        reason: format!("current working directory unavailable: {e}"),
    })?;
    Ok(cwd.join(path))
}

/// Build the [`SkillDiagnostic::ResolutionFailure`] for a `~`-prefixed path that
/// could not be expanded because no home directory was available.
fn missing_home(raw: &str) -> SkillDiagnostic {
    SkillDiagnostic::ResolutionFailure {
        path: raw.to_string(),
        reason: "home directory unavailable for `~` expansion".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockHome;

    /// A `ResolvedSkillsConfig` with explicit path arrays, bypassing the
    /// defaults so a test exercises exactly the paths it lists.
    fn config_with(user: Vec<&str>, project: Vec<&str>) -> ResolvedSkillsConfig {
        ResolvedSkillsConfig {
            user_paths: Some(user.into_iter().map(String::from).collect()),
            project_paths: Some(project.into_iter().map(String::from).collect()),
            disabled: Vec::new(),
        }
    }

    #[test]
    fn tilde_expands_against_home_source() {
        let home = MockHome::some("/home/alice");
        let config = config_with(vec!["~/.agents/skills"], vec![]);

        let (user, project, diags) = resolve_paths(&config, true, &home);

        assert_eq!(user, vec![PathBuf::from("/home/alice/.agents/skills")]);
        assert!(project.is_empty());
        assert!(diags.is_empty());
    }

    #[test]
    fn bare_tilde_expands_to_home() {
        let home = MockHome::some("/home/bob");
        let config = config_with(vec!["~"], vec![]);

        let (user, _project, diags) = resolve_paths(&config, true, &home);

        assert_eq!(user, vec![PathBuf::from("/home/bob")]);
        assert!(diags.is_empty());
    }

    #[test]
    fn absolute_path_returned_verbatim() {
        let home = MockHome::some("/home/carol");
        let config = config_with(vec!["/etc/pristine/skills"], vec![]);

        let (user, _project, diags) = resolve_paths(&config, true, &home);

        assert_eq!(user, vec![PathBuf::from("/etc/pristine/skills")]);
        assert!(diags.is_empty());
    }

    #[test]
    fn relative_path_resolves_against_cwd() {
        let home = MockHome::some("/home/dave");
        let config = config_with(vec!["relative/skills"], vec![]);

        let (user, _project, diags) = resolve_paths(&config, true, &home);

        let cwd = std::env::current_dir().expect("cwd readable in test");
        assert_eq!(user, vec![cwd.join("relative/skills")]);
        assert!(user[0].is_absolute());
        assert!(diags.is_empty());
    }

    #[test]
    fn missing_home_records_resolution_failure_and_excludes_path() {
        let home = MockHome::none();
        let config = config_with(vec!["~/.agents/skills", "/abs/skills"], vec![]);

        let (user, _project, diags) = resolve_paths(&config, true, &home);

        // The `~` path is dropped; the absolute path survives.
        assert_eq!(user, vec![PathBuf::from("/abs/skills")]);
        assert_eq!(diags.len(), 1);
        assert!(matches!(
            &diags[0],
            SkillDiagnostic::ResolutionFailure { path, .. } if path == "~/.agents/skills"
        ));
    }

    #[test]
    fn bare_tilde_missing_home_records_resolution_failure() {
        let home = MockHome::none();
        let config = config_with(vec!["~"], vec![]);

        let (user, _project, diags) = resolve_paths(&config, true, &home);

        assert!(user.is_empty());
        assert_eq!(diags.len(), 1);
        assert!(matches!(
            &diags[0],
            SkillDiagnostic::ResolutionFailure { path, .. } if path == "~"
        ));
    }

    #[test]
    fn untrusted_project_paths_are_bypassed_with_diagnostics() {
        let home = MockHome::some("/home/erin");
        let config = config_with(
            vec!["~/.agents/skills"],
            vec![".agents/skills", ".pristine/skills"],
        );

        let (user, project, diags) = resolve_paths(&config, false, &home);

        // User paths still resolve.
        assert_eq!(user, vec![PathBuf::from("/home/erin/.agents/skills")]);
        // Project paths are excluded entirely.
        assert!(project.is_empty());
        // One BypassedPath per effective project path.
        let bypassed: Vec<&PathBuf> = diags
            .iter()
            .filter_map(|d| match d {
                SkillDiagnostic::BypassedPath { path } => Some(path),
                _ => None,
            })
            .collect();
        assert_eq!(
            bypassed,
            vec![
                &PathBuf::from(".agents/skills"),
                &PathBuf::from(".pristine/skills"),
            ]
        );
    }

    #[test]
    fn untrusted_with_no_project_paths_emits_no_bypass() {
        let home = MockHome::some("/home/frank");
        let config = config_with(vec!["~/.agents/skills"], vec![]);

        let (_user, project, diags) = resolve_paths(&config, false, &home);

        assert!(project.is_empty());
        assert!(diags.is_empty());
    }

    #[test]
    fn trusted_project_paths_resolve() {
        let home = MockHome::some("/home/grace");
        let config = config_with(vec![], vec!["/abs/proj/skills"]);

        let (_user, project, diags) = resolve_paths(&config, true, &home);

        assert_eq!(project, vec![PathBuf::from("/abs/proj/skills")]);
        assert!(diags.is_empty());
    }

    #[test]
    fn default_arrays_resolve_for_both_scopes() {
        let home = MockHome::some("/home/heidi");
        // Default config: no explicit path arrays, so the four conventional
        // defaults apply.
        let config = ResolvedSkillsConfig::default();

        let (user, project, diags) = resolve_paths(&config, true, &home);

        assert_eq!(
            user,
            vec![
                PathBuf::from("/home/heidi/.agents/skills"),
                PathBuf::from("/home/heidi/.pristine/skills"),
            ]
        );
        let cwd = std::env::current_dir().expect("cwd readable in test");
        assert_eq!(
            project,
            vec![cwd.join(".agents/skills"), cwd.join(".pristine/skills"),]
        );
        assert!(diags.is_empty());
    }
}
