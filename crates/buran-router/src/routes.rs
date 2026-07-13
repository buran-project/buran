//! Route compilation (config -> matchers) and the routing state machine.

use std::collections::BTreeMap;
use std::net::IpAddr;

use buran_config::{Action as ConfAction, Application, ApplicationRef, Match, OneOrMany, Share, Validated};

use crate::matching::{CidrSet, PatternSet};
use crate::template::Template;

pub struct CompiledRoutes {
    routes: BTreeMap<String, Vec<Step>>,
}

pub struct Step {
    matcher: Option<Matcher>,
    rewrite: Option<Template>,
    response_headers: Vec<(String, Option<String>)>,
    pub action: Action,
}

struct Matcher {
    method: Option<PatternSet>,
    host: Option<PatternSet>,
    uri: Option<PatternSet>,
    query: Option<PatternSet>,
    headers: Vec<(Vec<u8>, PatternSet)>,
    arguments: Vec<(Vec<u8>, PatternSet)>,
    source: Option<CidrSet>,
}

pub enum Action {
    Application { name: String },
    Route { name: String },
    Share {
        template: String,
        index: String,
        /// MIME filter: file is served only if its type matches.
        types: Option<PatternSet>,
        /// Opt-out of module source-leak protection.
        serve_sources: bool,
        /// Per-app `execute` extensions of the fallback application
        /// (lowercase, no dot): forbidden for this share specifically.
        extra_source_exts: Vec<String>,
        fallback: Option<Box<Action>>,
    },
    Return { status: u16, location: Option<String> },
}

/// Terminal decision after walking routes.
pub enum Decision<'r> {
    Application(&'r str),
    Share {
        template: &'r str,
        index: &'r str,
        types: Option<&'r PatternSet>,
        serve_sources: bool,
        extra_source_exts: &'r [String],
        fallback: Option<&'r Action>,
    },
    Return { status: u16, location: Option<&'r str> },
    NotFound,
}

/// Result of the walk: the decision plus rewrite/header side effects
/// accumulated along the way.
pub struct RouteOutcome<'r> {
    pub decision: Decision<'r>,
    /// Set when a `rewrite` fired: the new (path, query).
    pub rewritten: Option<(Vec<u8>, Vec<u8>)>,
    /// `response_headers` ops in application order; None value = remove.
    pub response_headers: Vec<(&'r str, Option<&'r str>)>,
}

pub struct RequestMeta<'a> {
    pub method: &'a [u8],
    pub host: &'a [u8],
    pub path: &'a [u8],
    pub query: &'a [u8],
    pub headers: &'a [(Vec<u8>, Vec<u8>)],
    pub remote: IpAddr,
}

pub fn compile(validated: &Validated) -> anyhow::Result<CompiledRoutes> {
    let mut routes = BTreeMap::new();

    for (name, steps) in &validated.config.routes {
        let mut compiled = Vec::with_capacity(steps.len());
        for step in steps {
            let response_headers = step
                .action
                .response_headers
                .as_ref()
                .map(|map| {
                    map.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>()
                })
                .unwrap_or_default();
            compiled.push(Step {
                matcher: step.match_.as_ref().map(compile_match).transpose()?,
                rewrite: step
                    .action
                    .rewrite
                    .as_deref()
                    .map(Template::compile)
                    .transpose()?,
                response_headers,
                action: compile_action(&step.action, &validated.applications)?,
            });
        }
        routes.insert(name.clone(), compiled);
    }

    Ok(CompiledRoutes { routes })
}

fn compile_match(m: &Match) -> anyhow::Result<Matcher> {
    let set = |patterns: &Option<OneOrMany<String>>, ci: bool| {
        patterns
            .as_ref()
            .map(|p| PatternSet::compile(p.iter().map(String::as_str), ci))
            .transpose()
    };
    let sets = |map: &Option<BTreeMap<String, OneOrMany<String>>>, ci: bool| {
        map.as_ref()
            .map(|m| {
                m.iter()
                    .map(|(name, patterns)| {
                        PatternSet::compile(patterns.iter().map(String::as_str), ci)
                            .map(|set| (name.to_ascii_lowercase().into_bytes(), set))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()
            })
            .transpose()
    };

    Ok(Matcher {
        method: set(&m.method, false)?,
        host: set(&m.host, true)?,
        uri: set(&m.uri, false)?,
        query: set(&m.query, false)?,
        headers: sets(&m.headers, true)?.unwrap_or_default(),
        arguments: sets(&m.arguments, false)?.unwrap_or_default(),
        source: m
            .source
            .as_ref()
            .map(|s| CidrSet::compile(s.iter().map(String::as_str)))
            .transpose()?,
    })
}

fn compile_action(
    action: &ConfAction,
    apps: &BTreeMap<String, Application>,
) -> anyhow::Result<Action> {
    if let Some(app) = &action.application {
        let ApplicationRef::Name(name) = app else {
            unreachable!("inline apps are extracted during validation");
        };
        return Ok(Action::Application { name: name.clone() });
    }
    if let Some(route) = &action.route {
        return Ok(Action::Route { name: route.clone() });
    }
    if let Some(share) = &action.share {
        let (template, index, types, serve_sources) = match share {
            Share::Path(p) => (p.clone(), "index.html".to_string(), None, false),
            Share::Full(opts) => (
                opts.share.iter().next().cloned().unwrap_or_default(),
                opts.index.clone().unwrap_or_else(|| "index.html".to_string()),
                opts.types
                    .as_ref()
                    .map(|t| PatternSet::compile(t.iter().map(String::as_str), true))
                    .transpose()?,
                opts.serve_sources,
            ),
        };
        let fallback = action
            .fallback
            .as_ref()
            .map(|f| compile_action(f, apps).map(Box::new))
            .transpose()?;
        // The fallback application shares this file tree: its `execute`
        // extensions must not be served as static from here.
        let extra_source_exts = match fallback.as_deref() {
            Some(Action::Application { name }) => apps
                .get(name)
                .map(|a| {
                    a.execute
                        .iter()
                        .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        return Ok(Action::Share { template, index, types, serve_sources, extra_source_exts, fallback });
    }
    if let Some(status) = action.return_ {
        return Ok(Action::Return { status, location: action.location.clone() });
    }
    unreachable!("validated action has exactly one terminal");
}

impl CompiledRoutes {
    /// Walk a route chain to a terminal decision, applying rewrites and
    /// collecting response_headers ops. `route` jumps and rewrites are
    /// bounded by a hop limit.
    pub fn decide<'r>(&'r self, route: &str, meta: &RequestMeta<'_>) -> RouteOutcome<'r> {
        let mut current_route = route;
        let mut path: Vec<u8> = meta.path.to_vec();
        let mut query: Vec<u8> = meta.query.to_vec();
        let mut rewritten = false;
        let mut header_ops: Vec<(&'r str, Option<&'r str>)> = Vec::new();

        let remote_str = meta.remote.to_string();

        for _hop in 0..16 {
            let Some(steps) = self.routes.get(current_route) else {
                break;
            };
            let Some(step) = steps.iter().find(|s| s.matches(meta, &path, &query)) else {
                break;
            };

            for (name, value) in &step.response_headers {
                header_ops.push((name.as_str(), value.as_deref()));
            }

            if let Some(rewrite) = &step.rewrite {
                let vars = crate::template::Vars {
                    uri: &path,
                    args: &query,
                    host: meta.host,
                    method: meta.method,
                    remote_addr: &remote_str,
                };
                let target = rewrite.render(&vars);
                let (new_path, new_query) = match memchr::memchr(b'?', &target) {
                    Some(pos) => (target[..pos].to_vec(), target[pos + 1..].to_vec()),
                    None => (target, Vec::new()),
                };
                path = crate::uri::normalize_path(&new_path);
                query = new_query;
                rewritten = true;
            }

            let decision = match &step.action {
                Action::Route { name } => {
                    current_route = name;
                    continue;
                }
                Action::Application { name } => Decision::Application(name),
                Action::Share { template, index, types, serve_sources, extra_source_exts, fallback } => {
                    Decision::Share {
                        template,
                        index,
                        types: types.as_ref(),
                        serve_sources: *serve_sources,
                        extra_source_exts,
                        fallback: fallback.as_deref(),
                    }
                }
                Action::Return { status, location } => {
                    Decision::Return { status: *status, location: location.as_deref() }
                }
            };

            return RouteOutcome {
                decision,
                rewritten: rewritten.then_some((path, query)),
                response_headers: header_ops,
            };
        }

        RouteOutcome {
            decision: Decision::NotFound,
            rewritten: rewritten.then_some((path, query)),
            response_headers: header_ops,
        }
    }
}

impl Step {
    fn matches(&self, meta: &RequestMeta<'_>, path: &[u8], query: &[u8]) -> bool {
        let Some(m) = &self.matcher else { return true };

        if let Some(set) = &m.method {
            if !set.matches(meta.method, false) {
                return false;
            }
        }
        if let Some(set) = &m.host {
            // Host header may carry a port: match on the name part.
            let host = match memchr::memchr(b':', meta.host) {
                Some(pos) => &meta.host[..pos],
                None => meta.host,
            };
            if !set.matches(host, true) {
                return false;
            }
        }
        if let Some(set) = &m.uri {
            if !set.matches(path, false) {
                return false;
            }
        }
        if let Some(set) = &m.query {
            if !set.matches(query, false) {
                return false;
            }
        }
        for (name, set) in &m.headers {
            let value = meta
                .headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_slice())
                .unwrap_or(b"");
            if !set.matches(value, true) {
                return false;
            }
        }
        if !m.arguments.is_empty() {
            let args = parse_args(query);
            for (name, set) in &m.arguments {
                let value = args
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.as_slice())
                    .unwrap_or(b"");
                if !set.matches(value, false) {
                    return false;
                }
            }
        }
        if let Some(cidr) = &m.source {
            if !cidr.matches(meta.remote) {
                return false;
            }
        }
        true
    }
}

/// Decode `a=1&b=%20` into pairs; `+` becomes space per form encoding.
fn parse_args(query: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    query
        .split(|&b| b == b'&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (name, value) = match memchr::memchr(b'=', part) {
                Some(pos) => (&part[..pos], &part[pos + 1..]),
                None => (part, &[][..]),
            };
            (decode_component(name), decode_component(value))
        })
        .collect()
}

fn decode_component(input: &[u8]) -> Vec<u8> {
    let plus_mapped: Vec<u8> =
        input.iter().map(|&b| if b == b'+' { b' ' } else { b }).collect();
    crate::uri::percent_decode(&plus_mapped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiled(yaml: &str) -> CompiledRoutes {
        let validated = buran_config::from_str(yaml).unwrap();
        compile(&validated).unwrap()
    }

    fn meta<'a>(path: &'a [u8], query: &'a [u8]) -> RequestMeta<'a> {
        RequestMeta {
            method: b"GET",
            host: b"example.test",
            path,
            query,
            headers: &[],
            remote: IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        }
    }

    #[test]
    fn parse_args_decodes_pairs() {
        let args = parse_args(b"a=1&b=hello+world&c=%2F&flag");
        assert_eq!(args[0], (b"a".to_vec(), b"1".to_vec()));
        assert_eq!(args[1], (b"b".to_vec(), b"hello world".to_vec()));
        assert_eq!(args[2], (b"c".to_vec(), b"/".to_vec()));
        assert_eq!(args[3], (b"flag".to_vec(), b"".to_vec()));
    }

    #[test]
    fn parse_args_skips_empty_segments() {
        assert!(parse_args(b"").is_empty());
        assert_eq!(parse_args(b"&&x=1&&").len(), 1);
    }

    #[test]
    fn decode_component_plus_and_percent() {
        assert_eq!(decode_component(b"a+b%20c"), b"a b c");
    }

    #[test]
    fn matched_step_returns() {
        let routes = compiled(
            "\
listeners:
  \"*:8080\": { route: main }
routes:
  main:
    - match: { uri: \"/health\" }
      action: { return: 204 }
    - action: { return: 404 }
",
        );
        assert!(matches!(
            routes.decide("main", &meta(b"/health", b"")).decision,
            Decision::Return { status: 204, .. }
        ));
        assert!(matches!(
            routes.decide("main", &meta(b"/other", b"")).decision,
            Decision::Return { status: 404, .. }
        ));
    }

    #[test]
    fn route_jump_is_followed() {
        let routes = compiled(
            "\
listeners:
  \"*:8080\": { route: main }
routes:
  main:
    - action: { route: sub }
  sub:
    - action: { return: 200 }
",
        );
        assert!(matches!(
            routes.decide("main", &meta(b"/x", b"")).decision,
            Decision::Return { status: 200, .. }
        ));
    }

    #[test]
    fn no_matching_step_is_not_found() {
        let routes = compiled(
            "\
listeners:
  \"*:8080\": { route: main }
routes:
  main:
    - match: { uri: \"/only\" }
      action: { return: 200 }
",
        );
        assert!(matches!(routes.decide("main", &meta(b"/nope", b"")).decision, Decision::NotFound));
        // Unknown route name also yields NotFound.
        assert!(matches!(routes.decide("ghost", &meta(b"/x", b"")).decision, Decision::NotFound));
    }

    #[test]
    fn rewrite_replaces_path_and_query() {
        let routes = compiled(
            "\
listeners:
  \"*:8080\": { route: main }
routes:
  main:
    - action:
        rewrite: /index.php?$args
        return: 200
",
        );
        let outcome = routes.decide("main", &meta(b"/foo", b"a=1"));
        assert!(matches!(outcome.decision, Decision::Return { status: 200, .. }));
        let (path, query) = outcome.rewritten.unwrap();
        assert_eq!(path, b"/index.php");
        assert_eq!(query, b"a=1");
    }

    #[test]
    fn response_headers_are_collected() {
        let routes = compiled(
            "\
listeners:
  \"*:8080\": { route: main }
routes:
  main:
    - action:
        return: 200
        response_headers:
          X-Test: hello
",
        );
        let outcome = routes.decide("main", &meta(b"/x", b""));
        assert_eq!(outcome.response_headers, vec![("X-Test", Some("hello"))]);
    }

    #[test]
    fn share_decision_carries_defaults() {
        let routes = compiled(
            "\
listeners:
  \"*:8080\": { route: main }
routes:
  main:
    - action:
        share: /var/www$uri
",
        );
        match routes.decide("main", &meta(b"/a.txt", b"")).decision {
            Decision::Share { template, index, .. } => {
                assert_eq!(template, "/var/www$uri");
                assert_eq!(index, "index.html");
            }
            other => panic!("expected Share, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn route_jump_loop_is_bounded() {
        let routes = compiled(
            "\
listeners:
  \"*:8080\": { route: main }
routes:
  main:
    - action: { route: main }
",
        );
        // Self-jump must terminate via the hop limit, not spin forever.
        assert!(matches!(routes.decide("main", &meta(b"/x", b"")).decision, Decision::NotFound));
    }

    #[test]
    fn argument_matcher_reads_query() {
        let routes = compiled(
            "\
listeners:
  \"*:8080\": { route: main }
routes:
  main:
    - match:
        arguments: { debug: \"1\" }
      action: { return: 200 }
    - action: { return: 403 }
",
        );
        assert!(matches!(
            routes.decide("main", &meta(b"/x", b"debug=1")).decision,
            Decision::Return { status: 200, .. }
        ));
        assert!(matches!(
            routes.decide("main", &meta(b"/x", b"debug=0")).decision,
            Decision::Return { status: 403, .. }
        ));
    }
}
