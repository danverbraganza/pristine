//! `{{ENV_VAR}}` templating over a parsed `toml::Value` tree.
//!
//! Substitution happens after TOML lex/parse but before struct deserialization:
//! the input is first parsed into a generic `toml::Value`, then this module
//! walks the tree and rewrites every string node, replacing `{{NAME}}`
//! placeholders with values supplied by an `EnvSource`. Missing variables are
//! recorded into a `ConfigErrors` aggregate so the caller can report every
//! problem at once rather than peeling them off one at a time.
//!
//! Placeholder syntax is intentionally narrow: exactly `{{NAME}}` with `NAME`
//! matching `[A-Za-z_][A-Za-z0-9_]*`. Anything else inside `{{...}}` is left
//! verbatim. Partial markers (`{{ABC`, `ABC}}`, `{ABC}`) are also literal.

use crate::config::error::{ConfigError, ConfigErrors};

/// Abstraction over the process environment used during templating. The
/// production impl reads from `std::env`; tests use an in-memory map.
pub trait EnvSource {
    /// Look up `name` and return its value if present.
    fn get(&self, name: &str) -> Option<String>;
}

/// Default `EnvSource` backed by `std::env::var`.
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// Recursively walk `value` and substitute `{{NAME}}` placeholders in every
/// string node. Missing variables are appended to `errors` as
/// `ConfigError::UnknownEnvVar` and the placeholder is replaced with an empty
/// string so deserialization can proceed; the caller is expected to check
/// `errors.is_empty()` before trusting the resulting tree.
pub fn template_value<E: EnvSource>(value: &mut toml::Value, env: &E, errors: &mut ConfigErrors) {
    template_value_at(value, env, errors, String::new());
}

fn template_value_at<E: EnvSource>(
    value: &mut toml::Value,
    env: &E,
    errors: &mut ConfigErrors,
    location: String,
) {
    match value {
        toml::Value::String(s) => {
            let replaced = template_string(s, env, &location, errors);
            *s = replaced;
        }
        toml::Value::Array(arr) => {
            for (idx, item) in arr.iter_mut().enumerate() {
                let child_location = if location.is_empty() {
                    format!("[{idx}]")
                } else {
                    format!("{location}[{idx}]")
                };
                template_value_at(item, env, errors, child_location);
            }
        }
        toml::Value::Table(tbl) => {
            for (key, item) in tbl.iter_mut() {
                let child_location = if location.is_empty() {
                    key.clone()
                } else {
                    format!("{location}.{key}")
                };
                template_value_at(item, env, errors, child_location);
            }
        }
        toml::Value::Integer(_)
        | toml::Value::Float(_)
        | toml::Value::Boolean(_)
        | toml::Value::Datetime(_) => {}
    }
}

/// Substitute every `{{NAME}}` placeholder in `input`. Unrecognized
/// placeholders (bad identifier shape) and partial markers are preserved
/// verbatim. Missing env vars produce a `ConfigError::UnknownEnvVar` and the
/// placeholder is replaced with an empty string.
fn template_string<E: EnvSource>(
    input: &str,
    env: &E,
    location: &str,
    errors: &mut ConfigErrors,
) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len()
            && bytes[i] == b'{'
            && bytes[i + 1] == b'{'
            && let Some((name, end)) = try_parse_placeholder(bytes, i)
        {
            match env.get(name) {
                Some(value) => out.push_str(&value),
                None => {
                    errors.push(ConfigError::UnknownEnvVar {
                        name: name.to_string(),
                        location: location.to_string(),
                    });
                }
            }
            i = end;
            continue;
        }
        // Pushing one byte at a time is safe because `input` is valid UTF-8
        // and we only branch at ASCII `{`; non-ASCII multi-byte sequences are
        // copied byte-by-byte without splitting code points.
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Try to read a `{{NAME}}` placeholder starting at `start` (which must point
/// at the first `{` of a `{{` pair). On success returns the identifier and the
/// byte index immediately after the closing `}}`. Returns `None` if the bytes
/// do not form a valid placeholder; the caller should then treat the opening
/// `{{` as literal text.
fn try_parse_placeholder(bytes: &[u8], start: usize) -> Option<(&str, usize)> {
    debug_assert!(start + 1 < bytes.len() && bytes[start] == b'{' && bytes[start + 1] == b'{');
    let name_start = start + 2;
    if name_start >= bytes.len() {
        return None;
    }
    let first = bytes[name_start];
    if !is_ident_start(first) {
        return None;
    }
    let mut cursor = name_start + 1;
    while cursor < bytes.len() && is_ident_cont(bytes[cursor]) {
        cursor += 1;
    }
    if cursor + 1 >= bytes.len() {
        return None;
    }
    if bytes[cursor] != b'}' || bytes[cursor + 1] != b'}' {
        return None;
    }
    // SAFETY: `name_start..cursor` was built from ASCII-only bytes, so it is
    // valid UTF-8.
    let name = std::str::from_utf8(&bytes[name_start..cursor]).ok()?;
    Some((name, cursor + 2))
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MapEnv;

    fn render(input: &str, env: &impl EnvSource) -> (String, ConfigErrors) {
        let mut value = toml::Value::String(input.to_string());
        let mut errors = ConfigErrors::new();
        template_value(&mut value, env, &mut errors);
        match value {
            toml::Value::String(s) => (s, errors),
            other => panic!("expected String, got {other:?}"),
        }
    }

    #[test]
    fn substitutes_single_placeholder() {
        let env = MapEnv::new([("NAME", "world")]);
        let (out, errors) = render("hello {{NAME}}!", &env);
        assert_eq!(out, "hello world!");
        assert!(errors.is_empty());
    }

    #[test]
    fn missing_env_var_records_error_and_empties_placeholder() {
        let env = MapEnv::default();
        let (out, errors) = render("{{NAME}}", &env);
        assert_eq!(out, "");
        assert_eq!(errors.len(), 1);
        match &errors.as_slice()[0] {
            ConfigError::UnknownEnvVar { name, location: _ } => {
                assert_eq!(name, "NAME");
            }
            other => panic!("expected UnknownEnvVar, got {other:?}"),
        }
    }

    #[test]
    fn multi_occurrence_substitutes_all() {
        let env = MapEnv::new([("A", "1"), ("B", "2")]);
        let (out, errors) = render("{{A}} {{B}} {{A}}", &env);
        assert_eq!(out, "1 2 1");
        assert!(errors.is_empty());
    }

    #[test]
    fn mid_string_substitution_preserves_surroundings() {
        let env = MapEnv::new([("X", "value")]);
        let (out, errors) = render("prefix {{X}} suffix", &env);
        assert_eq!(out, "prefix value suffix");
        assert!(errors.is_empty());
    }

    #[test]
    fn partial_marker_is_literal() {
        let env = MapEnv::new([("NOT_CLOSED", "x")]);
        let (out, errors) = render("{{NOT_CLOSED", &env);
        assert_eq!(out, "{{NOT_CLOSED");
        assert!(errors.is_empty());
    }

    #[test]
    fn trailing_marker_is_literal() {
        let env = MapEnv::new([("X", "v")]);
        let (out, errors) = render("ABC}}", &env);
        assert_eq!(out, "ABC}}");
        assert!(errors.is_empty());
    }

    #[test]
    fn single_brace_marker_is_literal() {
        let env = MapEnv::new([("X", "v")]);
        let (out, errors) = render("{X}", &env);
        assert_eq!(out, "{X}");
        assert!(errors.is_empty());
    }

    #[test]
    fn invalid_identifier_inside_marker_is_literal() {
        let env = MapEnv::new([("X", "v")]);
        let (out, errors) = render("{{1NAME}}", &env);
        assert_eq!(out, "{{1NAME}}");
        assert!(errors.is_empty());
    }

    #[test]
    fn empty_marker_is_literal() {
        let env = MapEnv::default();
        let (out, errors) = render("{{}}", &env);
        assert_eq!(out, "{{}}");
        assert!(errors.is_empty());
    }

    #[test]
    fn special_chars_in_env_value_are_copied_verbatim() {
        let env = MapEnv::new([("K", r#"has "quotes" and \backslashes"#)]);
        let (out, errors) = render("{{K}}", &env);
        assert_eq!(out, r#"has "quotes" and \backslashes"#);
        assert!(errors.is_empty());
    }

    #[test]
    fn nested_location_reflects_table_path() {
        let toml_input = r#"
[models.default]
api_key = "{{MISSING}}"
"#;
        let mut tree = toml::from_str::<toml::Value>(toml_input).expect("parse toml");
        let env = MapEnv::default();
        let mut errors = ConfigErrors::new();
        template_value(&mut tree, &env, &mut errors);
        assert_eq!(errors.len(), 1);
        match &errors.as_slice()[0] {
            ConfigError::UnknownEnvVar { name, location } => {
                assert_eq!(name, "MISSING");
                assert!(
                    location.contains("models")
                        && location.contains("default")
                        && location.contains("api_key"),
                    "location {location:?} should reference the nested key path"
                );
            }
            other => panic!("expected UnknownEnvVar, got {other:?}"),
        }
    }

    #[test]
    fn array_location_includes_index() {
        let toml_input = r#"
values = ["{{MISSING}}"]
"#;
        let mut tree = toml::from_str::<toml::Value>(toml_input).expect("parse toml");
        let env = MapEnv::default();
        let mut errors = ConfigErrors::new();
        template_value(&mut tree, &env, &mut errors);
        assert_eq!(errors.len(), 1);
        match &errors.as_slice()[0] {
            ConfigError::UnknownEnvVar { location, .. } => {
                assert!(
                    location.contains("values") && location.contains("[0]"),
                    "location {location:?} should reference the array index"
                );
            }
            other => panic!("expected UnknownEnvVar, got {other:?}"),
        }
    }

    #[test]
    fn non_string_values_are_left_alone() {
        let toml_input = r#"
count = 7
flag = true
ratio = 1.5
"#;
        let mut tree = toml::from_str::<toml::Value>(toml_input).expect("parse toml");
        let env = MapEnv::default();
        let mut errors = ConfigErrors::new();
        template_value(&mut tree, &env, &mut errors);
        assert!(errors.is_empty());
        let table = tree.as_table().expect("root is a table");
        assert_eq!(table.get("count").and_then(|v| v.as_integer()), Some(7));
        assert_eq!(table.get("flag").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(table.get("ratio").and_then(|v| v.as_float()), Some(1.5));
    }
}
