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

/// Pair lines for side-by-side display: removed lines on the left aligned with added lines
/// on the right; context on both sides.
pub fn side_by_side(hunk: &Hunk) -> Vec<(Option<&DiffLine>, Option<&DiffLine>)> {
    let mut rows = Vec::new();
    let mut i = 0;
    let lines = &hunk.lines;
    while i < lines.len() {
        match lines[i].kind {
            DiffLineKind::Context => {
                rows.push((Some(&lines[i]), Some(&lines[i])));
                i += 1;
            }
            _ => {
                let mut removed = Vec::new();
                let mut added = Vec::new();
                while i < lines.len() && lines[i].kind == DiffLineKind::Removed {
                    removed.push(&lines[i]);
                    i += 1;
                }
                while i < lines.len() && lines[i].kind == DiffLineKind::Added {
                    added.push(&lines[i]);
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
}
