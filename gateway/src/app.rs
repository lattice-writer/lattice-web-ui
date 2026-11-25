//! Routes, sign-in and the two listeners.

use crate::proxy::{self, empty, full, Body, Bridge, Upstream};
use crate::store::{self, Store, StoreError};
use crate::Args;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::broadcast;

const OWNER_SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const FAILURE_WINDOW: Duration = Duration::from_secs(10 * 60);
const MAX_FAILURES: u32 = 20;
const MAX_JSON: usize = 4096;

const PAGE_HTML: &str = include_str!("../assets/page.html");
const PAGE_CSS: &str = include_str!("../assets/gateway.css");
const PAGE_JS: &str = include_str!("../assets/gateway.js");
const BRIDGE_JS: &str = include_str!("../assets/bridge.js");
const INTER_LATIN: &[u8] = include_bytes!("../assets/vendor/inter-latin-wght-normal.woff2");
const INTER_LATIN_EXT: &[u8] = include_bytes!("../assets/vendor/inter-latin-ext-wght-normal.woff2");
// lattice-web-ui's own icon: the browser tab, a phone's home screen, Lattice's welcome screen.
const BRAND_LOGO: &str = include_str!("../assets/brand/logo.svg");
const BRAND_ICON_192: &[u8] = include_bytes!("../assets/brand/icon-192.png");
const BRAND_ICON_512: &[u8] = include_bytes!("../assets/brand/icon-512.png");
const BRAND_TOUCH: &[u8] = include_bytes!("../assets/brand/apple-touch-icon.png");
const BRAND_FAVICON: &[u8] = include_bytes!("../assets/brand/favicon-32.png");
const BRAND_MANIFEST: &str = include_str!("../assets/brand/manifest.webmanifest");
/// How long an address Lattice asked to open waits for a tab to claim it.
const OPEN_REQUEST_TTL: Duration = Duration::from_secs(60);
/// Set by the gateway on requests that tailscaled delivered over its socket;
/// stripped from everything else, so only Tailscale can vouch for a login.
const TAILNET_MARK: &str = "x-lattice-gateway-tailnet";
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; manifest-src 'self'";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Entrance {
    Public,
    Agent,
}

#[derive(Clone, Debug, PartialEq)]
enum Principal {
    Device { id: String, label: String },
    Owner,
}

impl Principal {
    fn name(&self) -> String {
        match self {
            Principal::Device { id, .. } => format!("device:{id}"),
            Principal::Owner => "owner".into(),
        }
    }
}

pub struct App {
    store: Store,
    upstream: Upstream,
    public_origin: String,
    owner_logins: Vec<String>,
    token: String,
    owner_sessions: Mutex<HashMap<String, Instant>>,
    failures: Mutex<(Instant, u32)>,
    revocations: broadcast::Sender<String>,
    device_cookie: &'static str,
    owner_cookie: &'static str,
    secure: bool,
    bridge: Bridge,
    open_requests: Option<std::path::PathBuf>,
    projects_root: Option<std::path::PathBuf>,
    agent_origin: Option<String>,
    agent_marker: Option<std::path::PathBuf>,
    agent_upstream: Mutex<Option<Arc<Upstream>>>,
    agent_bridge: Bridge,
    tex: Option<crate::tex::Tex>,
    /// Lattice gives "/" its first workspace; with this, a tab that has no
    /// session of its own resumes the workspace used last, on any device.
    last_workspace: LastWorkspace,
    updates: Option<crate::update::Updates>,
    /// Present once the owner has answered the phone hint.
    phone_hint: std::path::PathBuf,
    /// Gzip and conditional answers for whatever goes out.
    encoder: crate::compress::Encoder,
    /// Files tabs save, handed to the device as downloads.
    downloads: crate::downloads::Downloads,
    /// The folder a tab's Open panel browses: the home folder the gateway runs in.
    home: Option<std::path::PathBuf>,
}

/// The Lattice workspaces browser tabs hold through the gateway.
///
/// Lattice gives a tab with no session of its own the first workspace "/"
/// opened, and a new workspace once that one is gone, which then fails to
/// open a project another workspace still holds. The gateway sees every tab's
/// bridge connection, so it sends such a tab to the workspace attached to last
/// that is still alive: one a tab holds now, or held within Lattice's
/// five-second reconnect grace.
#[derive(Default, Clone)]
struct LastWorkspace(Arc<Mutex<HashMap<String, Attachment>>>);

#[derive(Clone, Copy)]
struct Attachment {
    attached: Instant,
    open: u32,
    closed: Option<Instant>,
}

/// Lattice keeps a workspace this long after its last tab leaves (RECONNECT_GRACE).
const WORKSPACE_GRACE: Duration = Duration::from_secs(5);

/// Held for as long as a tab's bridge connection is open.
struct Attached {
    workspaces: LastWorkspace,
    token: String,
}

impl Drop for Attached {
    fn drop(&mut self) {
        let mut map = self.workspaces.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = map.get_mut(&self.token) {
            entry.open = entry.open.saturating_sub(1);
            if entry.open == 0 {
                entry.closed = Some(Instant::now());
            }
        }
    }
}

impl LastWorkspace {
    /// Note a tab attaching to its workspace, and send a tab with no session
    /// of its own (a new device, a new tab, a restarted browser) to the live
    /// workspace attached to last. Lattice ignores a token it no longer knows
    /// and then picks a workspace as before. The returned guard is held for
    /// the life of a bridge connection.
    fn follow(&self, uri: &mut hyper::Uri) -> Option<Attached> {
        let path = uri.path();
        let query = uri.query().unwrap_or("");
        let param = |name: &str| {
            query
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        };
        if path == "/__lattice_bridge" {
            if param("role").as_deref() != Some("browser") {
                return None;
            }
            let token = param("token").filter(|t| valid_session_token(t))?;
            let mut map = self.0.lock().unwrap_or_else(|p| p.into_inner());
            let now = Instant::now();
            // Forget workspaces Lattice has let go of.
            map.retain(|_, a| a.open > 0 || a.closed.is_none_or(|c| now - c < WORKSPACE_GRACE * 4));
            let entry = map.entry(token.clone()).or_insert(Attachment {
                attached: now,
                open: 0,
                closed: None,
            });
            entry.attached = now;
            entry.open += 1;
            entry.closed = None;
            return Some(Attached {
                workspaces: self.clone(),
                token,
            });
        }
        if path != "/__lattice_session" || param("token").is_some() || param("entry").is_some() {
            return None;
        }
        let token = {
            let map = self.0.lock().unwrap_or_else(|p| p.into_inner());
            let now = Instant::now();
            map.iter()
                .filter(|(_, a)| a.open > 0 || a.closed.is_some_and(|c| now - c < WORKSPACE_GRACE))
                .max_by_key(|(_, a)| a.attached)
                .map(|(token, _)| token.clone())
        }?;
        let joined = if query.is_empty() {
            format!("{path}?token={token}")
        } else {
            format!("{path}?{query}&token={token}")
        };
        if let Ok(resumed) = joined.parse() {
            *uri = resumed;
        }
        None
    }
}

fn canonical_origin(value: &str, allow_http: bool) -> Result<String, String> {
    let ok_scheme = value.starts_with("https://") || (allow_http && value.starts_with("http://"));
    let rest = value.split_once("://").map(|(_, r)| r).unwrap_or("");
    if !ok_scheme
        || rest.is_empty()
        || rest.contains('/')
        || rest.contains('@')
        || value.ends_with(":443")
    {
        return Err(format!("{value:?} is not an origin like https://workspace.example.ts.net (no path, no default port)."));
    }
    Ok(value.to_ascii_lowercase())
}

fn read_or_create_token(path: &Path) -> Result<String, String> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let mode = fs::metadata(path)
                .map_err(|e| e.to_string())?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                return Err(format!("{} must be private (chmod 600).", path.display()));
            }
            let token = text.trim().to_string();
            if token.len() < 32 {
                return Err(format!(
                    "{} holds a token shorter than 32 characters.",
                    path.display()
                ));
            }
            Ok(token)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let token = store::random_hex(32);
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            writeln!(file, "{token}").map_err(|e| e.to_string())?;
            Ok(token)
        }
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

impl App {
    pub fn new(args: &Args) -> Result<Self, String> {
        let public_origin = canonical_origin(&args.public_origin, args.insecure_dev_cookies)?;
        let agent_entrance = args.agent_listen.is_some() || args.agent_serve_socket.is_some();
        if agent_entrance != args.agent_origin.is_some()
            || agent_entrance != args.agent_process_marker.is_some()
        {
            return Err(
                "An Agent entrance (--agent-listen or --agent-serve-socket) needs --agent-origin and --agent-process-marker.".into(),
            );
        }
        if (args.serve_socket.is_some() || args.agent_serve_socket.is_some())
            && args.owner_logins.is_none()
        {
            return Err(
                "A Tailscale socket needs --owner-logins: the logins it signs in as the owner."
                    .into(),
            );
        }
        let owner_logins = match &args.owner_logins {
            Some(path) => fs::read_to_string(path)
                .map_err(|e| format!("{}: {e}", path.display()))?
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_string)
                .collect(),
            None => Vec::new(),
        };
        if args.owner_logins.is_some() && owner_logins.is_empty() {
            return Err("The owner logins file lists no login.".into());
        }
        let state_dir = &args.state_dir;
        if !args.upstream.ip().is_loopback() {
            return Err("--upstream must be a loopback address: Lattice's browser host has no authentication of its own.".into());
        }
        let token = read_or_create_token(&args.token_file)?;
        let (revocations, _) = broadcast::channel(64);
        let secure = !args.insecure_dev_cookies;
        Ok(Self {
            store: Store::open(state_dir)?,
            upstream: Upstream::new(args.upstream),
            public_origin,
            owner_logins,
            token,
            owner_sessions: Mutex::new(HashMap::new()),
            failures: Mutex::new((Instant::now(), 0)),
            revocations,
            device_cookie: if secure {
                "__Host-lattice_device"
            } else {
                "lattice_device"
            },
            owner_cookie: if secure {
                "__Host-lattice_owner"
            } else {
                "lattice_owner"
            },
            secure,
            bridge: Bridge {
                inject: true,
                rewrite: args.rewrite_loopback_page,
                ..Bridge::default()
            },
            agent_origin: args
                .agent_origin
                .as_deref()
                .map(|o| canonical_origin(o, args.insecure_dev_cookies))
                .transpose()?,
            agent_marker: args.agent_process_marker.clone(),
            agent_upstream: Mutex::new(None),
            agent_bridge: Bridge::default(),
            last_workspace: LastWorkspace::default(),
            updates: args.update_dir.clone().map(crate::update::Updates::new),
            phone_hint: args.state_dir.join("phone-hint-answered"),
            encoder: Default::default(),
            downloads: crate::downloads::Downloads::new(args.state_dir.join("downloads")),
            home: std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .filter(|h| h.is_absolute()),
            tex: args.tlmgr.clone().map(|tlmgr| crate::tex::Tex {
                tlmgr,
                dry_run: args.tlmgr_dry_run,
            }),
            open_requests: args.open_requests.clone(),
            projects_root: args.projects_root.clone(),
        })
    }

    fn cookie(&self, headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.to_string())
    }

    fn set_cookie(&self, name: &str, value: &str, max_age: Option<u64>) -> HeaderValue {
        let mut cookie = format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax");
        if self.secure {
            cookie.push_str("; Secure");
        }
        if let Some(seconds) = max_age {
            cookie.push_str(&format!("; Max-Age={seconds}"));
        }
        HeaderValue::from_str(&cookie).expect("cookie is ASCII")
    }

    fn principal(&self, entrance: Entrance, headers: &HeaderMap) -> Option<Principal> {
        let _ = entrance;
        // On the tailnet, Tailscale tells us who is asking: the owner needs no sign-in.
        if headers.contains_key(TAILNET_MARK) {
            if let Some(login) = headers
                .get("tailscale-user-login")
                .and_then(|v| v.to_str().ok())
            {
                if self
                    .owner_logins
                    .iter()
                    .any(|l| l.eq_ignore_ascii_case(login))
                {
                    return Some(Principal::Owner);
                }
            }
        }
        if let Some(session) = self.cookie(headers, self.owner_cookie) {
            let mut sessions = self
                .owner_sessions
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            sessions.retain(|_, created| created.elapsed() < OWNER_SESSION_TTL);
            if sessions
                .keys()
                .any(|k| store::constant_eq(k.as_bytes(), store::digest(&session).as_bytes()))
            {
                return Some(Principal::Owner);
            }
        }
        let credential = self.cookie(headers, self.device_cookie)?;
        self.store
            .authenticate(&credential)
            .map(|d| Principal::Device {
                id: d.id,
                label: d.label,
            })
    }

    /// Count a failed sign-in or invite; true when the caller should be slowed down.
    fn too_many_failures(&self, failed: bool) -> bool {
        let mut failures = self.failures.lock().unwrap_or_else(|p| p.into_inner());
        if failures.0.elapsed() > FAILURE_WINDOW {
            *failures = (Instant::now(), 0);
        }
        if failed {
            failures.1 += 1;
        }
        failures.1 >= MAX_FAILURES
    }

    fn origin_for(&self, entrance: Entrance) -> &str {
        match entrance {
            Entrance::Public => &self.public_origin,
            Entrance::Agent => self.agent_origin.as_deref().unwrap_or(&self.public_origin),
        }
    }

    /// State-changing requests must come from our own page.
    fn same_origin(&self, entrance: Entrance, headers: &HeaderMap) -> bool {
        headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|o| o.eq_ignore_ascii_case(self.origin_for(entrance)))
    }
}

fn json_response(status: StatusCode, value: Value) -> Response<Body> {
    let mut response = Response::new(full(value.to_string()));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn error(status: StatusCode, code: &str, message: &str) -> Response<Body> {
    json_response(
        status,
        json!({ "error": { "code": code, "message": message } }),
    )
}

fn page(name: &str, origin: &str) -> Response<Body> {
    let html = PAGE_HTML
        .replace("{{page}}", name)
        .replace("{{origin}}", origin);
    let mut response = Response::new(full(html));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert("content-security-policy", HeaderValue::from_static(CSP));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn asset(content_type: &'static str, body: impl Into<bytes::Bytes>) -> Response<Body> {
    let body: bytes::Bytes = body.into();
    // "no-cache" revalidates on every load; the tag lets that be a 304.
    let digest = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(&body));
    let tag = format!("\"{}\"", &digest[..16]);
    let mut response = Response::new(full(body));
    if let Ok(tag) = HeaderValue::from_str(&tag) {
        response.headers_mut().insert(header::ETAG, tag);
    }
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn redirect(location: &str) -> Response<Body> {
    let mut response = Response::new(empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(location).unwrap_or(HeaderValue::from_static("/")),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn read_json(req: Request<Incoming>) -> Result<Value, Box<Response<Body>>> {
    let too_large = || {
        Box::new(error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "The request is too large.",
        ))
    };
    if req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|n| n > MAX_JSON)
    {
        return Err(too_large());
    }
    let limited = http_body_util::Limited::new(req.into_body(), MAX_JSON);
    let bytes = limited.collect().await.map_err(|_| too_large())?.to_bytes();
    serde_json::from_slice(&bytes).map_err(|_| {
        Box::new(error(
            StatusCode::BAD_REQUEST,
            "bad_json",
            "The request body is not JSON.",
        ))
    })
}

fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/html"))
}

fn device_json(d: &store::Device) -> Value {
    json!({ "id": d.id, "label": d.label, "createdAt": d.created_at, "lastSeenAt": d.last_seen_at, "expiresAt": d.expires_at })
}

/// A QR code in half-block characters, light on a dark terminal.
pub fn qr_text(text: &str) -> String {
    use qrcode::render::unicode::Dense1x2;
    qrcode::QrCode::new(text.as_bytes())
        .map(|code| {
            code.render::<Dense1x2>()
                .dark_color(Dense1x2::Light)
                .light_color(Dense1x2::Dark)
                .quiet_zone(true)
                .build()
        })
        .map(|art| format!("{art}\n"))
        .unwrap_or_default()
}

fn qr_svg(text: &str) -> String {
    use qrcode::render::svg;
    qrcode::QrCode::new(text.as_bytes())
        .map(|code| {
            code.render::<svg::Color>()
                .min_dimensions(220, 220)
                .quiet_zone(true)
                .build()
        })
        .unwrap_or_default()
}

async fn handle(app: Arc<App>, entrance: Entrance, req: Request<Incoming>) -> Response<Body> {
    let key = format!(
        "{entrance:?}{}",
        req.uri()
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
    );
    let asked = crate::compress::Asked::from(key, req.method(), req.headers());
    let response = route(app.clone(), entrance, req).await;
    app.encoder.finish(asked, response).await
}

async fn route(app: Arc<App>, entrance: Entrance, req: Request<Incoming>) -> Response<Body> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let principal = app.principal(entrance, req.headers());
    let origin = app.origin_for(entrance).to_string();

    if entrance == Entrance::Agent {
        return agent_request(app, principal, req).await;
    }

    match (entrance, &method, path.as_str()) {
        (_, &Method::GET, "/__gateway/gateway.css") => {
            return asset("text/css; charset=utf-8", PAGE_CSS)
        }
        (_, &Method::GET, "/__gateway/gateway.js") => {
            return asset("text/javascript; charset=utf-8", PAGE_JS)
        }
        (_, &Method::GET, "/__gateway/bridge.js") => {
            return asset("text/javascript; charset=utf-8", BRIDGE_JS)
        }
        (_, &Method::GET, "/__gateway/brand/logo.svg") => {
            return asset("image/svg+xml", BRAND_LOGO)
        }
        (_, &Method::GET, "/__gateway/brand/icon-192.png") => {
            return asset("image/png", BRAND_ICON_192)
        }
        (_, &Method::GET, "/__gateway/brand/icon-512.png") => {
            return asset("image/png", BRAND_ICON_512)
        }
        // Browsers ask for these at the root on their own; Lattice would answer
        // with its icon (or its page).
        (_, &Method::GET, "/favicon.ico") => return asset("image/png", BRAND_FAVICON),
        (_, &Method::GET, "/apple-touch-icon.png" | "/apple-touch-icon-precomposed.png") => {
            return asset("image/png", BRAND_TOUCH)
        }
        (_, &Method::GET, "/__gateway/brand/apple-touch-icon.png") => {
            return asset("image/png", BRAND_TOUCH)
        }
        (_, &Method::GET, "/__gateway/brand/favicon-32.png") => {
            return asset("image/png", BRAND_FAVICON)
        }
        (_, &Method::GET, "/__gateway/brand/manifest.webmanifest") => {
            return asset("application/manifest+json", BRAND_MANIFEST)
        }
        (_, &Method::GET, "/__gateway/health") => {
            let up = app.upstream.reachable().await;
            let status = if up {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            return json_response(
                status,
                json!({
                    "gateway": "ok",
                    "version": env!("CARGO_PKG_VERSION"),
                    "lattice": up,
                    "rewritePage": app.bridge.rewrite,
                    "rewrites": app.bridge.counters.snapshot(),
                }),
            );
        }
        _ => {}
    }

    match (&method, path.as_str()) {
        (&Method::GET, "/__gateway/login") if entrance == Entrance::Public => {
            return page("login", &origin)
        }
        (&Method::GET, "/__gateway/pair") if entrance == Entrance::Public => {
            return page("pair", &origin)
        }
        (&Method::GET, "/__gateway/api/projects") if entrance == Entrance::Public => {
            if principal.is_none() {
                return error(
                    StatusCode::UNAUTHORIZED,
                    "sign_in_required",
                    "Pair this browser or sign in with the owner token.",
                );
            }
            let projects = list_projects(app.projects_root.as_deref());
            let root = app
                .projects_root
                .as_deref()
                .map(|root| root.to_string_lossy().into_owned());
            return json_response(
                StatusCode::OK,
                json!({ "root": root, "projects": projects }),
            );
        }
        (&Method::GET, "/__gateway/vendor/inter-latin-wght-normal.woff2") => {
            return asset("font/woff2", INTER_LATIN)
        }
        (&Method::GET, "/__gateway/vendor/inter-latin-ext-wght-normal.woff2") => {
            return asset("font/woff2", INTER_LATIN_EXT)
        }
        // Phone and app is a dialog in the workspace now; this address opens it there.
        (&Method::GET, "/__gateway/devices") => {
            return if principal == Some(Principal::Owner) {
                redirect("/?lattice-gateway=phone")
            } else {
                redirect("/__gateway/login?next=%2F%3Flattice-gateway%3Dphone")
            };
        }
        (&Method::GET, "/__gateway/api/config") => {
            if principal.is_none() {
                return error(
                    StatusCode::UNAUTHORIZED,
                    "sign_in_required",
                    "Pair this browser or sign in with the owner token.",
                );
            }
            return json_response(
                StatusCode::OK,
                json!({
                    "agentOrigin": app.agent_origin,
                    "tex": app.tex.is_some(),
                }),
            );
        }
        (&Method::GET, "/__gateway/api/phone") => {
            if principal != Some(Principal::Owner) {
                return error(
                    StatusCode::UNAUTHORIZED,
                    "owner_required",
                    "Sign in as the owner to set up a phone.",
                );
            }
            return json_response(
                StatusCode::OK,
                json!({
                    "address": app.public_origin,
                    "qrSvg": qr_svg(&format!("{}/", app.public_origin)),
                    "tailnet": !app.owner_logins.is_empty(),
                    "signedIn": app.cookie(req.headers(), app.owner_cookie).is_some(),
                    "hintDismissed": app.phone_hint.exists(),
                }),
            );
        }
        (&Method::GET, "/__gateway/api/whoami") => {
            return match &principal {
                Some(Principal::Owner) => json_response(
                    StatusCode::OK,
                    json!({ "kind": "owner", "publicOrigin": app.public_origin }),
                ),
                Some(Principal::Device { label, .. }) => {
                    json_response(StatusCode::OK, json!({ "kind": "device", "label": label }))
                }
                None => json_response(StatusCode::OK, json!({ "kind": "anonymous" })),
            };
        }
        (&Method::POST, p) if p.starts_with("/__gateway/api/") => {
            if !app.same_origin(entrance, req.headers()) {
                return error(
                    StatusCode::FORBIDDEN,
                    "cross_origin",
                    "This request did not come from the gateway's own page.",
                );
            }
            return api(app.clone(), entrance, principal, p.to_string(), req).await;
        }
        (&Method::GET, p)
            if p.starts_with("/__gateway/download/") && entrance == Entrance::Public =>
        {
            if principal.is_none() {
                return error(
                    StatusCode::UNAUTHORIZED,
                    "sign_in_required",
                    "Sign in to download.",
                );
            }
            let id = p["/__gateway/download/".len()..].to_string();
            let Some(saved) = app.downloads.take(&id) else {
                return error(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "That download is not there.",
                );
            };
            {
                let app = app.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(crate::downloads::KEEP_AFTER_FETCH).await;
                    app.downloads.forget(&id);
                });
            }
            let mut response = Response::new(full(saved.bytes));
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(crate::downloads::content_type(&saved.name)),
            );
            if let Ok(value) = HeaderValue::from_str(&crate::downloads::disposition(&saved.name)) {
                headers.insert(header::CONTENT_DISPOSITION, value);
            }
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            headers.insert(
                "x-content-type-options",
                HeaderValue::from_static("nosniff"),
            );
            return response;
        }
        (&Method::GET, "/__gateway/api/update") => {
            if principal != Some(Principal::Owner) {
                return error(
                    StatusCode::UNAUTHORIZED,
                    "owner_required",
                    "Only the owner updates this host.",
                );
            }
            let state = match &app.updates {
                Some(updates) => updates.state(env!("CARGO_PKG_VERSION")),
                None => {
                    json!({ "current": env!("CARGO_PKG_VERSION"), "available": false, "updates": false })
                }
            };
            return json_response(StatusCode::OK, state);
        }
        (&Method::GET, "/__gateway/api/devices") => {
            if principal != Some(Principal::Owner) {
                return error(
                    StatusCode::UNAUTHORIZED,
                    "owner_required",
                    "Sign in as the owner to manage devices.",
                );
            }
            let devices: Vec<Value> = app.store.list().iter().map(device_json).collect();
            return json_response(StatusCode::OK, json!({ "devices": devices }));
        }
        (_, p) if p.starts_with("/__gateway/") => {
            return error(StatusCode::NOT_FOUND, "not_found", "No such gateway page.")
        }
        _ => {}
    }

    let Some(principal) = principal else {
        if method == Method::GET && wants_html(req.headers()) {
            let next = req
                .uri()
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("/");
            let safe = if next.starts_with('/') && !next.starts_with("//") {
                next
            } else {
                "/"
            };
            return redirect(&format!("/__gateway/login?next={}", urlencode(safe)));
        }
        return error(
            StatusCode::UNAUTHORIZED,
            "sign_in_required",
            "Pair this browser or sign in with the owner token.",
        );
    };
    // Cross-site WebSocket hijacking and CSRF: the browser's Origin, when sent,
    // must be ours. Same-origin fetches and navigations pass.
    if let Some(sent) = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        if !sent.eq_ignore_ascii_case(&app.public_origin) {
            return error(
                StatusCode::FORBIDDEN,
                "cross_origin",
                "Requests from other sites cannot reach this workspace.",
            );
        }
    }
    let mut req = req;
    let attached = app.last_workspace.follow(req.uri_mut());
    proxy::forward(
        req,
        &app.upstream,
        &app.public_origin,
        principal.name(),
        app.revocations.subscribe(),
        &app.bridge,
        attached.map(|a| Box::new(a) as Box<dyn std::any::Any + Send>),
    )
    .await
}

/// The Agent entrance: the same sign-in as the workspace (cookies are shared
/// across ports of one host), then Synara on the port the workspace registered.
async fn agent_request(
    app: Arc<App>,
    principal: Option<Principal>,
    req: Request<Incoming>,
) -> Response<Body> {
    let Some(principal) = principal else {
        return error(
            StatusCode::UNAUTHORIZED,
            "sign_in_required",
            "Open the workspace and sign in first.",
        );
    };
    let agent_origin = app.agent_origin.clone().unwrap_or_default();
    if let Some(sent) = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        if !sent.eq_ignore_ascii_case(&agent_origin)
            && !sent.eq_ignore_ascii_case(&app.public_origin)
        {
            return error(
                StatusCode::FORBIDDEN,
                "cross_origin",
                "Requests from other sites cannot reach the Agent.",
            );
        }
    }
    let upstream = app
        .agent_upstream
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let Some(upstream) = upstream else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_not_started",
            "Open the Agent panel in the workspace first.",
        );
    };
    proxy::forward(
        req,
        &upstream,
        &agent_origin,
        principal.name(),
        app.revocations.subscribe(),
        &app.agent_bridge,
        None,
    )
    .await
}

/// Subfolders of the projects folder, newest first. Symlinks and hidden folders are skipped.
/// Lattice's session tokens are URL-safe; anything else is not remembered.
fn valid_session_token(token: &str) -> bool {
    (16..=128).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A file a tab uploads from its device, written into the folder its Open
/// panel shows (headers `x-upload-folder` and `x-upload-name`, both
/// %-encoded), under a free name. Answers its path on the host.
async fn upload(app: &App, req: Request<Incoming>) -> Response<Body> {
    use tokio::io::AsyncWriteExt;
    let Some(home) = app.home.as_deref() else {
        return error(
            StatusCode::NOT_FOUND,
            "no_home",
            "The gateway does not know its home folder.",
        );
    };
    let header = |name: &str| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(crate::files::percent_decode)
    };
    let (Some(folder), Some(name)) = (header("x-upload-folder"), header("x-upload-name")) else {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_upload",
            "The upload names no folder or file.",
        );
    };
    let too_large = || {
        error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "Files larger than 1 GB cannot be uploaded here.",
        )
    };
    if req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > crate::files::MAX_UPLOAD)
    {
        return too_large();
    }
    let target = match crate::files::upload_path(home, Path::new(&folder), &name) {
        Ok(target) => target,
        Err(message) => return error(StatusCode::BAD_REQUEST, "upload_refused", &message),
    };
    let part = target.with_file_name(format!(".lattice-upload-{}.part", store::random_hex(6)));
    let failed = |e: String| {
        let _ = fs::remove_file(&part);
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upload_failed",
            &format!("The upload failed: {e}"),
        )
    };
    let mut file = match tokio::fs::File::create(&part).await {
        Ok(file) => file,
        Err(e) => return failed(e.to_string()),
    };
    let mut body = req.into_body();
    let mut total: u64 = 0;
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(e) => return failed(e.to_string()),
        };
        if let Ok(data) = frame.into_data() {
            total += data.len() as u64;
            if total > crate::files::MAX_UPLOAD {
                let _ = fs::remove_file(&part);
                return too_large();
            }
            if let Err(e) = file.write_all(&data).await {
                return failed(e.to_string());
            }
        }
    }
    if let Err(e) = file.flush().await {
        return failed(e.to_string());
    }
    drop(file);
    // Never over a file that appeared meanwhile: a hard link fails if the name is taken.
    let mut target = target;
    for _ in 0..3 {
        match fs::hard_link(&part, &target) {
            Ok(()) => {
                let _ = fs::remove_file(&part);
                return json_response(StatusCode::OK, json!({ "path": target.to_string_lossy() }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match crate::files::upload_path(home, Path::new(&folder), &name) {
                    Ok(next) => target = next,
                    Err(message) => return failed(message),
                }
            }
            Err(e) => return failed(e.to_string()),
        }
    }
    failed("no free name".into())
}

fn list_projects(root: Option<&Path>) -> Vec<Value> {
    let Some(root) = root else { return Vec::new() };
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut projects: Vec<(std::time::SystemTime, Value)> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if name.starts_with('.') {
                return None;
            }
            let path = entry.path();
            let has_tex = fs::read_dir(&path)
                .ok()?
                .filter_map(Result::ok)
                .any(|f| f.file_name().to_string_lossy().ends_with(".tex"));
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            Some((
                modified,
                json!({ "name": name, "path": path.to_string_lossy(), "hasTex": has_tex }),
            ))
        })
        .collect();
    projects.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    projects.into_iter().take(100).map(|(_, v)| v).collect()
}

/// Take the newest address Lattice asked to open in the last minute, if any.
/// Only `http://127.0.0.1:18452/?entry=<nonce>` is accepted; it is handed back
/// as a path on the public origin.
fn claim_open_request(dir: Option<&Path>, public_origin: &str) -> Option<String> {
    let dir = dir?;
    let mut newest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in fs::read_dir(dir).ok()?.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        if !meta.is_file()
            || modified
                .elapsed()
                .map_or(true, |age| age > OPEN_REQUEST_TTL)
        {
            let _ = fs::remove_file(&path);
            continue;
        }
        if newest.as_ref().is_none_or(|(time, _)| modified > *time) {
            newest = Some((modified, path));
        }
    }
    let (_, path) = newest?;
    let text = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    let text = text.trim();
    // The plugin's "Remote devices" command opens the gateway's device page.
    if text == format!("{public_origin}/__gateway/devices") {
        return Some("/__gateway/devices".into());
    }
    let rest = text.strip_prefix("http://127.0.0.1:18452/?entry=")?;
    (!rest.is_empty()
        && rest.len() <= 128
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then(|| format!("/?entry={rest}"))
}

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn api(
    app: Arc<App>,
    entrance: Entrance,
    principal: Option<Principal>,
    path: String,
    req: Request<Incoming>,
) -> Response<Body> {
    let owner = principal == Some(Principal::Owner);
    match path.as_str() {
        "/__gateway/api/login" if entrance == Entrance::Public => {
            if app.too_many_failures(false) {
                return error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "slow_down",
                    "Too many failed attempts. Wait ten minutes and try again.",
                );
            }
            let body = match read_json(req).await {
                Ok(v) => v,
                Err(r) => return *r,
            };
            let given = body
                .get("token")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if !store::constant_eq(given.as_bytes(), app.token.as_bytes()) {
                app.too_many_failures(true);
                return error(
                    StatusCode::UNAUTHORIZED,
                    "wrong_token",
                    "That token does not match this host's owner token.",
                );
            }
            let session = store::random_hex(32);
            app.owner_sessions
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(store::digest(&session), Instant::now());
            let mut response = json_response(StatusCode::OK, json!({ "kind": "owner" }));
            response.headers_mut().append(
                header::SET_COOKIE,
                app.set_cookie(app.owner_cookie, &session, None),
            );
            response
        }
        "/__gateway/api/logout" => {
            let mut response = json_response(StatusCode::OK, json!({ "kind": "anonymous" }));
            if let Some(session) = app.cookie(req.headers(), app.owner_cookie) {
                app.owner_sessions
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&store::digest(&session));
            }
            response.headers_mut().append(
                header::SET_COOKIE,
                app.set_cookie(app.owner_cookie, "", Some(0)),
            );
            response
        }
        "/__gateway/api/redeem" if entrance == Entrance::Public => {
            if app.too_many_failures(false) {
                return error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "slow_down",
                    "Too many failed attempts. Wait ten minutes and try again.",
                );
            }
            let body = match read_json(req).await {
                Ok(v) => v,
                Err(r) => return *r,
            };
            let code = body.get("code").and_then(Value::as_str).unwrap_or("");
            let label = body.get("label").and_then(Value::as_str).unwrap_or("");
            match app.store.redeem(code, label) {
                Ok((device, credential)) => {
                    let mut response = json_response(
                        StatusCode::OK,
                        json!({ "kind": "device", "label": device.label }),
                    );
                    response.headers_mut().append(
                        header::SET_COOKIE,
                        app.set_cookie(
                            app.device_cookie,
                            &credential,
                            Some(store::DEVICE_TTL_SECS),
                        ),
                    );
                    response
                }
                Err(StoreError::InvalidLabel) => error(
                    StatusCode::BAD_REQUEST,
                    "bad_label",
                    "Name this device with 1 to 48 characters.",
                ),
                Err(StoreError::InvalidInvite) => {
                    app.too_many_failures(true);
                    error(StatusCode::GONE, "invite_used", "This pairing link was already used or has expired. Ask the owner for a new one.")
                }
                Err(StoreError::TooMany) => error(
                    StatusCode::CONFLICT,
                    "too_many_devices",
                    "This workspace already has 32 paired devices. Revoke one first.",
                ),
                Err(StoreError::Storage(e)) => error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "storage",
                    &format!("Could not save the device: {e}"),
                ),
            }
        }
        "/__gateway/api/agent-port" if entrance == Entrance::Public && principal.is_some() => {
            let (Some(agent_origin), Some(marker)) = (&app.agent_origin, &app.agent_marker) else {
                return error(
                    StatusCode::NOT_FOUND,
                    "no_agent",
                    "This host does not serve the Agent.",
                );
            };
            let body = match read_json(req).await {
                Ok(v) => v,
                Err(r) => return *r,
            };
            let Some(port) = body
                .get("port")
                .and_then(Value::as_u64)
                .and_then(|p| u16::try_from(p).ok())
            else {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_port",
                    "Send the Agent's loopback port.",
                );
            };
            if !crate::agent::port_belongs_to_lattice(port, marker) {
                return error(
                    StatusCode::FORBIDDEN,
                    "not_lattice",
                    "That port is not Lattice's Agent.",
                );
            }
            let upstream = Arc::new(Upstream::new(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                port,
            ))));
            *app.agent_upstream.lock().unwrap_or_else(|p| p.into_inner()) = Some(upstream);
            json_response(StatusCode::OK, json!({ "origin": agent_origin }))
        }
        "/__gateway/api/open-request" if entrance == Entrance::Public && principal.is_some() => {
            match claim_open_request(app.open_requests.as_deref(), &app.public_origin) {
                Some(url) => json_response(StatusCode::OK, json!({ "url": url })),
                None => {
                    let mut response = Response::new(empty());
                    *response.status_mut() = StatusCode::NO_CONTENT;
                    response
                }
            }
        }
        "/__gateway/api/download" if entrance == Entrance::Public && principal.is_some() => {
            let body = match read_json(req).await {
                Ok(v) => v,
                Err(r) => return *r,
            };
            let name = body.get("name").and_then(Value::as_str).unwrap_or("");
            match app.downloads.destination(name) {
                Ok((id, path)) => json_response(
                    StatusCode::OK,
                    json!({ "id": id, "path": path.to_string_lossy() }),
                ),
                Err(message) => error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "download_failed",
                    &message,
                ),
            }
        }
        "/__gateway/api/files" | "/__gateway/api/files/folder"
            if entrance == Entrance::Public && principal.is_some() =>
        {
            let Some(home) = app.home.as_deref() else {
                return error(
                    StatusCode::NOT_FOUND,
                    "no_home",
                    "The gateway does not know its home folder.",
                );
            };
            let body = match read_json(req).await {
                Ok(v) => v,
                Err(r) => return *r,
            };
            let asked = body.get("path").and_then(Value::as_str);
            let result = if path.ends_with("/folder") {
                let name = body.get("name").and_then(Value::as_str).unwrap_or("");
                crate::files::make_folder(home, Path::new(asked.unwrap_or("")), name)
                    .map(|made| json!({ "path": made.to_string_lossy() }))
            } else {
                let folder = if body.get("start").and_then(Value::as_bool) == Some(true) {
                    crate::files::start(home, asked, app.projects_root.as_deref())
                } else {
                    Ok(Path::new(asked.unwrap_or("")).to_path_buf())
                };
                let extensions: Vec<String> = body
                    .get("extensions")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .take(64)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let files = body.get("files").and_then(Value::as_bool) == Some(true);
                folder.and_then(|f| crate::files::list(home, &f, files, &extensions))
            };
            match result {
                Ok(value) => json_response(StatusCode::OK, value),
                Err(message) => error(StatusCode::BAD_REQUEST, "files_refused", &message),
            }
        }
        "/__gateway/api/upload" if entrance == Entrance::Public && principal.is_some() => {
            upload(&app, req).await
        }
        "/__gateway/api/phone/hint" if owner => match fs::write(&app.phone_hint, b"") {
            Ok(()) => json_response(StatusCode::OK, json!({ "hintDismissed": true })),
            Err(e) => error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "write_failed",
                &format!("The gateway could not record that: {e}"),
            ),
        },
        "/__gateway/api/update/check" if owner => {
            let Some(updates) = &app.updates else {
                return error(
                    StatusCode::NOT_FOUND,
                    "no_updates",
                    "This host was installed without updates from the browser.",
                );
            };
            match updates.check() {
                Ok(()) => json_response(
                    StatusCode::ACCEPTED,
                    updates.state(env!("CARGO_PKG_VERSION")),
                ),
                Err(reason) => error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "write_failed",
                    &format!("The gateway could not record the request: {reason:?}"),
                ),
            }
        }
        "/__gateway/api/update/start" | "/__gateway/api/update/dismiss" if owner => {
            let Some(updates) = &app.updates else {
                return error(
                    StatusCode::NOT_FOUND,
                    "no_updates",
                    "This host was installed without updates from the browser.",
                );
            };
            let body = match read_json(req).await {
                Ok(v) => v,
                Err(r) => return *r,
            };
            let version = body.get("version").and_then(Value::as_str).unwrap_or("");
            let result = if path.ends_with("/start") {
                updates.request(version, env!("CARGO_PKG_VERSION"))
            } else {
                updates.dismiss(version)
            };
            match result {
                Ok(()) => json_response(
                    StatusCode::ACCEPTED,
                    updates.state(env!("CARGO_PKG_VERSION")),
                ),
                Err(crate::update::Refusal::NotOffered) => error(
                    StatusCode::CONFLICT,
                    "not_offered",
                    "That version is not the update on offer. Reload the page.",
                ),
                Err(crate::update::Refusal::Running) => error(
                    StatusCode::CONFLICT,
                    "running",
                    "An update is already running.",
                ),
                Err(crate::update::Refusal::Io(reason)) => error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "write_failed",
                    &format!("The gateway could not record the request: {reason}"),
                ),
            }
        }
        "/__gateway/api/tex-install" if owner => {
            let Some(tex) = &app.tex else {
                return error(
                    StatusCode::NOT_FOUND,
                    "no_tex",
                    "This host has no TeX installation the gateway may change.",
                );
            };
            let body = match read_json(req).await {
                Ok(v) => v,
                Err(r) => return *r,
            };
            let name = body
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            match tex.install(&name).await {
                Ok((package, output)) => json_response(
                    StatusCode::OK,
                    json!({ "package": package, "output": output }),
                ),
                Err(crate::tex::TexError::Choose(packages)) => json_response(
                    StatusCode::CONFLICT,
                    json!({ "error": {
                        "code": "choose_package",
                        "message": "Several packages provide this file.",
                        "packages": packages,
                    } }),
                ),
                Err(crate::tex::TexError::Failed(message)) => {
                    error(StatusCode::BAD_REQUEST, "tex_install_failed", &message)
                }
            }
        }
        "/__gateway/api/invites" if owner => match app.store.create_invite() {
            Ok(code) => {
                let url = format!("{}/__gateway/pair#{code}", app.public_origin);
                json_response(
                    StatusCode::OK,
                    json!({ "url": url, "expiresIn": store::INVITE_TTL.as_secs(), "qrSvg": qr_svg(&url) }),
                )
            }
            Err(_) => error(
                StatusCode::TOO_MANY_REQUESTS,
                "too_many_invites",
                "Too many open invites. Wait two minutes for them to expire.",
            ),
        },
        p if owner && p.starts_with("/__gateway/api/devices/") && p.ends_with("/revoke") => {
            let id = &p["/__gateway/api/devices/".len()..p.len() - "/revoke".len()];
            match app.store.revoke(id) {
                Ok(true) => {
                    let _ = app.revocations.send(format!("device:{id}"));
                    json_response(StatusCode::OK, json!({ "revoked": id }))
                }
                Ok(false) => error(
                    StatusCode::NOT_FOUND,
                    "no_device",
                    "That device is not paired.",
                ),
                Err(StoreError::Storage(e)) => error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "storage",
                    &format!("Could not save the change: {e}"),
                ),
                Err(_) => error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "storage",
                    "Could not save the change.",
                ),
            }
        }
        _ if !owner => error(
            StatusCode::UNAUTHORIZED,
            "owner_required",
            "Sign in as the owner to do this.",
        ),
        _ => error(
            StatusCode::NOT_FOUND,
            "not_found",
            "No such gateway action.",
        ),
    }
}

fn peer_uid(stream: &tokio::net::UnixStream) -> Option<u32> {
    stream.peer_cred().ok().map(|c| c.uid())
}

/// Tailscale Serve (and Funnel) can hand connections to a Unix socket. Only
/// tailscaled (root, or `--ingress-uid` for a containerized one) may connect,
/// so its identity headers can be trusted: requests are marked as such.
fn listen_tailnet(
    app: Arc<App>,
    socket: &Path,
    entrance: Entrance,
    ingress_uid: Option<u32>,
) -> Result<(), String> {
    if socket.exists() {
        let meta = fs::symlink_metadata(socket).map_err(|e| e.to_string())?;
        use std::os::unix::fs::FileTypeExt;
        if !meta.file_type().is_socket() {
            return Err(format!("{} exists and is not a socket.", socket.display()));
        }
        fs::remove_file(socket).map_err(|e| e.to_string())?;
    }
    let listener = UnixListener::bind(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o660)).map_err(|e| e.to_string())?;
    eprintln!(
        "lattice-gateway: {entrance:?} entrance for Tailscale on {}",
        socket.display()
    );
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let uid = peer_uid(&stream);
            if !(uid == Some(0) || (ingress_uid.is_some() && uid == ingress_uid)) {
                continue; // Only the Tailscale daemon may speak for a login.
            }
            let app = app.clone();
            tokio::spawn(async move {
                let service = service_fn(move |mut req: Request<Incoming>| {
                    let app = app.clone();
                    req.headers_mut()
                        .insert(TAILNET_MARK, HeaderValue::from_static("1"));
                    async move { Ok::<_, std::convert::Infallible>(handle(app, entrance, req).await) }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    Ok(())
}

pub async fn serve(app: Arc<App>, args: &Args) -> Result<(), String> {
    let public = TcpListener::bind(args.listen)
        .await
        .map_err(|e| format!("{}: {e}", args.listen))?;
    eprintln!(
        "lattice-gateway: {} -> Lattice at {}, public origin {}",
        args.listen, app.upstream.addr, app.public_origin
    );
    {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                app.store.flush_seen();
            }
        });
    }
    for (socket, entrance) in [
        (&args.serve_socket, Entrance::Public),
        (&args.agent_serve_socket, Entrance::Agent),
    ] {
        if let Some(socket) = socket {
            listen_tailnet(app.clone(), socket, entrance, args.ingress_uid)?;
        }
    }
    if let Some(address) = args.agent_listen {
        let listener = TcpListener::bind(address)
            .await
            .map_err(|e| format!("{address}: {e}"))?;
        eprintln!("lattice-gateway: Agent entrance on {address}");
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let _ = stream.set_nodelay(true);
                let app = app.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |mut req: Request<Incoming>| {
                        let app = app.clone();
                        req.headers_mut().remove(TAILNET_MARK);
                        async move {
                            Ok::<_, std::convert::Infallible>(
                                handle(app, Entrance::Agent, req).await,
                            )
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await;
                });
            }
        });
    }
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = public.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let _ = stream.set_nodelay(true);
                let app = app.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |mut req: Request<Incoming>| {
                        let app = app.clone();
                        req.headers_mut().remove(TAILNET_MARK);
                        async move { Ok::<_, std::convert::Infallible>(handle(app, Entrance::Public, req).await) }
                    });
                    let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), service).with_upgrades().await;
                });
            }
            _ = &mut shutdown => {
                app.store.flush_seen();
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tab_without_a_session_resumes_the_live_workspace_attached_to_last() {
        const A: &str = "0123456789abcdef0123456789abcdef";
        const B: &str = "fedcba9876543210fedcba9876543210";
        let last = LastWorkspace::default();
        let follow = |uri: &str| {
            let mut uri: hyper::Uri = uri.parse().unwrap();
            let held = last.follow(&mut uri);
            (uri.to_string(), held)
        };
        let session = |uri: &str| follow(uri).0;
        // Nothing attached yet: Lattice picks as before.
        assert_eq!(session("/__lattice_session"), "/__lattice_session");
        // The host's own role and malformed tokens are not remembered.
        assert!(follow(&format!("/__lattice_bridge?token={A}&role=host"))
            .1
            .is_none());
        assert!(follow("/__lattice_bridge?token=bad%20token&role=browser")
            .1
            .is_none());
        assert_eq!(session("/__lattice_session"), "/__lattice_session");

        let tab_a = follow(&format!("/__lattice_bridge?token={A}&role=browser"))
            .1
            .unwrap();
        assert_eq!(
            session("/__lattice_session"),
            format!("/__lattice_session?token={A}")
        );
        // A tab's own session, and an explicit entry, are left alone.
        assert_eq!(
            session("/__lattice_session?token=mine"),
            "/__lattice_session?token=mine"
        );
        assert_eq!(
            session("/__lattice_session?entry=n"),
            "/__lattice_session?entry=n"
        );

        // A later tab moves it; when that tab's workspace is gone, A is still held.
        let tab_b = follow(&format!("/__lattice_bridge?role=browser&token={B}"))
            .1
            .unwrap();
        assert_eq!(
            session("/__lattice_session"),
            format!("/__lattice_session?token={B}")
        );
        drop(tab_b);
        assert_eq!(
            session("/__lattice_session"),
            format!("/__lattice_session?token={B}"),
            "within Lattice's grace"
        );
        last.0.lock().unwrap().get_mut(B).unwrap().closed = Some(Instant::now() - WORKSPACE_GRACE);
        assert_eq!(
            session("/__lattice_session"),
            format!("/__lattice_session?token={A}"),
            "past it"
        );

        // No live workspace: Lattice picks, rather than being sent to a dead one.
        drop(tab_a);
        last.0.lock().unwrap().get_mut(A).unwrap().closed = Some(Instant::now() - WORKSPACE_GRACE);
        assert_eq!(session("/__lattice_session"), "/__lattice_session");
    }

    #[test]
    fn origins_must_be_canonical() {
        assert!(canonical_origin("https://w.example.ts.net", false).is_ok());
        assert!(canonical_origin("https://w.example.ts.net:8443", false).is_ok());
        assert!(canonical_origin("https://w.example/path", false).is_err());
        assert!(canonical_origin("https://w.example:443", false).is_err());
        assert!(canonical_origin("http://127.0.0.1:18490", false).is_err());
        assert!(canonical_origin("http://127.0.0.1:18490", true).is_ok());
    }

    #[test]
    fn open_requests_are_claimed_once_and_only_for_entry_addresses() {
        let dir = std::env::temp_dir().join(format!("lattice-open-{}", store::random_hex(6)));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("1.url"),
            "http://127.0.0.1:18452/?entry=abc_DEF-123\n",
        )
        .unwrap();
        assert_eq!(
            claim_open_request(Some(&dir), "https://w.example").as_deref(),
            Some("/?entry=abc_DEF-123")
        );
        assert_eq!(claim_open_request(Some(&dir), "https://w.example"), None);
        fs::write(dir.join("2.url"), "https://evil.example/?entry=x").unwrap();
        assert_eq!(claim_open_request(Some(&dir), "https://w.example"), None);
        fs::write(dir.join("3.url"), "http://127.0.0.1:18452/?entry=a&b").unwrap();
        assert_eq!(claim_open_request(Some(&dir), "https://w.example"), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lists_project_folders_without_hidden_ones() {
        let dir = std::env::temp_dir().join(format!("lattice-projects-{}", store::random_hex(6)));
        fs::create_dir_all(dir.join("paper")).unwrap();
        fs::write(dir.join("paper/main.tex"), "").unwrap();
        fs::create_dir_all(dir.join(".cache")).unwrap();
        fs::write(dir.join("notes.txt"), "").unwrap();
        let projects = list_projects(Some(&dir));
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0]["name"], "paper");
        assert_eq!(projects[0]["hasTex"], true);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn next_redirects_stay_on_this_site() {
        assert_eq!(urlencode("/a b?x=1"), "/a%20b%3Fx%3D1");
    }
}
