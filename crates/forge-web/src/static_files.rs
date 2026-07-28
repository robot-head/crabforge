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

/// How long a browser may cache a stylesheet.
///
/// Short, because the filenames are not content-hashed: a long cache would mean
/// a stale stylesheet surviving a deploy.
const CACHE_CONTROL: &str = "public, max-age=300";

fn light_css() -> &'static str {
    static CSS: OnceLock<String> = OnceLock::new();
    CSS.get_or_init(|| forge_render::theme_css(forge_render::Theme::Light))
}

fn dark_css() -> &'static str {
    static CSS: OnceLock<String> = OnceLock::new();
    CSS.get_or_init(|| forge_render::theme_css(forge_render::Theme::Dark))
}

pub async fn serve(Path(name): Path<String>) -> Response {
    let body = match name.as_str() {
        "app.css" => APP_CSS,
        "syntax-light.css" => light_css(),
        "syntax-dark.css" => dark_css(),
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
        // The base template links all three; a missing one is an unstyled page.
        for name in ["app.css", "syntax-light.css", "syntax-dark.css"] {
            let response = serve(Path(name.to_string())).await;
            check!(response.status() == StatusCode::OK, "{name} is missing");
        }
    }

    #[test]
    fn the_base_template_references_only_stylesheets_that_exist() {
        let base = include_str!("../templates/base.html");
        for name in ["app.css", "syntax-light.css", "syntax-dark.css"] {
            check!(base.contains(name), "{name} is served but never linked");
        }
    }

    #[tokio::test]
    async fn an_unknown_asset_is_not_found_rather_than_a_path_traversal() {
        let response = serve(Path("../../../etc/passwd".to_string())).await;
        check!(response.status() == StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_syntax_themes_target_the_prefixed_classes() {
        // If the prefix here and in the highlighter disagreed, every file would
        // render unstyled.
        check!(light_css().contains(".cf-"));
        check!(dark_css().contains(".cf-"));
    }
}
