//! Editor insights: inline git blame on the caret line, merge conflict blocks with
//! accept actions, and language server hover (types and docs).

use std::ops::Range;
use std::path::PathBuf;
use std::time::Duration;

use editor::conflicts::{self, Conflict, Resolution};
use editor::{Language, highlight};
use git_engine::BlameLine;
use gpui::{
    AnyElement, ClickEvent, Context, FontWeight, HighlightStyle, Pixels, Point, SharedString, StyledText, Timer, div,
    point, prelude::*, px,
};

use super::{EditorEvent, EditorView};
use crate::theme;

/// Files longer than this get no blame (git gets slow and it's rarely useful).
const MAX_BLAME_LINES: usize = 20_000;

pub(super) struct BlameState {
    /// Document version the blame was computed for (line numbers match only then).
    pub version: u64,
    pub lines: Vec<Option<BlameLine>>,
    /// Repo `user.email`: those lines read "You".
    pub email: String,
}

pub(super) struct LspHover {
    pub at: Point<Pixels>,
    pub text: String,
}

#[derive(Default)]
pub(super) struct Insights {
    pub blame: Option<BlameState>,
    pub blame_task: Option<gpui::Task<()>>,
    /// Pointer over the blame annotation: show its card here.
    pub blame_hover: Option<Point<Pixels>>,
    conflicts: Option<(u64, Vec<Conflict>)>,
    /// The file had conflict markers at some point (offers "mark resolved" once they're gone).
    pub had_conflicts: bool,
    pub lsp_hover: Option<LspHover>,
    lsp_hover_word: Option<Range<usize>>,
    lsp_hover_task: Option<gpui::Task<()>>,
    /// Conflict action under the pointer (painted brighter).
    pub conflict_btn_hover: Option<(usize, Resolution)>,
}

impl EditorView {
    // ---- blame ------------------------------------------------------------------------------

    /// Recompute the blame for the current buffer after `delay` (debounced).
    pub(super) fn schedule_blame(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let Some(path) = self.doc.path.clone() else { return };
        if self.pure_asset() || self.doc.line_count() > MAX_BLAME_LINES || !path.is_file() {
            return;
        }
        let version = self.doc.version();
        let text = self.doc.text();
        self.ins.blame_task = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                Timer::after(delay).await;
            }
            let result = cx.background_executor().spawn(async move { git_engine::blame(&path, &text) }).await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((lines, email)) => this.ins.blame = Some(BlameState { version, lines, email }),
                    // Untracked file or not a repo: no annotation.
                    Err(_) => this.ins.blame = None,
                }
                cx.notify();
            });
        }));
    }

    /// Blame of a line, if it is current for this buffer version.
    pub(super) fn blame_for(&self, line: usize) -> Option<(&BlameLine, bool)> {
        let b = self.ins.blame.as_ref().filter(|b| b.version == self.doc.version())?;
        let l = b.lines.get(line)?.as_ref()?;
        let you = !b.email.is_empty() && l.author_mail.eq_ignore_ascii_case(&b.email);
        Some((l, you))
    }

    /// Ghost text after the caret line: "You, 3 days ago · Fix the parser".
    pub(super) fn blame_annotation(&self, line: usize) -> Option<String> {
        let (b, you) = self.blame_for(line)?;
        if b.uncommitted() {
            return Some("You, not committed yet".into());
        }
        let who = if you { "You".to_string() } else { b.author.clone() };
        let mut summary: String = b.summary.chars().take(80).collect();
        if b.summary.chars().count() > 80 {
            summary.push('…');
        }
        Some(format!("{who}, {} · {summary}", time_ago(b.author_time)))
    }

    pub(super) fn render_blame_card(&self, line: usize, at: Point<Pixels>) -> Option<AnyElement> {
        let (b, you) = self.blame_for(line)?;
        let body = if b.uncommitted() {
            div().text_size(px(12.5)).text_color(theme::rgb(theme::TEXT)).child("Not committed yet").into_any_element()
        } else {
            let who = if you { format!("{} (you)", b.author) } else { b.author.clone() };
            div()
                .flex()
                .flex_col()
                .gap(px(5.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(div().text_size(px(12.5)).font_weight(FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT)).child(who))
                        .child(div().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_FAINT)).child(b.author_mail.clone())),
                )
                .child(div().text_size(px(11.)).text_color(theme::rgb(theme::TEXT_MUTED)).child(format!("{} · {}", local_datetime(b.author_time), time_ago(b.author_time))))
                .child(div().h(px(1.)).my(px(2.)).bg(theme::rgb(theme::BORDER)))
                .child(div().text_size(px(12.5)).text_color(theme::rgb(theme::TEXT)).child(b.summary.clone()))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .pt(px(2.))
                        .child(
                            div()
                                .px(px(5.))
                                .rounded(px(3.))
                                .bg(theme::rgb(theme::SELECTED))
                                .font_family(theme::MONO_FONT)
                                .text_size(px(11.))
                                .text_color(theme::rgb(theme::ACCENT))
                                .child(b.hash[..b.hash.len().min(8)].to_string()),
                        )
                        .child(div().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child("click to copy")),
                )
                .into_any_element()
        };
        Some(
            gpui::deferred(
                gpui::anchored().position(point(at.x, at.y + px(14.))).snap_to_window_with_margin(px(8.)).child(
                    div()
                        .max_w(px(480.))
                        .p(px(10.))
                        .rounded(px(6.))
                        .bg(theme::rgb(0x202227))
                        .border_1()
                        .border_color(theme::rgb(theme::BORDER))
                        .shadow_lg()
                        .child(body),
                ),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    /// Click on the blame annotation: copy the commit hash.
    pub(super) fn copy_blame_hash(&mut self, line: usize, cx: &mut Context<Self>) {
        if let Some((b, _)) = self.blame_for(line).filter(|(b, _)| !b.uncommitted()) {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(b.hash.clone()));
        }
    }

    // ---- conflicts --------------------------------------------------------------------------

    /// Conflict blocks of the current buffer (cached per version).
    pub(super) fn conflicts(&mut self) -> &[Conflict] {
        let v = self.doc.version();
        if self.ins.conflicts.as_ref().is_none_or(|(cv, _)| *cv != v) {
            let found = if self.pure_asset() { Vec::new() } else { conflicts::find(&self.doc) };
            if !found.is_empty() {
                self.ins.had_conflicts = true;
            }
            self.ins.conflicts = Some((v, found));
        }
        &self.ins.conflicts.as_ref().unwrap().1
    }

    pub(super) fn resolve_conflict(&mut self, ix: usize, how: Resolution, cx: &mut Context<Self>) {
        let Some(c) = self.conflicts().get(ix).cloned() else { return };
        self.ins.conflict_btn_hover = None;
        self.run(cx, |d| conflicts::resolve(d, &c, how));
    }

    /// Resolve every block the same way (last first so line numbers stay valid).
    fn resolve_all(&mut self, how: Resolution, cx: &mut Context<Self>) {
        let all: Vec<Conflict> = self.conflicts().to_vec();
        self.run(cx, |d| {
            for c in all.iter().rev() {
                conflicts::resolve(d, c, how);
            }
        });
    }

    fn next_conflict(&mut self, cx: &mut Context<Self>) {
        let (cur, _) = self.doc.line_col(self.doc.selection.head);
        let starts: Vec<usize> = self.conflicts().iter().map(|c| c.start).collect();
        let Some(&line) = starts.iter().find(|&&s| s > cur).or(starts.first()) else { return };
        let at = self.doc.pos(line, 0);
        self.doc.set_cursor(at, false);
        self.autoscroll = true;
        cx.notify();
    }

    /// Save, then `git add` the file so Git sees the conflict as resolved.
    fn mark_resolved(&mut self, cx: &mut Context<Self>) {
        if self.doc.is_dirty() && !self.save(cx) {
            return;
        }
        let Some(path) = self.doc.path.clone() else { return };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let dir = path.parent().map(PathBuf::from).unwrap_or_default();
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    git_engine::stage(&dir, &[&name])
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.ins.had_conflicts = false,
                    Err(e) => cx.emit(EditorEvent::Error(format!("git add failed: {e:#}"))),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Bar above the text while the file has conflict markers (or just lost them).
    pub(super) fn render_conflict_bar(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let n = self.conflicts().len();
        if n == 0 && !self.ins.had_conflicts {
            return None;
        }
        let btn = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px(px(8.))
                .py(px(2.))
                .rounded(px(4.))
                .cursor_pointer()
                .bg(theme::rgb(theme::SELECTED))
                .text_color(theme::rgb(theme::TEXT))
                .hover(|s| s.bg(theme::rgb(0x3a3d45)))
                .child(label)
        };
        let bar = div().flex().items_center().gap(px(8.)).px(px(12.)).py(px(5.)).text_size(px(12.)).border_b_1().border_color(theme::rgb(theme::BORDER));
        Some(if n == 0 {
            bar.bg(theme::rgb(0x1f3326))
                .child(div().flex_1().text_color(theme::rgb(theme::ADDED)).child("All conflicts in this file are resolved."))
                .child(btn("conflict-mark", "Save and mark resolved (git add)").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.mark_resolved(cx))))
                .child(btn("conflict-dismiss", "Dismiss").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.ins.had_conflicts = false;
                    cx.notify();
                })))
                .into_any_element()
        } else {
            bar.bg(theme::rgb(0x3a2a1c))
                .child(div().flex_1().text_color(theme::rgb(theme::MODIFIED)).child(format!("{n} merge {} in this file", if n == 1 { "conflict" } else { "conflicts" })))
                .child(btn("conflict-next", "Next ↓").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.next_conflict(cx))))
                .child(btn("conflict-all-current", "Accept all current").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.resolve_all(Resolution::Current, cx))))
                .child(btn("conflict-all-incoming", "Accept all incoming").on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.resolve_all(Resolution::Incoming, cx))))
                .into_any_element()
        })
    }

    // ---- LSP hover --------------------------------------------------------------------------

    /// Pointer moved over the text (no diagnostic there): ask the TypeScript server about the
    /// word under it after a short pause.
    pub(super) fn hover_lsp_at(&mut self, pos: Option<usize>, at: Point<Pixels>, cx: &mut Context<Self>) {
        let word = pos.and_then(|p| word_at(&self.doc, p));
        if word == self.ins.lsp_hover_word {
            return;
        }
        self.ins.lsp_hover_word = word.clone();
        if self.ins.lsp_hover.take().is_some() {
            cx.notify();
        }
        self.ins.lsp_hover_task = None;
        let Some(word) = word else { return };
        let Some(server) = self.lsp.iter().find(|s| s.name == "tsgo").cloned() else { return };
        let Some(path) = self.doc.path.clone() else { return };
        let (line, col) = self.doc.line_col(word.start);
        let character: usize = self.doc.line(line).chars().take(col).map(|c| c.len_utf16()).sum();
        let version = self.doc.version();
        self.ins.lsp_hover_task = Some(cx.spawn(async move |this, cx| {
            Timer::after(Duration::from_millis(350)).await;
            let Ok(Some(text)) = server.hover(&path, line, character).await else { return };
            let _ = this.update(cx, |this, cx| {
                if this.doc.version() == version && this.ins.lsp_hover_word.as_ref() == Some(&word) {
                    this.ins.lsp_hover = Some(LspHover { at, text });
                    cx.notify();
                }
            });
        }));
    }

    pub(super) fn clear_lsp_hover(&mut self, cx: &mut Context<Self>) {
        self.ins.lsp_hover_word = None;
        self.ins.lsp_hover_task = None;
        if self.ins.lsp_hover.take().is_some() || self.ins.blame_hover.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn render_lsp_hover(&self) -> Option<AnyElement> {
        let h = self.ins.lsp_hover.as_ref()?;
        let font_size = self.font_size * 0.92;
        let blocks = parse_markdown(&h.text);
        let mut col = div().flex().flex_col().gap(px(6.));
        let mut first = true;
        for b in blocks {
            match b {
                MdPiece::Code { lang, code } => {
                    let language = fence_language(&lang);
                    let spans = highlight(language, &code);
                    let hl = spans.iter().filter(|s| s.start < s.end && s.end <= code.len()).map(|s| {
                        (s.start..s.end, HighlightStyle { color: Some(theme::rgb(theme::syntax_color(editor::HIGHLIGHT_NAMES[s.kind as usize]))), ..Default::default() })
                    });
                    let hl: Vec<_> = hl.collect();
                    let el = div()
                        .font_family(theme::MONO_FONT)
                        .text_size(font_size)
                        .text_color(theme::rgb(theme::TEXT))
                        .child(StyledText::new(SharedString::from(code)).with_highlights(hl));
                    col = col.child(if first { el } else { el.p(px(6.)).rounded(px(4.)).bg(theme::rgb(0x1a1c20)) });
                }
                MdPiece::Text(t) => {
                    if !first {
                        col = col.child(div().h(px(1.)).bg(theme::rgb(theme::BORDER)));
                    }
                    col = col.child(div().text_size(px(12.5)).text_color(theme::rgb(theme::TEXT_MUTED)).child(t));
                }
            }
            first = false;
        }
        Some(
            gpui::deferred(
                gpui::anchored().position(point(h.at.x, h.at.y + px(14.))).snap_to_window_with_margin(px(8.)).child(
                    div()
                        .id("lsp-hover")
                        .max_w(px(600.))
                        .max_h(px(360.))
                        .overflow_y_scroll()
                        .p(px(10.))
                        .rounded(px(6.))
                        .bg(theme::rgb(0x202227))
                        .border_1()
                        .border_color(theme::rgb(theme::BORDER))
                        .shadow_lg()
                        .child(col),
                ),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }
}

/// Identifier (letters, digits, `_`, `$`) at a char position.
fn word_at(doc: &editor::Document, pos: usize) -> Option<Range<usize>> {
    let (line, col) = doc.line_col(pos);
    let chars: Vec<char> = doc.line(line).chars().collect();
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    if !chars.get(col).is_some_and(|&c| is_word(c)) {
        return None;
    }
    let mut a = col;
    while a > 0 && is_word(chars[a - 1]) {
        a -= 1;
    }
    let mut b = col;
    while b < chars.len() && is_word(chars[b]) {
        b += 1;
    }
    let start = doc.pos(line, 0);
    Some(start + a..start + b)
}

enum MdPiece {
    Code { lang: String, code: String },
    Text(String),
}

/// Hover Markdown → code fences and plain paragraphs (light inline markup removed).
fn parse_markdown(md: &str) -> Vec<MdPiece> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut code: Option<(String, String)> = None;
    let flush = |text: &mut String, out: &mut Vec<MdPiece>| {
        let t = text.trim();
        if !t.is_empty() {
            out.push(MdPiece::Text(clean_inline(t)));
        }
        text.clear();
    };
    for line in md.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("```") {
            match code.take() {
                Some((lang, c)) => out.push(MdPiece::Code { lang, code: c.trim_end().to_string() }),
                None => {
                    flush(&mut text, &mut out);
                    code = Some((rest.trim().to_string(), String::new()));
                }
            }
            continue;
        }
        match &mut code {
            Some((_, c)) => {
                c.push_str(line);
                c.push('\n');
            }
            None if line.trim() == "---" => flush(&mut text, &mut out),
            None => {
                text.push_str(line);
                text.push('\n');
            }
        }
    }
    if let Some((lang, c)) = code {
        out.push(MdPiece::Code { lang, code: c.trim_end().to_string() });
    }
    flush(&mut text, &mut out);
    out
}

fn clean_inline(t: &str) -> String {
    // Bullets become "•"; other `*` is emphasis (JSDoc `*@param*`), dropped like backticks.
    t.lines()
        .map(|l| {
            let l = l.trim_end();
            let (bullet, rest) = match l.trim_start().strip_prefix("* ").or_else(|| l.trim_start().strip_prefix("- ")) {
                Some(rest) => ("• ", rest),
                None => ("", l),
            };
            format!("{bullet}{}", rest.replace(['`', '*'], ""))
        })
        .collect::<Vec<_>>()
        .join("\n")
        .replace("\n\n\n", "\n\n")
}

fn fence_language(lang: &str) -> Language {
    let ext = match lang {
        "typescript" | "ts" => "ts",
        "tsx" | "typescriptreact" => "tsx",
        "javascript" | "js" => "js",
        "jsx" | "javascriptreact" => "jsx",
        "rust" => "rs",
        "python" => "py",
        other => other,
    };
    Language::from_path(&PathBuf::from(format!("x.{ext}")))
}

/// "3 days ago", "2 months ago"…
fn time_ago(ts: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(ts);
    let s = (now - ts).max(0);
    let (n, unit) = match s {
        0..60 => return "just now".into(),
        60..3600 => (s / 60, "minute"),
        3600..86_400 => (s / 3600, "hour"),
        86_400..2_592_000 => (s / 86_400, "day"),
        2_592_000..31_536_000 => (s / 2_592_000, "month"),
        _ => (s / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// "Nov 25, 2026 14:03" in local time.
fn local_datetime(ts: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    // SAFETY: localtime_r only writes into the struct we pass.
    let tm = unsafe {
        let t: libc::time_t = ts as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    format!("{} {}, {} {:02}:{:02}", MONTHS[tm.tm_mon.clamp(0, 11) as usize], tm.tm_mday, tm.tm_year + 1900, tm.tm_hour, tm.tm_min)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hover_markdown_splits_code_and_text() {
        let p = parse_markdown("```typescript\nfunction f(a: number): void\n```\nDoes **things**.\n\n*@param* `a` — the input");
        assert_eq!(p.len(), 2);
        assert!(matches!(&p[0], MdPiece::Code { lang, code } if lang == "typescript" && code == "function f(a: number): void"));
        assert!(matches!(&p[1], MdPiece::Text(t) if t.contains("Does things.") && t.contains("@param a — the input")));
    }

    #[test]
    fn words_include_dollar_and_underscore() {
        let d = editor::Document::from_str("let $foo_1 = bar;", Language::PlainText);
        assert_eq!(word_at(&d, 6), Some(4..10));
        assert_eq!(word_at(&d, 3), None);
    }
}
