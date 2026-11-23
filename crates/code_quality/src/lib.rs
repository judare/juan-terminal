//! Oxlint / Oxfmt integration through their real CLIs. Detection, on-demand runs, and parsing
//! of `oxlint --format=json`. Nothing runs in the background or continuously.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Oxlint,
    Oxfmt,
}

impl Tool {
    pub fn bin_name(self) -> &'static str {
        match self {
            Tool::Oxlint => "oxlint",
            Tool::Oxfmt => "oxfmt",
        }
    }

    fn config_names(self) -> &'static [&'static str] {
        match self {
            Tool::Oxlint => &[".oxlintrc.json", ".oxlintrc.jsonc", "oxlint.config.ts", "oxlint.config.js", "oxlint.json"],
            Tool::Oxfmt => &[".oxfmtrc.json", ".oxfmtrc.jsonc", "oxfmt.config.ts", "oxfmt.config.js"],
        }
    }
}

/// A located tool: binary, version and the directory to run it from (where its config and
/// `node_modules` live).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located {
    pub bin: PathBuf,
    pub version: Option<String>,
    pub cwd: PathBuf,
    pub config: Option<PathBuf>,
}

/// Find a tool for a repository: `<repo>/node_modules/.bin`, then one level of packages
/// (`<repo>/<pkg>/node_modules/.bin`, common in monorepos), then `$PATH`.
pub fn locate(tool: Tool, repo: &Path) -> Option<Located> {
    let name = tool.bin_name();
    let mut candidates: Vec<(PathBuf, PathBuf)> = vec![(repo.join("node_modules/.bin").join(name), repo.to_path_buf())];
    if let Ok(entries) = std::fs::read_dir(repo) {
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for d in dirs {
            let n = d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if n.starts_with('.') || n == "node_modules" {
                continue;
            }
            candidates.push((d.join("node_modules/.bin").join(name), d));
        }
    }
    let (bin, cwd) = candidates
        .into_iter()
        .find(|(b, _)| b.is_file())
        .or_else(|| which(name).map(|b| (b, repo.to_path_buf())))?;
    let version = version(&bin);
    let config = find_config(tool, &cwd).or_else(|| find_config(tool, repo));
    Some(Located { bin, version, cwd, config })
}

pub fn find_config(tool: Tool, dir: &Path) -> Option<PathBuf> {
    tool.config_names().iter().map(|n| dir.join(n)).find(|p| p.is_file())
}

pub fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

fn version(bin: &Path) -> Option<String> {
    let out = Command::new(bin).arg("--version").stdin(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v = text.trim().trim_start_matches("Version:").trim().to_string();
    (!v.is_empty()).then_some(v)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Advice,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: PathBuf,
    /// 1-based, as reported by oxlint.
    pub line: usize,
    pub column: usize,
    /// Span length in bytes (0 when unknown).
    pub length: usize,
    pub severity: Severity,
    pub message: String,
    pub code: Option<String>,
    pub help: Option<String>,
}

#[derive(Deserialize)]
struct JsonReport {
    diagnostics: Vec<JsonDiagnostic>,
}

#[derive(Deserialize)]
struct JsonDiagnostic {
    message: String,
    code: Option<String>,
    severity: String,
    help: Option<String>,
    filename: String,
    #[serde(default)]
    labels: Vec<JsonLabel>,
}

#[derive(Deserialize)]
struct JsonLabel {
    span: JsonSpan,
}

#[derive(Deserialize)]
struct JsonSpan {
    line: usize,
    column: usize,
    #[serde(default)]
    length: usize,
}

pub fn parse_oxlint_json(json: &str, cwd: &Path) -> Result<Vec<Diagnostic>> {
    let report: JsonReport = serde_json::from_str(json).context("unexpected oxlint output")?;
    let mut out: Vec<Diagnostic> = report
        .diagnostics
        .into_iter()
        .map(|d| {
            let (line, column, length) = d.labels.first().map(|l| (l.span.line, l.span.column, l.span.length)).unwrap_or((1, 1, 0));
            let file = PathBuf::from(&d.filename);
            Diagnostic {
                file: if file.is_absolute() { file } else { cwd.join(file) },
                line,
                column,
                length,
                severity: match d.severity.as_str() {
                    "error" => Severity::Error,
                    "warning" => Severity::Warning,
                    _ => Severity::Advice,
                },
                message: d.message,
                code: d.code,
                help: d.help,
            }
        })
        .collect();
    out.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)).then(a.column.cmp(&b.column)));
    Ok(out)
}

/// Run oxlint on `paths` (or the whole `cwd` when empty).
pub fn lint(tool: &Located, paths: &[PathBuf]) -> Result<Vec<Diagnostic>> {
    let out = Command::new(&tool.bin)
        .current_dir(&tool.cwd)
        .arg("--format=json")
        .args(paths)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run {}", tool.bin.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    // oxlint exits non-zero when it finds errors; only a missing/invalid report is a failure.
    let json_start = stdout.find('{');
    match json_start {
        Some(i) => parse_oxlint_json(&stdout[i..], &tool.cwd),
        None => bail!("{}", String::from_utf8_lossy(&out.stderr).trim()),
    }
}

/// Format files in place with oxfmt. Returns oxfmt's summary line.
pub fn format(tool: &Located, paths: &[PathBuf]) -> Result<String> {
    if paths.is_empty() {
        bail!("No files to format");
    }
    let out = Command::new(&tool.bin)
        .current_dir(&tool.cwd)
        .args(paths)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run {}", tool.bin.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        bail!("{}", if stderr.is_empty() { stdout } else { stderr });
    }
    Ok(stdout.lines().last().unwrap_or("Formatted").to_string())
}

/// Format `text` as if it were the file at `path` (`oxfmt --stdin-filepath`), honoring the
/// project's config. `Ok(None)` when oxfmt doesn't support this file type.
pub fn format_stdin(tool: &Located, path: &Path, text: &str) -> Result<Option<String>> {
    use std::io::Write;
    let mut child = Command::new(&tool.bin)
        .current_dir(&tool.cwd)
        .arg(format!("--stdin-filepath={}", path.display()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run {}", tool.bin.display()))?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    let input = text.to_string();
    // Write on a separate thread so a large file can't deadlock on a full stdout pipe.
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child.wait_with_output()?;
    let _ = writer.join();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        if stderr.contains("Unsupported file type") {
            return Ok(None);
        }
        bail!("{}", if stderr.is_empty() { "oxfmt failed".to_string() } else { stderr });
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// Extensions oxlint lints (per its docs: JS/TS family plus script blocks of Vue/Svelte/Astro).
pub fn lintable(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("js" | "mjs" | "cjs" | "jsx" | "ts" | "mts" | "cts" | "tsx" | "vue" | "svelte" | "astro")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{ "diagnostics": [{"message": "`debugger` statement is not allowed","code": "eslint(no-debugger)","severity": "warning","url": "https://x","help": "Remove the debugger statement","filename": "src/t.vue","labels": [{"span": {"offset": 80,"length": 8,"line": 3,"column": 16}}]},
{"message": "boom","code": "eslint(x)","severity": "error","filename": "/abs/a.ts","labels": [{"label": "here","span": {"offset": 4,"length": 1,"line": 1,"column": 5}}]}],
"number_of_files": 2, "number_of_rules": 116, "threads_count": 8, "start_time": 0.1 }"#;

    #[test]
    fn parses_report() {
        let d = parse_oxlint_json(SAMPLE, Path::new("/repo")).unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].file, PathBuf::from("/abs/a.ts"));
        assert_eq!(d[0].severity, Severity::Error);
        assert_eq!((d[0].line, d[0].column), (1, 5));
        assert_eq!(d[1].file, PathBuf::from("/repo/src/t.vue"));
        assert_eq!(d[1].help.as_deref(), Some("Remove the debugger statement"));
    }

    #[test]
    fn lintable_extensions() {
        assert!(lintable(Path::new("a.vue")));
        assert!(lintable(Path::new("a.tsx")));
        assert!(!lintable(Path::new("a.css")));
    }

    /// Runs the real binaries when `JT_OX_REPO` points at a repo with oxlint/oxfmt installed.
    #[test]
    fn real_tools_when_available() {
        let Some(repo) = std::env::var_os("JT_OX_REPO").map(PathBuf::from) else { return };
        let dir = std::env::temp_dir().join(format!("jt-ox-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.ts");
        std::fs::write(&file, "const   a = 1\ndebugger\n").unwrap();
        let lint_tool = locate(Tool::Oxlint, &repo).expect("oxlint");
        let diags = lint(&lint_tool, &[file.clone()]).unwrap();
        assert!(diags.iter().any(|d| d.code.as_deref() == Some("eslint(no-debugger)") && d.line == 2), "{diags:?}");
        let fmt_tool = locate(Tool::Oxfmt, &repo).expect("oxfmt");
        let formatted = format_stdin(&fmt_tool, &file, "const   b=2\n").unwrap();
        assert_eq!(formatted.as_deref(), Some("const b = 2\n"));
        assert_eq!(format_stdin(&fmt_tool, &dir.join("x.unknownext"), "x").unwrap(), None);
        assert!(format_stdin(&fmt_tool, &file, "const = ").is_err());
        format(&fmt_tool, &[file.clone()]).unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().starts_with("const a = 1"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
