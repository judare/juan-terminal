//! Project file index (via `git ls-files`, honoring .gitignore), fuzzy file matching and
//! full-text search. Pure functions, run on background threads by the UI.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A file in the project index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedFile {
    pub path: PathBuf,
    /// Display path: "<root label>/<relative path>".
    pub display: String,
}

/// A searchable root: a repository (uses git) or a plain folder (bounded walk).
#[derive(Clone, Debug)]
pub struct Root {
    pub label: String,
    pub path: PathBuf,
    pub is_git: bool,
    /// Sub-paths to skip (e.g. repositories nested inside a plain project root).
    pub exclude: Vec<PathBuf>,
}

const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", "dist", "build", ".next", ".nuxt", "coverage", ".turbo", ".cache", ".venv", "vendor"];
const MAX_FILES: usize = 200_000;

/// List files of all roots. Git roots use `git ls-files` (tracked + untracked, not ignored).
pub fn list_files(roots: &[Root]) -> Vec<IndexedFile> {
    let mut out = Vec::new();
    for root in roots {
        let before = out.len();
        if root.is_git && git_ls_files(root, &mut out).is_ok() {
            continue;
        }
        out.truncate(before);
        walk(root, &root.path, 0, &mut out);
    }
    out.truncate(MAX_FILES);
    out
}

fn git_ls_files(root: &Root, out: &mut Vec<IndexedFile>) -> anyhow::Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(&root.path)
        .args(["ls-files", "--cached", "--others", "--exclude-standard", "-z"])
        .output()?;
    anyhow::ensure!(output.status.success(), "git ls-files failed");
    for rel in output.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let rel = String::from_utf8_lossy(rel);
        let path = root.path.join(rel.as_ref());
        // Deleted-but-tracked files still appear in --cached.
        if !path.is_file() {
            continue;
        }
        out.push(IndexedFile { display: format!("{}/{}", root.label, rel), path });
        if out.len() >= MAX_FILES {
            break;
        }
    }
    Ok(())
}

fn walk(root: &Root, dir: &Path, depth: usize, out: &mut Vec<IndexedFile>) {
    if depth > 12 || out.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if root.exclude.iter().any(|x| x == &path) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if SKIP_DIRS.contains(&name.as_ref()) || name.starts_with('.') {
                continue;
            }
            walk(root, &path, depth + 1, out);
        } else if ft.is_file() {
            if name == ".DS_Store" {
                continue;
            }
            let rel = path.strip_prefix(&root.path).unwrap_or(&path).to_string_lossy().into_owned();
            out.push(IndexedFile { display: format!("{}/{}", root.label, rel), path });
        }
    }
}

/// Fuzzy match `query` against `candidate`. Returns a score (higher is better) and the
/// matched char indices, or None. Case-insensitive; rewards matches in the file name,
/// consecutive runs and segment starts.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<(i64, Vec<usize>)> {
    let q: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).flat_map(|c| c.to_lowercase()).collect();
    if q.is_empty() {
        return Some((0, Vec::new()));
    }
    let chars: Vec<char> = candidate.chars().collect();
    let lower: Vec<char> = chars.iter().map(|c| c.to_ascii_lowercase()).collect();
    let name_start = candidate.rfind('/').map(|i| candidate[..=i].chars().count()).unwrap_or(0);

    // Greedy from the end for the file name, then fall back to a left-to-right scan.
    let mut positions = Vec::with_capacity(q.len());
    let mut qi = q.len();
    let mut i = lower.len();
    while qi > 0 && i > 0 {
        i -= 1;
        if lower[i] == q[qi - 1] {
            positions.push(i);
            qi -= 1;
        }
    }
    if qi > 0 {
        return None;
    }
    positions.reverse();

    let mut score: i64 = 0;
    let mut prev: Option<usize> = None;
    for &p in &positions {
        score += 10;
        if p >= name_start {
            score += 12;
        }
        if p == name_start || p == 0 || matches!(chars.get(p.wrapping_sub(1)), Some('/' | '_' | '-' | '.' | ' ')) {
            score += 15;
        }
        if let Some(pr) = prev {
            if p == pr + 1 {
                score += 18;
            } else {
                score -= (p - pr) as i64 / 4;
            }
        }
        prev = Some(p);
    }
    // Exact file name prefix is a strong signal.
    let name: String = lower[name_start..].iter().collect();
    let qs: String = q.iter().collect();
    if name.starts_with(&qs) {
        score += 60;
    }
    score -= (chars.len() as i64) / 8;
    Some((score, positions))
}

/// Best `limit` matches for a query, sorted by score.
pub fn fuzzy_filter(query: &str, files: &[IndexedFile], limit: usize) -> Vec<(usize, Vec<usize>)> {
    let mut scored: Vec<(i64, usize, Vec<usize>)> = files
        .iter()
        .enumerate()
        .filter_map(|(i, f)| fuzzy_match(query, &f.display).map(|(s, pos)| (s, i, pos)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| files[a.1].display.len().cmp(&files[b.1].display.len())));
    scored.truncate(limit);
    scored.into_iter().map(|(_, i, p)| (i, p)).collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub file: usize,
    /// 0-based line.
    pub line: usize,
    /// Char column of the match start.
    pub column: usize,
    pub line_text: String,
    /// Byte range of the match within `line_text`.
    pub range: std::ops::Range<usize>,
    /// Byte range of the match within the full source line.
    pub byte_range: std::ops::Range<usize>,
}

pub struct SearchResults {
    pub matches: Vec<SearchMatch>,
    pub files_searched: usize,
    pub truncated: bool,
}

const MAX_SEARCH_FILE_BYTES: u64 = 1024 * 1024;

/// Search options (VS Code: Aa, whole word, .*).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

/// Compile the query into a line regex honoring the options.
pub fn build_regex(query: &str, opts: SearchOptions) -> anyhow::Result<regex::Regex> {
    let mut pat = if opts.regex { query.to_string() } else { regex::escape(query) };
    if opts.whole_word {
        pat = format!(r"\b(?:{pat})\b");
    }
    let re = regex::RegexBuilder::new(&pat).case_insensitive(!opts.case_sensitive).multi_line(false).size_limit(1 << 22).build()?;
    Ok(re)
}

/// Search across indexed files. Cancellable via `cancel`.
pub fn search(query: &str, opts: SearchOptions, files: &[IndexedFile], max_matches: usize, cancel: &Arc<AtomicBool>) -> anyhow::Result<SearchResults> {
    let mut results = SearchResults { matches: Vec::new(), files_searched: 0, truncated: false };
    if query.is_empty() {
        return Ok(results);
    }
    let re = build_regex(query, opts)?;
    for (fi, file) in files.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Some(text) = read_text(&file.path) else { continue };
        results.files_searched += 1;
        for (li, line) in text.lines().enumerate() {
            for m in re.find_iter(line) {
                if m.start() == m.end() {
                    continue; // empty regex matches are useless in a result list
                }
                let (start, end) = (m.start(), m.end());
                let column = line[..start].chars().count();
                let (line_text, range) = if line.len() > 400 {
                    // Show a window around the match for very long lines.
                    let a = floor_boundary(line, start.saturating_sub(80));
                    let b = floor_boundary(line, (end + 200).min(line.len()));
                    (line[a..b].to_string(), start - a..end - a)
                } else {
                    (line.to_string(), start..end)
                };
                results.matches.push(SearchMatch { file: fi, line: li, column, line_text, range, byte_range: start..end });
                if results.matches.len() >= max_matches {
                    results.truncated = true;
                    return Ok(results);
                }
            }
        }
    }
    Ok(results)
}

fn read_text(path: &std::path::Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    if meta.len() > MAX_SEARCH_FILE_BYTES {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    if bytes.iter().take(4096).any(|b| *b == 0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Replace matches in one file on disk. `only` limits it to a single match (line, byte range
/// within the line) as returned by `search`; it is re-validated against the current content.
/// In regex mode `$1`-style groups expand; otherwise the replacement is literal.
/// Returns the number of replacements written.
pub fn replace_in_file(path: &std::path::Path, query: &str, opts: SearchOptions, replacement: &str, only: Option<(usize, std::ops::Range<usize>)>) -> anyhow::Result<usize> {
    let re = build_regex(query, opts)?;
    let text = read_text(path).ok_or_else(|| anyhow::anyhow!("{} is not a searchable text file", path.display()))?;
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    // Split keeping line terminators so the file round-trips byte for byte.
    for (li, line_with_nl) in text.split_inclusive('\n').enumerate() {
        let (line, nl) = match line_with_nl.strip_suffix("\r\n") {
            Some(l) => (l, "\r\n"),
            None => match line_with_nl.strip_suffix('\n') {
                Some(l) => (l, "\n"),
                None => (line_with_nl, ""),
            },
        };
        let mut new_line = String::with_capacity(line.len());
        let mut last = 0;
        for caps in re.captures_iter(line) {
            let m = caps.get(0).expect("group 0");
            if m.start() == m.end() {
                continue;
            }
            if let Some((ol, r)) = &only {
                if *ol != li || m.range() != *r {
                    continue;
                }
            }
            new_line.push_str(&line[last..m.start()]);
            if opts.regex {
                caps.expand(replacement, &mut new_line);
            } else {
                new_line.push_str(replacement);
            }
            last = m.end();
            count += 1;
        }
        new_line.push_str(&line[last..]);
        out.push_str(&new_line);
        out.push_str(nl);
    }
    if count > 0 {
        let tmp = path.with_extension(format!("{}.jt-tmp", path.extension().and_then(|e| e.to_str()).unwrap_or("")));
        fs::write(&tmp, &out)?;
        if let Ok(meta) = fs::metadata(path) {
            let _ = fs::set_permissions(&tmp, meta.permissions());
        }
        fs::rename(&tmp, path)?;
    }
    Ok(count)
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(names: &[&str]) -> Vec<IndexedFile> {
        names.iter().map(|n| IndexedFile { path: PathBuf::from(n), display: n.to_string() }).collect()
    }

    #[test]
    fn fuzzy_prefers_file_name_matches() {
        let f = files(&["frontend/src/components/UserCard.vue", "frontend/src/utils/user.ts", "backend/src/users/controller.ts", "frontend/README.md"]);
        let r = fuzzy_filter("usercard", &f, 10);
        assert_eq!(f[r[0].0].display, "frontend/src/components/UserCard.vue");
        let r = fuzzy_filter("user.ts", &f, 10);
        assert_eq!(f[r[0].0].display, "frontend/src/utils/user.ts");
        assert!(fuzzy_match("zzz", "abc").is_none());
    }

    #[test]
    fn walks_and_uses_git() {
        let dir = std::env::temp_dir().join(format!("jt-search-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("plain/src")).unwrap();
        fs::create_dir_all(dir.join("plain/node_modules/x")).unwrap();
        fs::write(dir.join("plain/src/a.ts"), "const hello = 1;\nHello world\n").unwrap();
        fs::write(dir.join("plain/node_modules/x/i.js"), "hello").unwrap();

        let repo = dir.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| Command::new("git").arg("-C").arg(&repo).args(args).output().unwrap();
        git(&["init", "-q"]);
        fs::write(repo.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(repo.join("ignored.txt"), "hello").unwrap();
        fs::write(repo.join("main.rs"), "fn hello() {}").unwrap();

        let roots = vec![
            Root { label: "plain".into(), path: dir.join("plain"), is_git: false, exclude: vec![] },
            Root { label: "repo".into(), path: repo.clone(), is_git: true, exclude: vec![] },
        ];
        let mut listed: Vec<String> = list_files(&roots).into_iter().map(|f| f.display).collect();
        listed.sort();
        assert_eq!(listed, vec!["plain/src/a.ts", "repo/.gitignore", "repo/main.rs"]);

        let idx = list_files(&roots);
        let cancel = Arc::new(AtomicBool::new(false));
        let plain = SearchOptions::default();
        let r = search("hello", plain, &idx, 100, &cancel).unwrap();
        assert_eq!(r.matches.len(), 3); // case-insensitive by default
        let cs = SearchOptions { case_sensitive: true, ..plain };
        let r = search("Hello", cs, &idx, 100, &cancel).unwrap();
        assert_eq!(r.matches.len(), 1);
        assert_eq!(r.matches[0].line, 1);
        assert_eq!(&r.matches[0].line_text[r.matches[0].range.clone()], "Hello");
        // Whole word: "hell" is inside "hello", so no match.
        let ww = SearchOptions { whole_word: true, ..plain };
        assert_eq!(search("hell", ww, &idx, 100, &cancel).unwrap().matches.len(), 0);
        // Regex with groups, and an invalid regex is an error (not a panic).
        let rx = SearchOptions { regex: true, ..plain };
        assert_eq!(search(r"fn (\w+)", rx, &idx, 100, &cancel).unwrap().matches.len(), 1);
        assert!(search("(", rx, &idx, 100, &cancel).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replace_all_single_and_regex_groups() {
        let dir = std::env::temp_dir().join(format!("jt-replace-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.txt");
        fs::write(&f, "foo bar foo\r\nFoo\nfoobar foo").unwrap();
        let plain = SearchOptions { whole_word: true, ..Default::default() };
        // Single match: second "foo" on line 0 (bytes 8..11).
        assert_eq!(replace_in_file(&f, "foo", plain, "X", Some((0, 8..11))).unwrap(), 1);
        assert_eq!(fs::read_to_string(&f).unwrap(), "foo bar X\r\nFoo\nfoobar foo");
        // All remaining whole-word, case-insensitive; CRLF preserved; "foobar" untouched.
        assert_eq!(replace_in_file(&f, "foo", plain, "Y", None).unwrap(), 3);
        assert_eq!(fs::read_to_string(&f).unwrap(), "Y bar X\r\nY\nfoobar Y");
        // Regex groups expand.
        let rx = SearchOptions { regex: true, ..Default::default() };
        assert_eq!(replace_in_file(&f, r"(\w+)bar", rx, "${1}_baz", None).unwrap(), 1);
        assert_eq!(fs::read_to_string(&f).unwrap(), "Y bar X\r\nY\nfoo_baz Y");
        fs::remove_dir_all(dir).unwrap();
    }
}
