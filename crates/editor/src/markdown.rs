//! Markdown structure for the live (WYSIWYG-style) editor: block classification per line and
//! per-character inline styling. The document text is never changed by rendering; markers are
//! only hidden or replaced on screen, so editing stays plain Markdown.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MdBlock {
    Blank,
    Paragraph,
    Heading(u8),
    /// ``` or ~~~ line.
    Fence,
    /// Line inside a fenced code block.
    Code,
    Quote,
    /// List item; `marker_chars` = chars of indentation + marker + space.
    ListItem { marker_chars: usize, ordered: bool, task: Option<bool> },
    Rule,
    Table,
}

/// Classify every line (fenced code state requires a pass from the top).
pub fn classify<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<MdBlock> {
    let mut out = Vec::new();
    let mut fence: Option<char> = None;
    for line in lines {
        let t = line.trim_start();
        if let Some(f) = fence {
            if t.starts_with(&f.to_string().repeat(3)) {
                fence = None;
                out.push(MdBlock::Fence);
            } else {
                out.push(MdBlock::Code);
            }
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = t.chars().next();
            out.push(MdBlock::Fence);
            continue;
        }
        out.push(classify_line(line));
    }
    out
}

fn classify_line(line: &str) -> MdBlock {
    let t = line.trim_start();
    if t.is_empty() {
        return MdBlock::Blank;
    }
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
        return MdBlock::Heading(hashes as u8);
    }
    let compact: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() >= 3 && (compact.chars().all(|c| c == '-') || compact.chars().all(|c| c == '*') || compact.chars().all(|c| c == '_')) {
        return MdBlock::Rule;
    }
    if t.starts_with('>') {
        return MdBlock::Quote;
    }
    if t.starts_with('|') {
        return MdBlock::Table;
    }
    let indent = line.chars().count() - t.chars().count();
    let mut chars = t.chars();
    let first = chars.next().unwrap_or(' ');
    if matches!(first, '-' | '*' | '+') && t[1..].starts_with(' ') {
        let rest = &t[2..];
        let task = if rest.starts_with("[ ] ") {
            Some(false)
        } else if rest.starts_with("[x] ") || rest.starts_with("[X] ") {
            Some(true)
        } else {
            None
        };
        return MdBlock::ListItem { marker_chars: indent + 2 + if task.is_some() { 4 } else { 0 }, ordered: false, task };
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && digits < 10 && (t[digits..].starts_with(". ") || t[digits..].starts_with(") ")) {
        return MdBlock::ListItem { marker_chars: indent + digits + 2, ordered: true, task: None };
    }
    MdBlock::Paragraph
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CharStyle {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
    pub link: bool,
    /// Syntax marker (e.g. `**`, `#`, backticks, `](url)`): hidden when not editing the line.
    pub marker: bool,
    /// Shown instead of the char when markers are hidden (bullets, checkboxes).
    pub replace: Option<&'static str>,
}

/// Per-char inline styles for one line, given its block kind.
pub fn line_styles(line: &str, block: MdBlock) -> Vec<CharStyle> {
    let chars: Vec<char> = line.chars().collect();
    let mut styles = vec![CharStyle::default(); chars.len()];
    let mut content_start = 0;
    match block {
        MdBlock::Code | MdBlock::Table => {
            for s in &mut styles {
                s.code = true;
            }
            return styles;
        }
        MdBlock::Fence | MdBlock::Rule => {
            for s in &mut styles {
                s.marker = true;
            }
            return styles;
        }
        MdBlock::Heading(level) => {
            let lead = chars.iter().take_while(|c| c.is_whitespace()).count();
            let n = lead + level as usize + 1;
            for s in styles.iter_mut().take(n) {
                s.marker = true;
            }
            for s in styles.iter_mut().skip(n) {
                s.bold = true;
            }
            content_start = n;
        }
        MdBlock::Quote => {
            let lead = chars.iter().take_while(|c| c.is_whitespace()).count();
            let mut n = lead + 1;
            if chars.get(n) == Some(&' ') {
                n += 1;
            }
            for s in styles.iter_mut().take(n) {
                s.marker = true;
            }
            for s in styles.iter_mut().skip(n) {
                s.italic = true;
            }
            content_start = n;
        }
        MdBlock::ListItem { marker_chars, ordered, task } => {
            let lead = chars.iter().take_while(|c| c.is_whitespace()).count();
            if !ordered {
                styles[lead].replace = Some("•");
                if let Some(done) = task {
                    // "- [ ] " → "☐ " / "☑ "
                    styles[lead].marker = true;
                    styles[lead].replace = None;
                    styles[lead + 1].marker = true;
                    styles[lead + 2].replace = Some(if done { "☑" } else { "☐" });
                    styles[lead + 3].marker = true;
                    styles[lead + 4].marker = true;
                }
            }
            content_start = marker_chars.min(chars.len());
            if task == Some(true) {
                for s in styles.iter_mut().skip(content_start) {
                    s.strike = true;
                }
            }
        }
        _ => {}
    }
    inline(&chars, content_start, &mut styles);
    styles
}

fn inline(chars: &[char], start: usize, styles: &mut [CharStyle]) {
    let n = chars.len();
    let mut i = start;
    while i < n {
        let c = chars[i];
        // Escaped char: hide the backslash.
        if c == '\\' && i + 1 < n && chars[i + 1].is_ascii_punctuation() {
            styles[i].marker = true;
            i += 2;
            continue;
        }
        if c == '`' {
            if let Some(end) = (i + 1..n).find(|&j| chars[j] == '`') {
                styles[i].marker = true;
                styles[end].marker = true;
                for s in &mut styles[i + 1..end] {
                    s.code = true;
                }
                i = end + 1;
                continue;
            }
        }
        if (c == '*' || c == '_' || c == '~') && i + 1 < n && chars[i + 1] == c {
            let pat = [c, c];
            if let Some(end) = find_pair(chars, i + 2, &pat) {
                styles[i].marker = true;
                styles[i + 1].marker = true;
                styles[end].marker = true;
                styles[end + 1].marker = true;
                for s in &mut styles[i + 2..end] {
                    if c == '~' {
                        s.strike = true;
                    } else {
                        s.bold = true;
                    }
                }
                inline(&chars[..end], i + 2, &mut styles[..end]);
                i = end + 2;
                continue;
            }
        }
        if (c == '*' || c == '_') && i + 1 < n && !chars[i + 1].is_whitespace() {
            // Intraword underscores (snake_case) are not emphasis.
            let intraword = c == '_' && i > 0 && chars[i - 1].is_alphanumeric();
            if !intraword {
                if let Some(end) = (i + 1..n).find(|&j| chars[j] == c && !chars[j - 1].is_whitespace() && !(c == '_' && j + 1 < n && chars[j + 1].is_alphanumeric())) {
                    styles[i].marker = true;
                    styles[end].marker = true;
                    for s in &mut styles[i + 1..end] {
                        s.italic = true;
                    }
                    i = end + 1;
                    continue;
                }
            }
        }
        if c == '[' {
            if let Some(close) = (i + 1..n).find(|&j| chars[j] == ']') {
                if close + 1 < n && chars[close + 1] == '(' {
                    if let Some(paren) = (close + 2..n).find(|&j| chars[j] == ')') {
                        styles[i].marker = true;
                        for s in &mut styles[close..=paren] {
                            s.marker = true;
                        }
                        for s in &mut styles[i + 1..close] {
                            s.link = true;
                        }
                        i = paren + 1;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
}

fn find_pair(chars: &[char], from: usize, pat: &[char; 2]) -> Option<usize> {
    (from..chars.len().saturating_sub(1)).find(|&j| chars[j] == pat[0] && chars[j + 1] == pat[1] && j > from)
}

/// Position of the task checkbox char (`[ ]`'s inner char) for toggling, if this is a task item.
pub fn task_toggle_col(line: &str, block: MdBlock) -> Option<usize> {
    match block {
        MdBlock::ListItem { task: Some(_), .. } => Some(line.chars().take_while(|c| c.is_whitespace()).count() + 3),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(line: &str, block: MdBlock) -> String {
        let styles = line_styles(line, block);
        line.chars()
            .zip(styles)
            .filter_map(|(c, s)| match (s.replace, s.marker) {
                (Some(r), _) => Some(r.to_string()),
                (None, true) => None,
                (None, false) => Some(c.to_string()),
            })
            .collect()
    }

    #[test]
    fn classifies_blocks() {
        let text = "# Title\n\nText\n- item\n- [x] done\n1. one\n> quote\n---\n```rust\nlet a = 1;\n```\n| a | b |";
        let b = classify(text.lines());
        assert_eq!(b[0], MdBlock::Heading(1));
        assert_eq!(b[1], MdBlock::Blank);
        assert_eq!(b[2], MdBlock::Paragraph);
        assert_eq!(b[3], MdBlock::ListItem { marker_chars: 2, ordered: false, task: None });
        assert_eq!(b[4], MdBlock::ListItem { marker_chars: 6, ordered: false, task: Some(true) });
        assert_eq!(b[5], MdBlock::ListItem { marker_chars: 3, ordered: true, task: None });
        assert_eq!(b[6], MdBlock::Quote);
        assert_eq!(b[7], MdBlock::Rule);
        assert_eq!(&b[8..11], &[MdBlock::Fence, MdBlock::Code, MdBlock::Fence]);
        assert_eq!(b[11], MdBlock::Table);
    }

    #[test]
    fn hides_markers() {
        assert_eq!(shown("## Hello **world**", MdBlock::Heading(2)), "Hello world");
        assert_eq!(shown("a `code` and *it* and [link](http://x)", MdBlock::Paragraph), "a code and it and link");
        assert_eq!(shown("- item", classify_line("- item")), "• item");
        assert_eq!(shown("- [ ] todo", classify_line("- [ ] todo")), "☐ todo");
        assert_eq!(shown("snake_case_name stays", MdBlock::Paragraph), "snake_case_name stays");
        assert_eq!(shown("~~gone~~", MdBlock::Paragraph), "gone");
    }

    #[test]
    fn styles_are_applied() {
        let s = line_styles("**b** *i*", MdBlock::Paragraph);
        assert!(s[2].bold && !s[2].marker);
        assert!(s[7].italic);
        let s = line_styles("let x", MdBlock::Code);
        assert!(s.iter().all(|c| c.code && !c.marker));
    }

    #[test]
    fn task_toggle_position() {
        let line = "  - [ ] x";
        assert_eq!(task_toggle_col(line, classify_line(line)), Some(5));
        assert_eq!(line.chars().nth(5), Some(' '));
    }
}

// ---- tables -------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    None,
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    /// Row 0 is the header.
    pub rows: Vec<Vec<String>>,
    pub aligns: Vec<Align>,
}

/// Cell spans of a table row as char-column ranges (content trimmed).
pub fn row_cells(line: &str) -> Vec<std::ops::Range<usize>> {
    let chars: Vec<char> = line.chars().collect();
    let pipes: Vec<usize> = chars
        .iter()
        .enumerate()
        .filter(|&(i, c)| *c == '|' && (i == 0 || chars[i - 1] != '\\'))
        .map(|(i, _)| i)
        .collect();
    let mut bounds = pipes.clone();
    let first_non_ws = chars.iter().position(|c| !c.is_whitespace()).unwrap_or(0);
    if pipes.first() != Some(&first_non_ws) {
        bounds.insert(0, first_non_ws.saturating_sub(1).max(0));
    }
    let last_non_ws = chars.iter().rposition(|c| !c.is_whitespace()).unwrap_or(0);
    if pipes.last() != Some(&last_non_ws) {
        bounds.push(chars.len());
    }
    bounds
        .windows(2)
        .map(|w| {
            let (mut a, mut b) = (w[0] + 1, w[1]);
            if w[0] == 0 && chars.first() != Some(&'|') {
                a = 0;
            }
            while a < b && chars[a].is_whitespace() {
                a += 1;
            }
            while b > a && chars[b - 1].is_whitespace() {
                b -= 1;
            }
            a..b
        })
        .collect()
}

pub fn is_separator_row(line: &str) -> bool {
    let t = line.trim();
    t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

pub fn parse_table(lines: &[&str]) -> Table {
    let mut rows = Vec::new();
    let mut aligns = Vec::new();
    for line in lines {
        let chars: Vec<char> = line.chars().collect();
        let cells: Vec<String> = row_cells(line).into_iter().map(|r| chars[r].iter().collect()).collect();
        if is_separator_row(line) {
            aligns = cells
                .iter()
                .map(|c| match (c.starts_with(':'), c.ends_with(':')) {
                    (true, true) => Align::Center,
                    (true, false) => Align::Left,
                    (false, true) => Align::Right,
                    _ => Align::None,
                })
                .collect();
        } else {
            rows.push(cells);
        }
    }
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(1).max(aligns.len()).max(1);
    for r in &mut rows {
        r.resize(cols, String::new());
    }
    aligns.resize(cols, Align::None);
    Table { rows, aligns }
}

impl Table {
    pub fn cols(&self) -> usize {
        self.aligns.len()
    }

    /// Pretty-printed Markdown (aligned columns).
    pub fn to_markdown(&self) -> Vec<String> {
        let cols = self.cols();
        let width = |s: &str| s.chars().count();
        let widths: Vec<usize> = (0..cols).map(|c| self.rows.iter().map(|r| width(&r[c])).max().unwrap_or(0).max(3)).collect();
        let fmt_row = |r: &Vec<String>| {
            let cells: Vec<String> = (0..cols).map(|c| format!("{}{}", r[c], " ".repeat(widths[c] - width(&r[c])))).collect();
            format!("| {} |", cells.join(" | "))
        };
        let mut out = Vec::new();
        if let Some(h) = self.rows.first() {
            out.push(fmt_row(h));
        }
        let sep: Vec<String> = (0..cols)
            .map(|c| {
                let w = widths[c];
                match self.aligns[c] {
                    Align::None => "-".repeat(w),
                    Align::Left => format!(":{}", "-".repeat(w - 1)),
                    Align::Right => format!("{}:", "-".repeat(w - 1)),
                    Align::Center => format!(":{}:", "-".repeat(w - 2)),
                }
            })
            .collect();
        out.push(format!("| {} |", sep.join(" | ")));
        for r in self.rows.iter().skip(1) {
            out.push(fmt_row(r));
        }
        out
    }

    pub fn insert_row(&mut self, at: usize) {
        let at = at.clamp(1, self.rows.len());
        self.rows.insert(at, vec![String::new(); self.cols()]);
    }

    pub fn insert_col(&mut self, at: usize) {
        let at = at.min(self.cols());
        for (i, r) in self.rows.iter_mut().enumerate() {
            r.insert(at, if i == 0 { format!("Column {}", at + 1) } else { String::new() });
        }
        self.aligns.insert(at, Align::None);
    }

    pub fn delete_row(&mut self, at: usize) {
        if at > 0 && at < self.rows.len() {
            self.rows.remove(at);
        }
    }

    pub fn delete_col(&mut self, at: usize) {
        if self.cols() > 1 && at < self.cols() {
            for r in &mut self.rows {
                r.remove(at);
            }
            self.aligns.remove(at);
        }
    }
}

/// Map a markdown line index within a table (including the separator) to a data row index.
pub fn data_row(line_in_table: usize, separator_at: Option<usize>) -> usize {
    match separator_at {
        Some(s) if line_in_table > s => line_in_table - 1,
        _ => line_in_table,
    }
}

pub const NEW_TABLE: &str = "| Column 1 | Column 2 | Column 3 |\n| -------- | -------- | -------- |\n|          |          |          |\n|          |          |          |\n";

#[cfg(test)]
mod table_tests {
    use super::*;

    #[test]
    fn cells_and_parse() {
        assert_eq!(row_cells("| a | bb |"), vec![2..3, 6..8]);
        assert_eq!(row_cells("|a||c|"), vec![1..2, 3..3, 4..5]);
        let t = parse_table(&["| Name | Age |", "| :--- | --: |", "| Ana | 3 |", "| Bo |"]);
        assert_eq!(t.rows, vec![vec!["Name", "Age"], vec!["Ana", "3"], vec!["Bo", ""]]);
        assert_eq!(t.aligns, vec![Align::Left, Align::Right]);
    }

    #[test]
    fn edit_and_format() {
        let mut t = parse_table(&["| a | b |", "|---|---|", "| 1 | 2 |"]);
        t.insert_col(1);
        t.insert_row(2);
        assert_eq!(t.to_markdown(), vec![
            "| a   | Column 2 | b   |",
            "| --- | -------- | --- |",
            "| 1   |          | 2   |",
            "|     |          |     |",
        ]);
        t.delete_col(1);
        t.delete_row(2);
        t.delete_row(0); // header can't be deleted
        assert_eq!(t.to_markdown(), vec!["| a   | b   |", "| --- | --- |", "| 1   | 2   |"]);
        assert!(NEW_TABLE.lines().all(|l| l.starts_with('|')));
    }
}
