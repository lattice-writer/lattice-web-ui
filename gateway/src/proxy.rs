//! Forward an authenticated request to Lattice's browser host on loopback.
//!
//! The browser host expects to be reached at its own loopback origin, so the
//! request's Host, Origin and Referer are rewritten to it, and redirects that
//! name it are rewritten back to the public origin. WebSocket upgrades are
//! tunneled byte for byte, or with permessage-deflate on the browser's leg
//! when the browser offers it (see `wsdeflate.rs`), and end when the device
//! that opened them is revoked.

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty};
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::net::SocketAddr;
use tokio::net::TcpStream;
use tokio::sync::broadcast;

pub type Body = BoxBody<Bytes, hyper::Error>;

pub fn empty() -> Body {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

pub fn full(bytes: impl Into<Bytes>) -> Body {
    http_body_util::Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed()
}

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Cookies the gateway owns. They never reach Lattice.
pub const GATEWAY_COOKIES: &[&str] = &[
    "__Host-lattice_device",
    "__Host-lattice_owner",
    "lattice_device",
    "lattice_owner",
];

/// Largest script the gateway will hold in memory to rewrite (Lattice's biggest is about 1.3 MB).
const MAX_REWRITE_BYTES: usize = 32 * 1024 * 1024;

/// How the gateway adapts Lattice's page in transit.
#[derive(Default)]
pub struct Bridge {
    /// Load the gateway's page script into Lattice's workspace page.
    pub inject: bool,
    /// Rewrite the page's loopback-only expressions (see `bridge.rs`).
    pub rewrite: bool,
    pub counters: crate::bridge::Counters,
}

pub struct Upstream {
    pub addr: SocketAddr,
    pub origin: String,
    client: Client<HttpConnector, Incoming>,
}

impl Upstream {
    pub fn new(addr: SocketAddr) -> Self {
        let mut connector = HttpConnector::new();
        connector.set_nodelay(true);
        let client = Client::builder(TokioExecutor::new()).build(connector);
        Self {
            addr,
            origin: format!("http://{addr}"),
            client,
        }
    }

    pub async fn reachable(&self) -> bool {
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            TcpStream::connect(self.addr),
        )
        .await
        .is_ok_and(|r| r.is_ok())
    }
}

fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.get(header::UPGRADE).is_some()
        && headers
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| {
                v.split(',')
                    .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
            })
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let listed: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| {
            v.split(',')
                .map(|t| t.trim().to_ascii_lowercase())
                .collect::<Vec<_>>()
        })
        .filter_map(|name| HeaderName::from_bytes(name.as_bytes()).ok())
        .collect();
    for name in listed {
        headers.remove(name);
    }
    for name in HOP_BY_HOP {
        headers.remove(*name);
    }
}

/// Keep every cookie except the gateway's own.
fn filter_cookies(headers: &mut HeaderMap) {
    let kept: Vec<String> = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| {
            v.split(';')
                .map(str::trim)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|pair| {
            let name = pair.split('=').next().unwrap_or("").trim();
            !pair.is_empty() && !GATEWAY_COOKIES.contains(&name)
        })
        .collect();
    headers.remove(header::COOKIE);
    if !kept.is_empty() {
        if let Ok(value) = HeaderValue::from_str(&kept.join("; ")) {
            headers.insert(header::COOKIE, value);
        }
    }
}

fn rewrite_prefix(headers: &mut HeaderMap, name: HeaderName, from: &str, to: &str) {
    if let Some(value) = headers.get(&name).and_then(|v| v.to_str().ok()) {
        if let Some(rest) = value.strip_prefix(from) {
            if rest.is_empty() || rest.starts_with('/') || rest.starts_with('?') {
                if let Ok(next) = HeaderValue::from_str(&format!("{to}{rest}")) {
                    headers.insert(name, next);
                }
            }
        }
    }
}

fn prepare_request_headers(headers: &mut HeaderMap, upstream: &Upstream, public_origin: &str) {
    filter_cookies(headers);
    headers.insert(
        header::HOST,
        HeaderValue::from_str(&upstream.addr.to_string()).expect("socket address is a valid host"),
    );
    rewrite_prefix(headers, header::ORIGIN, public_origin, &upstream.origin);
    rewrite_prefix(headers, header::REFERER, public_origin, &upstream.origin);
    for name in [
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
        "x-real-ip",
    ] {
        headers.remove(name);
    }
    // Identity headers from Tailscale Serve or an OAuth proxy are for the gateway only.
    let identity: Vec<HeaderName> = headers
        .keys()
        .filter(|name| {
            name.as_str().starts_with("tailscale-") || name.as_str().starts_with("x-auth-request-")
        })
        .cloned()
        .collect();
    for name in identity {
        headers.remove(name);
    }
}

fn prepare_response_headers(headers: &mut HeaderMap, upstream: &Upstream, public_origin: &str) {
    rewrite_prefix(headers, header::LOCATION, &upstream.origin, public_origin);
    // Lattice serves loopback pages; behind HTTPS its cookies must be Secure
    // and must not name the loopback host.
    let cookies: Vec<HeaderValue> = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(|cookie| {
            let mut parts: Vec<String> = cookie
                .split(';')
                .map(str::trim)
                .filter(|part| !part.to_ascii_lowercase().starts_with("domain="))
                .map(str::to_string)
                .collect();
            if public_origin.starts_with("https://")
                && !parts.iter().any(|p| p.eq_ignore_ascii_case("secure"))
            {
                parts.push("Secure".into());
            }
            HeaderValue::from_str(&parts.join("; "))
                .unwrap_or_else(|_| HeaderValue::from_static(""))
        })
        .filter(|v| !v.is_empty())
        .collect();
    headers.remove(header::SET_COOKIE);
    for cookie in cookies {
        headers.append(header::SET_COOKIE, cookie);
    }
}

fn bad_gateway() -> Response<Body> {
    let mut response = Response::new(full(
        r#"{"error":"Lattice is not running on this host yet. Start it, then reload this page."}"#,
    ));
    *response.status_mut() = StatusCode::BAD_GATEWAY;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// Forward `req`. `principal` names who opened it; a revocation broadcast with
/// that name closes any tunnel it holds.
pub async fn forward(
    mut req: Request<Incoming>,
    upstream: &Upstream,
    public_origin: &str,
    principal: String,
    revocations: broadcast::Receiver<String>,
    bridge: &Bridge,
    hold: Option<Box<dyn std::any::Any + Send>>,
) -> Response<Body> {
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let upgrade = is_upgrade(req.headers());
    let upgrade_headers = upgrade.then(|| {
        (
            req.headers().get(header::UPGRADE).cloned(),
            req.headers().get(header::CONNECTION).cloned(),
        )
    });
    let mut headers = req.headers().clone();
    strip_hop_by_hop(&mut headers);
    prepare_request_headers(&mut headers, upstream, public_origin);
    // The page is rewritten in transit, so ask Lattice for it uncompressed.
    headers.remove(header::ACCEPT_ENCODING);
    let uri: hyper::Uri = match format!("{}{}", upstream.origin, path).parse() {
        Ok(uri) => uri,
        Err(_) => return bad_gateway(),
    };

    if let Some((upgrade_value, connection_value)) = upgrade_headers {
        let client_upgrade = hyper::upgrade::on(&mut req);
        let mut outbound = Request::builder().method(req.method()).uri(path.as_str());
        *outbound.headers_mut().expect("fresh builder") = headers;
        let headers_mut = outbound.headers_mut().expect("fresh builder");
        if let Some(value) = upgrade_value {
            headers_mut.insert(header::UPGRADE, value);
        }
        headers_mut.insert(
            header::CONNECTION,
            connection_value.unwrap_or(HeaderValue::from_static("upgrade")),
        );
        let outbound = outbound.body(Empty::<Bytes>::new()).expect("valid request");
        return tunnel(
            outbound,
            client_upgrade,
            upstream,
            public_origin,
            principal,
            revocations,
            hold,
        )
        .await;
    }

    let (mut parts, body) = req.into_parts();
    parts.uri = uri;
    parts.headers = headers;
    match upstream
        .client
        .request(Request::from_parts(parts, body))
        .await
    {
        Ok(response) => {
            let (mut parts, body) = response.into_parts();
            strip_hop_by_hop(&mut parts.headers);
            prepare_response_headers(&mut parts.headers, upstream, public_origin);
            let kind = parts
                .headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_ascii_lowercase();
            let html = bridge.inject
                && kind.starts_with("text/html")
                && (path == "/" || path.starts_with("/?") || path.starts_with("/index.html"));
            let script = bridge.rewrite && kind.contains("javascript");
            if parts.status != StatusCode::OK || !(html || script) {
                return Response::from_parts(parts, body.boxed());
            }
            let Ok(collected) = http_body_util::Limited::new(body, MAX_REWRITE_BYTES)
                .collect()
                .await
            else {
                return bad_gateway();
            };
            let Ok(text) = String::from_utf8(collected.to_bytes().to_vec()) else {
                return bad_gateway();
            };
            let text = if html {
                crate::bridge::inject_html(&text).unwrap_or(text)
            } else {
                let (text, counts) = crate::bridge::rewrite_js(&text);
                crate::bridge::record(&bridge.counters, counts);
                text
            };
            parts.headers.remove(header::CONTENT_LENGTH);
            parts.headers.remove(header::ETAG);
            parts.headers.remove(header::CONTENT_ENCODING);
            Response::from_parts(parts, full(text))
        }
        Err(_) => bad_gateway(),
    }
}

async fn tunnel(
    mut outbound: Request<Empty<Bytes>>,
    client_upgrade: hyper::upgrade::OnUpgrade,
    upstream: &Upstream,
    public_origin: &str,
    principal: String,
    mut revocations: broadcast::Receiver<String>,
    hold: Option<Box<dyn std::any::Any + Send>>,
) -> Response<Body> {
    // The gateway compresses the browser's leg itself, so Lattice is not asked to.
    let deflate = crate::wsdeflate::negotiate(outbound.headers());
    if deflate.is_some() {
        outbound
            .headers_mut()
            .remove(header::SEC_WEBSOCKET_EXTENSIONS);
    }
    let Ok(stream) = TcpStream::connect(upstream.addr).await else {
        return bad_gateway();
    };
    let _ = stream.set_nodelay(true);
    let Ok((mut sender, connection)) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await
    else {
        return bad_gateway();
    };
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });
    let Ok(mut response) = sender.send_request(outbound).await else {
        return bad_gateway();
    };
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        let (mut parts, body) = response.into_parts();
        strip_hop_by_hop(&mut parts.headers);
        prepare_response_headers(&mut parts.headers, upstream, public_origin);
        return Response::from_parts(parts, body.boxed());
    }
    let upstream_upgrade = hyper::upgrade::on(&mut response);
    tokio::spawn(async move {
        // Whatever the caller ties to this connection lives as long as it does.
        let _hold = hold;
        let (Ok(client), Ok(server)) = tokio::join!(client_upgrade, upstream_upgrade) else {
            return;
        };
        let mut client = TokioIo::new(client);
        let mut server = TokioIo::new(server);
        let relay = async {
            match deflate {
                Some(agreed) => crate::wsdeflate::relay(&mut client, &mut server, agreed).await,
                None => {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                }
            }
        };
        let revoked = async {
            loop {
                match revocations.recv().await {
                    Ok(name) if name == principal || name == "*" => return,
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => std::future::pending::<()>().await,
                }
            }
        };
        tokio::select! {
            _ = relay => {}
            _ = revoked => {}
        }
    });
    // A 101 keeps its Upgrade and Connection headers: the client needs them.
    let (mut parts, _) = response.into_parts();
    prepare_response_headers(&mut parts.headers, upstream, public_origin);
    // Lattice was offered no extension; what the browser gets is the gateway's.
    if let Some(agreed) = deflate {
        parts
            .headers
            .insert(header::SEC_WEBSOCKET_EXTENSIONS, agreed.response());
    }
    Response::from_parts(parts, empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_gateway_cookies_only() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; __Host-lattice_device=abc; b=2"),
        );
        filter_cookies(&mut headers);
        assert_eq!(headers.get(header::COOKIE).unwrap(), "a=1; b=2");
    }

    #[test]
    fn rewrites_origin_and_location() {
        let upstream = Upstream::new("127.0.0.1:18452".parse().unwrap());
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://w.example"),
        );
        headers.insert(
            "tailscale-user-login",
            HeaderValue::from_static("me@example.com"),
        );
        prepare_request_headers(&mut headers, &upstream, "https://w.example");
        assert_eq!(
            headers.get(header::ORIGIN).unwrap(),
            "http://127.0.0.1:18452"
        );
        assert_eq!(headers.get(header::HOST).unwrap(), "127.0.0.1:18452");
        assert!(headers.get("tailscale-user-login").is_none());

        let mut response = HeaderMap::new();
        response.insert(
            header::LOCATION,
            HeaderValue::from_static("http://127.0.0.1:18452/?entry=x"),
        );
        response.insert(
            header::SET_COOKIE,
            HeaderValue::from_static("s=1; Path=/; Domain=127.0.0.1; HttpOnly"),
        );
        prepare_response_headers(&mut response, &upstream, "https://w.example");
        assert_eq!(
            response.get(header::LOCATION).unwrap(),
            "https://w.example/?entry=x"
        );
        assert_eq!(
            response.get(header::SET_COOKIE).unwrap(),
            "s=1; Path=/; HttpOnly; Secure"
        );
    }

    #[test]
    fn a_foreign_origin_is_left_alone_for_the_caller_to_reject() {
        let upstream = Upstream::new("127.0.0.1:18452".parse().unwrap());
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        prepare_request_headers(&mut headers, &upstream, "https://w.example");
        assert_eq!(headers.get(header::ORIGIN).unwrap(), "https://evil.example");
    }
}
