//! Markdown rendering.
//!
//! Everything rendered here is written by users — READMEs, issue bodies,
//! comments — so the security posture is the whole point. Raw HTML is escaped
//! rather than passed through, and dangerous URL schemes are neutralised.

use comrak::{Options, markdown_to_html};

/// Prefix on generated heading anchors.
///
/// Without one, a heading called "Login" would produce `id="login"` and could
/// collide with the forge's own element ids on the same page. GitHub uses the
/// same prefix for the same reason.
const ANCHOR_PREFIX: &str = "user-content-";

/// Build the markdown options.
///
/// Constructed once and shared: parsing the option set is trivial but the
/// struct is large, and it never varies per request.
pub fn options() -> Options<'static> {
    let mut o = Options::default();

    // GitHub-flavoured markdown, which is what people write.
    o.extension.table = true;
    o.extension.strikethrough = true;
    o.extension.tasklist = true;
    o.extension.autolink = true;
    o.extension.footnotes = true;
    o.extension.description_lists = true;
    o.extension.alerts = true;
    o.extension.header_id_prefix = Some(ANCHOR_PREFIX.to_string());

    // The security posture, and the two settings never to flip.
    //
    // `unsafe = false` blanks dangerous URL schemes (`javascript:`) and refuses
    // to emit raw HTML. `escape = true` makes that raw HTML visible as text
    // rather than replaced with a comment, so a user who writes `<b>` sees
    // `<b>` instead of their content silently vanishing.
    o.render.r#unsafe = false;
    o.render.escape = true;
    // Filters a denylist of tags as defence in depth, in case the above is ever
    // loosened.
    o.extension.tagfilter = true;

    // `github_pre_lang` puts the language on the `<pre>` where a highlighter
    // can find it; `full_info_string` keeps the rest of the fence info.
    o.render.github_pre_lang = true;
    o.render.full_info_string = true;
    o.render.tasklist_classes = true;
    o.render.gfm_quirks = true;
    o.render.ignore_empty_links = true;
    // A single newline is a line break in a comment box, but not in a README.
    // GitHub applies it to comments only; we take the conservative default and
    // let the caller override.
    o.render.hardbreaks = false;

    o
}

/// Render markdown to HTML.
///
/// The result is trusted output — the escaping happened here — so templates
/// must inject it verbatim rather than escaping it a second time.
pub fn render(source: &str, options: &Options<'_>) -> String {
    markdown_to_html(source, options)
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    fn html(source: &str) -> String {
        render(source, &options())
    }

    #[test]
    fn ordinary_markdown_renders() {
        let out = html("# Title\n\nSome **bold** text.\n");
        check!(out.contains("<h1"));
        check!(out.contains("<strong>bold</strong>"));
    }

    #[test]
    fn raw_html_is_escaped_rather_than_executed() {
        // The single most important property in this module.
        let out = html("<script>alert(1)</script>");
        check!(!out.contains("<script>"), "script tag survived: {out}");
        check!(out.contains("&lt;script&gt;"), "got: {out}");
    }

    #[test]
    fn an_img_onerror_payload_does_not_survive() {
        let out = html(r#"<img src=x onerror="alert(1)">"#);
        check!(!out.contains("onerror=\""), "got: {out}");
    }

    #[test]
    fn javascript_urls_are_neutralised() {
        let out = html("[click me](javascript:alert(1))");
        check!(!out.contains("javascript:"), "got: {out}");
        // The link text survives; only the destination is removed.
        check!(out.contains("click me"));
    }

    #[test]
    fn ordinary_links_are_left_alone() {
        let out = html("[docs](https://example.com/page)");
        check!(
            out.contains(r#"href="https://example.com/page""#),
            "got: {out}"
        );
    }

    #[test]
    fn github_extensions_are_available() {
        check!(html("~~gone~~").contains("<del>"));
        check!(html("| a | b |\n|---|---|\n| 1 | 2 |\n").contains("<table>"));
        check!(html("- [x] done\n").contains("checkbox"));
        check!(html("Visit https://example.com today").contains("<a href=\"https://example.com\""));
    }

    #[test]
    fn heading_anchors_are_namespaced() {
        // So a heading cannot collide with the forge's own element ids.
        let out = html("## Hello World\n");
        check!(
            out.contains(r#"id="user-content-hello-world""#),
            "got: {out}"
        );
    }

    #[test]
    fn code_fences_carry_their_language_for_highlighting() {
        let out = html("```rust\nfn main() {}\n```\n");
        check!(
            out.contains("rust"),
            "language should reach the markup: {out}"
        );
        check!(out.contains("fn main()"));
    }

    #[test]
    fn code_inside_a_fence_is_still_escaped() {
        let out = html("```html\n<script>alert(1)</script>\n```\n");
        check!(!out.contains("<script>alert"), "got: {out}");
        check!(out.contains("&lt;script&gt;"));
    }

    #[test]
    fn empty_input_renders_to_nothing_rather_than_failing() {
        check!(html("").trim().is_empty());
    }
}
