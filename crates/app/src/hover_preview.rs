//! Hover previews in terminals (like midudev's Quick Peek / Image Preview for Claude Code, but
//! native): a path in the output shows an image, the first lines of a file (highlighted), a
//! folder's entries, or a Quick Look thumbnail; `[Image #N]` shows the image pasted in the
//! Claude session.

use std::path::{Path, PathBuf};

use base64::Engine;
use gpui::{HighlightStyle, SharedString, StyledText, div, prelude::*, px};

use crate::theme;

#[derive(Clone, Debug)]
pub enum Preview {
    /// `size`: natural pixel size, to fit the image in the space available.
    Image { path: PathBuf, title: String, caption: String, size: Option<(f32, f32)> },
    Code { title: String, caption: String, lines: Vec<(usize, String, Vec<(std::ops::Range<usize>, u32)>)>, lit: Option<usize> },
    Folder { title: String, entries: Vec<(String, bool)>, more: usize },
    File { title: String, caption: String, thumb: Option<PathBuf> },
}

const CODE_LINES: usize = 20;

fn human_size(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.1} KB", n as f64 / 1024.),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.),
    }
}

fn cache_dir() -> PathBuf {
    let d = workspace_state::state_dir().join("cache/previews");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Quick Look thumbnail (PDF page, video frame, document…), cached per file+mtime.
fn quicklook_thumb(path: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(path).ok()?;
    let stamp = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let key: String = format!("{}-{stamp}", path.display()).bytes().fold(0u64, |h, b| h.wrapping_mul(1_099_511_628_211) ^ b as u64).to_string();
    let out_dir = cache_dir().join(key);
    let name = format!("{}.png", path.file_name()?.to_string_lossy());
    let png = out_dir.join(&name);
    if png.is_file() {
        return Some(png);
    }
    std::fs::create_dir_all(&out_dir).ok()?;
    let mut child = std::process::Command::new("/usr/bin/qlmanage")
        .args(["-t", "-s", "480", "-o"])
        .arg(&out_dir)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // qlmanage can hang (no thumbnailer, sandboxed contexts): 4 s budget.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    png.is_file().then_some(png)
}

fn is_text(bytes: &[u8]) -> bool {
    // NUL never appears in text (and is valid UTF-8, so check it first).
    !bytes.iter().take(8192).any(|b| *b == 0) && std::str::from_utf8(bytes).is_ok()
}

/// Build the preview for a path (blocking: run it in the background).
pub fn for_path(path: &Path, line: Option<usize>) -> Option<Preview> {
    let meta = std::fs::metadata(path).ok()?;
    let title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    if meta.is_dir() {
        let mut entries: Vec<(String, bool)> = std::fs::read_dir(path)
            .ok()?
            .flatten()
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.file_type().map(|t| t.is_dir()).unwrap_or(false)))
            .collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.to_lowercase().cmp(&b.0.to_lowercase())));
        let more = entries.len().saturating_sub(14);
        entries.truncate(14);
        return Some(Preview::Folder { title, entries, more });
    }
    let size = meta.len();
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "svg") {
        let natural = imagesize::size(path).ok().map(|s| (s.width as f32, s.height as f32)).filter(|(w, h)| *w > 0. && *h > 0.);
        return Some(Preview::Image { path: path.to_path_buf(), title, caption: format!("{} · {}", ext.to_uppercase(), human_size(size)), size: natural });
    }
    if size <= 1_048_576 {
        if let Ok(bytes) = std::fs::read(path) {
            if is_text(&bytes) {
                let text = String::from_utf8_lossy(&bytes);
                let all: Vec<&str> = text.lines().collect();
                let total = all.len();
                // Window: around `line` (1-based) when given, else the top.
                let start = match line {
                    Some(l) if l > 0 => l.saturating_sub(CODE_LINES / 2 + 1).min(total.saturating_sub(CODE_LINES)),
                    _ => 0,
                };
                let end = (start + CODE_LINES).min(total);
                let snippet = all[start..end].join("\n");
                let lang = editor::Language::from_path(path);
                let spans = editor::highlight(lang, &snippet);
                let mut lines = Vec::new();
                let mut offset = 0usize;
                for (i, l) in all[start..end].iter().enumerate() {
                    let (a, b) = (offset, offset + l.len());
                    let mut hl = Vec::new();
                    for s in spans.iter().filter(|s| s.end > a && s.start < b) {
                        let color = theme::syntax_color(editor::HIGHLIGHT_NAMES[s.kind as usize]);
                        hl.push((s.start.max(a) - a..s.end.min(b) - a, color));
                    }
                    let mut shown = l.replace('\t', "    ");
                    if shown.len() > 160 {
                        let mut cut = 160;
                        while !shown.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        shown.truncate(cut);
                    }
                    // Tabs widen the line: keep highlights only when it didn't change.
                    let hl = if shown.len() == l.len() { hl } else { Vec::new() };
                    lines.push((start + i + 1, shown, hl));
                    offset = b + 1;
                }
                return Some(Preview::Code { title, caption: format!("{} · {} lines · {}", lang.name(), total, human_size(size)), lines, lit: line });
            }
        }
    }
    let kind = if ext.is_empty() { "File".to_string() } else { ext.to_uppercase() };
    Some(Preview::File { title, caption: format!("{kind} · {}", human_size(size)), thumb: quicklook_thumb(path) })
}

/// The `[Image #N]` around column `col` of `line`, if any.
pub fn image_ref_at(line: &str, col: usize) -> Option<usize> {
    let chars: Vec<char> = line.chars().collect();
    let s: String = chars.iter().collect();
    for (byte_i, _) in s.match_indices("[Image #") {
        let start_col = s[..byte_i].chars().count();
        let rest = &s[byte_i + 8..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() || !rest[digits.len()..].starts_with(']') {
            continue;
        }
        let end_col = start_col + 8 + digits.len() + 1;
        if col >= start_col && col < end_col {
            return digits.parse().ok();
        }
    }
    None
}

/// Extract image #`n` from a Claude session transcript into the preview cache.
pub fn session_image(session_id: &str, cwd: Option<&Path>, n: usize) -> Option<PathBuf> {
    let projects = crate::claude_sessions::projects_dir();
    let mut candidates: Vec<PathBuf> = cwd.map(|c| projects.join(crate::claude_sessions::encode_dir(c)).join(format!("{session_id}.jsonl"))).into_iter().collect();
    if candidates.iter().all(|p| !p.is_file()) {
        candidates = std::fs::read_dir(&projects).ok()?.flatten().map(|d| d.path().join(format!("{session_id}.jsonl"))).filter(|p| p.is_file()).collect();
    }
    let file = candidates.into_iter().find(|p| p.is_file())?;
    let text = std::fs::read_to_string(file).ok()?;
    // Later messages win (an id can be re-used after /clear).
    for line in text.lines().rev() {
        if !line.contains("\"imagePasteIds\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let ids: Vec<usize> = v["imagePasteIds"].as_array()?.iter().filter_map(|x| x.as_u64().map(|x| x as usize)).collect();
        let Some(pos) = ids.iter().position(|id| *id == n) else { continue };
        let images: Vec<&serde_json::Value> = v["message"]["content"].as_array()?.iter().filter(|b| b["type"] == "image").collect();
        let img = images.get(pos)?;
        let data = img["source"]["data"].as_str()?;
        let ext = img["source"]["media_type"].as_str().and_then(|m| m.split('/').nth(1)).unwrap_or("png");
        let out = cache_dir().join(format!("claude-{session_id}-{n}.{ext}"));
        if !out.is_file() {
            let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
            std::fs::write(&out, bytes).ok()?;
        }
        return Some(out);
    }
    None
}

/// Card chrome around the content: padding 8 + border 1 on each side.
const CHROME: f32 = 18.;
/// Header row + gap under the image.
const HEADER_H: f32 = 24.;

/// Image box that fits `natural` inside `max_w`×`max_h` (capped at 520×360, never upscaled).
fn fit_image(natural: Option<(f32, f32)>, max_w: f32, max_h: f32) -> (f32, f32) {
    let (bw, bh) = (max_w.min(520.).max(60.), max_h.min(360.).max(60.));
    let (w, h) = natural.unwrap_or((bw, bh));
    let k = (bw / w).min(bh / h).min(1.);
    ((w * k).max(1.), (h * k).max(1.))
}

/// Approximate card width, for placing it inside the view.
pub fn card_width(p: &Preview, max_w: f32, max_h: f32) -> f32 {
    match p {
        Preview::Image { size, .. } => (fit_image(*size, max_w - CHROME, max_h - CHROME - HEADER_H).0 + CHROME).max(200.).min(max_w),
        _ => 380f32.min(max_w),
    }
}

/// `max_w`/`max_h`: space available for the whole card.
pub fn render(p: &Preview, max_w: f32, max_h: f32) -> gpui::AnyElement {
    let card = div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .p(px(8.))
        .rounded(px(8.))
        .bg(theme::rgb(0x1e2025))
        .border_1()
        .border_color(theme::rgb(0x363a42))
        .shadow_lg()
        .overflow_hidden()
        .max_w(px(max_w.min(560.)))
        .max_h(px(max_h.max(80.)));
    let header = |title: &str, caption: &str| {
        div()
            .flex()
            .items_baseline()
            .gap(px(8.))
            .px(px(2.))
            .child(div().text_size(px(12.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT)).child(title.to_string()))
            .child(div().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(caption.to_string()))
    };
    match p {
        Preview::Image { path, title, caption, size } => {
            let (w, h) = fit_image(*size, max_w - CHROME, max_h - CHROME - HEADER_H);
            card.child(gpui::img(path.clone()).w(px(w)).h(px(h)).flex_none().object_fit(gpui::ObjectFit::Contain))
                .child(header(title, caption))
                .into_any_element()
        }
        Preview::Code { title, caption, lines, lit } => {
            let mut body = div().flex().flex_col().font_family(theme::MONO_FONT).text_size(px(11.5));
            for (n, text, hl) in lines {
                let highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = hl.iter().map(|(r, c)| (r.clone(), HighlightStyle { color: Some(theme::rgb(*c)), ..Default::default() })).collect();
                let on = *lit == Some(*n);
                body = body.child(
                    div()
                        .flex()
                        .gap(px(10.))
                        .px(px(4.))
                        .rounded(px(3.))
                        .when(on, |d| d.bg(theme::rgb(theme::SELECTION)))
                        .child(div().w(px(30.)).flex_none().flex().justify_end().text_color(theme::rgb(if on { theme::TEXT } else { theme::GUTTER_TEXT })).child(n.to_string()))
                        .child(div().whitespace_nowrap().overflow_hidden().text_color(theme::rgb(theme::TEXT)).child(StyledText::new(SharedString::from(text.clone())).with_highlights(highlights))),
                );
            }
            card.min_w(px(360.)).child(header(title, caption)).child(body).into_any_element()
        }
        Preview::Folder { title, entries, more } => {
            let mut list = div().flex().flex_col().gap(px(1.));
            for (name, is_dir) in entries {
                let (icon, color) = if *is_dir { ("icons/folder.svg", 0x7aa2f7) } else { crate::assets::file_icon(name) };
                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .h(px(20.))
                        .child(gpui::svg().path(icon).size(px(12.)).flex_none().text_color(theme::rgb(color)))
                        .child(div().text_size(px(12.)).text_color(theme::rgb(theme::TEXT)).child(name.clone())),
                );
            }
            if *more > 0 {
                list = list.child(div().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(format!("…and {more} more")));
            }
            card.min_w(px(240.)).child(header(&format!("{title}/"), "Folder")).child(list).into_any_element()
        }
        Preview::File { title, caption, thumb } => card
            .when_some(thumb.clone(), |d, t| d.child(gpui::img(t).max_w(px(360.)).max_h(px(300.)).object_fit(gpui::ObjectFit::Contain)))
            .child(header(title, caption))
            .into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_refs_and_previews() {
        assert_eq!(image_ref_at("❯ mira esto [Image #12] y esto", 14), Some(12));
        assert_eq!(image_ref_at("❯ mira esto [Image #12] y esto", 2), None);
        assert_eq!(image_ref_at("[Image #x]", 3), None);

        let d = std::env::temp_dir().join(format!("jt-prev-{}", std::process::id()));
        std::fs::create_dir_all(d.join("sub")).unwrap();
        let src: String = (1..=50).map(|i| format!("let x{i} = {i};\n")).collect();
        std::fs::write(d.join("a.rs"), &src).unwrap();
        match for_path(&d.join("a.rs"), Some(30)).unwrap() {
            Preview::Code { lines, lit, caption, .. } => {
                assert_eq!(lines.len(), CODE_LINES);
                assert!(lines.iter().any(|(n, t, hl)| *n == 30 && t.contains("x30") && !hl.is_empty()));
                assert_eq!(lit, Some(30));
                assert!(caption.starts_with("Rust · 50 lines"));
            }
            other => panic!("{other:?}"),
        }
        match for_path(&d, None).unwrap() {
            Preview::Folder { entries, .. } => assert_eq!(entries[0], ("sub".to_string(), true)),
            other => panic!("{other:?}"),
        }
        std::fs::write(d.join("b.bin"), [0u8, 1, 2, 0, 9]).unwrap();
        assert!(matches!(for_path(&d.join("b.bin"), None), Some(Preview::File { .. })));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn extracts_pasted_image_from_session() {
        let projects = std::env::temp_dir().join(format!("jt-proj-{}", std::process::id()));
        let cwd = PathBuf::from("/tmp/x");
        let dir = projects.join(crate::claude_sessions::encode_dir(&cwd));
        std::fs::create_dir_all(&dir).unwrap();
        let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNGfake");
        let line = serde_json::json!({"type":"user","imagePasteIds":[3],"message":{"content":[{"type":"text","text":"[Image #3] hi"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":png}}]}});
        std::fs::write(dir.join("sess-1.jsonl"), line.to_string()).unwrap();
        // SAFETY: test-only env override, read by projects_dir().
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &projects.parent().unwrap().join(projects.file_name().unwrap())) };
        // projects_dir() = $CLAUDE_CONFIG_DIR/projects
        let real = projects.join("projects");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::rename(&dir, real.join(crate::claude_sessions::encode_dir(&cwd))).unwrap();
        let out = session_image("sess-1", Some(&cwd), 3).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"\x89PNGfake");
        assert!(session_image("sess-1", Some(&cwd), 4).is_none());
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
        let _ = std::fs::remove_dir_all(projects);
    }
}
