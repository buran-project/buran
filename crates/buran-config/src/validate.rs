//! Reference validation and inline-application extraction.
//!
//! Regex patterns (`~...`) are compiled by the router at startup, which for a
//! static configuration is still "before accepting traffic", so they are not
//! re-validated here to avoid a duplicate regex dependency.

use std::collections::BTreeMap;

use crate::schema::*;
use crate::ConfigError;

/// A fully validated configuration. Inline applications are extracted into
/// `applications` under generated names (`main[2]`), and the corresponding
/// route actions are rewritten to name references.
#[derive(Debug, Clone)]
pub struct Validated {
    pub config: Config,
    /// All applications: named ones plus extracted inline ones.
    pub applications: BTreeMap<String, Application>,
}

pub fn validate(mut config: Config) -> Result<Validated, ConfigError> {
    // All settings.http timeouts are live (spec 2.8); zero would stall or
    // instantly kill every connection, so reject it outright.
    let http = &config.settings.http;
    for (name, value) in [
        ("header_read_timeout", http.header_read_timeout),
        ("body_read_timeout", http.body_read_timeout),
        ("send_timeout", http.send_timeout),
        ("idle_timeout", http.idle_timeout),
        ("websocket.idle_timeout", http.websocket.idle_timeout),
        ("websocket.max_message_size", http.websocket.max_message_size),
    ] {
        if value == 0 {
            return Err(ConfigError::invalid(format!("settings.http.{name}"), "must be >= 1"));
        }
    }

    let mut applications = config.applications.clone();

    // Extract inline applications first so listener/route checks see them.
    let route_names: Vec<String> = config.routes.keys().cloned().collect();
    for route_name in &route_names {
        let steps = config.routes.get_mut(route_name).expect("just listed");
        for (i, step) in steps.iter_mut().enumerate() {
            extract_inline_apps(
                &mut step.action,
                &format!("routes.{route_name}[{i}].action"),
                &format!("{route_name}[{i}]"),
                &mut applications,
            )?;
        }
    }

    if config.listeners.is_empty() {
        return Err(ConfigError::invalid("listeners", "at least one listener is required"));
    }

    for (addr, listener) in &config.listeners {
        let path = format!("listeners.\"{addr}\"");
        validate_listener_addr(addr, &path)?;
        match (&listener.route, listener.status) {
            (Some(route), false) => {
                if !config.routes.contains_key(route) {
                    return Err(ConfigError::invalid(
                        format!("{path}.route"),
                        format!(
                            "unknown route \"{route}\"; available: {}",
                            available(config.routes.keys())
                        ),
                    ));
                }
            }
            (None, true) => {}
            (Some(_), true) => {
                return Err(ConfigError::invalid(path, "\"route\" and \"status\" are mutually exclusive"))
            }
            (None, false) => {
                return Err(ConfigError::invalid(path, "either \"route\" or \"status: true\" is required"))
            }
        }
    }

    for (name, steps) in &config.routes {
        if steps.is_empty() {
            return Err(ConfigError::invalid(format!("routes.{name}"), "route has no steps"));
        }
        for (i, step) in steps.iter().enumerate() {
            validate_action(
                &step.action,
                &format!("routes.{name}[{i}].action"),
                &config.routes,
                &applications,
            )?;
        }
    }

    for (name, app) in &applications {
        validate_application(app, &format!("applications.{name}"))?;
    }

    Ok(Validated { config, applications })
}

/// Replace inline application objects with generated name references,
/// moving the definitions into the global application map.
fn extract_inline_apps(
    action: &mut Action,
    path: &str,
    generated_name: &str,
    applications: &mut BTreeMap<String, Application>,
) -> Result<(), ConfigError> {
    if let Some(ApplicationRef::Inline(app)) = &action.application {
        if applications.contains_key(generated_name) {
            return Err(ConfigError::invalid(
                format!("{path}.application"),
                format!("generated name \"{generated_name}\" collides with a named application"),
            ));
        }
        applications.insert(generated_name.to_string(), (**app).clone());
        action.application = Some(ApplicationRef::Name(generated_name.to_string()));
    }
    if let Some(fallback) = &mut action.fallback {
        extract_inline_apps(
            fallback,
            &format!("{path}.fallback"),
            &format!("{generated_name}.fallback"),
            applications,
        )?;
    }
    Ok(())
}

fn validate_action(
    action: &Action,
    path: &str,
    routes: &BTreeMap<String, Vec<RouteStep>>,
    applications: &BTreeMap<String, Application>,
) -> Result<(), ConfigError> {
    let terminals = [
        action.application.is_some(),
        action.route.is_some(),
        action.share.is_some(),
        action.return_.is_some(),
    ]
    .iter()
    .filter(|&&t| t)
    .count();

    if terminals != 1 {
        return Err(ConfigError::invalid(
            path,
            "exactly one of \"application\", \"route\", \"share\", \"return\" is required",
        ));
    }

    if let Some(ApplicationRef::Name(name)) = &action.application {
        if !applications.contains_key(name) {
            return Err(ConfigError::invalid(
                format!("{path}.application"),
                format!(
                    "unknown application \"{name}\"; available: {}",
                    available(applications.keys())
                ),
            ));
        }
    }

    if let Some(route) = &action.route {
        if !routes.contains_key(route) {
            return Err(ConfigError::invalid(
                format!("{path}.route"),
                format!("unknown route \"{route}\"; available: {}", available(routes.keys())),
            ));
        }
    }

    if let Some(code) = action.return_ {
        if !(100..=599).contains(&code) {
            return Err(ConfigError::invalid(
                format!("{path}.return"),
                format!("status code {code} is out of range 100..=599"),
            ));
        }
        if action.location.is_some() && !(300..=399).contains(&code) {
            return Err(ConfigError::invalid(
                format!("{path}.location"),
                "\"location\" requires a 3xx \"return\" code",
            ));
        }
    } else if action.location.is_some() {
        return Err(ConfigError::invalid(
            format!("{path}.location"),
            "\"location\" is only valid together with \"return\"",
        ));
    }

    // Multiple candidate paths (Unit-style fallback search) are not
    // implemented: the router silently uses only the first. Reject the array
    // rather than quietly ignore the rest.
    if let Some(Share::Full(opts)) = &action.share {
        if opts.share.len() != 1 {
            return Err(ConfigError::invalid(
                format!("{path}.share"),
                "\"share\" takes exactly one path; a list of candidate paths is not supported",
            ));
        }
    }

    if action.fallback.is_some() && action.share.is_none() {
        return Err(ConfigError::invalid(
            format!("{path}.fallback"),
            "\"fallback\" is only valid together with \"share\"",
        ));
    }

    // A fallback is a full action: it may re-enter routing via `route`.
    if let Some(fallback) = &action.fallback {
        validate_action(fallback, &format!("{path}.fallback"), routes, applications)?;
    }

    Ok(())
}

fn validate_application(app: &Application, path: &str) -> Result<(), ConfigError> {
    if app.module.is_empty()
        || !app.module.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(ConfigError::invalid(
            format!("{path}.module"),
            format!("module name \"{}\" must be non-empty [a-z0-9_-]", app.module),
        ));
    }

    match app.processes {
        Processes::Fixed(n) if n == 0 => {
            return Err(ConfigError::invalid(format!("{path}.processes"), "must be >= 1"))
        }
        Processes::Dynamic { max, spare, .. } => {
            if max == 0 {
                return Err(ConfigError::invalid(format!("{path}.processes.max"), "must be >= 1"));
            }
            if spare > max {
                return Err(ConfigError::invalid(
                    format!("{path}.processes.spare"),
                    format!("spare ({spare}) must not exceed max ({max})"),
                ));
            }
        }
        _ => {}
    }

    if app.concurrency == Some(0) {
        return Err(ConfigError::invalid(format!("{path}.concurrency"), "must be >= 1"));
    }

    if app.queue.max == 0 {
        return Err(ConfigError::invalid(format!("{path}.queue.max"), "must be >= 1"));
    }

    for ext in &app.execute {
        if !ext.starts_with('.') || ext.len() < 2 {
            return Err(ConfigError::invalid(
                format!("{path}.execute"),
                format!("extension \"{ext}\" must look like \".html\""),
            ));
        }
    }

    Ok(())
}

fn validate_listener_addr(addr: &str, path: &str) -> Result<(), ConfigError> {
    let (host, port) = addr
        .rsplit_once(':')
        .ok_or_else(|| ConfigError::invalid(path, "listener address must be \"host:port\" or \"*:port\""))?;
    if host.is_empty() {
        return Err(ConfigError::invalid(path, "listener host must not be empty (use \"*\" for any)"));
    }
    port.parse::<u16>().map_err(|_| {
        ConfigError::invalid(path, format!("\"{port}\" is not a valid port"))
    })?;
    Ok(())
}

fn available<'a>(keys: impl Iterator<Item = &'a String>) -> String {
    let list: Vec<_> = keys.map(String::as_str).collect();
    if list.is_empty() {
        "(none)".to_string()
    } else {
        list.join(", ")
    }
}
