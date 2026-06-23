//! `SKILL.md` parsing into a [`SkillRecord`] with lenient YAML handling.
//!
//! [`parse_skill_md`] locates the YAML frontmatter delimited by `---` lines,
//! parses it, extracts the Markdown body, and builds a [`SkillRecord`]. Per the
//! requirements doc (§8) the two failure severities are kept distinct:
//!
//! - Fatal (skip the skill, [`Err`]): completely unparseable frontmatter, or a
//!   missing/empty `description`.
//! - Non-fatal (load the skill, warnings ride alongside on the [`Ok`] side): the
//!   `name` not matching the parent directory, or a `name` exceeding 64 chars.
//!
//! Lenient YAML: skills authored for other clients sometimes contain unquoted
//! scalars containing a colon, which strict YAML rejects. When the initial parse
//! fails, a single quoting fallback retries with such suspect values quoted.

use std::path::Path;

use serde::Deserialize;

use crate::skills::types::{SkillDiagnostic, SkillRecord};

/// Maximum permitted `name` length before a warning is emitted.
const MAX_NAME_LEN: usize = 64;

/// Maximum permitted `description` length.
const MAX_DESCRIPTION_LEN: usize = 1024;

/// Frontmatter fields deserialized from a `SKILL.md`.
///
/// Optional fields (`license`, `compatibility`, `metadata`, `allowed-tools`)
/// are parsed so malformed values are caught, but per the bead they are not yet
/// persisted into [`SkillRecord`] nor enforced.
#[derive(Debug, Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    compatibility: Option<String>,
    #[serde(default)]
    metadata: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default, rename = "allowed-tools")]
    allowed_tools: Option<String>,
}

/// Parse a `SKILL.md` at `path` into a [`SkillRecord`] plus any non-fatal
/// warnings.
///
/// Returns `Ok((record, warnings))` when the skill loads — `warnings` carries
/// non-fatal [`SkillDiagnostic`] entries ([`SkillDiagnostic::NameMismatch`],
/// [`SkillDiagnostic::NameTooLong`]) and may be empty. Returns `Err(diagnostic)` when the skill must be skipped: an
/// unreadable file, frontmatter that cannot be parsed even after the lenient
/// fallback, a missing `name`, or a missing/empty `description`.
pub fn parse_skill_md(path: &Path) -> Result<(SkillRecord, Vec<SkillDiagnostic>), SkillDiagnostic> {
    let contents = std::fs::read_to_string(path).map_err(|e| SkillDiagnostic::MalformedYaml {
        path: path.to_path_buf(),
        reason: format!("could not read SKILL.md: {e}"),
    })?;

    let (frontmatter_src, body) =
        split_frontmatter(&contents).ok_or_else(|| SkillDiagnostic::MalformedYaml {
            path: path.to_path_buf(),
            reason: "missing `---`-delimited YAML frontmatter".to_string(),
        })?;

    let frontmatter =
        parse_frontmatter(frontmatter_src).map_err(|reason| SkillDiagnostic::MalformedYaml {
            path: path.to_path_buf(),
            reason,
        })?;

    let name = frontmatter
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| SkillDiagnostic::MalformedYaml {
            path: path.to_path_buf(),
            reason: "frontmatter is missing the required `name` field".to_string(),
        })?;

    let description = frontmatter
        .description
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .ok_or_else(|| SkillDiagnostic::DescriptionMissing {
            path: path.to_path_buf(),
        })?;

    if description.chars().count() > MAX_DESCRIPTION_LEN {
        return Err(SkillDiagnostic::MalformedYaml {
            path: path.to_path_buf(),
            reason: format!("`description` exceeds {MAX_DESCRIPTION_LEN} characters",),
        });
    }

    // Optional fields are parsed above to surface malformed values, but are not
    // yet stored or enforced (deferred to a later phase).
    let _ = (
        &frontmatter.license,
        &frontmatter.compatibility,
        &frontmatter.metadata,
        &frontmatter.allowed_tools,
    );

    let directory = path.parent().map(Path::to_path_buf).unwrap_or_default();

    let mut warnings = Vec::new();

    if name.chars().count() > MAX_NAME_LEN {
        warnings.push(SkillDiagnostic::NameTooLong {
            path: path.to_path_buf(),
            name: name.clone(),
            max: MAX_NAME_LEN,
        });
    }

    let dir_name = directory_name(&directory);
    if !dir_name.is_empty() && dir_name != name {
        warnings.push(SkillDiagnostic::NameMismatch {
            path: path.to_path_buf(),
            frontmatter_name: name.clone(),
            directory_name: dir_name,
        });
    }

    let record = SkillRecord {
        name,
        description,
        body: body.to_string(),
        directory,
    };

    Ok((record, warnings))
}

/// Split a `SKILL.md` into its raw YAML frontmatter and the Markdown body.
///
/// The frontmatter is the region between the leading `---` line and the next
/// `---` line. Returns `None` when no such delimited region exists.
fn split_frontmatter(contents: &str) -> Option<(&str, &str)> {
    let rest = contents.strip_prefix("---")?;
    // The opening fence must be its own line: what follows `---` is either a
    // newline or end-of-input.
    let rest = rest.strip_prefix('\n').or_else(|| {
        rest.strip_prefix("\r\n")
            .or(if rest.is_empty() { Some(rest) } else { None })
    })?;

    let close = find_closing_fence(rest)?;
    let frontmatter = &rest[..close.start];
    let body = &rest[close.end..];
    Some((frontmatter, body))
}

/// Byte range of a closing `---` fence line within `rest`.
struct Fence {
    /// Offset where the fence line begins.
    start: usize,
    /// Offset where the body begins (just past the fence line's newline).
    end: usize,
}

/// Locate the closing `---` fence line in the post-opening-fence remainder.
fn find_closing_fence(rest: &str) -> Option<Fence> {
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            return Some(Fence {
                start: offset,
                end: offset + line.len(),
            });
        }
        offset += line.len();
    }
    None
}

/// Parse the raw frontmatter, retrying with a lenient quoting fallback if the
/// strict parse fails on an unquoted scalar containing a colon.
fn parse_frontmatter(src: &str) -> Result<Frontmatter, String> {
    match serde_norway::from_str::<Frontmatter>(src) {
        Ok(parsed) => Ok(parsed),
        Err(first_err) => match serde_norway::from_str::<Frontmatter>(&quote_colon_values(src)) {
            Ok(parsed) => Ok(parsed),
            Err(_) => Err(format!("invalid YAML frontmatter: {first_err}")),
        },
    }
}

/// Rewrite `key: value` lines whose value contains an unquoted, unescaped colon
/// into `key: "value"` so a strict YAML parser accepts them.
///
/// Conservative: only top-level `key: value` lines are rewritten, the value is
/// left untouched if it already looks quoted or begins a block/flow construct,
/// and embedded double quotes are escaped.
fn quote_colon_values(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.split_inclusive('\n') {
        let newline_len = line.len() - line.trim_end_matches(['\r', '\n']).len();
        let content = &line[..line.len() - newline_len];
        let trailing = &line[line.len() - newline_len..];

        if let Some(rewritten) = rewrite_colon_line(content) {
            out.push_str(&rewritten);
        } else {
            out.push_str(content);
        }
        out.push_str(trailing);
    }
    out
}

/// Rewrite a single `key: value` line if its value is an unquoted scalar
/// containing a colon. Returns `None` when no rewrite applies.
fn rewrite_colon_line(content: &str) -> Option<String> {
    // Preserve leading indentation; only handle simple `key: value` shapes.
    let indent_len = content.len() - content.trim_start().len();
    let (indent, body) = content.split_at(indent_len);
    let colon = body.find(": ")?;
    let key = &body[..colon];
    let value = body[colon + 2..].trim();

    if key.is_empty() || key.contains(' ') || key.contains(':') {
        return None;
    }
    if value.is_empty() {
        return None;
    }
    // Already quoted, or a structural value we must not wrap.
    let first = value.chars().next()?;
    if matches!(first, '"' | '\'' | '[' | '{' | '|' | '>' | '#' | '&' | '*') {
        return None;
    }
    // Only rewrite when the value actually contains a colon — that is the case
    // strict YAML rejects.
    if !value.contains(':') {
        return None;
    }

    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    Some(format!("{indent}{key}: \"{escaped}\""))
}

/// The final component of a directory path, or an empty string if absent.
fn directory_name(directory: &Path) -> String {
    directory
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    use tempfile::TempDir;

    /// Write `contents` to `<tmp>/<dir_name>/SKILL.md` and return its path.
    fn write_skill(
        tmp: &TempDir,
        dir_name: &str,
        contents: &str,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let dir = tmp.path().join(dir_name);
        fs::create_dir_all(&dir)?;
        let path = dir.join("SKILL.md");
        fs::write(&path, contents)?;
        Ok(path)
    }

    #[test]
    fn valid_skill_parses() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = TempDir::new()?;
        let path = write_skill(
            &tmp,
            "pdf-tools",
            "---\nname: pdf-tools\ndescription: Work with PDF files.\n---\nBody text here.\n",
        )?;

        let (record, warnings) = parse_skill_md(&path).map_err(|d| format!("{d:?}"))?;

        assert_eq!(record.name, "pdf-tools");
        assert_eq!(record.description, "Work with PDF files.");
        assert_eq!(record.body, "Body text here.\n");
        assert_eq!(record.directory, tmp.path().join("pdf-tools"));
        assert!(warnings.is_empty());
        Ok(())
    }

    #[test]
    fn optional_fields_are_tolerated() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = TempDir::new()?;
        let path = write_skill(
            &tmp,
            "rich",
            concat!(
                "---\n",
                "name: rich\n",
                "description: Has optional fields.\n",
                "license: MIT\n",
                "compatibility: needs network\n",
                "allowed-tools: Read Write\n",
                "metadata:\n",
                "  vendor.key: value\n",
                "---\n",
                "Body.\n",
            ),
        )?;

        let (record, warnings) = parse_skill_md(&path).map_err(|d| format!("{d:?}"))?;
        assert_eq!(record.name, "rich");
        assert!(warnings.is_empty());
        Ok(())
    }

    #[test]
    fn missing_description_is_fatal() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = TempDir::new()?;
        let path = write_skill(&tmp, "no-desc", "---\nname: no-desc\n---\nBody.\n")?;

        let err = parse_skill_md(&path).unwrap_err();
        assert!(matches!(err, SkillDiagnostic::DescriptionMissing { .. }));
        Ok(())
    }

    #[test]
    fn empty_description_is_fatal() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = TempDir::new()?;
        let path = write_skill(
            &tmp,
            "blank-desc",
            "---\nname: blank-desc\ndescription: \"   \"\n---\nBody.\n",
        )?;

        let err = parse_skill_md(&path).unwrap_err();
        assert!(matches!(err, SkillDiagnostic::DescriptionMissing { .. }));
        Ok(())
    }

    #[test]
    fn unparseable_yaml_is_fatal() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = TempDir::new()?;
        // A broken block-sequence indentation strict YAML rejects, with no
        // single-scalar quoting fallback that recovers it.
        let path = write_skill(
            &tmp,
            "broken",
            "---\nname: broken\ndescription:\n  - a\n - b\n---\nBody.\n",
        )?;

        let err = parse_skill_md(&path).unwrap_err();
        assert!(matches!(err, SkillDiagnostic::MalformedYaml { .. }));
        Ok(())
    }

    #[test]
    fn missing_frontmatter_is_fatal() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = TempDir::new()?;
        let path = write_skill(&tmp, "no-fm", "Just a body, no frontmatter.\n")?;

        let err = parse_skill_md(&path).unwrap_err();
        assert!(matches!(err, SkillDiagnostic::MalformedYaml { .. }));
        Ok(())
    }

    #[test]
    fn quoting_fallback_recovers_unquoted_colon_value() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = TempDir::new()?;
        // `description` value has an unquoted colon that strict YAML rejects.
        let path = write_skill(
            &tmp,
            "colon",
            "---\nname: colon\ndescription: Use this: when handling colons.\n---\nBody.\n",
        )?;

        let (record, warnings) = parse_skill_md(&path).map_err(|d| format!("{d:?}"))?;
        assert_eq!(record.description, "Use this: when handling colons.");
        assert!(warnings.is_empty());
        Ok(())
    }

    #[test]
    fn name_mismatch_warns_but_loads() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = TempDir::new()?;
        let path = write_skill(
            &tmp,
            "actual-dir",
            "---\nname: declared-name\ndescription: Mismatched name.\n---\nBody.\n",
        )?;

        let (record, warnings) = parse_skill_md(&path).map_err(|d| format!("{d:?}"))?;
        assert_eq!(record.name, "declared-name");
        assert_eq!(warnings.len(), 1);
        assert!(matches!(
            &warnings[0],
            SkillDiagnostic::NameMismatch {
                frontmatter_name,
                directory_name,
                ..
            } if frontmatter_name == "declared-name" && directory_name == "actual-dir"
        ));
        Ok(())
    }

    #[test]
    fn oversized_name_warns_but_loads() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = TempDir::new()?;
        let long = "a".repeat(MAX_NAME_LEN + 1);
        let contents = format!("---\nname: {long}\ndescription: Long name.\n---\nBody.\n");
        let path = write_skill(&tmp, &long, &contents)?;

        let (record, warnings) = parse_skill_md(&path).map_err(|d| format!("{d:?}"))?;
        assert_eq!(record.name, long);
        // Directory matches name, so the only warning is the oversize one.
        assert_eq!(warnings.len(), 1);
        assert!(matches!(
            &warnings[0],
            SkillDiagnostic::NameTooLong { name, max, .. }
                if name == &long && *max == MAX_NAME_LEN
        ));
        Ok(())
    }

    #[test]
    fn oversized_name_and_mismatch_yield_distinct_diagnostics()
    -> Result<(), Box<dyn std::error::Error>> {
        let tmp = TempDir::new()?;
        let long = "a".repeat(MAX_NAME_LEN + 1);
        // Directory differs from the (oversized) frontmatter name.
        let contents = format!("---\nname: {long}\ndescription: Long name.\n---\nBody.\n");
        let path = write_skill(&tmp, "short-dir", &contents)?;

        let (record, warnings) = parse_skill_md(&path).map_err(|d| format!("{d:?}"))?;
        assert_eq!(record.name, long);

        // Exactly one of each distinct diagnostic — no duplicate NameMismatch.
        assert_eq!(warnings.len(), 2);

        let too_long = warnings
            .iter()
            .filter(|d| matches!(d, SkillDiagnostic::NameTooLong { .. }))
            .count();
        let mismatch = warnings
            .iter()
            .filter(|d| matches!(d, SkillDiagnostic::NameMismatch { .. }))
            .count();
        assert_eq!(too_long, 1, "expected exactly one NameTooLong");
        assert_eq!(mismatch, 1, "expected exactly one NameMismatch");

        assert!(warnings.iter().any(|d| matches!(
            d,
            SkillDiagnostic::NameTooLong { name, max, .. }
                if name == &long && *max == MAX_NAME_LEN
        )));
        assert!(warnings.iter().any(|d| matches!(
            d,
            SkillDiagnostic::NameMismatch { frontmatter_name, directory_name, .. }
                if frontmatter_name == &long && directory_name == "short-dir"
        )));
        Ok(())
    }
}
