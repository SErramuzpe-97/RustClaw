//! Static assets, compiled into the binary.
//!
//! `include_str!` puts these in the executable's read-only data, so they cost
//! file size but no heap: serving the UI does not grow RSS. That is why there is
//! no `tower-http::ServeDir` and no files to install beside the binary — the
//! single-binary deploy the whole project is built around still holds.

use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

macro_rules! asset {
    ($name:literal, $mime:literal) => {
        ($name, $mime, include_str!(concat!("assets/", $name)))
    };
}

/// (path under /assets, content type, body)
const ASSETS: &[(&str, &str, &str)] = &[
    asset!("app.js", "text/javascript; charset=utf-8"),
    asset!("styles.css", "text/css; charset=utf-8"),
    asset!("vendor/preact.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/hooks.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/htm.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/marked.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/hl/core.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/hl/rust.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/hl/javascript.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/hl/typescript.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/hl/python.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/hl/bash.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/hl/json.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/hl/xml.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/hl/css.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/hl/sql.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/hl/yaml.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/hl/markdown.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/hl/ini.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/hl/diff.mjs", "text/javascript; charset=utf-8"),
    asset!("vendor/hl/theme-light.css", "text/css; charset=utf-8"),
    asset!("vendor/hl/theme-dark.css", "text/css; charset=utf-8"),
];

const INDEX: &str = include_str!("assets/index.html");

pub async fn index() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX)
}

pub async fn serve(Path(path): Path<String>) -> Response {
    match ASSETS.iter().find(|(name, _, _)| *name == path) {
        Some((_, mime, body)) => (
            [
                (header::CONTENT_TYPE, *mime),
                // The assets change only when the binary does, and the binary
                // is the version, so a long cache is safe.
                (header::CACHE_CONTROL, "public, max-age=3600"),
            ],
            *body,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no such asset").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_references_only_assets_that_exist() {
        // A typo in a path would be a blank page at runtime, not a build error.
        let mut sources = vec![INDEX.to_string()];
        sources.extend(ASSETS.iter().map(|(_, _, body)| body.to_string()));

        let mut missing = Vec::new();
        for src in &sources {
            for (idx, _) in src.match_indices("/assets/") {
                // Only real references count: a quoted path in markup or code.
                // Prose in a comment mentioning /assets/vendor is not a link.
                let opener = src[..idx].chars().next_back();
                if !matches!(opener, Some('"') | Some('\'') | Some('`') | Some('(')) {
                    continue;
                }
                let rest = &src[idx + "/assets/".len()..];
                let end = rest
                    .find(['"', '\'', '`', ')'])
                    .unwrap_or(rest.len());
                let path = &rest[..end];
                // The language modules are imported through a template literal
                // built at runtime; the next test covers those.
                if path.contains("${") || path.is_empty() {
                    continue;
                }
                if !ASSETS.iter().any(|(name, _, _)| *name == path) {
                    missing.push(path.to_string());
                }
            }
        }
        missing.sort();
        missing.dedup();
        assert!(missing.is_empty(), "referenced but not embedded: {missing:?}");
    }

    #[test]
    fn every_language_the_ui_registers_is_embedded() {
        let app = ASSETS.iter().find(|(n, _, _)| *n == "app.js").unwrap().2;
        let list = app
            .split_once("const LANGUAGES = [")
            .and_then(|(_, rest)| rest.split_once("];"))
            .expect("LANGUAGES array in app.js")
            .0;
        for lang in list.split(',') {
            let lang = lang.trim().trim_matches(['"', '\n', ' ']);
            if lang.is_empty() {
                continue;
            }
            let path = format!("vendor/hl/{lang}.mjs");
            assert!(
                ASSETS.iter().any(|(n, _, _)| *n == path),
                "app.js registers `{lang}` but {path} is not embedded"
            );
        }
    }

    #[test]
    fn the_import_map_covers_every_bare_specifier() {
        for spec in ["preact", "preact/hooks", "htm", "marked", "hljs"] {
            assert!(
                INDEX.contains(&format!("\"{spec}\"")),
                "`{spec}` is imported by app.js but missing from the import map"
            );
        }
    }
}
