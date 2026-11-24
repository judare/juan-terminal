//! Save cell edits back into an .xlsx/.xlsm by patching only the touched `<c>` elements of the
//! sheet XML (styles, other cells, charts, etc. stay byte-identical). Formulas are recalculated
//! by Excel/Numbers on open (`fullCalcOnLoad`), and the stale calcChain is dropped.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use super::{Res, attr, children, element, elements, escape, inner, parse_rels, read_text, rels_path, resolve_target, set_attr, start_tag, tags};

pub fn is_editable(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("xlsx") || e.eq_ignore_ascii_case("xlsm"))
}

/// Excel column letters → 0-based index.
fn col_ix(letters: &str) -> usize {
    letters.bytes().fold(0usize, |acc, b| acc * 26 + (b.to_ascii_uppercase() - b'A' + 1) as usize) - 1
}

/// "B12" → (11, 1).
pub fn parse_ref(r: &str) -> Option<(usize, usize)> {
    let split = r.find(|c: char| c.is_ascii_digit())?;
    let (l, n) = r.split_at(split);
    if l.is_empty() || !l.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    Some((n.parse::<usize>().ok()?.checked_sub(1)?, col_ix(l)))
}

pub fn cell_ref(r: usize, c: usize) -> String {
    let mut s = Vec::new();
    let mut c = c;
    loop {
        s.push(b'A' + (c % 26) as u8);
        if c < 26 {
            break;
        }
        c = c / 26 - 1;
    }
    s.reverse();
    format!("{}{}", String::from_utf8(s).unwrap_or_default(), r + 1)
}

/// Sheet name → its part path inside the package.
fn sheet_parts(path: &Path) -> Res<HashMap<String, String>> {
    let wb = read_text(path, "xl/workbook.xml")?.ok_or("xl/workbook.xml missing")?;
    let rels = read_text(path, &rels_path("xl/workbook.xml"))?.unwrap_or_default();
    let rels = parse_rels(&rels);
    let mut out = HashMap::new();
    for t in tags(&wb).iter().filter(|t| t.name == "sheet") {
        let s = t.src(&wb);
        let (Some(name), Some(rid)) = (attr(s, "name"), attr(s, "r:id")) else { continue };
        if let Some((_, _, target, _)) = rels.iter().find(|r| r.0 == rid) {
            out.insert(name, resolve_target("xl/workbook.xml", target));
        }
    }
    Ok(out)
}

/// XML for one cell. `style` is the existing start-tag's `s` attribute (kept).
fn cell_xml(r: usize, c: usize, style: Option<&str>, value: &str) -> Option<String> {
    let rf = cell_ref(r, c);
    let s = style.map(|s| format!(" s=\"{s}\"")).unwrap_or_default();
    let v = value;
    if v.is_empty() {
        return style.map(|_| format!("<c r=\"{rf}\"{s}/>"));
    }
    if let Some(f) = v.strip_prefix('=').filter(|f| !f.trim().is_empty()) {
        return Some(format!("<c r=\"{rf}\"{s}><f>{}</f></c>", escape(f)));
    }
    if v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("false") {
        return Some(format!("<c r=\"{rf}\"{s} t=\"b\"><v>{}</v></c>", if v.eq_ignore_ascii_case("true") { 1 } else { 0 }));
    }
    if is_number(v) {
        return Some(format!("<c r=\"{rf}\"{s}><v>{}</v></c>", v.trim()));
    }
    let space = if v.starts_with(' ') || v.ends_with(' ') || v.contains('\n') { " xml:space=\"preserve\"" } else { "" };
    Some(format!("<c r=\"{rf}\"{s} t=\"inlineStr\"><is><t{space}>{}</t></is></c>", escape(v)))
}

/// Plain numbers only; "007" (ids, zip codes) stays text.
fn is_number(v: &str) -> bool {
    let t = v.trim();
    if t.is_empty() || t != v || t.parse::<f64>().map(|f| !f.is_finite()).unwrap_or(true) {
        return false;
    }
    let digits = t.trim_start_matches('-');
    if digits.len() > 1 && digits.starts_with('0') && !digits.starts_with("0.") {
        return false;
    }
    t.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'e' | b'E' | b'+'))
}

/// Apply `edits` (row, col → new text) to a sheet's XML.
pub fn patch_sheet(xml: &str, edits: &BTreeMap<(usize, usize), String>) -> Res<String> {
    if edits.is_empty() {
        return Ok(xml.to_string());
    }
    let sd = element(xml, "sheetData").ok_or("sheetData missing")?;
    let sd_inner = inner(xml, sd.clone());
    // Self-closing <sheetData/>: expand it.
    let (mut out_head, sd_body, out_tail) = if sd_inner.is_empty() && xml[sd.clone()].ends_with("/>") {
        (format!("{}<sheetData>", &xml[..sd.start]), String::new(), format!("</sheetData>{}", &xml[sd.end..]))
    } else {
        (xml[..sd_inner.start].to_string(), xml[sd_inner.clone()].to_string(), xml[sd_inner.end..].to_string())
    };

    let mut by_row: BTreeMap<usize, BTreeMap<usize, &String>> = BTreeMap::new();
    for ((r, c), v) in edits {
        by_row.entry(*r).or_default().insert(*c, v);
    }

    // Rows: (row index, range in sd_body).
    let wrapped = format!("<x>{sd_body}</x>");
    let off = 3usize;
    let mut rows: Vec<(usize, std::ops::Range<usize>)> = Vec::new();
    let mut next_r = 0usize;
    for (name, range) in children(&wrapped, 0..wrapped.len()) {
        if name != "row" {
            continue;
        }
        let tag = start_tag(&wrapped, range.start);
        let r = attr(tag, "r").and_then(|v| v.parse::<usize>().ok()).map(|v| v - 1).unwrap_or(next_r);
        next_r = r + 1;
        rows.push((r, range.start - off..range.end - off));
    }

    let mut body = String::with_capacity(sd_body.len() + 256);
    let mut pos = 0;
    let mut pending = by_row.into_iter().peekable();
    for (r, range) in &rows {
        // New rows that belong before this one.
        while let Some((er, _)) = pending.peek() {
            if er >= r {
                break;
            }
            let (er, cells) = pending.next().unwrap();
            body.push_str(&sd_body[pos..range.start]);
            pos = range.start;
            body.push_str(&new_row(er, &cells));
        }
        if pending.peek().is_some_and(|(er, _)| er == r) {
            let (_, cells) = pending.next().unwrap();
            body.push_str(&sd_body[pos..range.start]);
            body.push_str(&patch_row(&sd_body[range.clone()], *r, &cells)?);
            pos = range.end;
        }
    }
    body.push_str(&sd_body[pos..]);
    for (er, cells) in pending {
        body.push_str(&new_row(er, &cells));
    }

    out_head.push_str(&body);
    out_head.push_str(&out_tail);
    Ok(update_dimension(&out_head, edits))
}

fn new_row(r: usize, cells: &BTreeMap<usize, &String>) -> String {
    let inner: String = cells.iter().filter_map(|(c, v)| cell_xml(r, *c, None, v)).collect();
    if inner.is_empty() { String::new() } else { format!("<row r=\"{}\">{inner}</row>", r + 1) }
}

fn patch_row(row: &str, r: usize, cells: &BTreeMap<usize, &String>) -> Res<String> {
    let tag = start_tag(row, 0).to_string();
    let (open, content, close) = if tag.ends_with("/>") {
        (format!("{}>", tag.trim_end_matches("/>").trim_end()), String::new(), "</row>".to_string())
    } else {
        let i = inner(row, 0..row.len());
        (row[..i.start].to_string(), row[i.clone()].to_string(), row[i.end..].to_string())
    };
    let wrapped = format!("<x>{content}</x>");
    let mut existing: Vec<(usize, std::ops::Range<usize>)> = Vec::new();
    let mut next_c = 0usize;
    for (name, range) in children(&wrapped, 0..wrapped.len()) {
        if name != "c" {
            continue;
        }
        let t = start_tag(&wrapped, range.start);
        let c = attr(t, "r").and_then(|v| parse_ref(&v)).map(|(_, c)| c).unwrap_or(next_c);
        next_c = c + 1;
        existing.push((c, range.start - 3..range.end - 3));
    }
    let mut out = String::with_capacity(content.len() + 128);
    let mut pos = 0;
    let mut pending = cells.iter().peekable();
    for (c, range) in &existing {
        while let Some((ec, _)) = pending.peek() {
            if *ec >= c {
                break;
            }
            let (ec, v) = pending.next().unwrap();
            out.push_str(&content[pos..range.start]);
            pos = range.start;
            out.push_str(&cell_xml(r, *ec, None, v).unwrap_or_default());
        }
        if pending.peek().is_some_and(|(ec, _)| *ec == c) {
            let (_, v) = pending.next().unwrap();
            let old = &content[range.clone()];
            let ot = start_tag(old, 0);
            if let Some(f) = element(old, "f") {
                let ft = start_tag(old, f.start);
                if attr(ft, "t").as_deref() == Some("shared") && attr(ft, "ref").is_some() {
                    return Err(format!(
                        "{} anchors a shared formula used by other cells; edit it in Excel or change the other cells first",
                        cell_ref(r, *c)
                    ));
                }
                if attr(ft, "t").as_deref() == Some("array") {
                    return Err(format!("{} is part of an array formula", cell_ref(r, *c)));
                }
            }
            out.push_str(&content[pos..range.start]);
            out.push_str(&cell_xml(r, *c, attr(ot, "s").as_deref(), v).unwrap_or_default());
            pos = range.end;
        }
    }
    out.push_str(&content[pos..]);
    for (ec, v) in pending {
        out.push_str(&cell_xml(r, *ec, None, v).unwrap_or_default());
    }
    // Rows with explicit `spans` may not cover new columns: drop the hint (optional attribute).
    let open = if attr(&open, "spans").is_some() { strip_attr(&open, "spans") } else { open };
    Ok(format!("{open}{out}{close}"))
}

fn strip_attr(tag: &str, name: &str) -> String {
    let needle = format!(" {name}=\"");
    if let Some(i) = tag.find(&needle) {
        if let Some(e) = tag[i + needle.len()..].find('"') {
            return format!("{}{}", &tag[..i], &tag[i + needle.len() + e + 1..]);
        }
    }
    tag.to_string()
}

fn update_dimension(xml: &str, edits: &BTreeMap<(usize, usize), String>) -> String {
    let Some(d) = element(xml, "dimension") else { return xml.to_string() };
    let tag = start_tag(xml, d.start);
    let Some(rf) = attr(tag, "ref") else { return xml.to_string() };
    let mut parts = rf.split(':');
    let a = parts.next().and_then(parse_ref).unwrap_or((0, 0));
    let b = parts.next().and_then(parse_ref).unwrap_or(a);
    let (mut r1, mut c1) = (b.0, b.1);
    let (mut r0, mut c0) = (a.0, a.1);
    for ((r, c), v) in edits {
        if v.is_empty() {
            continue;
        }
        r0 = r0.min(*r);
        c0 = c0.min(*c);
        r1 = r1.max(*r);
        c1 = c1.max(*c);
    }
    let new = format!("{}:{}", cell_ref(r0, c0), cell_ref(r1, c1));
    format!("{}{}{}", &xml[..d.start], set_attr(tag, "ref", &new), &xml[d.start + tag.len()..])
}

/// Make Excel recompute formulas on open (cached values of dependents are stale).
fn force_recalc(wb: &str) -> String {
    if let Some(c) = element(wb, "calcPr") {
        let tag = start_tag(wb, c.start);
        return format!("{}{}{}", &wb[..c.start], set_attr(tag, "fullCalcOnLoad", "1"), &wb[c.start + tag.len()..]);
    }
    // CT_Workbook order: … sheets, functionGroups, externalReferences, definedNames, calcPr …
    for after in ["definedNames", "externalReferences", "functionGroups", "sheets"] {
        if let Some(r) = elements(wb, after).last() {
            return format!("{}<calcPr fullCalcOnLoad=\"1\"/>{}", &wb[..r.end], &wb[r.end..]);
        }
    }
    wb.to_string()
}

/// Write all `edits` (sheet name → cells) into the workbook at `path`.
pub fn save(path: &Path, edits: &BTreeMap<String, BTreeMap<(usize, usize), String>>) -> Res<()> {
    let parts = sheet_parts(path)?;
    let mut changes: HashMap<String, Option<Vec<u8>>> = HashMap::new();
    for (sheet, cells) in edits {
        if cells.is_empty() {
            continue;
        }
        let part = parts.get(sheet).ok_or_else(|| format!("sheet {sheet} not found"))?;
        let xml = read_text(path, part)?.ok_or_else(|| format!("{part} missing"))?;
        changes.insert(part.clone(), Some(patch_sheet(&xml, cells)?.into_bytes()));
    }
    if changes.is_empty() {
        return Ok(());
    }
    let wb = read_text(path, "xl/workbook.xml")?.unwrap_or_default();
    changes.insert("xl/workbook.xml".into(), Some(force_recalc(&wb).into_bytes()));
    // Drop calcChain (Excel rebuilds it; a stale one triggers "repair").
    if super::read_part(path, "xl/calcChain.xml")?.is_some() {
        changes.insert("xl/calcChain.xml".into(), None);
        let rp = rels_path("xl/workbook.xml");
        if let Some(rels) = read_text(path, &rp)? {
            changes.insert(rp, Some(remove_elements_matching(&rels, "Relationship", "calcChain").into_bytes()));
        }
        if let Some(ct) = read_text(path, "[Content_Types].xml")? {
            changes.insert("[Content_Types].xml".into(), Some(remove_elements_matching(&ct, "Override", "calcChain").into_bytes()));
        }
    }
    super::write_package(path, &changes)
}

pub fn remove_elements_matching(xml: &str, name: &str, needle: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut pos = 0;
    for r in elements(xml, name) {
        if xml[r.clone()].contains(needle) {
            out.push_str(&xml[pos..r.start]);
            pos = r.end;
        }
    }
    out.push_str(&xml[pos..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs() {
        assert_eq!(parse_ref("A1"), Some((0, 0)));
        assert_eq!(parse_ref("AB12"), Some((11, 27)));
        assert_eq!(cell_ref(11, 27), "AB12");
        assert!(is_number("12.5") && is_number("-3") && is_number("0") && is_number("0.5"));
        assert!(!is_number("007") && !is_number("12a") && !is_number(" 1") && !is_number("inf"));
    }

    #[test]
    fn patch() {
        let xml = r#"<worksheet><dimension ref="A1:B2"/><sheetData><row r="1" spans="1:2"><c r="A1" s="3" t="s"><v>0</v></c><c r="B1"><v>5</v></c></row><row r="3"><c r="B3"><f>A1+1</f><v>6</v></c></row></sheetData></worksheet>"#;
        let mut e = BTreeMap::new();
        e.insert((0, 0), "hola & <x>".to_string());
        e.insert((0, 2), "7".to_string());
        e.insert((1, 1), "=SUM(B1:B3)".to_string());
        e.insert((2, 1), String::new());
        e.insert((5, 0), "TRUE".to_string());
        let out = patch_sheet(xml, &e).unwrap();
        assert!(out.contains(r#"<c r="A1" s="3" t="inlineStr"><is><t>hola &amp; &lt;x&gt;</t></is></c><c r="B1"><v>5</v></c><c r="C1"><v>7</v></c>"#), "{out}");
        assert!(out.contains(r#"<row r="2"><c r="B2"><f>SUM(B1:B3)</f></c></row><row r="3"></row>"#), "{out}");
        assert!(out.contains(r#"<row r="6"><c r="A6" t="b"><v>1</v></c></row></sheetData>"#), "{out}");
        assert!(out.contains(r#"<dimension ref="A1:C6"/>"#), "{out}");
        assert!(!out.contains("spans"));
        let shared = r#"<worksheet><sheetData><row r="1"><c r="A1"><f t="shared" ref="A1:A3" si="0">B1</f></c></row></sheetData></worksheet>"#;
        let mut e = BTreeMap::new();
        e.insert((0, 0), "x".to_string());
        assert!(patch_sheet(shared, &e).is_err());
        let empty = r#"<worksheet><sheetData/></worksheet>"#;
        let mut e = BTreeMap::new();
        e.insert((0, 0), "x".to_string());
        assert!(patch_sheet(empty, &e).unwrap().contains(r#"<sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>x</t></is></c></row></sheetData>"#));
        let wb = r#"<workbook><sheets><sheet/></sheets><definedNames/></workbook>"#;
        assert_eq!(force_recalc(wb), r#"<workbook><sheets><sheet/></sheets><definedNames/><calcPr fullCalcOnLoad="1"/></workbook>"#);
    }
}
