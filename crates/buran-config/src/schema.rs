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
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub settings: Settings,
    pub listeners: BTreeMap<String, Listener>,
    pub routes: BTreeMap<String, Vec<RouteStep>>,
    pub applications: BTreeMap<String, Application>,
    pub access_log: Option<String>,
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
            body_temp_path: "/var/tmp/buran".to_string(),
            static_: None,
            server_version: true,
            websocket: WebsocketSettings::default(),
        }
    }
}

/// Upgraded (WebSocket) connections live outside the regular HTTP budgets:
/// limits.timeout and http.idle_timeout do not apply to them.
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
    /// default on purpose.
    #[serde(default)]
    pub serve_sources: bool,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Processes {
    Fixed(u32),
    Dynamic {
        max: u32,
        #[serde(default)]
        spare: u32,
        #[serde(default = "default_idle_timeout")]
        idle_timeout: u64,
    },
}

impl Processes {
    pub fn max(&self) -> u32 {
        match *self {
            Self::Fixed(n) => n,
            Self::Dynamic { max, .. } => max,
        }
    }
}

impl Default for Processes {
    fn default() -> Self {
        Self::Fixed(1)
    }
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
    /// Seconds a worker may spend on one request before SIGKILL + respawn.
    pub timeout: u64,
    /// Worker is recycled after this many requests (0 = never).
    pub requests: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { timeout: 30, requests: 0 }
    }
}
