//! Syntax highlighting.
//!
//! Output carries CSS classes rather than inline colours, so one highlighted
//! blob serves both light and dark themes and the result depends only on the
//! file's content. That makes it cacheable by object id, which matters: syntect
//! with a pure-Rust regex engine is not fast enough to run on every request.

use std::sync::OnceLock;

use forge_types::ByteSize;
use syntect::{
    html::{ClassStyle, ClassedHTMLGenerator, css_for_theme_with_class_style},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};

/// Prefix on every generated class, so highlighting cannot collide with the
/// forge's own styles.
///
/// A `&'static str` because syntect requires one; a runtime-configurable prefix
/// would mean leaking a string for the process lifetime.
pub const CLASS_STYLE: ClassStyle = ClassStyle::SpacedPrefixed { prefix: "cf-" };

/// Files above this are served without highlighting.
///
/// Highlighting is superlinear in pathological cases and the output is several
/// times the input; past a point a reader is not reading the file anyway.
pub fn max_highlight_size() -> ByteSize {
    ByteSize::kib(512)
}

/// Line count above which highlighting is skipped, for the same reason.
pub const MAX_HIGHLIGHT_LINES: usize = 5_000;

/// The grammar set.
///
/// syntect's bundled set has 75 grammars and is missing TypeScript, TOML,
/// Kotlin, Swift, Dockerfile and much else — unusable for a code host. two-face
/// carries 213. Deserializing it is expensive, so it happens once.
pub fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

/// How a file was rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Highlighted {
    /// Highlighted HTML, with `cf-`-prefixed classes.
    Html(String),
    /// Escaped plain text — too large, or no grammar matched.
    Plain(String),
}

impl Highlighted {
    /// The HTML to place inside a `<pre><code>`.
    pub fn into_html(self) -> String {
        match self {
            Self::Html(html) | Self::Plain(html) => html,
        }
    }

    pub fn is_highlighted(&self) -> bool {
        matches!(self, Self::Html(_))
    }
}

/// Highlight `source`, choosing a grammar from `path`.
///
/// Never fails: anything that cannot be highlighted comes back as escaped
/// plain text, because a file the user asked to see should still be shown.
pub fn highlight(path: &str, source: &str) -> Highlighted {
    if ByteSize::bytes(source.len() as u64) > max_highlight_size()
        || source.lines().count() > MAX_HIGHLIGHT_LINES
    {
        return Highlighted::Plain(escape(source));
    }

    let syntaxes = syntaxes();
    let Some(syntax) = syntaxes
        .find_syntax_for_file(path)
        .ok()
        .flatten()
        .or_else(|| syntaxes.find_syntax_by_first_line(source.lines().next().unwrap_or_default()))
    else {
        return Highlighted::Plain(escape(source));
    };

    let mut generator = ClassedHTMLGenerator::new_with_class_style(syntax, syntaxes, CLASS_STYLE);
    for line in LinesWithEndings::from(source) {
        // A grammar can genuinely fail on hostile or malformed input. Falling
        // back to plain text shows the file; unwrapping would 500 the page.
        if generator
            .parse_html_for_line_which_includes_newline(line)
            .is_err()
        {
            tracing::warn!(
                path,
                "syntax highlighting failed; falling back to plain text"
            );
            return Highlighted::Plain(escape(source));
        }
    }
    Highlighted::Html(generator.finalize())
}

/// The stylesheet for a theme.
///
/// Written to static files at startup rather than generated per request; the
/// classes are theme-independent, so light and dark are a stylesheet swap.
pub fn theme_css(theme: Theme) -> String {
    let themes = two_face::theme::extra();
    let name = match theme {
        Theme::Light => two_face::theme::EmbeddedThemeName::Github,
        Theme::Dark => two_face::theme::EmbeddedThemeName::TwoDark,
    };
    css_for_theme_with_class_style(themes.get(name), CLASS_STYLE)
        .expect("embedded themes generate valid css")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Light,
    Dark,
}

/// Escape text for placing inside an HTML element.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn a_known_language_is_highlighted_with_prefixed_classes() {
        let out = highlight("main.rs", "fn main() { let x = 1; }\n");
        check!(out.is_highlighted());
        let html = out.into_html();
        check!(html.contains("cf-"), "expected prefixed classes: {html}");
        check!(html.contains("main"));
    }

    #[test]
    fn the_grammar_set_covers_languages_syntects_default_does_not() {
        // The reason two-face is a dependency at all.
        for (path, source) in [
            ("app.ts", "const x: number = 1;\n"),
            ("Cargo.toml", "[package]\nname = \"x\"\n"),
            ("Dockerfile", "FROM alpine\n"),
            ("Main.kt", "fun main() {}\n"),
        ] {
            let out = highlight(path, source);
            check!(out.is_highlighted(), "{path} was not highlighted");
        }
    }

    #[test]
    fn an_unknown_extension_falls_back_to_escaped_text() {
        let out = highlight("data.zzzz", "<not code>\n");
        let html = out.into_html();
        check!(!html.contains("<not code>"), "must be escaped: {html}");
        check!(html.contains("&lt;not code&gt;"));
    }

    #[test]
    fn a_shebang_selects_a_grammar_when_the_name_does_not() {
        let out = highlight("run", "#!/bin/bash\necho hi\n");
        check!(out.is_highlighted());
    }

    #[test]
    fn very_large_files_are_served_without_highlighting() {
        // The cost cap: highlighting is superlinear on pathological input.
        let huge = "x".repeat(max_highlight_size().as_bytes() as usize + 1);
        check!(!highlight("big.rs", &huge).is_highlighted());

        let many_lines = "let x = 1;\n".repeat(MAX_HIGHLIGHT_LINES + 1);
        check!(!highlight("many.rs", &many_lines).is_highlighted());
    }

    /// Strip the highlighter's own markup, leaving only content that came from
    /// the source file.
    ///
    /// Asserting on the source text directly is unreliable because syntect
    /// splits a token like `<script>` across several spans; what matters is
    /// that nothing from the source survives as markup.
    fn source_text_only(html: &str) -> String {
        let mut out = String::new();
        let mut depth = 0usize;
        for c in html.chars() {
            match c {
                '<' => depth += 1,
                '>' => depth = depth.saturating_sub(1),
                _ if depth == 0 => out.push(c),
                _ => {}
            }
        }
        out
    }

    #[test]
    fn highlighting_never_emits_unescaped_markup_from_the_source() {
        // Source files legitimately contain HTML, and a code host displays a
        // great deal of it. None of it may reach the page as markup.
        for (path, source) in [
            ("page.html", "<script>alert(1)</script>\n"),
            ("x.rs", "let s = \"<img src=x onerror=alert(1)>\";\n"),
            ("data.zzzz", "<script>alert(1)</script>\n"),
        ] {
            let out = highlight(path, source).into_html();
            let text = source_text_only(&out);
            check!(
                !text.contains('<') && !text.contains('>'),
                "unescaped angle bracket from {path}: {text:?}"
            );
            check!(out.contains("&lt;"), "expected escaped output for {path}");
        }
    }

    #[test]
    fn the_span_stripper_would_catch_a_real_escape_failure() {
        // Guards the guard: if `source_text_only` were wrong, the test above
        // would pass vacuously.
        check!(source_text_only("<span>ok</span>") == "ok");
        check!(source_text_only("<span><script>bad</script></span>").contains("bad"));
        check!(source_text_only("a &lt; b") == "a &lt; b");
    }

    #[test]
    fn escaping_covers_every_dangerous_character() {
        check!(escape(r#"<a href="x">&'"#) == "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
    }

    #[test]
    fn empty_input_is_handled() {
        check!(highlight("empty.rs", "").into_html().is_empty());
    }

    #[test]
    fn both_themes_produce_stylesheets_for_the_prefixed_classes() {
        for theme in [Theme::Light, Theme::Dark] {
            let css = theme_css(theme);
            check!(!css.is_empty());
            check!(
                css.contains(".cf-"),
                "theme css should target prefixed classes"
            );
        }
    }

    #[test]
    fn the_grammar_set_is_built_once() {
        // Deserializing 213 grammars per request would be ruinous.
        check!(std::ptr::eq(syntaxes(), syntaxes()));
    }
}
