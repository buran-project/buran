//! Buran configuration: YAML schema, strict deserialization, reference
//! validation and `${ENV_VAR}` substitution.
//!
//! Parsing pipeline: YAML text -> `serde_norway::Value` -> env substitution
//! over string scalars -> typed `Config` (strict: unknown fields are errors,
//! anchors/aliases are rejected before parsing) -> `validate()`.

mod cpu;
mod schema;
mod subst;
mod validate;

pub use cpu::auto_worker_count;
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
    fn task_timeout_below_response_timeout_is_rejected() {
        let yaml = "\
listeners:
  \"*:8080\": { route: main }
routes:
  main:
    - action: { application: app }
applications:
  app:
    module: test
    limits:
      response_timeout: 60
      task_timeout: 30
";
        match err(yaml) {
            ConfigError::Invalid { path, .. } => {
                assert!(path.ends_with("limits.task_timeout"), "path: {path}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn split_limits_defaults_are_ordered() {
        let yaml = "\
listeners:
  \"*:8080\": { route: main }
routes:
  main:
    - action: { application: app }
applications:
  app:
    module: test
";
        let app = from_str(yaml).unwrap().applications.get("app").unwrap().clone();
        assert_eq!(app.limits.response_timeout, 60);
        assert_eq!(app.limits.task_timeout, 300);
        assert!(app.limits.task_timeout >= app.limits.response_timeout);
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
    fn explicit_auto_processes_resolves_to_fixed() {
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
    processes: auto
";
        let app = from_str(yaml).unwrap().applications.get("app").unwrap().clone();
        match app.processes {
            Processes::Fixed(n) => assert!(n >= 1),
            other => panic!("expected auto to resolve to Fixed, got {other:?}"),
        }
    }

    #[test]
    fn omitted_processes_defaults_to_auto() {
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
";
        let app = from_str(yaml).unwrap().applications.get("app").unwrap().clone();
        assert!(matches!(app.processes, Processes::Fixed(n) if n >= 1));
    }

    #[test]
    fn non_auto_processes_string_is_rejected() {
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
    processes: many
";
        assert!(matches!(err(yaml), ConfigError::Yaml(_)));
    }

    #[test]
    fn unknown_dynamic_processes_field_is_rejected() {
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
      max: 4
      spair: 2
";
        assert!(matches!(err(yaml), ConfigError::Yaml(_)));
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

    #[test]
    fn share_with_multiple_paths_is_rejected() {
        // Object form with a candidate list: the router only honors the first,
        // so the array is rejected rather than silently truncated.
        let yaml = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main:
    - action:
        share:
          share: [\"/srv/a$uri\", \"/srv/b$uri\"]
";
        match err(yaml) {
            ConfigError::Invalid { path, .. } => assert_eq!(path, "routes.main[0].action.share"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn share_with_single_path_object_is_accepted() {
        // Object form carrying exactly one path passes the len() == 1 check.
        let yaml = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main:
    - action:
        share:
          share: \"/srv/www$uri\"
          index: index.html
";
        assert!(from_str(yaml).is_ok());
    }

    #[test]
    fn plain_share_path_is_accepted() {
        // Scalar form (Share::Path) is not an object, so the len check is a
        // no-op and a single template path is always fine.
        let yaml = "\
listeners:
  \"*:8080\":
    route: main
routes:
  main:
    - action:
        share: \"/srv/www$uri\"
";
        assert!(from_str(yaml).is_ok());
    }

    #[test]
    fn parse_listener_addr_accepts_ipv4_host() {
        let addr = parse_listener_addr("127.0.0.1:8080").unwrap();
        assert_eq!(addr.to_string(), "127.0.0.1:8080");
    }

    #[test]
    fn parse_listener_addr_expands_star_to_unspecified_v4() {
        let addr = parse_listener_addr("*:8080").unwrap();
        assert!(addr.ip().is_unspecified());
        assert!(addr.is_ipv4());
        assert_eq!(addr.port(), 8080);
    }

    #[test]
    fn parse_listener_addr_accepts_bracketed_ipv6() {
        let addr = parse_listener_addr("[::1]:8080").unwrap();
        assert!(addr.is_ipv6());
        assert_eq!(addr.port(), 8080);
        assert_eq!(addr.ip().to_string(), "::1");
    }

    #[test]
    fn parse_listener_addr_accepts_bracketed_ipv6_unspecified() {
        let addr = parse_listener_addr("[::]:443").unwrap();
        assert!(addr.ip().is_unspecified());
        assert!(addr.is_ipv6());
        assert_eq!(addr.port(), 443);
    }

    #[test]
    fn parse_listener_addr_rejects_bare_ipv6() {
        // A bare IPv6 literal is ambiguous under host:port splitting; brackets
        // are required. Both a form that would otherwise parse "successfully"
        // (arbitrarily) and a loopback are rejected.
        let msg = parse_listener_addr("::1:8080").unwrap_err();
        assert!(msg.contains("bracket"), "msg: {msg}");
        assert!(parse_listener_addr("2001:db8::1:8080").is_err());
        assert!(parse_listener_addr("fe80::1").is_err());
    }

    #[test]
    fn parse_listener_addr_rejects_missing_closing_bracket() {
        let msg = parse_listener_addr("[::1:8080").unwrap_err();
        assert!(msg.contains(']'), "msg: {msg}");
    }

    #[test]
    fn parse_listener_addr_rejects_missing_port_after_bracket() {
        assert!(parse_listener_addr("[::1]").is_err());
        assert!(parse_listener_addr("[::1]8080").is_err());
    }

    #[test]
    fn parse_listener_addr_rejects_bad_port() {
        assert!(parse_listener_addr("127.0.0.1:notaport").is_err());
        assert!(parse_listener_addr("127.0.0.1:70000").is_err()); // > u16::MAX
    }

    #[test]
    fn parse_listener_addr_rejects_empty_host() {
        assert!(parse_listener_addr(":8080").is_err());
    }

    #[test]
    fn parse_listener_addr_rejects_non_ip_host() {
        // No name resolution: a hostname is not a valid listener host.
        assert!(parse_listener_addr("localhost:8080").is_err());
    }

    #[test]
    fn parse_listener_addr_rejects_missing_port_separator() {
        assert!(parse_listener_addr("8080").is_err());
    }

    #[test]
    fn ipv6_listener_validates_end_to_end() {
        let yaml = "\
listeners:
  \"[::1]:8080\":
    route: main
routes:
  main:
    - action:
        return: 200
";
        assert!(from_str(yaml).is_ok());
    }
}
