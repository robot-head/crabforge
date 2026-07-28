//! Server-side rendering.
//!
//! Markdown, source code and diffs are turned into HTML here, on the server.
//! Two reasons: the pages that matter most for a forge — a syntax-highlighted
//! file, a forty-file diff — are documents rather than applications, and the
//! escaping decisions are security-critical enough to belong in one place that
//! can be tested rather than spread across templates.
//!
//! Everything in this crate returns *trusted* HTML: it has already been
//! escaped, so templates must inject it verbatim.

pub mod diff;
pub mod highlight;
pub mod markdown;

pub use diff::{DiffLine, FileDiff, Hunk, LineKind, Run, diff_text};
pub use highlight::{Highlighted, Theme, escape, highlight, syntaxes, theme_css};
pub use markdown::{options as markdown_options, render as render_markdown};
