//! Typed configuration schema. Strict: unknown fields fail deserialization.

use std::collections::BTreeMap;

use serde::Deserialize;

/// One-or-many pattern value: `uri: "*.php"` or `uri: ["/a/*", "!/a/b"]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        match self {
            Self::One(v) => std::slice::from_ref(v).iter(),
            Self::Many(v) => v.iter(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::One(_) => 1,
            Self::Many(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub settings: Settings,
    pub listeners: BTreeMap<String, Listener>,
    pub routes: BTreeMap<String, Vec<RouteStep>>,
    pub applications: BTreeMap<String, Application>,
    pub access_log: Option<String>,
    /// Diagnostic/error log destination (server + worker stdout/stderr). A file
    /// path; omit to write to stderr. Level is controlled by `RUST_LOG`.
    pub error_log: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    pub listen_threads: Option<usize>,
    /// Runtime module directory; `module: php85` -> `<modules>/buran-php85`.
    pub modules: String,
    pub http: HttpSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            listen_threads: None,
            modules: "/usr/lib/buran/modules".to_string(),
            http: HttpSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HttpSettings {
    /// Seconds. All timeouts follow settings.http of the spec (section 2.8).
    pub header_read_timeout: u64,
    pub body_read_timeout: u64,
    pub send_timeout: u64,
    pub idle_timeout: u64,
    pub max_body_size: u64,
    /// Bytes/second. Sustained minimum request-body throughput after an
    /// initial grace window (body_read_timeout): a client sending the body
    /// slower than this is cut (slow-POST / RUDY defence). `0` disables it.
    pub min_body_rate: u64,
    /// Process-wide cap on concurrent connections (across all listeners). At
    /// the cap the server stops accepting, so surplus connections wait in the
    /// kernel backlog and consume no local fd. Counts long-lived tunnels
    /// (WebSocket) too — size it above expected concurrency. `0` disables it.
    pub max_connections: u64,
    pub body_temp_path: String,
    #[serde(rename = "static")]
    pub static_: Option<StaticSettings>,
    pub server_version: bool,
    pub websocket: WebsocketSettings,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            header_read_timeout: 30,
            body_read_timeout: 30,
            send_timeout: 30,
            idle_timeout: 180,
            max_body_size: 8 * 1024 * 1024,
            min_body_rate: 256,
            max_connections: 4096,
            body_temp_path: "/var/tmp/buran".to_string(),
            static_: None,
            server_version: true,
            websocket: WebsocketSettings::default(),
        }
    }
}

/// Upgraded (WebSocket) connections live outside the regular HTTP budgets:
/// the request limits and http.idle_timeout do not apply to them.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WebsocketSettings {
    /// Seconds of silence in BOTH directions before the tunnel is closed
    /// (1001). Application-level ping/pong keeps it alive naturally.
    pub idle_timeout: u64,
    /// Largest complete message (after fragment reassembly) accepted from
    /// a client; larger ones close the connection with 1009.
    pub max_message_size: u64,
}

impl Default for WebsocketSettings {
    fn default() -> Self {
        Self { idle_timeout: 600, max_message_size: 1024 * 1024 }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticSettings {
    #[serde(default)]
    pub mime_types: BTreeMap<String, Vec<String>>,
}

/// A listener is either an entry point into routing or the status endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listener {
    #[serde(default)]
    pub route: Option<String>,
    #[serde(default)]
    pub status: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteStep {
    #[serde(rename = "match", default)]
    pub match_: Option<Match>,
    pub action: Action,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Match {
    pub method: Option<OneOrMany<String>>,
    pub host: Option<OneOrMany<String>>,
    pub uri: Option<OneOrMany<String>>,
    pub query: Option<OneOrMany<String>>,
    pub headers: Option<BTreeMap<String, OneOrMany<String>>>,
    pub arguments: Option<BTreeMap<String, OneOrMany<String>>>,
    pub source: Option<OneOrMany<String>>,
}

/// Action: exactly one terminal (`application` | `route` | `share` | `return`)
/// plus optional modifiers (`rewrite`, `response_headers`). Exclusivity is
/// enforced by `validate()`, not by serde, to keep error messages precise.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Action {
    pub application: Option<ApplicationRef>,
    pub route: Option<String>,
    pub share: Option<Share>,
    #[serde(rename = "return")]
    pub return_: Option<u16>,
    pub location: Option<String>,
    pub rewrite: Option<String>,
    pub response_headers: Option<BTreeMap<String, Option<String>>>,
    /// Nested action taken when `share` fails with 40x.
    pub fallback: Option<Box<Action>>,
}

/// `application:` accepts a name (reference into `applications`) or an inline
/// anonymous application definition (k8s/compose style, no anchors).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ApplicationRef {
    Name(String),
    Inline(Box<Application>),
}

/// `share:` accepts a template path or an object with options.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Share {
    Path(String),
    Full(ShareOptions),
}

/// `serve_sources`: opt out of source-leak protection. `false`/absent keeps
/// protection on; `true` serves every source extension raw; a list serves only
/// the named extensions raw (least privilege), e.g. `[".php"]`.
#[derive(Debug, Clone, Default)]
pub enum ServeSources {
    #[default]
    None,
    All,
    Only(Vec<String>),
}

impl ServeSources {
    /// Whether this opts out of protection in any form (`true` or a list).
    pub fn is_enabled(&self) -> bool {
        !matches!(self, ServeSources::None)
    }
}

impl<'de> Deserialize<'de> for ServeSources {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = ServeSources;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a boolean or a list of source extensions to serve")
            }

            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<ServeSources, E> {
                Ok(if v { ServeSources::All } else { ServeSources::None })
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<ServeSources, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut exts = Vec::new();
                while let Some(e) = seq.next_element::<String>()? {
                    exts.push(e);
                }
                Ok(ServeSources::Only(exts))
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareOptions {
    pub share: OneOrMany<String>,
    #[serde(default)]
    pub index: Option<String>,
    #[serde(default)]
    pub types: Option<OneOrMany<String>>,
    #[serde(default)]
    pub follow_symlinks: Option<bool>,
    /// Opt-out of source-leak protection: serve files with extensions the
    /// runtime modules declared as executable sources (.php, ...). Off by
    /// default on purpose. `true` = all sources; `[".php", ...]` = only those.
    #[serde(default)]
    pub serve_sources: ServeSources,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Application {
    /// Module binary suffix: `php85` -> `<modules>/buran-php85`. Exact match,
    /// no version resolution by design.
    pub module: String,
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub index: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub working_directory: Option<String>,
    /// Extra extensions the module must treat as executable (legacy "PHP in
    /// .html" apps). Automatically excluded from static serving in shares
    /// that fall back to this application.
    #[serde(default)]
    pub execute: Vec<String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub processes: Processes,
    /// Cap on concurrent requests per worker process. The effective value
    /// is min(declared in Hello, this cap): 1 for blocking runtimes stays 1
    /// no matter what the config says; event-loop runtimes get bounded.
    /// Absent = trust the module's declaration.
    #[serde(default)]
    pub concurrency: Option<u32>,
    #[serde(default)]
    pub queue: Queue,
    #[serde(default)]
    pub limits: Limits,
    /// Module-specific options; passed through to the module verbatim,
    /// validated by the module itself at `--check-config` time.
    #[serde(default)]
    pub options: Option<serde_norway::Value>,
}

#[derive(Debug, Clone, Default)]
pub enum Processes {
    Fixed(u32),
    Dynamic {
        max: u32,
        spare: u32,
        idle_timeout: u64,
    },
    /// `processes: auto` or an omitted `processes:`. Resolved to `Fixed(N)`
    /// (N = effective CPUs) at config load, before validation or the router
    /// ever sees it — see `crate::auto_worker_count` and `validate`.
    #[default]
    Auto,
}

impl Processes {
    pub fn max(&self) -> u32 {
        match *self {
            Self::Fixed(n) => n,
            Self::Dynamic { max, .. } => max,
            Self::Auto => crate::auto_worker_count(),
        }
    }
}

/// Accepts a positive integer (`Fixed`), the string `auto` (`Auto`), or a
/// `{ max, spare?, idle_timeout? }` table (`Dynamic`). Hand-written because
/// `#[serde(untagged)]` cannot mix a bare string variant with a numeric and a
/// map one, and untagged silently swallows unknown keys — we want them to fail.
impl<'de> Deserialize<'de> for Processes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ProcessesVisitor;

        impl<'de> serde::de::Visitor<'de> for ProcessesVisitor {
            type Value = Processes;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a positive integer, \"auto\", or a { max, spare, idle_timeout } table")
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Processes, E> {
                let n = u32::try_from(v).map_err(|_| E::custom(format!("processes {v} exceeds u32")))?;
                Ok(Processes::Fixed(n))
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Processes, E> {
                let n = u32::try_from(v).map_err(|_| E::custom(format!("processes {v} out of range")))?;
                Ok(Processes::Fixed(n))
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Processes, E> {
                if v == "auto" {
                    Ok(Processes::Auto)
                } else {
                    Err(E::custom(format!("expected a number, \"auto\", or a table, got \"{v}\"")))
                }
            }

            fn visit_map<A>(self, map: A) -> Result<Processes, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let raw = <DynamicRaw as Deserialize>::deserialize(
                    serde::de::value::MapAccessDeserializer::new(map),
                )?;
                Ok(Processes::Dynamic { max: raw.max, spare: raw.spare, idle_timeout: raw.idle_timeout })
            }
        }

        deserializer.deserialize_any(ProcessesVisitor)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DynamicRaw {
    max: u32,
    #[serde(default)]
    spare: u32,
    #[serde(default = "default_idle_timeout")]
    idle_timeout: u64,
}

fn default_idle_timeout() -> u64 {
    20
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Queue {
    /// Memory guard: parked requests are cheap, the cap only bounds memory.
    pub max: u32,
    /// Seconds a request may wait in the queue before an instant 503.
    pub timeout: u64,
}

impl Default for Queue {
    fn default() -> Self {
        Self { max: 24000, timeout: 15 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    /// Seconds the router waits for the worker's next output while a client is
    /// attached. Exceeded -> 504 to the client + Abort to the worker; the
    /// worker is NOT killed (analogue of nginx `fastcgi_read_timeout`).
    pub response_timeout: u64,
    /// Seconds a worker may spend on one task in total, including background
    /// work after `fastcgi_finish_request`. Exceeded -> Abort; the worker is
    /// killed only if it refuses to wind down (analogue of fpm
    /// `request_terminate_timeout`, but wall-clock and background-inclusive).
    pub task_timeout: u64,
    /// Worker is recycled after this many requests (0 = never).
    pub requests: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { response_timeout: 60, task_timeout: 300, requests: 0 }
    }
}

/// Effective per-worker concurrency applied when a module declares
/// `CONCURRENCY_UNBOUNDED` and the application sets no `concurrency` cap: a
/// finite ceiling so the reaping/backstop always has a bound (memory is
/// separately bounded pool-wide by `queue.max`).
pub const DEFAULT_CONCURRENCY_CAP: u32 = 1024;
