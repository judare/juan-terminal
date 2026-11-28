//! Embedded assets (icons). Compiled into the binary: no files to ship.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

static ICONS: &[(&str, &[u8])] = &[
    ("icons/action-running.svg", include_bytes!("../assets/icons/action-running.svg")),
    ("icons/action-queued.svg", include_bytes!("../assets/icons/action-queued.svg")),
    ("icons/action-stop.svg", include_bytes!("../assets/icons/action-stop.svg")),
    ("icons/action-rerun.svg", include_bytes!("../assets/icons/action-rerun.svg")),
    ("icons/action-workflow.svg", include_bytes!("../assets/icons/action-workflow.svg")),
    ("icons/tag.svg", include_bytes!("../assets/icons/tag.svg")),
    ("icons/calendar.svg", include_bytes!("../assets/icons/calendar.svg")),
    ("icons/mail.svg", include_bytes!("../assets/icons/mail.svg")),
    ("icons/clipboard.svg", include_bytes!("../assets/icons/clipboard.svg")),
    ("icons/video-camera.svg", include_bytes!("../assets/icons/video-camera.svg")),
    ("icons/fragua-mark-a.svg", include_bytes!("../assets/icons/fragua-mark-a.svg")),
    ("icons/fragua-mark-b.svg", include_bytes!("../assets/icons/fragua-mark-b.svg")),
    ("icons/whatsapp.svg", include_bytes!("../assets/icons/whatsapp.svg")),
    ("icons/tail-in.svg", include_bytes!("../assets/icons/tail-in.svg")),
    ("icons/tail-out.svg", include_bytes!("../assets/icons/tail-out.svg")),
    ("icons/more.svg", include_bytes!("../assets/icons/more.svg")),
    ("icons/link.svg", include_bytes!("../assets/icons/link.svg")),
    ("icons/paperclip.svg", include_bytes!("../assets/icons/paperclip.svg")),
    ("icons/send.svg", include_bytes!("../assets/icons/send.svg")),
    ("icons/mic.svg", include_bytes!("../assets/icons/mic.svg")),
    ("icons/smile.svg", include_bytes!("../assets/icons/smile.svg")),
    ("icons/play.svg", include_bytes!("../assets/icons/play.svg")),
    ("icons/music.svg", include_bytes!("../assets/icons/music.svg")),
    ("icons/podcast.svg", include_bytes!("../assets/icons/podcast.svg")),
    ("icons/video.svg", include_bytes!("../assets/icons/video.svg")),
    ("icons/skip-next.svg", include_bytes!("../assets/icons/skip-next.svg")),
    ("icons/skip-prev.svg", include_bytes!("../assets/icons/skip-prev.svg")),
    ("icons/back-15.svg", include_bytes!("../assets/icons/back-15.svg")),
    ("icons/fwd-15.svg", include_bytes!("../assets/icons/fwd-15.svg")),
    ("icons/expand.svg", include_bytes!("../assets/icons/expand.svg")),
    ("icons/sparkle.svg", include_bytes!("../assets/icons/sparkle.svg")),
    ("icons/pause.svg", include_bytes!("../assets/icons/pause.svg")),
    ("icons/doc-file.svg", include_bytes!("../assets/icons/doc-file.svg")),
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
    ("icons/inspect.svg", include_bytes!("../assets/icons/inspect.svg")),
    ("icons/history.svg", include_bytes!("../assets/icons/history.svg")),
    ("icons/chart.svg", include_bytes!("../assets/icons/chart.svg")),
    ("icons/server.svg", include_bytes!("../assets/icons/server.svg")),
    ("icons/ports.svg", include_bytes!("../assets/icons/ports.svg")),
    ("icons/minus.svg", include_bytes!("../assets/icons/minus.svg")),
    ("icons/undo.svg", include_bytes!("../assets/icons/undo.svg")),
    ("icons/check.svg", include_bytes!("../assets/icons/check.svg")),
    ("icons/lint.svg", include_bytes!("../assets/icons/lint.svg")),
    ("icons/format.svg", include_bytes!("../assets/icons/format.svg")),
    ("icons/external.svg", include_bytes!("../assets/icons/external.svg")),
    ("icons/chevron-down.svg", include_bytes!("../assets/icons/chevron-down.svg")),
    ("icons/chevron-up.svg", include_bytes!("../assets/icons/chevron-up.svg")),
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
    ("icons/bookmark.svg", include_bytes!("../assets/icons/bookmark.svg")),
    ("icons/bookmark-fill.svg", include_bytes!("../assets/icons/bookmark-fill.svg")),
    ("icons/thumb-up.svg", include_bytes!("../assets/icons/thumb-up.svg")),
    ("icons/thumb-up-fill.svg", include_bytes!("../assets/icons/thumb-up-fill.svg")),
    ("icons/thumb-down.svg", include_bytes!("../assets/icons/thumb-down.svg")),
    ("icons/thumb-down-fill.svg", include_bytes!("../assets/icons/thumb-down-fill.svg")),
    ("icons/push.svg", include_bytes!("../assets/icons/push.svg")),
    ("icons/react.svg", include_bytes!("../assets/icons/react.svg")),
    ("icons/repo.svg", include_bytes!("../assets/icons/repo.svg")),
    ("icons/rust.svg", include_bytes!("../assets/icons/rust.svg")),
    ("icons/search.svg", include_bytes!("../assets/icons/search.svg")),
    ("icons/terminal.svg", include_bytes!("../assets/icons/terminal.svg")),
    ("icons/trash.svg", include_bytes!("../assets/icons/trash.svg")),
    ("icons/archive.svg", include_bytes!("../assets/icons/archive.svg")),
    ("icons/offline.svg", include_bytes!("../assets/icons/offline.svg")),
    ("icons/wa-reply.svg", include_bytes!("../assets/icons/wa-reply.svg")),
    ("icons/wa-forward.svg", include_bytes!("../assets/icons/wa-forward.svg")),
    ("icons/wa-alert.svg", include_bytes!("../assets/icons/wa-alert.svg")),
    ("icons/wa-audio.svg", include_bytes!("../assets/icons/wa-audio.svg")),
    ("icons/wa-check.svg", include_bytes!("../assets/icons/wa-check.svg")),
    ("icons/wa-check2.svg", include_bytes!("../assets/icons/wa-check2.svg")),
    ("icons/wa-clock.svg", include_bytes!("../assets/icons/wa-clock.svg")),
    ("icons/wa-contact.svg", include_bytes!("../assets/icons/wa-contact.svg")),
    ("icons/wa-location.svg", include_bytes!("../assets/icons/wa-location.svg")),
    ("icons/wa-muted.svg", include_bytes!("../assets/icons/wa-muted.svg")),
    ("icons/wa-photo.svg", include_bytes!("../assets/icons/wa-photo.svg")),
    ("icons/wa-pin.svg", include_bytes!("../assets/icons/wa-pin.svg")),
    ("icons/wa-poll.svg", include_bytes!("../assets/icons/wa-poll.svg")),
    ("icons/wa-revoked.svg", include_bytes!("../assets/icons/wa-revoked.svg")),
    ("icons/wa-spark.svg", include_bytes!("../assets/icons/wa-spark.svg")),
    ("icons/wa-star.svg", include_bytes!("../assets/icons/wa-star.svg")),
    ("icons/wa-sticker.svg", include_bytes!("../assets/icons/wa-sticker.svg")),
    ("icons/wa-video.svg", include_bytes!("../assets/icons/wa-video.svg")),
    ("icons/ts.svg", include_bytes!("../assets/icons/ts.svg")),
    ("icons/vue.svg", include_bytes!("../assets/icons/vue.svg")),
    ("icons/word.svg", include_bytes!("../assets/icons/word.svg")),
    ("icons/pptx.svg", include_bytes!("../assets/icons/pptx.svg")),
    ("icons/xlsx.svg", include_bytes!("../assets/icons/xlsx.svg")),
    ("icons/pdf.svg", include_bytes!("../assets/icons/pdf.svg")),
    ("icons/table.svg", include_bytes!("../assets/icons/table.svg")),
    ("icons/font.svg", include_bytes!("../assets/icons/font.svg")),
    ("icons/database.svg", include_bytes!("../assets/icons/database.svg")),
    ("icons/plugin.svg", include_bytes!("../assets/icons/plugin.svg")),
];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS.iter().chain(crate::folder_icons::FILES).find(|(p, _)| *p == path).map(|(_, b)| Cow::Borrowed(*b)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS.iter().chain(crate::folder_icons::FILES).filter(|(p, _)| p.starts_with(path)).map(|(p, _)| SharedString::from(*p)).collect())
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
        "ts" | "mts" | "cts" => ("icons/ts.svg", 0x3178c6),
        "js" | "mjs" | "cjs" => ("icons/js.svg", 0xe8c547),
        "tsx" | "jsx" => ("icons/react.svg", 0x61dafb),
        "vue" => ("icons/vue.svg", 0x42b883),
        "svelte" => ("icons/code.svg", 0xff3e00),
        "rs" => ("icons/rust.svg", 0xde7a4a),
        "json" | "jsonc" | "json5" => ("icons/braces.svg", 0xcbcb41),
        "md" | "mdx" | "markdown" => ("icons/markdown.svg", 0x7aa2f7),
        "txt" | "log" | "rtf" => ("icons/doc.svg", 0x8a8f98),
        "pdf" => ("icons/pdf.svg", 0xe5534b),
        "doc" | "docx" | "odt" | "pages" => ("icons/word.svg", 0x2b7cd3),
        "xls" | "xlsx" | "ods" | "numbers" => ("icons/xlsx.svg", 0x21a366),
        "ppt" | "pptx" | "odp" => ("icons/pptx.svg", 0xe86a3a),
        "csv" | "tsv" => ("icons/table.svg", 0x21a366),
        "toml" | "yaml" | "yml" | "ini" | "conf" | "config" | "editorconfig" | "prettierrc" | "eslintrc" | "npmrc" | "nvmrc" => ("icons/gear.svg", 0x9aa3b0),
        "sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" => ("icons/terminal.svg", 0x98c379),
        "css" | "scss" | "sass" | "less" | "postcss" => ("icons/css.svg", 0x56b6c2),
        "html" | "htm" | "xml" => ("icons/html.svg", 0xe07a5f),
        "svg" => ("icons/image.svg", 0xffb13b),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "ico" | "icns" | "bmp" | "tiff" | "tif" | "heic" | "avif" | "psd" | "fig" | "sketch" => ("icons/image.svg", 0xc678dd),
        "mp4" | "mov" | "mkv" | "webm" | "avi" | "m4v" => ("icons/video.svg", 0xe06c9f),
        "mp3" | "wav" | "flac" | "m4a" | "aac" | "ogg" | "opus" => ("icons/music.svg", 0xd19a66),
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "dmg" | "pkg" | "jar" | "whl" => ("icons/archive.svg", 0xb5a46a),
        "ttf" | "otf" | "woff" | "woff2" | "eot" => ("icons/font.svg", 0xe5786d),
        "sql" | "db" | "sqlite" | "sqlite3" | "prisma" => ("icons/database.svg", 0xe5a93d),
        "pem" | "key" | "crt" | "pub" | "p12" | "cer" => ("icons/key.svg", 0xe5c07b),
        "lock" => ("icons/lock.svg", 0x8a8f98),
        "py" | "pyi" | "ipynb" => ("icons/code.svg", 0x4b8bbe),
        "go" => ("icons/code.svg", 0x00add8),
        "rb" => ("icons/code.svg", 0xcc342d),
        "php" => ("icons/code.svg", 0x8892bf),
        "java" | "kt" | "kts" | "scala" => ("icons/code.svg", 0xe76f00),
        "swift" => ("icons/code.svg", 0xf05138),
        "c" | "h" | "cpp" | "cc" | "hpp" | "cs" | "zig" | "lua" | "dart" | "ex" | "exs" | "graphql" | "gql" => ("icons/code.svg", 0xa9b1d6),
        _ => ("icons/file.svg", 0x8a8f98),
    }
}

/// Generic folder tint.
pub const FOLDER_COLOR: u32 = 0x8fa3bf;

/// Folder icon: a solid folder, plus a badge in its corner for well-known names (src, tests,
/// jobs…). `faint` replaces both tints (dimmed rows); `virtual_others` is the "others" group.
pub fn folder_icon_el(name: &str, expanded: bool, size: f32, faint: Option<u32>, virtual_others: bool) -> gpui::AnyElement {
    use gpui::{IntoElement, ParentElement, Styled, div, px};
    let kind = if virtual_others { Some(crate::folder_icons::OTHERS) } else { crate::folder_icons::kind(&name.to_ascii_lowercase()) };
    let Some(k) = kind else {
        let path = if expanded { "icons/folder-open.svg" } else { "icons/folder.svg" };
        return gpui::svg().path(path).size(px(size)).flex_none().text_color(crate::theme::rgb(faint.unwrap_or(FOLDER_COLOR))).into_any_element();
    };
    div()
        .relative()
        .flex_none()
        .size(px(size))
        .child(gpui::svg().path(if expanded { k.open } else { k.closed }).size(px(size)).text_color(crate::theme::rgb(faint.unwrap_or(k.color))))
        .child(gpui::svg().path(k.badge).absolute().top_0().left_0().size(px(size)).text_color(crate::theme::rgb(faint.unwrap_or(k.badge_color))))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_used_by_mapping_exists() {
        for name in ["a.ts", "a.tsx", "a.vue", "a.rs", "a.json", "a.md", "x.toml", "x.sh", "x.css", "x.html", "x.png", ".env.local", "package.json", "Cargo.lock", ".gitignore", "x.pem", "x.py", "noext", "a.pdf", "a.docx", "a.xlsx", "a.pptx", "a.csv", "a.mp4", "a.mp3", "a.zip", "a.woff2", "a.sql", "a.svg", "a.js"] {
            let (path, _) = file_icon(name);
            assert!(Assets.load(path).unwrap().is_some(), "{path} missing");
        }
        for name in ["tests", "jobs", "src", "components", "assets", "styles", "docs", "config", "scripts", "api", "db", "dist", "hooks", "utils", "locales", "types", "store", ".github", "pages", "jobs", "vitest", "bridge", "migrations", ".claude"] {
            for open in [false, true] {
                let k = crate::folder_icons::kind(name).expect(name);
                for path in [if open { k.open } else { k.closed }, k.badge] {
                    assert!(Assets.load(path).unwrap().is_some(), "{path} missing");
                }
            }
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

/// The Fragua logo mark: two interlocking corners in the logo's two reds. `a`/`b` are the
/// (dark-theme) colors of the top-left and bottom-right pieces.
pub fn fragua_mark(size: f32, a: u32, b: u32) -> gpui::Div {
    use gpui::{div, px, ParentElement, Styled};
    let piece = |path: &'static str, color: u32| gpui::svg().path(path).absolute().top_0().left_0().size(px(size)).text_color(crate::theme::rgb(color));
    div().relative().flex_none().size(px(size)).child(piece("icons/fragua-mark-a.svg", a)).child(piece("icons/fragua-mark-b.svg", b))
}
