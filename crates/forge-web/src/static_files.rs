//! Stylesheets, served from the binary.
//!
//! Embedded rather than read from disk so a deployment is one file and there is
//! no path to misconfigure. They are small, and the syntax themes are generated
//! once on first request rather than at build time — the highlighted HTML
//! carries only class names, so a theme is entirely a matter of which
//! stylesheet the browser loads.

use std::sync::OnceLock;

use axum::{
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

const APP_CSS: &str = include_str!("../static/app.css");

/// The syntax theme is dark because the interface is.
///
/// Nocturne — the design system the forge wears — is a dark system with no
/// light ground defined for it, so there is no honest light pairing to offer
/// and a `prefers-color-scheme: light` reader would get pale code on a dark
/// page. `forge_render` still generates both; only one is served.
const SYNTAX_DARK: &str = "syntax-dark.css";

/// How long a browser may cache a stylesheet.
///
/// Short, because the filenames are not content-hashed: a long cache would mean
/// a stale stylesheet surviving a deploy.
const CACHE_CONTROL: &str = "public, max-age=300";

fn dark_css() -> &'static str {
    static CSS: OnceLock<String> = OnceLock::new();
    CSS.get_or_init(|| forge_render::theme_css(forge_render::Theme::Dark))
}

pub async fn serve(Path(name): Path<String>) -> Response {
    let body = match name.as_str() {
        "app.css" => APP_CSS,
        SYNTAX_DARK => dark_css(),
        _ => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };

    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, CACHE_CONTROL),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[tokio::test]
    async fn every_stylesheet_the_layout_references_is_served() {
        // The base template links both; a missing one is an unstyled page.
        for name in ["app.css", SYNTAX_DARK] {
            let response = serve(Path(name.to_string())).await;
            check!(response.status() == StatusCode::OK, "{name} is missing");
        }
    }

    #[test]
    fn the_base_template_references_only_stylesheets_that_exist() {
        let base = include_str!("../templates/base.html");
        for name in ["app.css", SYNTAX_DARK] {
            check!(base.contains(name), "{name} is served but never linked");
        }
    }

    #[test]
    fn the_light_syntax_theme_is_neither_served_nor_linked() {
        // The interface has one ground. Linking a light code theme it has no
        // matching page for is how a reader on a light system ends up with
        // pale syntax on a dark page.
        let base = include_str!("../templates/base.html");
        check!(!base.contains("syntax-light.css"));
    }

    #[tokio::test]
    async fn the_light_syntax_theme_is_not_a_route() {
        let response = serve(Path("syntax-light.css".to_string())).await;
        check!(response.status() == StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_unknown_asset_is_not_found_rather_than_a_path_traversal() {
        let response = serve(Path("../../../etc/passwd".to_string())).await;
        check!(response.status() == StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_syntax_theme_targets_the_prefixed_classes() {
        // If the prefix here and in the highlighter disagreed, every file would
        // render unstyled.
        check!(dark_css().contains(".cf-"));
    }
}
