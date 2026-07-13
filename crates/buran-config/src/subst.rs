//! `${ENV_VAR}` substitution over string scalars of a parsed YAML tree.
//! Performed after parsing so YAML quoting rules never interfere, and only
//! full `${NAME}` tokens are expanded (no `$NAME` shorthand).

use serde_norway::Value;

use crate::ConfigError;

pub fn substitute_env(value: &mut Value, path: &str) -> Result<(), ConfigError> {
    match value {
        Value::String(s) => {
            if s.contains("${") {
                *s = expand(s, path)?;
            }
        }
        Value::Sequence(seq) => {
            for (i, item) in seq.iter_mut().enumerate() {
                substitute_env(item, &format!("{path}[{i}]"))?;
            }
        }
        Value::Mapping(map) => {
            for (key, item) in map.iter_mut() {
                let key_str = key.as_str().unwrap_or("?");
                substitute_env(item, &format!("{path}.{key_str}"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn expand(input: &str, path: &str) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // Unterminated `${` is kept literally.
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let name = &after[..end];
        match std::env::var(name) {
            Ok(val) => out.push_str(&val),
            Err(_) => {
                return Err(ConfigError::EnvMissing {
                    name: name.to_string(),
                    path: path.trim_start_matches('$').trim_start_matches('.').to_string(),
                })
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Unique per-test var names keep the process-global env race-free under
    // the default parallel test runner.
    fn set(name: &str, val: &str) {
        // FIXME: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var(name, val) };
    }

    #[test]
    fn expands_full_token() {
        set("BURAN_SUBST_A", "world");
        assert_eq!(expand("hello ${BURAN_SUBST_A}!", "$").unwrap(), "hello world!");
    }

    #[test]
    fn expands_multiple_tokens() {
        set("BURAN_SUBST_H", "example.test");
        set("BURAN_SUBST_P", "8080");
        assert_eq!(
            expand("${BURAN_SUBST_H}:${BURAN_SUBST_P}", "$").unwrap(),
            "example.test:8080"
        );
    }

    #[test]
    fn shorthand_dollar_name_is_literal() {
        // Only `${NAME}` is a token; `$NAME` is passed through verbatim.
        set("BURAN_SUBST_S", "x");
        assert_eq!(expand("$BURAN_SUBST_S", "$").unwrap(), "$BURAN_SUBST_S");
    }

    #[test]
    fn unterminated_token_is_kept_literally() {
        assert_eq!(expand("prefix ${UNCLOSED", "$").unwrap(), "prefix ${UNCLOSED");
    }

    #[test]
    fn missing_variable_errors_with_path() {
        // FIXME: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("BURAN_SUBST_MISSING") };
        let err = expand("${BURAN_SUBST_MISSING}", "$.listeners").unwrap_err();
        match err {
            ConfigError::EnvMissing { name, path } => {
                assert_eq!(name, "BURAN_SUBST_MISSING");
                assert_eq!(path, "listeners");
            }
            other => panic!("expected EnvMissing, got {other:?}"),
        }
    }

    #[test]
    fn substitute_walks_nested_structures() {
        set("BURAN_SUBST_N", "resolved");
        let mut value: Value = serde_norway::from_str(
            "root: ${BURAN_SUBST_N}\nlist:\n  - plain\n  - ${BURAN_SUBST_N}\nmap:\n  key: ${BURAN_SUBST_N}\n",
        )
        .unwrap();
        substitute_env(&mut value, "$").unwrap();
        assert_eq!(value["root"].as_str().unwrap(), "resolved");
        assert_eq!(value["list"][1].as_str().unwrap(), "resolved");
        assert_eq!(value["map"]["key"].as_str().unwrap(), "resolved");
    }

    #[test]
    fn scalars_without_token_are_untouched() {
        let mut value: Value = serde_norway::from_str("n: 42\ns: plain\nb: true\n").unwrap();
        substitute_env(&mut value, "$").unwrap();
        assert_eq!(value["n"].as_u64().unwrap(), 42);
        assert_eq!(value["s"].as_str().unwrap(), "plain");
        assert!(value["b"].as_bool().unwrap());
    }
}
