//! Run tree-sitter highlighting over a whole document, producing flat byte-range spans.
//! Called off the UI thread; results are versioned by the caller.

use std::cell::RefCell;

use tree_sitter_highlight::{HighlightEvent, Highlighter};

use crate::language::{Language, injection_config};

/// Files larger than this are shown without highlighting (keeps memory and latency bounded).
pub const MAX_HIGHLIGHT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    /// Index into [`crate::HIGHLIGHT_NAMES`].
    pub kind: u16,
}

thread_local! {
    static HIGHLIGHTER: RefCell<Highlighter> = RefCell::new(Highlighter::new());
}

/// Highlight `source`. Returns spans sorted by start, non-overlapping (innermost capture wins).
pub fn highlight(language: Language, source: &str) -> Vec<HighlightSpan> {
    if source.len() > MAX_HIGHLIGHT_BYTES {
        return Vec::new();
    }
    let Some(config) = language.config() else { return Vec::new() };
    HIGHLIGHTER.with(|h| {
        let mut h = h.borrow_mut();
        let Ok(events) = h.highlight(config, source.as_bytes(), None, None, |name| injection_config(name)) else {
            return Vec::new();
        };
        let mut spans: Vec<HighlightSpan> = Vec::new();
        let mut stack: Vec<u16> = Vec::new();
        for event in events {
            match event {
                Ok(HighlightEvent::HighlightStart(h)) => stack.push(h.0 as u16),
                Ok(HighlightEvent::HighlightEnd) => {
                    stack.pop();
                }
                Ok(HighlightEvent::Source { start, end }) => {
                    if let Some(&kind) = stack.last() {
                        match spans.last_mut() {
                            Some(last) if last.end == start && last.kind == kind => last.end = end,
                            _ => spans.push(HighlightSpan { start, end, kind }),
                        }
                    }
                }
                Err(_) => break,
            }
        }
        spans
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HIGHLIGHT_NAMES;

    fn kinds(lang: Language, src: &str) -> Vec<(String, &'static str)> {
        highlight(lang, src)
            .into_iter()
            .map(|s| (src[s.start..s.end].to_string(), HIGHLIGHT_NAMES[s.kind as usize]))
            .collect()
    }

    #[test]
    fn all_configs_compile() {
        for lang in [
            Language::JavaScript,
            Language::TypeScript,
            Language::Tsx,
            Language::Rust,
            Language::Json,
            Language::Toml,
            Language::Yaml,
            Language::Markdown,
            Language::Bash,
            Language::Css,
            Language::Html,
            Language::Vue,
        ] {
            assert!(lang.config().is_some(), "{lang:?} failed to build");
        }
        assert!(injection_config("markdown_inline").is_some());
    }

    #[test]
    fn typescript_keywords_and_strings() {
        let k = kinds(Language::TypeScript, "const x: number = 1;\nfunction f() { return \"hi\" }");
        assert!(k.contains(&("const".into(), "keyword")), "{k:?}");
        assert!(k.contains(&("\"hi\"".into(), "string")), "{k:?}");
        assert!(k.iter().any(|(t, n)| t == "number" && n.starts_with("type")), "{k:?}");
    }

    #[test]
    fn tsx_and_rust() {
        let k = kinds(Language::Tsx, "const a = <div className=\"x\">hi</div>;");
        assert!(k.iter().any(|(t, n)| t == "div" && *n == "tag"), "{k:?}");
        let k = kinds(Language::Rust, "fn main() { let s = \"x\"; }");
        assert!(k.contains(&("fn".into(), "keyword")), "{k:?}");
    }

    #[test]
    fn vue_injects_script() {
        let src = "<template><div>{{ msg }}</div></template>\n<script setup lang=\"ts\">\nconst msg = 'hi'\n</script>\n";
        let k = kinds(Language::Vue, src);
        assert!(k.iter().any(|(t, n)| t == "const" && *n == "keyword"), "{k:?}");
        assert!(k.iter().any(|(t, n)| t.contains("div") && *n == "tag"), "{k:?}");
    }

    #[test]
    fn spans_are_sorted_and_disjoint() {
        let src = "# Title\n\nSome `code` and **bold**.\n\n```js\nconst a = 1\n```\n";
        let spans = highlight(Language::Markdown, src);
        assert!(!spans.is_empty());
        for w in spans.windows(2) {
            assert!(w[0].end <= w[1].start, "{w:?}");
        }
    }

    #[test]
    fn plain_text_and_huge_files_skip() {
        assert!(highlight(Language::PlainText, "anything").is_empty());
        let big = "a".repeat(MAX_HIGHLIGHT_BYTES + 1);
        assert!(highlight(Language::Rust, &big).is_empty());
    }
}
