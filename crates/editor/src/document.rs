//! Text document with selection, movement, editing and undo/redo. UI-agnostic and testable.
//! Positions are char indices into the rope.

use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use ropey::Rope;

use crate::language::Language;

/// Refuse to open files larger than this (the editor is for source code).
pub const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn caret(pos: usize) -> Self {
        Self { anchor: pos, head: pos }
    }
    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

#[derive(Clone, Debug)]
struct Edit {
    start: usize,
    old: String,
    new: String,
}

#[derive(Clone, Debug)]
struct Transaction {
    edits: Vec<Edit>,
    before: Selection,
    after: Selection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditKind {
    Typing,
    Deleting,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CharClass {
    Space,
    Word,
    Punct,
    Newline,
}

fn class(c: char) -> CharClass {
    if c == '\n' {
        CharClass::Newline
    } else if c.is_whitespace() {
        CharClass::Space
    } else if c.is_alphanumeric() || c == '_' || c == '$' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

pub struct Document {
    rope: Rope,
    pub path: Option<PathBuf>,
    pub language: Language,
    pub selection: Selection,
    goal_column: Option<usize>,
    undo_stack: Vec<Transaction>,
    redo_stack: Vec<Transaction>,
    last_edit: Option<(EditKind, Instant)>,
    version: u64,
    saved_version: u64,
    crlf: bool,
    /// Modification time when last loaded/saved, for external change detection.
    pub disk_mtime: Option<SystemTime>,
    pub tab_size: usize,
    pub indent_unit: String,
}

impl Document {
    pub fn from_str(text: &str, language: Language) -> Self {
        let crlf = text.contains("\r\n");
        let normalized = if crlf { text.replace("\r\n", "\n") } else { text.to_string() };
        let indent_unit = detect_indent(&normalized);
        Self {
            rope: Rope::from_str(&normalized),
            path: None,
            language,
            selection: Selection::default(),
            goal_column: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_edit: None,
            version: 0,
            saved_version: 0,
            crlf,
            disk_mtime: None,
            tab_size: 4,
            indent_unit,
        }
    }

    pub fn open(path: &Path) -> Result<Self> {
        let meta = fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
        if meta.len() > MAX_FILE_BYTES {
            bail!("{} is too large to open ({} MB; the limit is 100 MB)", path.display(), meta.len() / (1024 * 1024));
        }
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        if bytes.iter().take(8000).any(|&b| b == 0) {
            bail!("{} looks like a binary file", path.display());
        }
        let text = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("{} is not valid UTF-8", path.display()))?;
        let mut doc = Self::from_str(&text, Language::from_path(path));
        doc.path = Some(path.to_path_buf());
        doc.disk_mtime = meta.modified().ok();
        Ok(doc)
    }

    /// Reload from disk, keeping the caret near its old position. Clears undo history.
    pub fn reload(&mut self) -> Result<()> {
        let path = self.path.clone().context("document has no path")?;
        let mut fresh = Self::open(&path)?;
        let pos = self.selection.head.min(fresh.rope.len_chars());
        fresh.selection = Selection::caret(pos);
        *self = fresh;
        Ok(())
    }

    pub fn save(&mut self) -> Result<()> {
        let path = self.path.clone().context("document has no path")?;
        let mut text = self.rope.to_string();
        if self.crlf {
            text = text.replace('\n', "\r\n");
        }
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        self.saved_version = self.version;
        self.disk_mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
        Ok(())
    }

    /// True when the file on disk changed since we loaded or saved it.
    pub fn changed_on_disk(&self) -> bool {
        let Some(path) = &self.path else { return false };
        match (fs::metadata(path).and_then(|m| m.modified()).ok(), self.disk_mtime) {
            (Some(now), Some(then)) => now != then,
            (None, Some(_)) => false, // deleted: keep buffer
            _ => false,
        }
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }
    pub fn text(&self) -> String {
        self.rope.to_string()
    }
    pub fn version(&self) -> u64 {
        self.version
    }
    pub fn is_dirty(&self) -> bool {
        self.version != self.saved_version
    }
    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }
    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    /// Text of a line without the trailing newline.
    pub fn line(&self, line: usize) -> std::borrow::Cow<'_, str> {
        let slice = self.rope.line(line);
        let len = slice.len_chars();
        let slice = if len > 0 && slice.char(len - 1) == '\n' { slice.slice(..len - 1) } else { slice };
        slice.into()
    }

    pub fn line_len(&self, line: usize) -> usize {
        let slice = self.rope.line(line);
        let len = slice.len_chars();
        if len > 0 && slice.char(len - 1) == '\n' { len - 1 } else { len }
    }

    /// (line, column) of a char index.
    pub fn line_col(&self, pos: usize) -> (usize, usize) {
        let pos = pos.min(self.rope.len_chars());
        let line = self.rope.char_to_line(pos);
        (line, pos - self.rope.line_to_char(line))
    }

    pub fn pos(&self, line: usize, col: usize) -> usize {
        let line = line.min(self.line_count().saturating_sub(1));
        self.rope.line_to_char(line) + col.min(self.line_len(line))
    }

    pub fn line_start_byte(&self, line: usize) -> usize {
        self.rope.line_to_byte(line)
    }

    pub fn selected_text(&self) -> String {
        self.rope.slice(self.selection.range()).to_string()
    }

    // ---- movement -------------------------------------------------------------------------

    fn set_head(&mut self, pos: usize, extend: bool) {
        self.selection.head = pos;
        if !extend {
            self.selection.anchor = pos;
        }
    }

    pub fn set_cursor(&mut self, pos: usize, extend: bool) {
        self.goal_column = None;
        self.set_head(pos.min(self.len_chars()), extend);
    }

    pub fn move_left(&mut self, extend: bool) {
        self.goal_column = None;
        if !extend && !self.selection.is_empty() {
            let start = self.selection.range().start;
            return self.set_head(start, false);
        }
        self.set_head(self.selection.head.saturating_sub(1), extend);
    }

    pub fn move_right(&mut self, extend: bool) {
        self.goal_column = None;
        if !extend && !self.selection.is_empty() {
            let end = self.selection.range().end;
            return self.set_head(end, false);
        }
        self.set_head((self.selection.head + 1).min(self.len_chars()), extend);
    }

    pub fn move_vertical(&mut self, delta: isize, extend: bool) {
        let (line, col) = self.line_col(self.selection.head);
        let goal = *self.goal_column.get_or_insert(col);
        let target = line as isize + delta;
        let pos = if target < 0 {
            0
        } else if target as usize >= self.line_count() {
            self.len_chars()
        } else {
            self.pos(target as usize, goal)
        };
        self.set_head(pos, extend);
    }

    pub fn move_word(&mut self, forward: bool, extend: bool) {
        self.goal_column = None;
        let pos = if forward { self.next_word_boundary(self.selection.head) } else { self.prev_word_boundary(self.selection.head) };
        self.set_head(pos, extend);
    }

    fn next_word_boundary(&self, mut pos: usize) -> usize {
        let len = self.len_chars();
        while pos < len && class(self.rope.char(pos)) == CharClass::Space {
            pos += 1;
        }
        if pos < len {
            let c = class(self.rope.char(pos));
            if c == CharClass::Newline {
                return pos + 1;
            }
            while pos < len && class(self.rope.char(pos)) == c {
                pos += 1;
            }
        }
        pos
    }

    fn prev_word_boundary(&self, mut pos: usize) -> usize {
        while pos > 0 && class(self.rope.char(pos - 1)) == CharClass::Space {
            pos -= 1;
        }
        if pos > 0 {
            let c = class(self.rope.char(pos - 1));
            if c == CharClass::Newline {
                return pos - 1;
            }
            while pos > 0 && class(self.rope.char(pos - 1)) == c {
                pos -= 1;
            }
        }
        pos
    }

    /// Home: toggles between first non-blank character and column 0.
    pub fn move_line_start(&mut self, extend: bool) {
        self.goal_column = None;
        let (line, col) = self.line_col(self.selection.head);
        let indent = self.line(line).chars().take_while(|c| *c == ' ' || *c == '\t').count();
        let target = if col == indent { 0 } else { indent };
        self.set_head(self.pos(line, target), extend);
    }

    pub fn move_line_end(&mut self, extend: bool) {
        self.goal_column = None;
        let (line, _) = self.line_col(self.selection.head);
        self.set_head(self.pos(line, usize::MAX), extend);
    }

    pub fn move_doc_start(&mut self, extend: bool) {
        self.set_cursor(0, extend);
    }

    pub fn move_doc_end(&mut self, extend: bool) {
        self.set_cursor(self.len_chars(), extend);
    }

    pub fn select_all(&mut self) {
        self.selection = Selection { anchor: 0, head: self.len_chars() };
    }

    pub fn select_word_at(&mut self, pos: usize) {
        let len = self.len_chars();
        if len == 0 {
            return;
        }
        let pos = pos.min(len - 1);
        let c = class(self.rope.char(pos));
        let mut start = pos;
        while start > 0 && class(self.rope.char(start - 1)) == c {
            start -= 1;
        }
        let mut end = pos;
        while end < len && class(self.rope.char(end)) == c {
            end += 1;
        }
        self.selection = Selection { anchor: start, head: end };
    }

    pub fn select_line_at(&mut self, pos: usize) {
        let (line, _) = self.line_col(pos);
        let start = self.rope.line_to_char(line);
        let end = if line + 1 < self.line_count() { self.rope.line_to_char(line + 1) } else { self.len_chars() };
        self.selection = Selection { anchor: start, head: end };
    }

    // ---- editing --------------------------------------------------------------------------

    fn apply(&mut self, edits: Vec<Edit>, after: Selection, kind: EditKind) {
        let before = self.selection;
        for e in &edits {
            let old_len = e.old.chars().count();
            self.rope.remove(e.start..e.start + old_len);
            self.rope.insert(e.start, &e.new);
        }
        self.version += 1;
        self.selection = after;
        self.goal_column = None;
        self.redo_stack.clear();

        // Coalesce consecutive typing/deleting into one undo step.
        let now = Instant::now();
        let coalesce = matches!(self.last_edit, Some((k, t)) if k == kind && kind != EditKind::Other && now - t < Duration::from_millis(1200))
            && edits.len() == 1;
        if coalesce {
            if let Some(last) = self.undo_stack.last_mut().filter(|t| t.edits.len() == 1) {
                if let Some(prev) = last.edits.last_mut() {
                    let e = &edits[0];
                    // typing: e.start == prev.start + prev.new.len
                    if kind == EditKind::Typing && e.old.is_empty() && e.start == prev.start + prev.new.chars().count() {
                        prev.new.push_str(&e.new);
                        last.after = after;
                        self.last_edit = Some((kind, now));
                        return;
                    }
                    // backspace: deleting just before previous deletion
                    if kind == EditKind::Deleting && e.new.is_empty() && prev.new.is_empty() && e.start + e.old.chars().count() == prev.start {
                        prev.old.insert_str(0, &e.old);
                        prev.start = e.start;
                        last.after = after;
                        self.last_edit = Some((kind, now));
                        return;
                    }
                }
            }
        }
        self.undo_stack.push(Transaction { edits, before, after });
        if self.undo_stack.len() > 10_000 {
            self.undo_stack.remove(0);
        }
        self.last_edit = Some((kind, now));
    }

    fn replace_range(&mut self, range: Range<usize>, text: &str, kind: EditKind) {
        let old = self.rope.slice(range.clone()).to_string();
        let caret = range.start + text.chars().count();
        self.apply(vec![Edit { start: range.start, old, new: text.to_string() }], Selection::caret(caret), kind);
    }

    /// Insert text, replacing the selection.
    pub fn insert(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n");
        let kind = if text.chars().count() == 1 && !text.contains('\n') { EditKind::Typing } else { EditKind::Other };
        self.replace_range(self.selection.range(), &text, kind);
    }

    /// Newline keeping the current line's indentation (plus one level after an opening bracket).
    pub fn newline(&mut self) {
        let (line, col) = self.line_col(self.selection.range().start);
        let text = self.line(line);
        let indent: String = text.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        let before: String = text.chars().take(col).collect();
        let after_char = text.chars().nth(col);
        let opens = before.trim_end().ends_with(['{', '[', '(']);
        let mut insert = format!("\n{indent}");
        if opens {
            insert.push_str(&self.indent_unit);
            if matches!(after_char, Some('}' | ']' | ')')) {
                // Put the closing bracket on its own line, caret on the middle one.
                let caret_offset = insert.chars().count();
                let full = format!("{insert}\n{indent}");
                let start = self.selection.range().start;
                self.replace_range(self.selection.range(), &full, EditKind::Other);
                self.selection = Selection::caret(start + caret_offset);
                return;
            }
        }
        self.replace_range(self.selection.range(), &insert, EditKind::Other);
    }

    pub fn backspace(&mut self) {
        let sel = self.selection.range();
        if !sel.is_empty() {
            return self.replace_range(sel, "", EditKind::Other);
        }
        if sel.start == 0 {
            return;
        }
        // In leading whitespace made of spaces: delete back to the previous indent stop.
        let (line, col) = self.line_col(sel.start);
        let text = self.line(line);
        let leading = text.chars().take(col).all(|c| c == ' ');
        let unit = self.indent_unit.chars().count();
        let n = if leading && col > 0 && self.indent_unit.chars().all(|c| c == ' ') && unit > 0 {
            let r = col % unit;
            if r == 0 { unit } else { r }
        } else {
            1
        };
        self.replace_range(sel.start - n..sel.start, "", EditKind::Deleting);
    }

    pub fn delete_forward(&mut self) {
        let sel = self.selection.range();
        if !sel.is_empty() {
            return self.replace_range(sel, "", EditKind::Other);
        }
        if sel.start < self.len_chars() {
            self.replace_range(sel.start..sel.start + 1, "", EditKind::Other);
        }
    }

    pub fn delete_word_backward(&mut self) {
        if self.selection.is_empty() {
            let start = self.prev_word_boundary(self.selection.head);
            self.selection.anchor = start;
        }
        self.replace_range(self.selection.range(), "", EditKind::Other);
    }

    pub fn delete_to_line_start(&mut self) {
        if self.selection.is_empty() {
            let (line, _) = self.line_col(self.selection.head);
            self.selection.anchor = self.pos(line, 0);
        }
        self.replace_range(self.selection.range(), "", EditKind::Other);
    }

    fn selected_lines(&self) -> Range<usize> {
        let r = self.selection.range();
        let (l0, _) = self.line_col(r.start);
        let (mut l1, c1) = self.line_col(r.end);
        if c1 == 0 && l1 > l0 {
            l1 -= 1;
        }
        l0..l1 + 1
    }

    /// Tab: indent selected lines, or insert one indent unit at the caret.
    pub fn indent(&mut self) {
        let r = self.selection.range();
        if self.line_col(r.start).0 == self.line_col(r.end).0 {
            let unit = self.indent_unit.clone();
            return self.replace_range(r, &unit, EditKind::Other);
        }
        let unit = self.indent_unit.clone();
        let lines = self.selected_lines();
        let mut edits = Vec::new();
        let mut shift = 0;
        for line in lines.clone() {
            if self.line_len(line) == 0 {
                continue;
            }
            edits.push(Edit { start: self.rope.line_to_char(line) + shift, old: String::new(), new: unit.clone() });
            shift += unit.chars().count();
        }
        let start = self.rope.line_to_char(lines.start);
        let end = self.pos(lines.end - 1, usize::MAX) + shift;
        self.apply(edits, Selection { anchor: start, head: end }, EditKind::Other);
    }

    pub fn outdent(&mut self) {
        let lines = self.selected_lines();
        let unit = self.indent_unit.chars().count().max(1);
        let mut edits = Vec::new();
        let mut removed = 0;
        for line in lines.clone() {
            let text = self.line(line);
            let n = if text.starts_with('\t') {
                1
            } else {
                text.chars().take(unit).take_while(|c| *c == ' ').count()
            };
            if n > 0 {
                let start = self.rope.line_to_char(line) - removed;
                edits.push(Edit { start, old: text.chars().take(n).collect(), new: String::new() });
                removed += n;
            }
        }
        if edits.is_empty() {
            return;
        }
        let start = self.rope.line_to_char(lines.start);
        let end_line_start = self.rope.line_to_char(lines.end - 1);
        let end = end_line_start + self.line_len(lines.end - 1) - removed;
        self.apply(edits, Selection { anchor: start, head: end }, EditKind::Other);
    }

    pub fn toggle_comment(&mut self) {
        let Some(prefix) = self.language.line_comment() else { return };
        let bare = prefix.trim_end();
        let lines = self.selected_lines();
        let non_empty: Vec<usize> = lines.clone().filter(|&l| !self.line(l).trim().is_empty()).collect();
        if non_empty.is_empty() {
            return;
        }
        let all_commented = non_empty.iter().all(|&l| self.line(l).trim_start().starts_with(bare));
        let min_indent = non_empty
            .iter()
            .map(|&l| self.line(l).chars().take_while(|c| c.is_whitespace()).count())
            .min()
            .unwrap_or(0);
        let mut edits = Vec::new();
        let mut delta: isize = 0;
        for &l in &non_empty {
            let text = self.line(l).to_string();
            let line_start = (self.rope.line_to_char(l) as isize + delta) as usize;
            if all_commented {
                let indent = text.chars().take_while(|c| c.is_whitespace()).count();
                let rest: String = text.chars().skip(indent).collect();
                let remove = if rest.starts_with(prefix) { prefix.chars().count() } else { bare.chars().count() };
                edits.push(Edit { start: line_start + indent, old: rest.chars().take(remove).collect(), new: String::new() });
                delta -= remove as isize;
            } else {
                edits.push(Edit { start: line_start + min_indent, old: String::new(), new: prefix.to_string() });
                delta += prefix.chars().count() as isize;
            }
        }
        let start = self.rope.line_to_char(lines.start);
        let last = lines.end - 1;
        let end = (self.rope.line_to_char(last) as isize + self.line_len(last) as isize + delta).max(start as isize) as usize;
        self.apply(edits, Selection { anchor: start, head: end }, EditKind::Other);
    }

    /// Replace the whole text as one undoable edit, keeping the caret on the same line/column.
    pub fn replace_all_keep_cursor(&mut self, text: &str) {
        if self.rope == text {
            return;
        }
        let (line, col) = self.line_col(self.selection.head);
        let all = 0..self.len_chars();
        let old = self.rope.to_string();
        self.apply(vec![Edit { start: 0, old, new: text.to_string() }], Selection::caret(0), EditKind::Other);
        let _ = all;
        let pos = self.pos(line, col);
        self.selection = Selection::caret(pos);
    }

    pub fn undo(&mut self) -> bool {
        let Some(tx) = self.undo_stack.pop() else { return false };
        for e in tx.edits.iter().rev() {
            self.rope.remove(e.start..e.start + e.new.chars().count());
            self.rope.insert(e.start, &e.old);
        }
        self.selection = tx.before;
        self.version += 1;
        self.last_edit = None;
        self.redo_stack.push(tx);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(tx) = self.redo_stack.pop() else { return false };
        for e in &tx.edits {
            self.rope.remove(e.start..e.start + e.old.chars().count());
            self.rope.insert(e.start, &e.new);
        }
        self.selection = tx.after;
        self.version += 1;
        self.last_edit = None;
        self.undo_stack.push(tx);
        true
    }

    // ---- search ---------------------------------------------------------------------------

    /// All matches of `query` as char ranges. Case-insensitive unless the query has uppercase.
    pub fn find_all(&self, query: &str) -> Vec<Range<usize>> {
        if query.is_empty() {
            return Vec::new();
        }
        let smart_case = query.chars().any(|c| c.is_uppercase());
        let text = self.rope.to_string();
        let (hay, needle) = if smart_case { (text.clone(), query.to_string()) } else { (text.to_lowercase(), query.to_lowercase()) };
        // Lowercasing can change byte lengths for some scripts; fall back to exact search then.
        let (hay, needle) = if hay.len() == text.len() { (hay, needle) } else { (text.clone(), query.to_string()) };
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(i) = hay[from..].find(&needle) {
            let b = from + i;
            let start = self.rope.byte_to_char(b);
            let end = self.rope.byte_to_char(b + needle.len());
            out.push(start..end);
            from = b + needle.len().max(1);
            if out.len() >= 100_000 {
                break;
            }
        }
        out
    }
}

/// Guess indentation: tabs if most indented lines start with a tab, else the smallest
/// common space indent (2 or 4), defaulting to 4 spaces.
fn detect_indent(text: &str) -> String {
    let mut tabs = 0;
    let mut spaces = [0usize; 9];
    for line in text.lines().take(2000) {
        if line.starts_with('\t') {
            tabs += 1;
        } else {
            let n = line.chars().take_while(|c| *c == ' ').count();
            if (1..=8).contains(&n) && line.len() > n {
                spaces[n] += 1;
            }
        }
    }
    let space_lines: usize = spaces.iter().sum();
    if tabs > space_lines {
        return "\t".into();
    }
    if spaces[2] > 0 && spaces[2] * 3 >= spaces[4] / 2 {
        return "  ".into();
    }
    "    ".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(s: &str) -> Document {
        Document::from_str(s, Language::TypeScript)
    }

    #[test]
    fn typing_and_undo_coalesce() {
        let mut d = doc("");
        for c in "hello".chars() {
            d.insert(&c.to_string());
        }
        assert_eq!(d.text(), "hello");
        assert!(d.undo());
        assert_eq!(d.text(), "");
        assert!(d.redo());
        assert_eq!(d.text(), "hello");
        assert_eq!(d.selection, Selection::caret(5));
    }

    #[test]
    fn backspace_coalesces_and_respects_indent_stops() {
        let mut d = doc("abc");
        d.set_cursor(3, false);
        d.backspace();
        d.backspace();
        assert_eq!(d.text(), "a");
        d.undo();
        assert_eq!(d.text(), "abc");

        let mut d = doc("  x\n    y");
        d.indent_unit = "  ".into();
        d.set_cursor(d.pos(1, 4), false);
        d.backspace();
        assert_eq!(d.text(), "  x\n  y");
    }

    #[test]
    fn vertical_movement_keeps_goal_column() {
        let mut d = doc("abcdef\nab\nabcdef");
        d.set_cursor(5, false);
        d.move_vertical(1, false);
        assert_eq!(d.line_col(d.selection.head), (1, 2));
        d.move_vertical(1, false);
        assert_eq!(d.line_col(d.selection.head), (2, 5));
    }

    #[test]
    fn word_movement() {
        let mut d = doc("foo.bar  baz\nqux");
        d.move_word(true, false);
        assert_eq!(d.selection.head, 3);
        d.move_word(true, false);
        assert_eq!(d.selection.head, 4);
        d.move_word(true, false);
        assert_eq!(d.selection.head, 7);
        d.move_word(true, false);
        assert_eq!(d.selection.head, 12);
        d.move_word(false, true);
        assert_eq!(d.selection.range(), 9..12);
    }

    #[test]
    fn newline_auto_indent() {
        let mut d = doc("  if (x) {}");
        d.indent_unit = "  ".into();
        d.set_cursor(10, false);
        d.newline();
        assert_eq!(d.text(), "  if (x) {\n    \n  }");
        assert_eq!(d.line_col(d.selection.head), (1, 4));
    }

    #[test]
    fn indent_outdent_lines() {
        let mut d = doc("a\nb\nc");
        d.indent_unit = "  ".into();
        d.selection = Selection { anchor: 0, head: 3 };
        d.indent();
        assert_eq!(d.text(), "  a\n  b\nc");
        d.outdent();
        assert_eq!(d.text(), "a\nb\nc");
    }

    #[test]
    fn toggle_comment_roundtrip() {
        let mut d = doc("  let a = 1;\n  let b = 2;");
        d.select_all();
        d.toggle_comment();
        assert_eq!(d.text(), "  // let a = 1;\n  // let b = 2;");
        d.select_all();
        d.toggle_comment();
        assert_eq!(d.text(), "  let a = 1;\n  let b = 2;");
    }

    #[test]
    fn smart_home_and_selection_helpers() {
        let mut d = doc("    code here");
        d.move_line_end(false);
        d.move_line_start(false);
        assert_eq!(d.selection.head, 4);
        d.move_line_start(false);
        assert_eq!(d.selection.head, 0);
        d.select_word_at(6);
        assert_eq!(d.selected_text(), "code");
    }

    #[test]
    fn find_is_smart_case() {
        let d = doc("Foo foo FOO");
        assert_eq!(d.find_all("foo").len(), 3);
        assert_eq!(d.find_all("Foo"), vec![0..3]);
    }

    #[test]
    fn crlf_and_files_roundtrip() {
        let dir = std::env::temp_dir().join(format!("jt-doc-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.ts");
        fs::write(&path, "a\r\nb\r\n").unwrap();
        let mut d = Document::open(&path).unwrap();
        assert_eq!(d.text(), "a\nb\n");
        assert_eq!(d.language, Language::TypeScript);
        d.set_cursor(1, false);
        d.insert("!");
        assert!(d.is_dirty());
        d.save().unwrap();
        assert!(!d.is_dirty());
        assert_eq!(fs::read_to_string(&path).unwrap(), "a!\r\nb\r\n");
        assert!(!d.changed_on_disk());

        fs::write(dir.join("bin"), [0u8, 1, 2]).unwrap();
        assert!(Document::open(&dir.join("bin")).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replace_all_is_one_undo_step() {
        let mut d = doc("a\nbb\nccc");
        d.set_cursor(d.pos(2, 2), false);
        d.replace_all_keep_cursor("a\nbb\nccc d\n");
        assert_eq!(d.line_col(d.selection.head), (2, 2));
        assert!(d.undo());
        assert_eq!(d.text(), "a\nbb\nccc");
    }

    #[test]
    fn indent_detection() {
        assert_eq!(detect_indent("a\n  b\n    c\n"), "  ");
        assert_eq!(detect_indent("a\n    b\n        c\n"), "    ");
        assert_eq!(detect_indent("a\n\tb\n"), "\t");
    }
}
