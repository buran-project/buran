//! Buran configuration: YAML schema, strict deserialization, reference
//! validation and `${ENV_VAR}` substitution.
//!
//! Parsing pipeline: YAML text -> `serde_norway::Value` -> env substitution
//! over string scalars -> typed `Config` (strict: unknown fields are errors,
//! anchors/aliases are rejected before parsing) -> `validate()`.

mod schema;
mod subst;
mod validate;

pub use schema::*;
pub use validate::{parse_listener_addr, Validated};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("YAML anchors/aliases are not allowed (line {line})")]
    AnchorForbidden { line: usize },
    #[error("YAML parse error: {0}")]
    Yaml(#[from] serde_norway::Error),
    #[error("environment variable \"{name}\" referenced at {path} is not set")]
    EnvMissing { name: String, path: String },
    #[error("{path}: {message}")]
    Invalid { path: String, message: String },
}

impl ConfigError {
    pub(crate) fn invalid(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Invalid { path: path.into(), message: message.into() }
    }
}

/// Parse and fully validate a configuration from YAML text.
pub fn from_str(yaml: &str) -> Result<Validated, ConfigError> {
    reject_anchors(yaml)?;

    let mut value: serde_norway::Value = serde_norway::from_str(yaml)?;
    subst::substitute_env(&mut value, "$")?;

    let config: Config = serde_norway::from_value(value)?;
    validate::validate(config)
}

/// Parse and fully validate a configuration file.
pub fn from_file(path: &std::path::Path) -> Result<Validated, ConfigError> {
    from_str(&std::fs::read_to_string(path)?)
}

/// Anchors and aliases are forbidden by design (see the spec): an anchor name
/// does not survive parsing while applications are named runtime entities.
/// serde_norway silently expands aliases, so we scan the raw text upfront.
fn reject_anchors(yaml: &str) -> Result<(), ConfigError> {
    for (i, line) in yaml.lines().enumerate() {
        let code = match line.find(" #") {
            Some(pos) => &line[..pos],
            None => line,
        };
        // Quick scan: `&name` / `*name` tokens outside of quoted scalars. An
        // anchor/alias sits at a node position: after whitespace in block
        // style, or right after a flow indicator (`[`, `{`, `,`, `:`) — the
        // latter is how `key: [*alias]` slips past a whitespace-only check.
        let mut in_single = false;
        let mut in_double = false;
        let mut prev = ' ';
        for ch in code.chars() {
            let at_node = prev.is_whitespace() || matches!(prev, '[' | '{' | ',' | ':');
            match ch {
                '\'' if !in_double => in_single = !in_single,
                '"' if !in_single => in_double = !in_double,
                '&' | '*' if !in_single && !in_double && at_node => {
                    return Err(ConfigError::AnchorForbidden { line: i + 1 });
                }
                _ => {}
            }
            prev = ch;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal valid config with a single `return` route.
    const VALID: &str = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main:
    - action:
        return: 200
";

    fn err(yaml: &str) -> ConfigError {
        from_str(yaml).expect_err("expected config to be rejected")
    }

    #[test]
    fn minimal_config_validates() {
        let v = from_str(VALID).unwrap();
        assert!(v.config.listeners.contains_key("*:8080"));
        assert!(v.config.routes.contains_key("main"));
    }

    #[test]
    fn unknown_field_is_rejected() {
        let yaml = format!("{VALID}bogus_top_level: 1\n");
        assert!(matches!(err(&yaml), ConfigError::Yaml(_)));
    }

    #[test]
    fn anchor_is_forbidden() {
        let yaml = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main: &steps
    - action:
        return: 200
";
        assert!(matches!(err(yaml), ConfigError::AnchorForbidden { line: 5 }));
    }

    #[test]
    fn alias_is_forbidden() {
        let yaml = "\
routes:
  main:
    - action: &a
        return: 200
  other: *a
";
        assert!(matches!(err(yaml), ConfigError::AnchorForbidden { .. }));
    }

    #[test]
    fn ampersand_inside_quotes_is_allowed() {
        // A `&` inside a quoted scalar is data, not an anchor.
        let yaml = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main:
    - action:
        return: 302
        location: \"/a &b\"
";
        assert!(from_str(yaml).is_ok());
    }

    #[test]
    fn env_substitution_feeds_the_schema() {
        // Substitution operates on string scalars: env values naturally land
        // in string-typed fields such as `location`.
        // FIXME: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("BURAN_CFG_LOC", "/redirected") };
        let yaml = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main:
    - action:
        return: 301
        location: ${BURAN_CFG_LOC}
";
        let v = from_str(yaml).unwrap();
        assert_eq!(v.config.routes["main"][0].action.location.as_deref(), Some("/redirected"));
    }

    #[test]
    fn zero_http_timeout_is_rejected() {
        let yaml = format!("settings:\n  http:\n    idle_timeout: 0\n{VALID}");
        match err(&yaml) {
            ConfigError::Invalid { path, .. } => assert_eq!(path, "settings.http.idle_timeout"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn missing_listeners_is_rejected() {
        let yaml = "routes:\n  main:\n    - action:\n        return: 200\n";
        match err(yaml) {
            ConfigError::Invalid { path, .. } => assert_eq!(path, "listeners"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn route_and_status_are_mutually_exclusive() {
        let yaml = "\
listeners:
  \"*:8080\":
    route: main
    status: true
routes:
  main:
    - action:
        return: 200
";
        assert!(matches!(err(yaml), ConfigError::Invalid { .. }));
    }

    #[test]
    fn listener_needs_route_or_status() {
        let yaml = "\
listeners:
  \"*:8080\": {}
routes:
  main:
    - action:
        return: 200
";
        assert!(matches!(err(yaml), ConfigError::Invalid { .. }));
    }

    #[test]
    fn status_listener_needs_no_route() {
        let yaml = "listeners:\n  \"*:9000\":\n    status: true\n";
        assert!(from_str(yaml).is_ok());
    }

    #[test]
    fn unknown_route_reference_is_rejected() {
        let yaml = "\
listeners:
  \"*:8080\":
    route: nope
routes:
  main:
    - action:
        return: 200
";
        match err(yaml) {
            ConfigError::Invalid { path, .. } => assert_eq!(path, "listeners.\"*:8080\".route"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn unknown_application_reference_is_rejected() {
        let yaml = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main:
    - action:
        application: ghost
";
        assert!(matches!(err(yaml), ConfigError::Invalid { .. }));
    }

    #[test]
    fn return_out_of_range_is_rejected() {
        let yaml = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main:
    - action:
        return: 999
";
        match err(yaml) {
            ConfigError::Invalid { path, .. } => assert_eq!(path, "routes.main[0].action.return"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn location_requires_3xx_return() {
        let yaml = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main:
    - action:
        return: 200
        location: /elsewhere
";
        assert!(matches!(err(yaml), ConfigError::Invalid { .. }));
    }

    #[test]
    fn location_with_redirect_is_ok() {
        let yaml = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main:
    - action:
        return: 301
        location: /elsewhere
";
        assert!(from_str(yaml).is_ok());
    }

    #[test]
    fn empty_route_is_rejected() {
        let yaml = "listeners:\n  \"*:8080\":\n    route: main\nroutes:\n  main: []\n";
        match err(yaml) {
            ConfigError::Invalid { path, .. } => assert_eq!(path, "routes.main"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn bad_listener_address_is_rejected() {
        let yaml = "\
listeners:
  \"*:notaport\":
    route: main
routes:
  main:
    - action:
        return: 200
";
        assert!(matches!(err(yaml), ConfigError::Invalid { .. }));
    }

    #[test]
    fn invalid_module_name_is_rejected() {
        let yaml = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main:
    - action:
        application: app
applications:
  app:
    module: PHP_85
";
        match err(yaml) {
            ConfigError::Invalid { path, .. } => assert_eq!(path, "applications.app.module"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn zero_fixed_processes_is_rejected() {
        let yaml = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main:
    - action:
        application: app
applications:
  app:
    module: php85
    processes: 0
";
        assert!(matches!(err(yaml), ConfigError::Invalid { .. }));
    }

    #[test]
    fn dynamic_spare_over_max_is_rejected() {
        let yaml = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main:
    - action:
        application: app
applications:
  app:
    module: php85
    processes:
      max: 2
      spare: 5
";
        match err(yaml) {
            ConfigError::Invalid { path, .. } => assert_eq!(path, "applications.app.processes.spare"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn bad_execute_extension_is_rejected() {
        let yaml = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main:
    - action:
        application: app
applications:
  app:
    module: php85
    execute: [\"html\"]
";
        match err(yaml) {
            ConfigError::Invalid { path, .. } => assert_eq!(path, "applications.app.execute"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn inline_application_is_extracted_under_generated_name() {
        let yaml = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main:
    - action:
        application:
          module: php85
";
        let v = from_str(yaml).unwrap();
        // Inline app moved into the global map under `main[0]`, and the
        // action rewritten to a name reference.
        assert!(v.applications.contains_key("main[0]"));
        match &v.config.routes["main"][0].action.application {
            Some(ApplicationRef::Name(n)) => assert_eq!(n, "main[0]"),
            other => panic!("expected name reference, got {other:?}"),
        }
    }

    #[test]
    fn fallback_requires_share() {
        let yaml = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main:
    - action:
        return: 200
        fallback:
          return: 404
";
        assert!(matches!(err(yaml), ConfigError::Invalid { .. }));
    }

    const APP: &str = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main:
    - action:
        application: app
applications:
  app:
    module: php85
";

    #[test]
    fn zero_concurrency_is_rejected() {
        let yaml = format!("{APP}    concurrency: 0\n");
        match err(&yaml) {
            ConfigError::Invalid { path, .. } => assert_eq!(path, "applications.app.concurrency"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn concurrency_cap_is_parsed_and_optional() {
        let yaml = format!("{APP}    concurrency: 64\n");
        let v = from_str(&yaml).unwrap();
        assert_eq!(v.applications.get("app").unwrap().concurrency, Some(64));

        // Absent = trust the module's Hello declaration.
        let v = from_str(APP).unwrap();
        assert_eq!(v.applications.get("app").unwrap().concurrency, None);
    }

    #[test]
    fn websocket_defaults_are_sane() {
        let v = from_str(VALID).unwrap();
        let ws = &v.config.settings.http.websocket;
        assert_eq!(ws.idle_timeout, 600);
        assert_eq!(ws.max_message_size, 1024 * 1024);
    }

    #[test]
    fn websocket_settings_are_parsed_and_validated() {
        let yaml = format!(
            "settings:\n  http:\n    websocket:\n      idle_timeout: 30\n      max_message_size: 4096\n{VALID}"
        );
        let v = from_str(&yaml).unwrap();
        assert_eq!(v.config.settings.http.websocket.idle_timeout, 30);
        assert_eq!(v.config.settings.http.websocket.max_message_size, 4096);

        let yaml = format!("settings:\n  http:\n    websocket:\n      idle_timeout: 0\n{VALID}");
        match err(&yaml) {
            ConfigError::Invalid { path, .. } => {
                assert_eq!(path, "settings.http.websocket.idle_timeout")
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
