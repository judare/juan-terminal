//! Language detection and tree-sitter highlight configurations (created lazily, once).

use std::path::Path;
use std::sync::OnceLock;

use tree_sitter_highlight::HighlightConfiguration;

/// Capture names we style. tree-sitter-highlight matches the longest configured prefix,
/// so `function.method` falls back to `function`, etc. Index into this = highlight kind.
pub const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "boolean",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "embedded",
    "escape",
    "function",
    "function.builtin",
    "function.method",
    "keyword",
    "label",
    "module",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "string",
    "string.special",
    "tag",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
    "text.title",
    "text.literal",
    "text.uri",
    "text.reference",
    "text.emphasis",
    "text.strong",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Language {
    JavaScript,
    TypeScript,
    Tsx,
    Rust,
    Json,
    Toml,
    Yaml,
    Markdown,
    Bash,
    Css,
    Html,
    Vue,
    /// Highlighted with the HTML grammar (tags, attributes, strings, comments).
    Xml,
    /// Hand-written lexer (no grammar shipped): keywords, strings, numbers, comments.
    Sql,
    /// .env / .conf / .ini-like files: Bash grammar (KEY=value, # comments, strings).
    Config,
    PlainText,
}

impl Language {
    pub fn from_path(path: &Path) -> Self {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        match name {
            ".zshrc" | ".bashrc" | ".zprofile" | ".bash_profile" | ".profile" | ".envrc" => {
                return Language::Bash;
            }
            "Cargo.lock" | "rust-toolchain" | "Pipfile" => return Language::Toml,
            ".env" | ".flaskenv" => return Language::Config,
            _ => {}
        }
        // .env.local, .env.production…
        if name.starts_with(".env.") {
            return Language::Config;
        }
        // .cargo/config (no extension) is TOML.
        if name == "config" && path.parent().and_then(|p| p.file_name()).is_some_and(|d| d == ".cargo") {
            return Language::Toml;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
        match ext.as_str() {
            "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "rs" => Language::Rust,
            "json" | "jsonc" | "json5" => Language::Json,
            "toml" => Language::Toml,
            "yml" | "yaml" => Language::Yaml,
            "md" | "markdown" | "mdx" => Language::Markdown,
            "sh" | "bash" | "zsh" => Language::Bash,
            "css" | "scss" | "postcss" => Language::Css,
            "html" | "htm" => Language::Html,
            "vue" => Language::Vue,
            "xml" | "xsd" | "xsl" | "xslt" | "svg" | "plist" | "xaml" | "csproj" | "pom" | "rss" | "atom" => Language::Xml,
            "sql" | "psql" | "pgsql" | "mysql" => Language::Sql,
            "env" | "conf" | "cfg" | "ini" | "properties" | "service" | "desktop" => Language::Config,
            _ => Language::PlainText,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Language::JavaScript => "JavaScript",
            Language::TypeScript => "TypeScript",
            Language::Tsx => "TSX",
            Language::Rust => "Rust",
            Language::Json => "JSON",
            Language::Toml => "TOML",
            Language::Yaml => "YAML",
            Language::Markdown => "Markdown",
            Language::Bash => "Shell",
            Language::Css => "CSS",
            Language::Html => "HTML",
            Language::Vue => "Vue",
            Language::Xml => "XML",
            Language::Sql => "SQL",
            Language::Config => "Config",
            Language::PlainText => "Plain Text",
        }
    }

    /// Line comment prefix, used by toggle-comment.
    pub fn line_comment(self) -> Option<&'static str> {
        match self {
            Language::JavaScript | Language::TypeScript | Language::Tsx | Language::Rust | Language::Vue => Some("// "),
            Language::Toml | Language::Yaml | Language::Bash | Language::Config => Some("# "),
            Language::Sql => Some("-- "),
            _ => None,
        }
    }

    pub(crate) fn config(self) -> Option<&'static HighlightConfiguration> {
        match self {
            Language::JavaScript => configs::javascript(),
            Language::TypeScript => configs::typescript(),
            Language::Tsx => configs::tsx(),
            Language::Rust => configs::rust(),
            Language::Json => configs::json(),
            Language::Toml => configs::toml(),
            Language::Yaml => configs::yaml(),
            Language::Markdown => configs::markdown(),
            Language::Bash => configs::bash(),
            Language::Css => configs::css(),
            Language::Html => configs::html(),
            Language::Vue => configs::vue(),
            Language::Xml => configs::html(),
            Language::Config => configs::bash(),
            Language::Sql | Language::PlainText => None,
        }
    }
}

/// Resolve an injected language name (e.g. inside Vue `<script lang="ts">`, Markdown fences).
pub(crate) fn injection_config(name: &str) -> Option<&'static HighlightConfiguration> {
    match name.to_ascii_lowercase().as_str() {
        "javascript" | "js" | "jsx" => configs::javascript(),
        "typescript" | "ts" => configs::typescript(),
        "tsx" => configs::tsx(),
        "rust" | "rs" => configs::rust(),
        "json" => configs::json(),
        "toml" => configs::toml(),
        "yaml" | "yml" => configs::yaml(),
        "bash" | "sh" | "shell" | "zsh" => configs::bash(),
        "css" | "scss" => configs::css(),
        "html" => configs::html(),
        "markdown" | "md" => configs::markdown(),
        "markdown_inline" => configs::markdown_inline(),
        "vue" => configs::vue(),
        _ => None,
    }
}

mod configs {
    use super::*;

    fn build(
        language: tree_sitter_language::LanguageFn,
        name: &str,
        highlights: &str,
        injections: &str,
        locals: &str,
    ) -> Option<HighlightConfiguration> {
        match HighlightConfiguration::new(language.into(), name, highlights, injections, locals) {
            Ok(mut config) => {
                config.configure(HIGHLIGHT_NAMES);
                Some(config)
            }
            Err(err) => {
                eprintln!("juan-terminal: highlight query error for {name}: {err}");
                None
            }
        }
    }

    macro_rules! lazy_config {
        ($fn:ident, $build:expr) => {
            pub fn $fn() -> Option<&'static HighlightConfiguration> {
                static CELL: OnceLock<Option<HighlightConfiguration>> = OnceLock::new();
                CELL.get_or_init(|| $build).as_ref()
            }
        };
    }

    lazy_config!(
        javascript,
        build(
            tree_sitter_javascript::LANGUAGE,
            "javascript",
            &format!("{}\n{}", tree_sitter_javascript::HIGHLIGHT_QUERY, tree_sitter_javascript::JSX_HIGHLIGHT_QUERY),
            tree_sitter_javascript::INJECTIONS_QUERY,
            tree_sitter_javascript::LOCALS_QUERY,
        )
    );
    lazy_config!(
        typescript,
        build(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
            "typescript",
            &format!("{}\n{}", tree_sitter_typescript::HIGHLIGHTS_QUERY, tree_sitter_javascript::HIGHLIGHT_QUERY),
            tree_sitter_javascript::INJECTIONS_QUERY,
            &format!("{}\n{}", tree_sitter_typescript::LOCALS_QUERY, tree_sitter_javascript::LOCALS_QUERY),
        )
    );
    lazy_config!(
        tsx,
        build(
            tree_sitter_typescript::LANGUAGE_TSX,
            "tsx",
            &format!(
                "{}\n{}\n{}",
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
            ),
            tree_sitter_javascript::INJECTIONS_QUERY,
            &format!("{}\n{}", tree_sitter_typescript::LOCALS_QUERY, tree_sitter_javascript::LOCALS_QUERY),
        )
    );
    lazy_config!(
        rust,
        build(tree_sitter_rust::LANGUAGE, "rust", tree_sitter_rust::HIGHLIGHTS_QUERY, tree_sitter_rust::INJECTIONS_QUERY, "")
    );
    lazy_config!(json, build(tree_sitter_json::LANGUAGE, "json", tree_sitter_json::HIGHLIGHTS_QUERY, "", ""));
    lazy_config!(toml, build(tree_sitter_toml_ng::LANGUAGE, "toml", tree_sitter_toml_ng::HIGHLIGHTS_QUERY, "", ""));
    lazy_config!(yaml, build(tree_sitter_yaml::LANGUAGE, "yaml", tree_sitter_yaml::HIGHLIGHTS_QUERY, "", ""));
    lazy_config!(
        markdown,
        build(
            tree_sitter_md::LANGUAGE,
            "markdown",
            tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
            tree_sitter_md::INJECTION_QUERY_BLOCK,
            ""
        )
    );
    lazy_config!(
        markdown_inline,
        build(
            tree_sitter_md::INLINE_LANGUAGE,
            "markdown_inline",
            tree_sitter_md::HIGHLIGHT_QUERY_INLINE,
            tree_sitter_md::INJECTION_QUERY_INLINE,
            ""
        )
    );
    lazy_config!(bash, build(tree_sitter_bash::LANGUAGE, "bash", tree_sitter_bash::HIGHLIGHT_QUERY, "", ""));
    lazy_config!(css, build(tree_sitter_css::LANGUAGE, "css", tree_sitter_css::HIGHLIGHTS_QUERY, "", ""));
    lazy_config!(
        html,
        build(tree_sitter_html::LANGUAGE, "html", tree_sitter_html::HIGHLIGHTS_QUERY, tree_sitter_html::INJECTIONS_QUERY, "")
    );
    lazy_config!(
        vue,
        build(
            tree_sitter_vue_next::LANGUAGE,
            "vue",
            include_str!("../queries/vue/highlights.scm"),
            include_str!("../queries/vue/injections.scm"),
            ""
        )
    );
}
