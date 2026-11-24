//! Office Open XML helpers shared by the xlsx / docx / pptx editors: read parts out of the zip
//! package, write it back with only the changed parts replaced (everything else is copied raw,
//! so untouched content keeps full fidelity), and a small tolerant XML tag scanner used to
//! splice elements in place.

pub mod docx;
pub mod pptx;
pub mod xlsx;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::ops::Range;
use std::path::Path;

pub type Res<T> = Result<T, String>;

/// Blank document for a new file with this extension (docx/xlsx/pptx).
pub fn template(path: &Path) -> Option<&'static [u8]> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "docx" => Some(include_bytes!("../../assets/templates/blank.docx")),
        "xlsx" => Some(include_bytes!("../../assets/templates/blank.xlsx")),
        "pptx" => Some(include_bytes!("../../assets/templates/blank.pptx")),
        _ => None,
    }
}

/// Text form of an Office document for diffs/search: Word as Markdown, sheets as TSV, slides
/// as their text.
pub fn to_text(path: &Path) -> Res<String> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "docx" | "docm" => Ok(docx::DocxModel::load(path)?.markdown),
        "pptx" | "pptm" => {
            let m = pptx::PptxModel::load(path)?;
            let mut out = String::new();
            for i in 0..m.len() {
                let Some(s) = m.slide(i) else { continue };
                out.push_str(&format!("## Slide {}\n", i + 1));
                for sh in s.shapes.iter().filter(|s| s.editable.is_some()) {
                    for p in &sh.paras {
                        if !p.text.trim().is_empty() {
                            out.push_str(&format!("{}{}\n", if p.bullet { "- " } else { "" }, p.text.replace('\u{b}', " ")));
                        }
                    }
                }
                for sh in &s.shapes {
                    for r in &sh.table {
                        out.push_str(&format!("| {} |\n", r.join(" | ")));
                    }
                }
                out.push('\n');
            }
            Ok(out)
        }
        _ => {
            use calamine::Reader;
            let mut wb = calamine::open_workbook_auto(path).map_err(err)?;
            let mut out = String::new();
            for name in wb.sheet_names().to_vec() {
                out.push_str(&format!("## {name}\n"));
                let mut cells: std::collections::BTreeMap<(u32, u32), String> = std::collections::BTreeMap::new();
                if let Ok(range) = wb.worksheet_range(&name) {
                    let (r0, c0) = range.start().unwrap_or((0, 0));
                    for (r, c, d) in range.used_cells() {
                        cells.insert((r0 + r as u32, c0 + c as u32), d.to_string());
                    }
                }
                if let Ok(range) = wb.worksheet_formula(&name) {
                    let (r0, c0) = range.start().unwrap_or((0, 0));
                    for (r, c, f) in range.used_cells().filter(|(_, _, f)| !f.is_empty()) {
                        cells.insert((r0 + r as u32, c0 + c as u32), format!("={f}"));
                    }
                }
                let mut row: Option<u32> = None;
                let mut line: Vec<String> = Vec::new();
                for ((r, c), v) in cells {
                    if row != Some(r) {
                        if let Some(pr) = row {
                            out.push_str(&format!("{}\t{}\n", pr + 1, line.join("\t")));
                        }
                        row = Some(r);
                        line.clear();
                    }
                    while line.len() < c as usize {
                        line.push(String::new());
                    }
                    line.push(v);
                }
                if let Some(pr) = row {
                    out.push_str(&format!("{}\t{}\n", pr + 1, line.join("\t")));
                }
                out.push('\n');
            }
            Ok(out)
        }
    }
}

/// Write the git attributes file that routes Office files through our textconv, keeping the
/// user's global attributes. Returns its path.
pub fn install_git_textconv() -> Option<std::path::PathBuf> {
    let home = std::path::PathBuf::from(std::env::var_os("HOME")?);
    let dir = home.join("Library/Application Support/juan-terminal");
    std::fs::create_dir_all(&dir).ok()?;
    let user = std::process::Command::new("git").args(["config", "--global", "core.attributesFile"]).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty());
    let user = user.map(|u| if let Some(r) = u.strip_prefix("~/") { home.join(r) } else { u.into() }).unwrap_or_else(|| home.join(".config/git/attributes"));
    let mut text = std::fs::read_to_string(user).unwrap_or_default();
    text.push_str("\n# juan-terminal: Office documents diff as text\n");
    for e in ["docx", "docm", "xlsx", "xlsm", "xls", "ods", "pptx", "pptm"] {
        text.push_str(&format!("*.{e} diff=jtoffice\n*.{} diff=jtoffice\n", e.to_ascii_uppercase()));
    }
    let path = dir.join("gitattributes");
    std::fs::write(&path, text).ok()?;
    Some(path)
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Read one part of the package (`None` when missing).
pub fn read_part(path: &Path, name: &str) -> Res<Option<Vec<u8>>> {
    let f = std::fs::File::open(path).map_err(err)?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(f)).map_err(err)?;
    let Ok(mut entry) = zip.by_name(name) else { return Ok(None) };
    let mut out = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut out).map_err(err)?;
    Ok(Some(out))
}

/// Read every part whose name passes `want` (e.g. all slides), in one pass over the zip.
pub fn read_parts(path: &Path, want: impl Fn(&str) -> bool) -> Res<HashMap<String, Vec<u8>>> {
    let f = std::fs::File::open(path).map_err(err)?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(f)).map_err(err)?;
    let mut out = HashMap::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(err)?;
        let name = entry.name().map_err(err)?.to_string();
        if want(&name) {
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut buf).map_err(err)?;
            out.insert(name, buf);
        }
    }
    Ok(out)
}

pub fn read_text(path: &Path, name: &str) -> Res<Option<String>> {
    Ok(read_part(path, name)?.map(|b| String::from_utf8_lossy(&b).into_owned()))
}

/// Rewrite the package: `Some(bytes)` replaces/adds a part, `None` removes it. Untouched parts
/// are copied without recompressing. Written to a temp file and renamed over the original.
pub fn write_package(path: &Path, changes: &HashMap<String, Option<Vec<u8>>>) -> Res<()> {
    let f = std::fs::File::open(path).map_err(err)?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(f)).map_err(err)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.jt-save"));
    let result = (|| -> Res<()> {
        let out = std::fs::File::create(&tmp).map_err(err)?;
        let mut w = zip::ZipWriter::new(std::io::BufWriter::new(out));
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let mut done = std::collections::HashSet::new();
        for i in 0..zip.len() {
            let entry_name = zip.by_index_raw(i).map_err(err)?.name().map_err(err)?.to_string();
            match changes.get(&entry_name) {
                Some(Some(bytes)) => {
                    w.start_file(entry_name.as_str(), opts).map_err(err)?;
                    w.write_all(bytes).map_err(err)?;
                }
                Some(None) => {}
                None => w.raw_copy_file(zip.by_index_raw(i).map_err(err)?).map_err(err)?,
            }
            done.insert(entry_name);
        }
        let mut added: Vec<_> = changes.iter().filter(|(k, v)| v.is_some() && !done.contains(*k)).collect();
        added.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in added {
            w.start_file(k.as_str(), opts).map_err(err)?;
            w.write_all(v.as_ref().unwrap()).map_err(err)?;
        }
        let mut file = w.finish().map_err(err)?.into_inner().map_err(err)?;
        file.flush().map_err(err)?;
        file.sync_all().map_err(err)?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).map_err(err)
}

// ---- XML scanning -----------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TagKind {
    Open,
    Close,
    Empty,
}

#[derive(Clone, Copy, Debug)]
pub struct Tag<'a> {
    pub start: usize,
    pub end: usize,
    pub name: &'a str,
    pub kind: TagKind,
}

impl<'a> Tag<'a> {
    pub fn src(&self, xml: &'a str) -> &'a str {
        &xml[self.start..self.end]
    }
}

/// All element tags in document order (comments, PIs, CDATA and doctypes are skipped).
pub fn tags(xml: &str) -> Vec<Tag<'_>> {
    let b = xml.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = memchr(b'<', &b[i..]) {
        let s = i + off;
        if xml[s..].starts_with("<!--") {
            i = xml[s..].find("-->").map(|e| s + e + 3).unwrap_or(b.len());
            continue;
        }
        if xml[s..].starts_with("<![CDATA[") {
            i = xml[s..].find("]]>").map(|e| s + e + 3).unwrap_or(b.len());
            continue;
        }
        if xml[s..].starts_with("<?") || xml[s..].starts_with("<!") {
            i = xml[s..].find('>').map(|e| s + e + 1).unwrap_or(b.len());
            continue;
        }
        // Find the closing '>' outside quotes.
        let mut j = s + 1;
        let mut quote = 0u8;
        while j < b.len() {
            let c = b[j];
            if quote != 0 {
                if c == quote {
                    quote = 0;
                }
            } else if c == b'"' || c == b'\'' {
                quote = c;
            } else if c == b'>' {
                break;
            }
            j += 1;
        }
        let e = (j + 1).min(b.len());
        let close = b.get(s + 1) == Some(&b'/');
        let name_start = if close { s + 2 } else { s + 1 };
        let name_end = xml[name_start..e].find(|c: char| c.is_whitespace() || c == '>' || c == '/').map(|x| name_start + x).unwrap_or(e);
        let kind = if close {
            TagKind::Close
        } else if e >= 2 && b[e - 2] == b'/' {
            TagKind::Empty
        } else {
            TagKind::Open
        };
        out.push(Tag { start: s, end: e, name: &xml[name_start..name_end], kind });
        i = e;
    }
    out
}

fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&c| c == needle)
}

/// Outermost occurrences of element `name` (full ranges, including the tags).
pub fn elements(xml: &str, name: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for t in tags(xml) {
        if t.name != name {
            continue;
        }
        match t.kind {
            TagKind::Empty if depth == 0 => out.push(t.start..t.end),
            TagKind::Empty => {}
            TagKind::Open => {
                if depth == 0 {
                    start = t.start;
                }
                depth += 1;
            }
            TagKind::Close => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    out.push(start..t.end);
                }
            }
        }
    }
    out
}

/// First outermost occurrence of `name`.
pub fn element(xml: &str, name: &str) -> Option<Range<usize>> {
    elements(xml, name).into_iter().next()
}

/// Direct children of the element spanning `range` (whose first tag is its start tag):
/// (name, full range) for each child element, in order.
pub fn children<'a>(xml: &'a str, range: Range<usize>) -> Vec<(&'a str, Range<usize>)> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur: Option<(&str, usize)> = None;
    for t in tags(&xml[range.clone()]) {
        let (s, e) = (t.start + range.start, t.end + range.start);
        match t.kind {
            TagKind::Open => {
                depth += 1;
                if depth == 2 {
                    cur = Some((t.name, s));
                }
            }
            TagKind::Close => {
                if depth == 2 {
                    if let Some((n, st)) = cur.take() {
                        out.push((n, st..e));
                    }
                }
                depth -= 1;
            }
            TagKind::Empty => {
                if depth == 1 {
                    out.push((t.name, s..e));
                }
            }
        }
    }
    out
}

/// Inner content range of the element at `range` (empty at the end for self-closing ones).
pub fn inner(xml: &str, range: Range<usize>) -> Range<usize> {
    let open_end = xml[range.clone()].find('>').map(|i| range.start + i + 1).unwrap_or(range.end);
    if xml[..open_end].ends_with("/>") {
        return open_end..open_end;
    }
    let close_start = xml[..range.end].rfind("</").filter(|&i| i >= open_end).unwrap_or(range.end);
    open_end..close_start
}

/// Value of attribute `name` in a start tag.
pub fn attr(tag: &str, name: &str) -> Option<String> {
    let b = tag.as_bytes();
    let mut i = 0;
    while let Some(off) = tag[i..].find(name) {
        let p = i + off;
        let before_ok = p > 0 && b[p - 1].is_ascii_whitespace();
        let rest = &tag[p + name.len()..];
        let rest_trim = rest.trim_start();
        if before_ok && rest_trim.starts_with('=') {
            let v = rest_trim[1..].trim_start();
            let q = v.chars().next()?;
            if q == '"' || q == '\'' {
                let end = v[1..].find(q)?;
                return Some(unescape(&v[1..1 + end]));
            }
        }
        i = p + name.len();
    }
    None
}

/// Start tag (`<name ...>` or `<name .../>`) of the element starting at `pos`.
pub fn start_tag(xml: &str, pos: usize) -> &str {
    let end = xml[pos..].find('>').map(|e| pos + e + 1).unwrap_or(xml.len());
    &xml[pos..end]
}

/// Set (or add) an attribute on a start tag.
pub fn set_attr(tag: &str, name: &str, value: &str) -> String {
    let v = escape(value);
    if attr(tag, name).is_some() {
        // Replace the existing value.
        let mut i = 0;
        while let Some(off) = tag[i..].find(name) {
            let p = i + off;
            if p > 0 && tag.as_bytes()[p - 1].is_ascii_whitespace() {
                let rest = &tag[p + name.len()..];
                let eq = rest.find('=').unwrap_or(0);
                let after = &rest[eq + 1..];
                let ws = after.len() - after.trim_start().len();
                let q = after.trim_start().chars().next().unwrap_or('"');
                let vstart = p + name.len() + eq + 1 + ws + 1;
                if let Some(vlen) = tag[vstart..].find(q) {
                    return format!("{}{}{}", &tag[..vstart], v, &tag[vstart + vlen..]);
                }
            }
            i = p + name.len();
        }
        tag.to_string()
    } else {
        let cut = if tag.ends_with("/>") { tag.len() - 2 } else { tag.len() - 1 };
        format!("{} {}=\"{}\"{}", &tag[..cut], name, v, &tag[cut..])
    }
}

pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            // Control characters are invalid in XML 1.0.
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' && c != '\r' => {}
            c => o.push(c),
        }
    }
    o
}

pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut o = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        o.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest.find(';').filter(|&j| j < 12) else {
            o.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => {
                o.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                o.push('&');
                rest = &rest[1..];
            }
        }
    }
    o.push_str(rest);
    o
}

/// Text of every `<tname>` element inside `xml` (e.g. `w:t`, `a:t`), concatenated.
pub fn text_of(xml: &str, tname: &str) -> String {
    let mut out = String::new();
    for r in elements(xml, tname) {
        let i = inner(xml, r);
        out.push_str(&unescape(&xml[i]));
    }
    out
}

/// Resolve a relationship target relative to the part that owns the .rels file.
pub fn resolve_target(owner_part: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut parts: Vec<&str> = owner_part.split('/').collect();
    parts.pop();
    for seg in target.split('/') {
        match seg {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// `.rels` path for a part: `xl/workbook.xml` → `xl/_rels/workbook.xml.rels`.
pub fn rels_path(part: &str) -> String {
    match part.rfind('/') {
        Some(i) => format!("{}/_rels/{}.rels", &part[..i], &part[i + 1..]),
        None => format!("_rels/{part}.rels"),
    }
}

/// Relationships of a .rels file: (Id, Type, Target, external).
pub fn parse_rels(xml: &str) -> Vec<(String, String, String, bool)> {
    tags(xml)
        .iter()
        .filter(|t| t.name == "Relationship" && t.kind != TagKind::Close)
        .map(|t| {
            let s = t.src(xml);
            (
                attr(s, "Id").unwrap_or_default(),
                attr(s, "Type").unwrap_or_default(),
                attr(s, "Target").unwrap_or_default(),
                attr(s, "TargetMode").is_some_and(|m| m == "External"),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Edits real files made by other tools: `JT_OFFICE_FIXTURES=dir cargo test -- --ignored real_files`.
    #[test]
    #[ignore]
    fn real_files() {
        let dir = std::path::PathBuf::from(std::env::var("JT_OFFICE_FIXTURES").unwrap());
        let mut e = std::collections::BTreeMap::new();
        let mut cells = std::collections::BTreeMap::new();
        cells.insert((1, 1), "5".to_string());
        cells.insert((0, 4), "Nota".to_string());
        cells.insert((5, 0), "=D4*2".to_string());
        e.insert("Ventas".to_string(), cells);
        xlsx::save(&dir.join("t.xlsx"), &e).unwrap();

        let mut m = docx::DocxModel::load(&dir.join("t.docx")).unwrap();
        println!("DOCX MD:\n{}", m.markdown);
        let md = m.markdown.replace("negrita", "*cursiva* nueva").replace("- dos", "- dos\n- tres").replace("| 1 | 2 |", "| 1 | 22 |") + "\n## Nuevo título\n\nPárrafo [link](https://example.com)\n";
        m.save(&md).unwrap();
        println!("DOCX MD AFTER:\n{}", m.markdown);

        let mut p = pptx::PptxModel::load(&dir.join("t.pptx")).unwrap();
        let s = p.slide(0).unwrap();
        println!("SLIDE0 {} shapes, title {:?}", s.shapes.len(), s.title);
        for sh in &s.shapes {
            println!("  {:?} {:?} {:?}", sh.rect, sh.editable, sh.paras.iter().map(|p| (p.text.clone(), p.size)).collect::<Vec<_>>());
        }
        let sp = s.shapes.iter().find(|s| s.paras.first().is_some_and(|p| p.text == "Hola")).unwrap().editable.unwrap();
        p.set_text(0, sp, "Hola editado\nsegunda").unwrap();
        p.duplicate_slide(1).unwrap();
        p.move_slide(2, false);
        p.delete_slide(0);
        p.save().unwrap();
    }

    #[test]
    fn scan_and_attrs() {
        let x = r#"<a><b x="1>2"/><c>t<c>in</c></c><!-- <b/> --><d y='q'>z</d></a>"#;
        assert_eq!(elements(x, "b").len(), 1);
        let c = elements(x, "c");
        assert_eq!(c.len(), 1);
        assert_eq!(&x[c[0].clone()], "<c>t<c>in</c></c>");
        let kids: Vec<_> = children(x, 0..x.len()).into_iter().map(|(n, _)| n).collect();
        assert_eq!(kids, vec!["b", "c", "d"]);
        assert_eq!(attr(r#"<b x="1>2"/>"#, "x").as_deref(), Some("1>2"));
        assert_eq!(attr(r#"<d y='q'>"#, "y").as_deref(), Some("q"));
        assert_eq!(set_attr(r#"<d y='q'>"#, "y", "r"), "<d y='r'>");
        assert_eq!(set_attr(r#"<d/>"#, "k", "v"), r#"<d k="v"/>"#);
        let d = element(x, "d").unwrap();
        assert_eq!(&x[inner(x, d)], "z");
        assert_eq!(unescape("a&amp;b&#65;&#x42;&bogus"), "a&bAB&bogus");
        assert_eq!(resolve_target("xl/workbook.xml", "worksheets/sheet1.xml"), "xl/worksheets/sheet1.xml");
        assert_eq!(resolve_target("ppt/slides/slide1.xml", "../media/image1.png"), "ppt/media/image1.png");
        assert_eq!(rels_path("ppt/slides/slide1.xml"), "ppt/slides/_rels/slide1.xml.rels");
    }
}
