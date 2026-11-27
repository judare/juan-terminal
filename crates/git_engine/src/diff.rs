//! Unified diff parsing, side-by-side alignment and single-hunk patch generation.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    pub text: String,
    /// "\ No newline at end of file" follows this line.
    pub no_newline: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub header: String,
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileDiff {
    /// Raw header lines (diff --git, index, ---, +++), needed to build patches.
    pub header: Vec<String>,
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    pub fn display_path(&self) -> &str {
        self.new_path.as_deref().or(self.old_path.as_deref()).unwrap_or("")
    }

    /// A patch containing only hunk `ix`, suitable for `git apply --cached`.
    pub fn hunk_patch(&self, ix: usize) -> String {
        let mut out = String::new();
        for h in &self.header {
            out.push_str(h);
            out.push('\n');
        }
        let hunk = &self.hunks[ix];
        out.push_str(&hunk.header);
        out.push('\n');
        for l in &hunk.lines {
            out.push(match l.kind {
                DiffLineKind::Context => ' ',
                DiffLineKind::Added => '+',
                DiffLineKind::Removed => '-',
            });
            out.push_str(&l.text);
            out.push('\n');
            if l.no_newline {
                out.push_str("\\ No newline at end of file\n");
            }
        }
        out
    }

    /// A patch for one change block (`lines` range of hunk `hi`, see [`Hunk::change_blocks`])
    /// with up to 3 lines of surrounding context, suitable for `git apply --cached --unidiff-zero`.
    /// Lets full-context diffs (one hunk per file) still stage change by change.
    pub fn block_patch(&self, hi: usize, block: std::ops::Range<usize>) -> String {
        const CTX: usize = 3;
        let hunk = &self.hunks[hi];
        let lines = &hunk.lines;
        let mut s = block.start;
        while s > 0 && block.start - s < CTX && lines[s - 1].kind == DiffLineKind::Context {
            s -= 1;
        }
        let mut e = block.end;
        while e < lines.len() && e - block.end < CTX && lines[e].kind == DiffLineKind::Context {
            e += 1;
        }
        let slice = &lines[s..e];
        let ol = slice.iter().filter(|l| l.old_no.is_some()).count();
        let nl = slice.iter().filter(|l| l.new_no.is_some()).count();
        // Empty ranges name the line *before* the change (0 at the start of the file).
        let os = slice.iter().find_map(|l| l.old_no).unwrap_or_else(|| (hunk.old_start + lines[..s].iter().filter(|l| l.old_no.is_some()).count()).saturating_sub(1));
        let ns = slice.iter().find_map(|l| l.new_no).unwrap_or_else(|| (hunk.new_start + lines[..s].iter().filter(|l| l.new_no.is_some()).count()).saturating_sub(1));
        let mut out = String::new();
        for h in &self.header {
            out.push_str(h);
            out.push('\n');
        }
        out.push_str(&format!("@@ -{os},{ol} +{ns},{nl} @@\n"));
        for l in slice {
            out.push(match l.kind {
                DiffLineKind::Context => ' ',
                DiffLineKind::Added => '+',
                DiffLineKind::Removed => '-',
            });
            out.push_str(&l.text);
            out.push('\n');
            if l.no_newline {
                out.push_str("\\ No newline at end of file\n");
            }
        }
        out
    }

    /// 0-based line (new side) of the first change. A pure deletion maps to the line that
    /// follows it (or the one before, at end of file).
    pub fn first_change_line(&self) -> Option<usize> {
        for h in &self.hunks {
            let Some(i) = h.lines.iter().position(|l| l.kind != DiffLineKind::Context) else { continue };
            let after = h.lines[i..].iter().find_map(|l| l.new_no);
            let before = h.lines[..i].iter().rev().find_map(|l| l.new_no);
            return Some(after.or(before).unwrap_or(h.new_start).saturating_sub(1));
        }
        None
    }

    pub fn stats(&self) -> (usize, usize) {
        let mut add = 0;
        let mut del = 0;
        for h in &self.hunks {
            for l in &h.lines {
                match l.kind {
                    DiffLineKind::Added => add += 1,
                    DiffLineKind::Removed => del += 1,
                    DiffLineKind::Context => {}
                }
            }
        }
        (add, del)
    }
}

fn parse_range(s: &str) -> (usize, usize) {
    let mut it = s.splitn(2, ',');
    let start = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let len = it.next().and_then(|x| x.parse().ok()).unwrap_or(1);
    (start, len)
}

fn strip_prefix_path(p: &str) -> Option<String> {
    if p == "/dev/null" {
        return None;
    }
    Some(p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p).to_string())
}

pub fn parse_diff(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut old_no = 0;
    let mut new_no = 0;
    let mut in_hunk = false;
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            files.push(FileDiff { header: vec![line.to_string()], ..Default::default() });
            in_hunk = false;
            continue;
        }
        let Some(file) = files.last_mut() else { continue };
        if let Some(rest) = line.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@ context
            let end = rest.find(" @@").unwrap_or(rest.len());
            let ranges: Vec<&str> = rest[..end].split(' ').collect();
            let (os, ol) = parse_range(ranges.first().map(|s| s.trim_start_matches('-')).unwrap_or("0"));
            let (ns, nl) = parse_range(ranges.get(1).map(|s| s.trim_start_matches('+')).unwrap_or("0"));
            file.hunks.push(Hunk { header: line.to_string(), old_start: os, old_len: ol, new_start: ns, new_len: nl, lines: Vec::new() });
            old_no = os;
            new_no = ns;
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
                file.binary = true;
            }
            if let Some(p) = line.strip_prefix("--- ") {
                file.old_path = strip_prefix_path(p.trim_end_matches('\t'));
            } else if let Some(p) = line.strip_prefix("+++ ") {
                file.new_path = strip_prefix_path(p.trim_end_matches('\t'));
            } else if let Some(p) = line.strip_prefix("rename from ") {
                file.old_path = Some(p.to_string());
            } else if let Some(p) = line.strip_prefix("rename to ") {
                file.new_path = Some(p.to_string());
            }
            file.header.push(line.to_string());
            continue;
        }
        let Some(hunk) = file.hunks.last_mut() else { continue };
        if line.starts_with('\\') {
            if let Some(l) = hunk.lines.last_mut() {
                l.no_newline = true;
            }
            continue;
        }
        let (kind, body) = match line.as_bytes().first() {
            Some(b'+') => (DiffLineKind::Added, &line[1..]),
            Some(b'-') => (DiffLineKind::Removed, &line[1..]),
            Some(b' ') => (DiffLineKind::Context, &line[1..]),
            None => (DiffLineKind::Context, ""),
            _ => continue,
        };
        let (o, n) = match kind {
            DiffLineKind::Context => {
                old_no += 1;
                new_no += 1;
                (Some(old_no - 1), Some(new_no - 1))
            }
            DiffLineKind::Added => {
                new_no += 1;
                (None, Some(new_no - 1))
            }
            DiffLineKind::Removed => {
                old_no += 1;
                (Some(old_no - 1), None)
            }
        };
        hunk.lines.push(DiffLine { kind, old_no: o, new_no: n, text: body.to_string(), no_newline: false });
    }
    files
}

impl Hunk {
    /// Maximal runs of non-context lines (line index ranges): the individual changes.
    pub fn change_blocks(&self) -> Vec<std::ops::Range<usize>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.lines.len() {
            if self.lines[i].kind == DiffLineKind::Context {
                i += 1;
                continue;
            }
            let start = i;
            while i < self.lines.len() && self.lines[i].kind != DiffLineKind::Context {
                i += 1;
            }
            out.push(start..i);
        }
        out
    }
}

/// Pair lines for side-by-side display: removed lines on the left aligned with added lines
/// on the right; context on both sides.
pub fn side_by_side(hunk: &Hunk) -> Vec<(Option<&DiffLine>, Option<&DiffLine>)> {
    side_by_side_indices(hunk).into_iter().map(|(l, r)| (l.map(|i| &hunk.lines[i]), r.map(|i| &hunk.lines[i]))).collect()
}

/// [`side_by_side`] as line indices into `hunk.lines`.
pub fn side_by_side_indices(hunk: &Hunk) -> Vec<(Option<usize>, Option<usize>)> {
    let mut rows = Vec::new();
    let mut i = 0;
    let lines = &hunk.lines;
    while i < lines.len() {
        match lines[i].kind {
            DiffLineKind::Context => {
                rows.push((Some(i), Some(i)));
                i += 1;
            }
            _ => {
                let mut removed = Vec::new();
                let mut added = Vec::new();
                while i < lines.len() && lines[i].kind == DiffLineKind::Removed {
                    removed.push(i);
                    i += 1;
                }
                while i < lines.len() && lines[i].kind == DiffLineKind::Added {
                    added.push(i);
                    i += 1;
                }
                for k in 0..removed.len().max(added.len()) {
                    rows.push((removed.get(k).copied(), added.get(k).copied()));
                }
            }
        }
    }
    rows
}

/// Byte ranges that differ between a removed line and the added line replacing it
/// (word-level LCS), like VS Code's inline change highlight. Empty when the lines share
/// nothing meaningful (the whole line is already marked as changed).
pub fn inline_changes(old: &str, new: &str) -> (Vec<std::ops::Range<usize>>, Vec<std::ops::Range<usize>>) {
    let a = tokenize(old);
    let b = tokenize(new);
    if a.is_empty() || b.is_empty() || a.len() * b.len() > 250_000 {
        return (Vec::new(), Vec::new());
    }
    let (n, m) = (a.len(), b.len());
    // lcs[i][j] = LCS length of a[i..], b[j..]
    let mut lcs = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[at(i, j)] = if old[a[i].clone()] == new[b[j].clone()] { lcs[at(i + 1, j + 1)] + 1 } else { lcs[at(i + 1, j)].max(lcs[at(i, j + 1)]) };
        }
    }
    let mut keep_a = vec![false; n];
    let mut keep_b = vec![false; m];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old[a[i].clone()] == new[b[j].clone()] {
            keep_a[i] = true;
            keep_b[j] = true;
            i += 1;
            j += 1;
        } else if lcs[at(i + 1, j)] >= lcs[at(i, j + 1)] {
            i += 1;
        } else {
            j += 1;
        }
    }
    let meaningful = a.iter().zip(&keep_a).any(|(r, k)| *k && !old[r.clone()].trim().is_empty());
    if !meaningful {
        return (Vec::new(), Vec::new());
    }
    let ranges = |toks: &[std::ops::Range<usize>], keep: &[bool]| {
        let mut out: Vec<std::ops::Range<usize>> = Vec::new();
        for (r, k) in toks.iter().zip(keep) {
            if *k {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.end == r.start => last.end = r.end,
                _ => out.push(r.clone()),
            }
        }
        out
    };
    (ranges(&a, &keep_a), ranges(&b, &keep_b))
}

/// Words (alphanumeric/underscore runs), whitespace runs and single punctuation characters.
fn tokenize(s: &str) -> Vec<std::ops::Range<usize>> {
    let class = |c: char| if c.is_alphanumeric() || c == '_' { 1 } else if c.is_whitespace() { 2 } else { 0 };
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    let mut prev = None;
    for (i, c) in s.char_indices() {
        let k = class(c);
        let end = i + c.len_utf8();
        match out.last_mut() {
            Some(last) if k != 0 && prev == Some(k) => last.end = end,
            _ => out.push(i..end),
        }
        prev = Some(k);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "diff --git a/src/a.ts b/src/a.ts
index 111..222 100644
--- a/src/a.ts
+++ b/src/a.ts
@@ -1,3 +1,4 @@ fn
 one
-two
+TWO
+three
 four
\\ No newline at end of file
";

    #[test]
    fn parses_hunks_and_numbers() {
        let d = parse_diff(SAMPLE);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].display_path(), "src/a.ts");
        let h = &d[0].hunks[0];
        assert_eq!((h.old_start, h.old_len, h.new_start, h.new_len), (1, 3, 1, 4));
        assert_eq!(h.lines.len(), 5);
        assert_eq!(h.lines[1].old_no, Some(2));
        assert_eq!(h.lines[2].new_no, Some(2));
        assert_eq!(h.lines[4].new_no, Some(4));
        assert!(h.lines[4].no_newline);
        assert_eq!(d[0].stats(), (2, 1));
    }

    #[test]
    fn hunk_patch_roundtrips() {
        let d = parse_diff(SAMPLE);
        let p = d[0].hunk_patch(0);
        assert!(p.starts_with("diff --git a/src/a.ts b/src/a.ts\nindex 111..222 100644\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1,3 +1,4 @@ fn\n"));
        assert!(p.ends_with(" four\n\\ No newline at end of file\n"));
    }

    #[test]
    fn side_by_side_aligns_changes() {
        let d = parse_diff(SAMPLE);
        let rows = side_by_side(&d[0].hunks[0]);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[1].0.unwrap().text, "two");
        assert_eq!(rows[1].1.unwrap().text, "TWO");
        assert!(rows[2].0.is_none());
        assert_eq!(rows[2].1.unwrap().text, "three");
    }

    #[test]
    fn inline_changes_marks_changed_words() {
        let (a, b) = inline_changes("let x = foo(1);", "let x = bar(1, 2);");
        assert_eq!(a, vec![8..11]);
        assert_eq!(&"let x = bar(1, 2);"[b[0].clone()], "bar");
        assert_eq!(&"let x = bar(1, 2);"[b[1].clone()], ", 2");
        assert_eq!(inline_changes("abc", "xyz"), (vec![], vec![]));
    }

    #[test]
    fn block_patch_has_own_header() {
        let d = parse_diff(SAMPLE);
        let h = &d[0].hunks[0];
        assert_eq!(h.change_blocks(), vec![1..4]);
        let p = d[0].block_patch(0, 1..4);
        assert!(p.contains("@@ -1,3 +1,4 @@\n one\n-two\n+TWO\n+three\n four\n"), "{p}");
    }
}
