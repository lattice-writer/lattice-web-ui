//! Make Lattice's browser-host page work from a public origin, until Lattice
//! supports one itself (leo1oel/lattice#1190).
//!
//! The page refuses to start anywhere but `http://127.0.0.1:18452`, and it
//! builds its session, asset and bridge URLs from that address. The gateway
//! rewrites exactly those expressions in the JavaScript it forwards, and
//! counts each rewrite so that `lattice-host` can check them against the
//! counts pinned for each Lattice release. Lattice itself is not changed.

use regex::Regex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

pub struct Rule {
    pub name: &'static str,
    pattern: &'static str,
    replacement: &'static str,
}

/// The rewrites, in the order they are applied. Each pattern matches a whole
/// expression of the minified bundle, never a fragment of another one.
pub const RULES: &[Rule] = &[
    // `http://127.0.0.1:${port}` inside a template literal: session, asset and reachability URLs.
    Rule {
        name: "http",
        pattern: r"http://127\.0\.0\.1:\$\{[^}`]+\}",
        replacement: "${location.origin}",
    },
    // `ws://127.0.0.1:${port}`: the bridge WebSocket, wss: under https.
    Rule {
        name: "ws",
        pattern: r"ws://127\.0\.0\.1:\$\{[^}`]+\}",
        replacement: r#"${location.protocol==="https:"?"wss:":"ws:"}//${location.host}"#,
    },
    // The loopback-only start check and the fixed-entry check.
    Rule {
        name: "protocol",
        pattern: r"window\.location\.protocol===`http:`",
        replacement: "!0",
    },
    Rule {
        name: "hostname",
        pattern: r"window\.location\.hostname===`127\.0\.0\.1`",
        replacement: "!0",
    },
    Rule {
        name: "port",
        pattern: r"window\.location\.port===[A-Za-z_$][\w$]*",
        replacement: "!0",
    },
];

fn compiled() -> &'static Vec<Regex> {
    static COMPILED: OnceLock<Vec<Regex>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        RULES
            .iter()
            .map(|rule| Regex::new(rule.pattern).expect("rule pattern compiles"))
            .collect()
    })
}

#[derive(Default)]
pub struct Counters([AtomicU64; 5]);

impl Counters {
    pub fn snapshot(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for (rule, counter) in RULES.iter().zip(&self.0) {
            map.insert(rule.name.into(), counter.load(Ordering::Relaxed).into());
        }
        serde_json::Value::Object(map)
    }
}

/// Rewrite one JavaScript file. Returns the new text and per-rule counts.
pub fn rewrite_js(source: &str) -> (String, [u64; 5]) {
    let mut text = source.to_string();
    let mut counts = [0u64; 5];
    for (index, (rule, regex)) in RULES.iter().zip(compiled()).enumerate() {
        let found = regex.find_iter(&text).count() as u64;
        if found > 0 {
            counts[index] = found;
            text = regex
                .replace_all(&text, regex::NoExpand(rule.replacement))
                .into_owned();
        }
    }
    (text, counts)
}

pub fn record(counters: &Counters, counts: [u64; 5]) {
    for (counter, count) in counters.0.iter().zip(counts) {
        counter.fetch_add(count, Ordering::Relaxed);
    }
}

/// lattice-web-ui's name and icons, in the page as it arrives: Safari and the
/// home screen read them from the HTML, before any script runs. bridge.js
/// keeps them (it leaves links marked `data-lattice-gateway` alone).
const BRAND_HEAD: &str = concat!(
    r#"<link rel="icon" type="image/svg+xml" href="/__gateway/brand/logo.svg" data-lattice-gateway="brand">"#,
    r#"<link rel="icon" type="image/png" sizes="32x32" href="/__gateway/brand/favicon-32.png" data-lattice-gateway="brand">"#,
    r#"<link rel="apple-touch-icon" href="/__gateway/brand/apple-touch-icon.png" data-lattice-gateway="brand">"#,
    r#"<link rel="manifest" href="/__gateway/brand/manifest.webmanifest" data-lattice-gateway="brand">"#,
    r#"<meta name="apple-mobile-web-app-title" content="Lattice-web-ui" data-lattice-gateway="brand">"#,
);

/// Load the gateway's page script before Lattice's own scripts, and put
/// lattice-web-ui's name and icons in place of Lattice's.
pub fn inject_html(html: &str) -> Option<String> {
    static LATTICE_ICONS: OnceLock<Regex> = OnceLock::new();
    let icons = LATTICE_ICONS.get_or_init(|| {
        Regex::new(r#"(?i)\s*<link\b[^>]*\brel=["']?(?:shortcut icon|icon|apple-touch-icon(?:-precomposed)?|manifest)["'\s>][^>]*>"#)
            .expect("valid pattern")
    });
    let tag = r#"<script src="/__gateway/bridge.js"></script>"#;
    let at = html.find("<head>").map(|i| i + "<head>".len())?;
    let rest = icons.replace_all(&html[at..], "");
    let rest = rest.replacen("<title>Lattice</title>", "<title>Lattice-web-ui</title>", 1);
    let mut out = String::with_capacity(html.len() + tag.len() + BRAND_HEAD.len());
    out.push_str(&html[..at]);
    out.push_str(tag);
    out.push_str(BRAND_HEAD);
    out.push_str(&rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Excerpts of Lattice 0.18.0's built browser runtime and relay.
    const RUNTIME: &str = concat!(
        "let r=new URL(`http://127.0.0.1:${e}/__lattice_session`);",
        "n=window.location.hostname===`127.0.0.1`&&window.location.port===t,",
        "function $r(){return window.location.protocol===`http:`&&(window.location.hostname===`127.0.0.1`||window.location.hostname===`localhost`)}",
        "let t=new URL(`http://127.0.0.1:${Dr.bridgePort}/__lattice_asset`);",
    );
    const RELAY: &str = "let n=new URL(`ws://127.0.0.1:${e.bridgePort}/__lattice_bridge`);";

    #[test]
    fn rewrites_the_loopback_expressions_of_the_runtime() {
        let (text, counts) = rewrite_js(RUNTIME);
        assert_eq!(counts, [2, 0, 1, 2, 1]);
        assert!(text.contains("new URL(`${location.origin}/__lattice_session`)"));
        assert!(text.contains("n=!0&&!0,"));
        assert!(text.contains("return !0&&(!0||window.location.hostname===`localhost`)"));
        assert!(!text.contains("127.0.0.1"));
    }

    #[test]
    fn rewrites_the_bridge_socket_to_the_page_origin() {
        let (text, counts) = rewrite_js(RELAY);
        assert_eq!(counts, [0, 1, 0, 0, 0]);
        assert_eq!(
            text,
            r#"let n=new URL(`${location.protocol==="https:"?"wss:":"ws:"}//${location.host}/__lattice_bridge`);"#
        );
    }

    #[test]
    fn leaves_other_code_alone() {
        let other = "fetch(`https://example.org/${x}`);if(a.port===b)c();";
        assert_eq!(rewrite_js(other), (other.to_string(), [0; 5]));
    }

    #[test]
    fn injects_the_page_script_first() {
        let html = "<!doctype html><html><head><script type=\"module\" src=\"/assets/app.js\"></script></head></html>";
        let out = inject_html(html).unwrap();
        assert!(out.starts_with("<!doctype html><html><head><script src=\"/__gateway/bridge.js\"></script><link rel=\"icon\""));
        assert!(out.contains("brand\"><script type=\"module\" src=\"/assets/app.js\">"));
        assert!(inject_html("<html></html>").is_none());
    }

    #[test]
    fn puts_lattice_web_uis_name_and_icons_in_place_of_lattices() {
        // Lattice 0.19.0's page head, as its browser host serves it.
        let html = concat!(
            "<!DOCTYPE html><html lang=\"en\"><head>\n",
            "    <meta charset=\"UTF-8\">\n",
            "    <meta name=\"theme-color\" content=\"#171718\">\n",
            "    <link rel=\"icon\" type=\"image/svg+xml\" href=\"/assets/app-icon-Dp3s_p_l.svg\">\n",
            "    <title>Lattice</title>\n",
            "    <link rel=\"modulepreload\" crossorigin=\"\" href=\"/assets/ui-Ds96uiLR.js\">\n",
            "</head></html>",
        );
        let out = inject_html(html).unwrap();
        assert!(!out.contains("app-icon-Dp3s_p_l.svg"));
        assert!(out.contains("<title>Lattice-web-ui</title>"));
        assert!(out.contains(
            r#"<link rel="apple-touch-icon" href="/__gateway/brand/apple-touch-icon.png""#
        ));
        assert!(out.contains(
            r#"<link rel="modulepreload" crossorigin="" href="/assets/ui-Ds96uiLR.js">"#
        ));
        assert_eq!(out.matches("rel=\"icon\"").count(), 2);
        assert!(out.contains("<meta name=\"theme-color\""));
    }
}
