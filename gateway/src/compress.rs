//! Smaller responses for a far-away browser.
//!
//! Lattice's browser host serves its frontend uncompressed (it expects a
//! browser on the same machine), and the gateway asks it for plain bodies so
//! it can rewrite them. Across an ocean that is most of a cold load: the first
//! paint alone is ~1.8 MB of JavaScript, ~0.55 MB gzipped. So the gateway
//! gzips what it sends when the browser accepts it, keeps the result for
//! Lattice's immutable (hashed) assets, and answers a matching `If-None-Match`
//! with 304 for responses that carry an ETag.

use crate::proxy::{empty, full, Body};
use flate2::write::GzEncoder;
use flate2::Compression;
use http_body_util::BodyExt;
use hyper::body::Body as _;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Response, StatusCode};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;

/// Bodies smaller than this gain too little to be worth a gzip header.
const MIN_BYTES: u64 = 1024;
/// Bodies larger than this pass through untouched (and unbuffered).
const MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Compressed immutable assets kept in memory. Lattice's whole frontend is
/// ~9 MB gzipped, so this holds it with room for an update's second copy.
const CACHE_BYTES: usize = 48 * 1024 * 1024;

/// What the response step needs to know about the request.
pub struct Asked {
    pub key: String,
    pub method: Method,
    pub gzip: bool,
    pub if_none_match: Option<HeaderValue>,
}

impl Asked {
    pub fn from(key: String, method: &Method, headers: &HeaderMap) -> Self {
        Asked {
            key,
            method: method.clone(),
            gzip: accepts_gzip(headers),
            if_none_match: headers.get(header::IF_NONE_MATCH).cloned(),
        }
    }
}

#[derive(Default)]
pub struct Encoder {
    cache: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, bytes::Bytes>,
    bytes: usize,
}

impl Encoder {
    pub async fn finish(&self, asked: Asked, response: Response<Body>) -> Response<Body> {
        if response.status() != StatusCode::OK
            || !(asked.method == Method::GET || asked.method == Method::HEAD)
        {
            return response;
        }
        if let (Some(tag), Some(wanted)) = (
            response.headers().get(header::ETAG),
            asked.if_none_match.as_ref(),
        ) {
            if etag_matches(wanted, tag) {
                return not_modified(response.headers());
            }
        }
        let compressible = compressible(response.headers());
        let (mut parts, body) = response.into_parts();
        if compressible {
            add_vary(&mut parts.headers);
        }
        let size = body.size_hint().exact();
        if !asked.gzip
            || !compressible
            || asked.method != Method::GET
            || parts.headers.contains_key(header::CONTENT_ENCODING)
            || !size.is_some_and(|n| (MIN_BYTES..=MAX_BYTES).contains(&n))
        {
            return Response::from_parts(parts, body);
        }
        let immutable = parts
            .headers
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("immutable"));
        let cached = immutable
            .then(|| self.cache.lock().ok()?.entries.get(&asked.key).cloned())
            .flatten();
        let gzipped = match cached {
            Some(gzipped) => gzipped,
            None => {
                let Ok(collected) = body.collect().await else {
                    return Response::from_parts(parts, empty());
                };
                let plain = collected.to_bytes();
                let Ok(Ok(gzipped)) = tokio::task::spawn_blocking(move || gzip(&plain)).await
                else {
                    return Response::from_parts(parts, empty());
                };
                let gzipped = bytes::Bytes::from(gzipped);
                if immutable {
                    self.remember(&asked.key, &gzipped);
                }
                gzipped
            }
        };
        let headers = &mut parts.headers;
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(gzipped.len()));
        headers.remove(header::ACCEPT_RANGES);
        weaken_etag(headers);
        Response::from_parts(parts, full(gzipped))
    }

    fn remember(&self, key: &str, gzipped: &bytes::Bytes) {
        let Ok(mut cache) = self.cache.lock() else {
            return;
        };
        if cache.bytes + gzipped.len() > CACHE_BYTES {
            cache.entries.clear();
            cache.bytes = 0;
        }
        cache.bytes += gzipped.len();
        if let Some(old) = cache.entries.insert(key.to_string(), gzipped.clone()) {
            cache.bytes -= old.len();
        }
    }
}

fn gzip(plain: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::with_capacity(plain.len() / 3), Compression::new(6));
    encoder.write_all(plain)?;
    encoder.finish()
}

/// `gzip` listed in Accept-Encoding without `q=0`.
fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|item| {
            let mut fields = item.split(';').map(str::trim);
            let name = fields.next().unwrap_or("");
            let refused = fields.any(|f| {
                f.strip_prefix("q=")
                    .and_then(|q| q.parse::<f32>().ok())
                    .is_some_and(|q| q == 0.0)
            });
            (name.eq_ignore_ascii_case("gzip") || name == "*") && !refused
        })
}

/// Text, scripts, JSON, SVG, manifests and fonts that are not compressed
/// already. Event streams are left alone: they are read as they arrive.
fn compressible(headers: &HeaderMap) -> bool {
    let Some(kind) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let kind = kind
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if kind == "text/event-stream" {
        return false;
    }
    kind.starts_with("text/")
        || kind.ends_with("+json")
        || kind.ends_with("+xml")
        || matches!(
            kind.as_str(),
            "application/javascript"
                | "application/x-javascript"
                | "application/json"
                | "application/xml"
                | "application/wasm"
                | "application/x-font-ttf"
                | "font/ttf"
                | "font/otf"
                | "image/x-icon"
                | "image/vnd.microsoft.icon"
        )
}

fn add_vary(headers: &mut HeaderMap) {
    let has = headers
        .get_all(header::VARY)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| v.trim().eq_ignore_ascii_case("accept-encoding") || v.trim() == "*");
    if !has {
        headers.append(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
}

/// A gzipped body is a different byte sequence: its ETag can only be weak.
fn weaken_etag(headers: &mut HeaderMap) {
    let Some(tag) = headers.get(header::ETAG).and_then(|v| v.to_str().ok()) else {
        return;
    };
    if tag.starts_with("W/") {
        return;
    }
    if let Ok(weak) = HeaderValue::from_str(&format!("W/{tag}")) {
        headers.insert(header::ETAG, weak);
    }
}

/// Weak comparison (RFC 9110 §8.8.3.2), as If-None-Match uses.
fn etag_matches(wanted: &HeaderValue, tag: &HeaderValue) -> bool {
    let (Ok(wanted), Ok(tag)) = (wanted.to_str(), tag.to_str()) else {
        return false;
    };
    let opaque = |t: &str| t.trim().trim_start_matches("W/").to_string();
    let tag = opaque(tag);
    wanted.trim() == "*" || wanted.split(',').any(|w| opaque(w) == tag)
}

fn not_modified(headers: &HeaderMap) -> Response<Body> {
    let mut response = Response::new(empty());
    *response.status_mut() = StatusCode::NOT_MODIFIED;
    for name in [
        header::ETAG,
        header::CACHE_CONTROL,
        header::VARY,
        header::EXPIRES,
    ] {
        for value in headers.get_all(&name) {
            response.headers_mut().append(name.clone(), value.clone());
        }
    }
    if compressible(headers) {
        add_vary(response.headers_mut());
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::Read;

    fn asked(gzip: bool) -> Asked {
        let mut headers = HeaderMap::new();
        if gzip {
            headers.insert(
                header::ACCEPT_ENCODING,
                HeaderValue::from_static("gzip, deflate, br"),
            );
        }
        Asked::from("/assets/app.js".into(), &Method::GET, &headers)
    }

    fn script(body: &str, cache: &'static str) -> Response<Body> {
        let mut response = Response::new(full(body.to_string()));
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/javascript"),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
        response
    }

    async fn bytes(response: Response<Body>) -> Vec<u8> {
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec()
    }

    #[tokio::test]
    async fn gzips_a_script_for_a_browser_that_accepts_it() {
        let source = "export const answer = 42;\n".repeat(200);
        let encoder = Encoder::default();
        let response = encoder
            .finish(
                asked(true),
                script(&source, "public, max-age=31536000, immutable"),
            )
            .await;
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
        assert_eq!(response.headers()[header::VARY], "Accept-Encoding");
        let length: usize = response.headers()[header::CONTENT_LENGTH]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let body = bytes(response).await;
        assert_eq!(body.len(), length);
        assert!(body.len() < source.len() / 4);
        let mut plain = String::new();
        GzDecoder::new(&body[..])
            .read_to_string(&mut plain)
            .unwrap();
        assert_eq!(plain, source);
        // The second request is answered from the cache, with the same bytes.
        let again = encoder
            .finish(
                asked(true),
                script(&source, "public, max-age=31536000, immutable"),
            )
            .await;
        assert_eq!(bytes(again).await, body);
    }

    #[tokio::test]
    async fn leaves_bodies_alone_when_gzip_is_not_accepted_or_not_worth_it() {
        let source = "x".repeat(4096);
        let encoder = Encoder::default();
        let plain = encoder
            .finish(asked(false), script(&source, "no-cache"))
            .await;
        assert!(plain.headers().get(header::CONTENT_ENCODING).is_none());
        assert_eq!(plain.headers()[header::VARY], "Accept-Encoding");
        assert_eq!(bytes(plain).await.len(), 4096);

        let small = encoder
            .finish(asked(true), script("let a = 1;", "no-cache"))
            .await;
        assert!(small.headers().get(header::CONTENT_ENCODING).is_none());

        let mut pdf = Response::new(full(vec![b'%'; 8192]));
        pdf.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/pdf"),
        );
        let pdf = encoder.finish(asked(true), pdf).await;
        assert!(pdf.headers().get(header::CONTENT_ENCODING).is_none());
        assert!(pdf.headers().get(header::VARY).is_none());
    }

    #[tokio::test]
    async fn answers_a_matching_etag_with_304() {
        let encoder = Encoder::default();
        let mut response = script(&"y".repeat(4096), "no-cache");
        response
            .headers_mut()
            .insert(header::ETAG, HeaderValue::from_static("\"abc\""));
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("W/\"abc\""));
        let asked = Asked::from("/x".into(), &Method::GET, &headers);
        let response = encoder.finish(asked, response).await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[header::ETAG], "\"abc\"");
        assert!(bytes(response).await.is_empty());

        let mut tagged = script(&"y".repeat(4096), "no-cache");
        tagged
            .headers_mut()
            .insert(header::ETAG, HeaderValue::from_static("\"abc\""));
        let gzipped = encoder.finish(self::asked(true), tagged).await;
        assert_eq!(gzipped.headers()[header::ETAG], "W/\"abc\"");
    }

    #[test]
    fn reads_accept_encoding() {
        let check = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static(value));
            accepts_gzip(&headers)
        };
        assert!(check("gzip, deflate, br"));
        assert!(check("br;q=1.0, gzip;q=0.8"));
        assert!(check("*"));
        assert!(!check("gzip;q=0, br"));
        assert!(!check("br, deflate"));
        assert!(!check("identity"));
    }
}
