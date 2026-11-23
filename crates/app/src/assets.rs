//! Embedded assets (icons). Compiled into the binary: no files to ship.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

static ICONS: &[(&str, &[u8])] = &[
    ("icons/braces.svg", include_bytes!("../assets/icons/braces.svg")),
    ("icons/ring-1.svg", include_bytes!("../assets/icons/ring-1.svg")),
    ("icons/ring-2.svg", include_bytes!("../assets/icons/ring-2.svg")),
    ("icons/ring-3.svg", include_bytes!("../assets/icons/ring-3.svg")),
    ("icons/ring-4.svg", include_bytes!("../assets/icons/ring-4.svg")),
    ("icons/ring-5.svg", include_bytes!("../assets/icons/ring-5.svg")),
    ("icons/ring-6.svg", include_bytes!("../assets/icons/ring-6.svg")),
    ("icons/ring-7.svg", include_bytes!("../assets/icons/ring-7.svg")),
    ("icons/ring-8.svg", include_bytes!("../assets/icons/ring-8.svg")),
    ("icons/ring-9.svg", include_bytes!("../assets/icons/ring-9.svg")),
    ("icons/ring-10.svg", include_bytes!("../assets/icons/ring-10.svg")),
    ("icons/ring-11.svg", include_bytes!("../assets/icons/ring-11.svg")),
    ("icons/ring-12.svg", include_bytes!("../assets/icons/ring-12.svg")),
    ("icons/ring-track.svg", include_bytes!("../assets/icons/ring-track.svg")),
    ("icons/history.svg", include_bytes!("../assets/icons/history.svg")),
    ("icons/server.svg", include_bytes!("../assets/icons/server.svg")),
    ("icons/ports.svg", include_bytes!("../assets/icons/ports.svg")),
    ("icons/minus.svg", include_bytes!("../assets/icons/minus.svg")),
    ("icons/undo.svg", include_bytes!("../assets/icons/undo.svg")),
    ("icons/check.svg", include_bytes!("../assets/icons/check.svg")),
    ("icons/lint.svg", include_bytes!("../assets/icons/lint.svg")),
    ("icons/format.svg", include_bytes!("../assets/icons/format.svg")),
    ("icons/external.svg", include_bytes!("../assets/icons/external.svg")),
    ("icons/chevron-down.svg", include_bytes!("../assets/icons/chevron-down.svg")),
    ("icons/chevron-right.svg", include_bytes!("../assets/icons/chevron-right.svg")),
    ("icons/claude.svg", include_bytes!("../assets/icons/claude.svg")),
    ("icons/claude-spark.svg", include_bytes!("../assets/icons/claude-spark.svg")),
    ("icons/close.svg", include_bytes!("../assets/icons/close.svg")),
    ("icons/code.svg", include_bytes!("../assets/icons/code.svg")),
    ("icons/copy.svg", include_bytes!("../assets/icons/copy.svg")),
    ("icons/css.svg", include_bytes!("../assets/icons/css.svg")),
    ("icons/doc.svg", include_bytes!("../assets/icons/doc.svg")),
    ("icons/edit.svg", include_bytes!("../assets/icons/edit.svg")),
    ("icons/fetch.svg", include_bytes!("../assets/icons/fetch.svg")),
    ("icons/file.svg", include_bytes!("../assets/icons/file.svg")),
    ("icons/files.svg", include_bytes!("../assets/icons/files.svg")),
    ("icons/finder.svg", include_bytes!("../assets/icons/finder.svg")),
    ("icons/folder-others.svg", include_bytes!("../assets/icons/folder-others.svg")),
    ("icons/folder-open.svg", include_bytes!("../assets/icons/folder-open.svg")),
    ("icons/folder.svg", include_bytes!("../assets/icons/folder.svg")),
    ("icons/gear.svg", include_bytes!("../assets/icons/gear.svg")),
    ("icons/git.svg", include_bytes!("../assets/icons/git.svg")),
    ("icons/html.svg", include_bytes!("../assets/icons/html.svg")),
    ("icons/image.svg", include_bytes!("../assets/icons/image.svg")),
    ("icons/js.svg", include_bytes!("../assets/icons/js.svg")),
    ("icons/key.svg", include_bytes!("../assets/icons/key.svg")),
    ("icons/lock.svg", include_bytes!("../assets/icons/lock.svg")),
    ("icons/project-empty.svg", include_bytes!("../assets/icons/project-empty.svg")),
    ("icons/markdown.svg", include_bytes!("../assets/icons/markdown.svg")),
    ("icons/new-file.svg", include_bytes!("../assets/icons/new-file.svg")),
    ("icons/new-folder.svg", include_bytes!("../assets/icons/new-folder.svg")),
    ("icons/package.svg", include_bytes!("../assets/icons/package.svg")),
    ("icons/plus.svg", include_bytes!("../assets/icons/plus.svg")),
    ("icons/pull.svg", include_bytes!("../assets/icons/pull.svg")),
    ("icons/push.svg", include_bytes!("../assets/icons/push.svg")),
    ("icons/react.svg", include_bytes!("../assets/icons/react.svg")),
    ("icons/repo.svg", include_bytes!("../assets/icons/repo.svg")),
    ("icons/rust.svg", include_bytes!("../assets/icons/rust.svg")),
    ("icons/search.svg", include_bytes!("../assets/icons/search.svg")),
    ("icons/terminal.svg", include_bytes!("../assets/icons/terminal.svg")),
    ("icons/trash.svg", include_bytes!("../assets/icons/trash.svg")),
    ("icons/ts.svg", include_bytes!("../assets/icons/ts.svg")),
    ("icons/vue.svg", include_bytes!("../assets/icons/vue.svg")),
];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS.iter().find(|(p, _)| *p == path).map(|(_, b)| Cow::Borrowed(*b)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS.iter().filter(|(p, _)| p.starts_with(path)).map(|(p, _)| SharedString::from(*p)).collect())
    }
}

/// Icon path and tint color for a file name.
pub fn file_icon(name: &str) -> (&'static str, u32) {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "package.json" => return ("icons/package.svg", 0xe06c75),
        "cargo.toml" => return ("icons/rust.svg", 0xde7a4a),
        "cargo.lock" | "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock" | "bun.lockb" => return ("icons/lock.svg", 0x8a8f98),
        ".gitignore" | ".gitattributes" | ".gitmodules" => return ("icons/git.svg", 0xe0714f),
        "dockerfile" | "makefile" | "justfile" => return ("icons/terminal.svg", 0x8a9bb0),
        _ => {}
    }
    if lower.starts_with(".env") {
        return ("icons/key.svg", 0xe5c07b);
    }
    if lower.ends_with(".d.ts") {
        return ("icons/ts.svg", 0x3f7fc6);
    }
    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "ts" | "mts" | "cts" => ("icons/ts.svg", 0x4f9ee8),
        "js" | "mjs" | "cjs" => ("icons/js.svg", 0xe8c547),
        "tsx" | "jsx" => ("icons/react.svg", 0x61dafb),
        "vue" => ("icons/vue.svg", 0x42b883),
        "rs" => ("icons/rust.svg", 0xde7a4a),
        "json" | "jsonc" | "json5" => ("icons/braces.svg", 0xcbcb41),
        "md" | "mdx" | "markdown" => ("icons/markdown.svg", 0x7aa2f7),
        "txt" | "log" => ("icons/doc.svg", 0x8a8f98),
        "toml" | "yaml" | "yml" | "ini" | "conf" | "config" | "editorconfig" | "prettierrc" | "eslintrc" | "npmrc" | "nvmrc" => ("icons/gear.svg", 0x9aa3b0),
        "sh" | "bash" | "zsh" | "fish" => ("icons/terminal.svg", 0x98c379),
        "css" | "scss" | "sass" | "less" | "postcss" => ("icons/css.svg", 0x56b6c2),
        "html" | "htm" | "xml" | "svg" => ("icons/html.svg", 0xe07a5f),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "ico" | "bmp" => ("icons/image.svg", 0xc678dd),
        "pem" | "key" | "crt" | "pub" => ("icons/key.svg", 0xe5c07b),
        "lock" => ("icons/lock.svg", 0x8a8f98),
        "py" | "go" | "rb" | "php" | "java" | "kt" | "swift" | "c" | "h" | "cpp" | "sql" | "graphql" | "prisma" => ("icons/code.svg", 0xa9b1d6),
        _ => ("icons/file.svg", 0x8a8f98),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_used_by_mapping_exists() {
        for name in ["a.ts", "a.tsx", "a.vue", "a.rs", "a.json", "a.md", "x.toml", "x.sh", "x.css", "x.html", "x.png", ".env.local", "package.json", "Cargo.lock", ".gitignore", "x.pem", "x.py", "noext"] {
            let (path, _) = file_icon(name);
            assert!(Assets.load(path).unwrap().is_some(), "{path} missing");
        }
        for path in ["icons/folder.svg", "icons/folder-open.svg", "icons/chevron-right.svg", "icons/chevron-down.svg", "icons/repo.svg", "icons/plus.svg", "icons/close.svg", "icons/search.svg"] {
            assert!(Assets.load(path).unwrap().is_some(), "{path} missing");
        }
    }
}

/// Claude mark as in the brand icon: cream spark on a rounded terracotta tile.
pub fn claude_logo(size: f32) -> gpui::Div {
    use gpui::{Styled, div, px, ParentElement};
    div()
        .flex_none()
        .size(px(size))
        .rounded(px((size * 0.26).round()))
        .bg(crate::theme::rgb(0xd97757))
        .flex()
        .items_center()
        .justify_center()
        .child(gpui::svg().path("icons/claude-spark.svg").size(px(size * 0.78)).text_color(crate::theme::rgb(0xfbf1ec)))
}
