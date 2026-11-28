//! Notion-style "/" menu for the live Markdown editor: typing `/` on an empty line opens a
//! block picker (headings, lists, table, image…) that filters as you keep typing.

use gpui::{ClickEvent, Context, PathPromptOptions, div, point, prelude::*, px};

use crate::editor_view::EditorView;
use crate::theme;

pub struct SlashItem {
    pub label: &'static str,
    pub hint: &'static str,
    /// Markdown shown as the item's glyph.
    pub glyph: &'static str,
    pub keywords: &'static str,
    /// Inserted text; `$0` marks where the caret ends up. `None` = special (image picker).
    pub snippet: Option<&'static str>,
}

pub const ITEMS: &[SlashItem] = &[
    SlashItem { label: "Heading 1", hint: "Big section heading", glyph: "H1", keywords: "h1 title heading", snippet: Some("# $0") },
    SlashItem { label: "Heading 2", hint: "Medium section heading", glyph: "H2", keywords: "h2 subtitle heading", snippet: Some("## $0") },
    SlashItem { label: "Heading 3", hint: "Small section heading", glyph: "H3", keywords: "h3 heading", snippet: Some("### $0") },
    SlashItem { label: "Text", hint: "Plain paragraph", glyph: "¶", keywords: "paragraph plain text", snippet: Some("$0") },
    SlashItem { label: "Bulleted list", hint: "Simple bulleted list", glyph: "•", keywords: "ul bullet list unordered", snippet: Some("- $0") },
    SlashItem { label: "Numbered list", hint: "List with numbering", glyph: "1.", keywords: "ol numbered ordered list", snippet: Some("1. $0") },
    SlashItem { label: "To-do list", hint: "Track tasks with checkboxes", glyph: "☐", keywords: "todo task checkbox check", snippet: Some("- [ ] $0") },
    SlashItem { label: "Table", hint: "Editable grid", glyph: "▦", keywords: "table grid columns rows", snippet: Some("TABLE") },
    SlashItem { label: "Image", hint: "Pick an image file", glyph: "🖼", keywords: "image picture photo img", snippet: None },
    SlashItem { label: "Link", hint: "Link to a URL", glyph: "🔗", keywords: "link url href", snippet: Some("[$0](https://)") },
    SlashItem { label: "Quote", hint: "Capture a quote", glyph: "❝", keywords: "quote blockquote", snippet: Some("> $0") },
    SlashItem { label: "Callout", hint: "Highlighted note", glyph: "💡", keywords: "callout note tip info", snippet: Some("> **Note:** $0") },
    SlashItem { label: "Code block", hint: "Fenced code", glyph: "</>", keywords: "code fence snippet pre", snippet: Some("```$0\n\n```") },
    SlashItem { label: "Divider", hint: "Horizontal rule", glyph: "—", keywords: "divider hr rule line separator", snippet: Some("---\n$0") },
];

/// Open menu state: the `/` sits at (line, col).
pub struct SlashMenu {
    pub line: usize,
    pub col: usize,
    pub selected: usize,
}

pub fn filter(query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    let mut scored: Vec<(i64, usize)> = ITEMS
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            if q.is_empty() {
                return Some((0, i));
            }
            let hay = format!("{} {}", it.label.to_lowercase(), it.keywords);
            if it.label.to_lowercase().starts_with(&q) {
                Some((1000, i))
            } else if hay.split_whitespace().any(|w| w.starts_with(&q)) {
                Some((500, i))
            } else {
                project_search::fuzzy_match(&q, &hay).map(|(s, _)| (s, i))
            }
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, i)| i).collect()
}

/// (text without the marker, caret offset in chars).
pub fn expand(snippet: &str) -> (String, usize) {
    let s = if snippet == "TABLE" {
        // Caret in the first body cell.
        let t = editor::markdown::NEW_TABLE.trim_end();
        let third = t.match_indices('\n').nth(1).map(|(i, _)| i + 1).unwrap_or(0);
        format!("{}$0{}", &t[..third + 2], &t[third + 2..])
    } else {
        snippet.to_string()
    };
    let at = s.find("$0").unwrap_or(s.len());
    let caret = s[..at].chars().count();
    (s.replacen("$0", "", 1), caret)
}

impl EditorView {
    /// Called after every edit: open on `/` typed at the start of a blank line, keep the query
    /// in sync, close when the caret leaves it.
    pub(crate) fn update_slash_menu(&mut self, typed: Option<&str>) {
        if !self.markdown_live {
            self.slash = None;
            return;
        }
        let (line, col) = self.doc.line_col(self.doc.selection.head);
        if typed == Some("/") && self.doc.selection.is_empty() {
            let text = self.doc.line(line);
            let before: String = text.chars().take(col.saturating_sub(1)).collect();
            if before.trim().is_empty() && text.chars().count() == col {
                self.slash = Some(SlashMenu { line, col: col - 1, selected: 0 });
                return;
            }
        }
        let Some(m) = &self.slash else { return };
        let ok = line == m.line && col > m.col && self.doc.line(line).chars().nth(m.col) == Some('/') && !self.slash_query().contains(char::is_whitespace);
        if !ok {
            self.slash = None;
        }
    }

    pub(crate) fn slash_query(&self) -> String {
        let Some(m) = &self.slash else { return String::new() };
        let (_, col) = self.doc.line_col(self.doc.selection.head);
        self.doc.line(m.line).chars().skip(m.col + 1).take(col.saturating_sub(m.col + 1)).collect()
    }

    /// Up/down/enter/escape while the menu is open. Returns true when consumed.
    pub(crate) fn slash_key(&mut self, key: SlashKey, cx: &mut Context<Self>) -> bool {
        let Some((line, col)) = self.slash.as_ref().map(|m| (m.line, m.col)) else { return false };
        let n = filter(&self.slash_query_static(line, col)).len();
        let m = self.slash.as_mut().expect("checked");
        match key {
            SlashKey::Up if n > 0 => m.selected = (m.selected + n - 1) % n,
            SlashKey::Down if n > 0 => m.selected = (m.selected + 1) % n,
            SlashKey::Escape => self.slash = None,
            SlashKey::Enter => {
                if n == 0 {
                    self.slash = None;
                    return false; // plain newline
                }
                let sel = m.selected.min(n - 1);
                self.apply_slash(sel, cx);
            }
            _ => {}
        }
        cx.notify();
        true
    }

    fn slash_query_static(&self, line: usize, col: usize) -> String {
        let (_, cur) = self.doc.line_col(self.doc.selection.head);
        self.doc.line(line).chars().skip(col + 1).take(cur.saturating_sub(col + 1)).collect()
    }

    /// Replace `/query` with the chosen block.
    pub(crate) fn apply_slash(&mut self, filtered_ix: usize, cx: &mut Context<Self>) {
        let Some(m) = self.slash.take() else { return };
        let items = filter(&self.slash_query_static(m.line, m.col));
        let Some(&item_ix) = items.get(filtered_ix) else { return };
        let start = self.doc.pos(m.line, m.col);
        let end = self.doc.selection.head;
        match ITEMS[item_ix].snippet {
            Some(snippet) => {
                let (text, caret) = expand(snippet);
                self.run(cx, move |d| {
                    d.selection = editor::Selection { anchor: start, head: end };
                    d.insert(&text);
                    d.set_cursor(start + caret, false);
                });
            }
            None => {
                // Image: remove the "/query", then ask for a file and insert a relative link.
                self.run(cx, move |d| {
                    d.selection = editor::Selection { anchor: start, head: end };
                    d.insert("");
                });
                let base = self.doc.path.as_ref().and_then(|p| p.parent()).map(|p| p.to_path_buf());
                let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Insert image".into()) });
                cx.spawn(async move |this, cx| {
                    let Ok(Ok(Some(paths))) = paths.await else { return };
                    let Some(p) = paths.into_iter().next() else { return };
                    let rel = base.as_ref().and_then(|b| relative_path(&p, b)).unwrap_or_else(|| p.to_string_lossy().into_owned());
                    let alt = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    let text = format!("![{alt}]({})", rel.replace(' ', "%20"));
                    let _ = this.update(cx, |this, cx| {
                        this.run(cx, move |d| {
                            d.set_cursor(start, false);
                            d.insert(&text);
                        })
                    });
                })
                .detach();
            }
        }
    }

    pub(crate) fn render_slash_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let m = self.slash.as_ref()?;
        let md = self.md_layout.as_ref()?;
        let l = md.lines.iter().find(|l| l.line == m.line)?;
        let c = md.caret(l, m.col, &self.doc.line(m.line));
        let pos = point(md.origin.x + c.x - md.bounds_origin.x, md.origin.y + c.y + l.line_height + px(4.) - md.bounds_origin.y);
        let items = filter(&self.slash_query());
        let selected = m.selected.min(items.len().saturating_sub(1));
        let mut menu = div()
            .absolute()
            .left(pos.x)
            .top(pos.y)
            .w(px(270.))
            .max_h(px(340.))
            .flex()
            .flex_col()
            .p(px(4.))
            .rounded(px(8.))
            .bg(theme::rgb(0x202227))
            .border_1()
            .border_color(theme::rgb(0x33363d))
            .shadow_lg()
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation());
        if items.is_empty() {
            menu = menu.child(div().px(px(8.)).py(px(6.)).text_size(px(12.)).text_color(theme::rgb(theme::TEXT_FAINT)).child("No matching blocks"));
        } else {
            menu = menu.child(div().px(px(8.)).pt(px(4.)).pb(px(2.)).text_size(px(10.5)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::rgb(theme::TEXT_FAINT)).child("BLOCKS"));
        }
        // Keep the selected row in the visible window of ≤ 8 items.
        let first = selected.saturating_sub(7);
        for (fi, &ix) in items.iter().enumerate().skip(first).take(8) {
            let it = &ITEMS[ix];
            let on = fi == selected;
            menu = menu.child(
                div()
                    .id(("slash", fi))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(6.))
                    .h(px(38.))
                    .rounded(px(5.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(theme::rgb(theme::SELECTED)))
                    .hover(|s| s.bg(theme::rgb(theme::HOVER)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.apply_slash(fi, cx)))
                    .child(
                        div()
                            .flex_none()
                            .size(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.))
                            .border_1()
                            .border_color(theme::rgb(0x33363d))
                            .bg(theme::rgb(theme::SIDEBAR_BG))
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme::rgb(theme::TEXT_MUTED))
                            .child(it.glyph),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .child(div().text_size(px(12.5)).text_color(theme::rgb(theme::TEXT)).child(it.label))
                            .child(div().text_size(px(10.5)).text_color(theme::rgb(theme::TEXT_FAINT)).child(it.hint)),
                    ),
            );
        }
        Some(menu.into_any_element())
    }
}

pub enum SlashKey {
    Up,
    Down,
    Enter,
    Escape,
}

fn relative_path(target: &std::path::Path, base: &std::path::Path) -> Option<String> {
    let t: Vec<_> = target.components().collect();
    let b: Vec<_> = base.components().collect();
    let common = t.iter().zip(&b).take_while(|(a, b)| a == b).count();
    if common == 0 {
        return None;
    }
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), b.len() - common).collect();
    parts.extend(t[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()));
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_prefers_label_prefix_and_keywords() {
        assert_eq!(ITEMS[filter("tab")[0]].label, "Table");
        assert_eq!(ITEMS[filter("h2")[0]].label, "Heading 2");
        assert_eq!(ITEMS[filter("todo")[0]].label, "To-do list");
        assert_eq!(filter("").len(), ITEMS.len());
        assert!(filter("zzzz").is_empty());
    }

    #[test]
    fn expand_places_caret() {
        assert_eq!(expand("## $0"), ("## ".to_string(), 3));
        let (t, c) = expand("TABLE");
        assert_eq!(t, editor::markdown::NEW_TABLE.trim_end());
        let third = t.match_indices('\n').nth(1).unwrap().0 + 1;
        assert_eq!(c, third + 2);
        assert_eq!(relative_path(std::path::Path::new("/a/b/img/x.png"), std::path::Path::new("/a/b/docs")).unwrap(), "../img/x.png");
    }
}
