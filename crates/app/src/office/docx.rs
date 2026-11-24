//! Word documents edited as live Markdown. Each body paragraph becomes one Markdown line
//! (headings `#`, lists `-`/`1.`, **bold**, *italic*, ~~strike~~, <u>underline</u>, links) and
//! each table a Markdown table. On save the new Markdown is diffed against the original units:
//! unchanged paragraphs keep their exact XML, edited ones are regenerated from the original
//! paragraph (its paragraph and run properties are kept), new ones copy their neighbour's.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::{Res, TagKind, attr, children, element, elements, escape, inner, parse_rels, read_text, rels_path, start_tag, tags, unescape};

const DOC: &str = "word/document.xml";
const HYPERLINK_TYPE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";

pub fn is_docx(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("docx") || e.eq_ignore_ascii_case("docm"))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Para,
    Table,
    /// Content controls (e.g. a table of contents): shown, kept as-is.
    Opaque,
}

#[derive(Clone, Debug)]
struct Unit {
    kind: Kind,
    md: String,
    xml: String,
    /// Empty paragraphs/bookmarks that followed it (spacing), kept with it.
    trail: String,
    list: bool,
}

#[derive(Default)]
struct Styles {
    /// styleId → heading level (Title = 1).
    heading: HashMap<String, u8>,
    /// heading level → styleId.
    heading_id: HashMap<u8, String>,
    list_paragraph: Option<String>,
    /// numId → ordered list?
    ordered: HashMap<String, bool>,
    /// styleId → numId for list styles ("List Bullet").
    style_num: HashMap<String, String>,
}

pub struct DocxModel {
    pub path: PathBuf,
    xml: String,
    /// `xml[..body_start]` and `xml[body_end..]` (sectPr included) are kept verbatim.
    body_start: usize,
    body_end: usize,
    lead: String,
    units: Vec<Unit>,
    tail: String,
    styles: Styles,
    /// Image/object runs referenced from Markdown as `![imagen N]`.
    drawings: Vec<String>,
    /// rId → URL and back.
    links: HashMap<String, String>,
    rels_xml: String,
    /// numId used by existing bullet / numbered paragraphs (for new list items).
    bullet_num: Option<String>,
    ordered_num: Option<String>,
    normal_ppr: Option<String>,
    pub markdown: String,
}

impl DocxModel {
    pub fn load(path: &Path) -> Res<Self> {
        let xml = read_text(path, DOC)?.ok_or("not a Word document (word/document.xml missing)")?;
        let styles_xml = read_text(path, "word/styles.xml")?.unwrap_or_default();
        let numbering_xml = read_text(path, "word/numbering.xml")?.unwrap_or_default();
        let rels_xml = read_text(path, &rels_path(DOC))?.unwrap_or_default();
        let links = parse_rels(&rels_xml).into_iter().filter(|r| r.1 == HYPERLINK_TYPE).map(|r| (r.0, r.2)).collect();
        let mut m = Self {
            path: path.to_path_buf(),
            body_start: 0,
            body_end: 0,
            lead: String::new(),
            units: Vec::new(),
            tail: String::new(),
            styles: parse_styles(&styles_xml, &numbering_xml),
            drawings: Vec::new(),
            links,
            rels_xml,
            bullet_num: None,
            ordered_num: None,
            normal_ppr: None,
            markdown: String::new(),
            xml,
        };
        m.parse_body()?;
        m.markdown = m.render_md();
        Ok(m)
    }

    fn parse_body(&mut self) -> Res<()> {
        let body = element(&self.xml, "w:body").ok_or("w:body missing")?;
        let bi = inner(&self.xml, body.clone());
        self.body_start = bi.start;
        self.body_end = bi.end;
        let xml = self.xml.clone();
        let kids = children(&xml, body);
        let mut tail_from = bi.end;
        for (name, r) in &kids {
            if *name == "w:sectPr" {
                tail_from = r.start;
                break;
            }
            let src = &xml[r.clone()];
            let unit = match *name {
                "w:p" => {
                    let (md, list) = self.para_md(src);
                    (!md.trim().is_empty()).then(|| Unit { kind: Kind::Para, md, xml: src.to_string(), trail: String::new(), list })
                }
                "w:tbl" => Some(Unit { kind: Kind::Table, md: self.table_md(src), xml: src.to_string(), trail: String::new(), list: false }),
                _ => {
                    let text = super::text_of(src, "w:t");
                    (!text.trim().is_empty()).then(|| {
                        let t: String = text.chars().take(80).collect();
                        Unit { kind: Kind::Opaque, md: format!("> 🔒 {}", t.replace('\n', " ")), xml: src.to_string(), trail: String::new(), list: false }
                    })
                }
            };
            match unit {
                Some(u) => {
                    if u.kind == Kind::Para {
                        self.note_template(src, &u);
                    }
                    self.units.push(u);
                }
                None => match self.units.last_mut() {
                    Some(last) => last.trail.push_str(src),
                    None => self.lead.push_str(src),
                },
            }
        }
        // Whitespace/sectPr after the last unit.
        let last_end = kids.iter().take_while(|(n, _)| *n != "w:sectPr").last().map(|(_, r)| r.end).unwrap_or(bi.start);
        self.tail = xml[last_end.max(bi.start)..bi.end].to_string();
        let _ = tail_from;
        Ok(())
    }

    /// Remember templates for new paragraphs: a plain paragraph's pPr and list numIds.
    fn note_template(&mut self, p: &str, u: &Unit) {
        let ppr = element(p, "w:pPr").map(|r| p[r].to_string());
        if let Some(num) = self.num_of(p) {
            let ordered = self.styles.ordered.get(&num).copied().unwrap_or(false);
            let slot = if ordered { &mut self.ordered_num } else { &mut self.bullet_num };
            slot.get_or_insert(num);
        } else if !u.md.starts_with('#') && self.normal_ppr.is_none() {
            self.normal_ppr = Some(ppr.unwrap_or_default());
        }
    }

    /// List numId of a paragraph: direct numbering or its style's.
    fn num_of(&self, p: &str) -> Option<String> {
        if let Some(ppr) = element(p, "w:pPr").map(|r| &p[r]) {
            if element(ppr, "w:numPr").is_some() {
                return num_id(p);
            }
            let style = element(ppr, "w:pStyle").and_then(|r| attr(&ppr[r], "w:val"))?;
            return self.styles.style_num.get(&style).cloned();
        }
        None
    }

    fn heading_level(&self, p: &str) -> Option<u8> {
        let ppr = element(p, "w:pPr").map(|r| &p[r])?;
        if let Some(s) = element(ppr, "w:pStyle").and_then(|r| attr(&ppr[r], "w:val")) {
            if let Some(l) = self.styles.heading.get(&s) {
                return Some(*l);
            }
        }
        element(ppr, "w:outlineLvl").and_then(|r| attr(&ppr[r], "w:val")).and_then(|v| v.parse::<u8>().ok()).filter(|l| *l < 6).map(|l| l + 1)
    }

    /// Markdown for one paragraph, and whether it's a list item.
    fn para_md(&mut self, p: &str) -> (String, bool) {
        let inline = self.inline_md(p);
        if let Some(l) = self.heading_level(p) {
            return (format!("{} {}", "#".repeat(l as usize), inline.trim_start()), false);
        }
        if let Some(num) = self.num_of(p) {
            let ilvl = element(p, "w:ilvl").and_then(|r| attr(&p[r], "w:val")).and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
            let marker = if self.styles.ordered.get(&num).copied().unwrap_or(false) { "1." } else { "-" };
            return (format!("{}{} {}", "  ".repeat(ilvl.min(8)), marker, inline.trim_start()), true);
        }
        // A plain line that looks like Markdown syntax must not turn into it on save.
        let t = inline.trim_start();
        let guard = t.starts_with('#') || t.starts_with("- ") || t.starts_with("> ") || t.starts_with('|') || is_ordered_marker(t);
        (if guard { format!("\\{inline}") } else { inline }, false)
    }

    /// Runs → inline Markdown. Image/object runs become `![imagen N]` placeholders.
    fn inline_md(&mut self, p: &str) -> String {
        let mut segs: Vec<(Fmt, String)> = Vec::new();
        let ts = tags(p);
        let mut del = 0usize;
        let mut link: Option<String> = None;
        let mut run: Option<(usize, Fmt, bool)> = None; // (start, fmt, in rPr)
        let mut i = 0;
        while i < ts.len() {
            let t = ts[i];
            match (t.name, t.kind) {
                ("w:del" | "w:moveFrom", TagKind::Open) => del += 1,
                ("w:del" | "w:moveFrom", TagKind::Close) => del = del.saturating_sub(1),
                ("w:hyperlink", TagKind::Open) => {
                    let s = t.src(p);
                    link = attr(s, "r:id").and_then(|id| self.links.get(&id).cloned()).or_else(|| attr(s, "w:anchor").map(|a| format!("#{a}")));
                }
                ("w:hyperlink", TagKind::Close) => link = None,
                ("w:r", TagKind::Open) if del == 0 => run = Some((t.start, Fmt { link: link.clone(), ..Fmt::default() }, false)),
                ("w:r", TagKind::Close) => run = None,
                ("w:rPr", TagKind::Open) => {
                    if let Some(r) = run.as_mut() {
                        r.2 = true;
                    }
                }
                ("w:rPr", TagKind::Close) => {
                    if let Some(r) = run.as_mut() {
                        r.2 = false;
                    }
                }
                (name @ ("w:b" | "w:i" | "w:strike" | "w:u"), TagKind::Empty | TagKind::Open) => {
                    if let Some((_, f, true)) = run.as_mut() {
                        let s = t.src(p);
                        let on = attr(s, "w:val").is_none_or(|v| !matches!(v.as_str(), "0" | "false" | "off" | "none"));
                        match name {
                            "w:b" => f.b = on,
                            "w:i" => f.i = on,
                            "w:strike" => f.s = on,
                            _ => f.u = on,
                        }
                    }
                }
                ("w:t", TagKind::Open) => {
                    if let Some((_, f, _)) = &run {
                        let end = ts.get(i + 1).map(|n| n.start).unwrap_or(p.len());
                        push_seg(&mut segs, f, &unescape(&p[t.end..end]));
                    }
                }
                ("w:tab", TagKind::Empty) if run.is_some() => push_seg(&mut segs, &run.as_ref().unwrap().1, "\t"),
                ("w:br" | "w:cr", TagKind::Empty) if run.is_some() => {
                    if attr(t.src(p), "w:type").is_none_or(|v| v == "textWrapping") {
                        push_seg(&mut segs, &run.as_ref().unwrap().1, "<br>");
                    }
                }
                ("w:drawing" | "w:pict" | "w:object", TagKind::Open | TagKind::Empty) => {
                    if let Some((start, _, _)) = run.take() {
                        // Whole run holding the object; skip to its end.
                        let mut depth = 0;
                        let mut j = i;
                        while j < ts.len() {
                            match (ts[j].name, ts[j].kind) {
                                ("w:r", TagKind::Open) => depth += 1,
                                ("w:r", TagKind::Close) if depth == 0 => break,
                                ("w:r", TagKind::Close) => depth -= 1,
                                _ => {}
                            }
                            j += 1;
                        }
                        let end = ts.get(j).map(|e| e.end).unwrap_or(p.len());
                        self.drawings.push(p[start..end].to_string());
                        segs.push((Fmt::default(), format!("![imagen {}]", self.drawings.len())));
                        i = j;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        segs_to_md(&segs)
    }

    fn table_md(&mut self, tbl: &str) -> String {
        let mut lines = Vec::new();
        let rows = elements_direct(tbl, "w:tr");
        for (ri, row) in rows.iter().enumerate() {
            let rx = &tbl[row.clone()];
            let cells: Vec<String> = elements_direct(rx, "w:tc").iter().map(|c| self.cell_md(&rx[c.clone()])).collect();
            lines.push(format!("| {} |", cells.join(" | ")));
            if ri == 0 {
                lines.push(format!("|{}", " --- |".repeat(cells.len().max(1))));
            }
        }
        if lines.is_empty() {
            lines.push("| |".into());
            lines.push("| --- |".into());
        }
        lines.join("\n")
    }

    fn cell_md(&mut self, tc: &str) -> String {
        let paras: Vec<String> = elements(tc, "w:p").iter().map(|r| self.inline_md(&tc[r.clone()])).collect();
        paras.join("<br>").replace('|', "\\|").replace('\n', " ")
    }

    fn render_md(&self) -> String {
        let mut out = String::new();
        for (i, u) in self.units.iter().enumerate() {
            if i > 0 {
                let both_list = u.list && self.units[i - 1].list;
                out.push_str(if both_list { "\n" } else { "\n\n" });
            }
            out.push_str(&u.md);
        }
        out.push('\n');
        out
    }

    /// Write `markdown` back into the .docx.
    pub fn save(&mut self, markdown: &str) -> Res<()> {
        let new_units = split_units(markdown);
        let old: Vec<&str> = self.units.iter().map(|u| u.md.as_str()).collect();
        let new: Vec<&str> = new_units.iter().map(|(_, s)| s.as_str()).collect();
        let ops = diff(&old, &new);
        let mut body = self.lead.clone();
        let mut new_rels: Vec<(String, String)> = Vec::new();
        // Template paragraph for inserted lines: the last one emitted.
        let mut last_para: Option<String> = None;
        let mut oi = 0;
        let mut ni = 0;
        let mut k = 0;
        while k < ops.len() {
            match ops[k] {
                Op::Keep => {
                    let u = &self.units[oi];
                    body.push_str(&u.xml);
                    body.push_str(&u.trail);
                    if u.kind == Kind::Para {
                        last_para = Some(u.xml.clone());
                    }
                    oi += 1;
                    ni += 1;
                    k += 1;
                }
                _ => {
                    // A run of changes: pair deletions with insertions (edited units).
                    let start = k;
                    while k < ops.len() && ops[k] != Op::Keep {
                        k += 1;
                    }
                    let dels = ops[start..k].iter().filter(|o| **o == Op::Del).count();
                    let ins = ops[start..k].iter().filter(|o| **o == Op::Ins).count();
                    for j in 0..ins.max(dels) {
                        let old_u = (j < dels).then(|| self.units[oi + j].clone());
                        if j < ins {
                            let (kind, md) = &new_units[ni + j];
                            let xml = match (kind, &old_u) {
                                (Kind::Table, Some(o)) if o.kind == Kind::Table => self.regen_table(&o.xml, md, &mut new_rels),
                                (Kind::Table, _) => self.regen_table(&simple_table(md), md, &mut new_rels),
                                (_, Some(o)) if o.kind == Kind::Opaque && md.starts_with("> 🔒") => o.xml.clone(),
                                (_, Some(o)) if o.kind == Kind::Para => self.regen_para(&o.xml, md, &mut new_rels),
                                _ => {
                                    let tpl = last_para.clone().unwrap_or_else(|| "<w:p></w:p>".into());
                                    self.regen_para(&tpl, md, &mut new_rels)
                                }
                            };
                            body.push_str(&xml);
                            if let Some(o) = &old_u {
                                body.push_str(&o.trail);
                            }
                            if *kind == Kind::Para {
                                last_para = Some(xml);
                            }
                        }
                    }
                    oi += dels;
                    ni += ins;
                }
            }
        }
        body.push_str(&self.tail);
        let xml = format!("{}{}{}", &self.xml[..self.body_start], body, &self.xml[self.body_end..]);
        let mut changes = HashMap::new();
        changes.insert(DOC.to_string(), Some(xml.into_bytes()));
        if !new_rels.is_empty() {
            let mut rels = self.rels_xml.clone();
            let add: String = new_rels
                .iter()
                .map(|(id, url)| format!("<Relationship Id=\"{id}\" Type=\"{HYPERLINK_TYPE}\" Target=\"{}\" TargetMode=\"External\"/>", escape(url)))
                .collect();
            match rels.rfind("</Relationships>") {
                Some(i) => rels.insert_str(i, &add),
                None => rels = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{add}</Relationships>"),
            }
            changes.insert(rels_path(DOC), Some(rels.into_bytes()));
        }
        super::write_package(&self.path, &changes)?;
        // New baseline for the next save.
        *self = Self::load(&self.path)?;
        Ok(())
    }

    fn link_id(&mut self, url: &str, new_rels: &mut Vec<(String, String)>) -> Option<String> {
        if url.starts_with('#') {
            return None;
        }
        if let Some((id, _)) = self.links.iter().find(|(_, u)| *u == url) {
            return Some(id.clone());
        }
        let id = format!("rIdJt{}", self.links.len() + new_rels.len() + 1);
        new_rels.push((id.clone(), url.to_string()));
        self.links.insert(id.clone(), url.to_string());
        Some(id)
    }

    /// Rebuild a paragraph from Markdown, keeping `tpl`'s paragraph/run properties.
    fn regen_para(&mut self, tpl: &str, md: &str, new_rels: &mut Vec<(String, String)>) -> String {
        let (block, text) = parse_block(md);
        let tpl_ppr = element(tpl, "w:pPr").map(|r| tpl[r].to_string()).unwrap_or_default();
        let tpl_heading = self.heading_level(tpl);
        let tpl_list = self.num_of(tpl).is_some();
        let mut extra_rpr = String::new();
        let mut text = text.to_string();
        let ppr = match block {
            Block::Heading(l) if tpl_heading == Some(l) => tpl_ppr.clone(),
            Block::Heading(l) => match self.styles.heading_id.get(&l) {
                Some(id) => format!("<w:pPr><w:pStyle w:val=\"{}\"/></w:pPr>", escape(id)),
                None => {
                    extra_rpr = format!("<w:b/><w:sz w:val=\"{}\"/>", [40, 32, 28, 26, 24, 22][(l as usize - 1).min(5)]);
                    String::new()
                }
            },
            Block::List { ordered, level } => {
                let num = if ordered { self.ordered_num.clone() } else { self.bullet_num.clone() };
                let same_kind = self.num_of(tpl).is_some_and(|n| self.styles.ordered.get(&n).copied().unwrap_or(false) == ordered);
                match (same_kind, num) {
                    (true, _) => set_ilvl(&tpl_ppr, level),
                    (false, Some(n)) => {
                        let style = self.styles.list_paragraph.as_ref().map(|s| format!("<w:pStyle w:val=\"{}\"/>", escape(s))).unwrap_or_default();
                        format!("<w:pPr>{style}<w:numPr><w:ilvl w:val=\"{level}\"/><w:numId w:val=\"{n}\"/></w:numPr></w:pPr>")
                    }
                    (false, None) => {
                        text = format!("{}{} {text}", "    ".repeat(level), if ordered { "1." } else { "•" });
                        self.normal_ppr.clone().unwrap_or_default()
                    }
                }
            }
            Block::Plain if tpl_heading.is_some() || tpl_list => self.normal_ppr.clone().unwrap_or_default(),
            Block::Plain => tpl_ppr.clone(),
        };
        let base_rpr = base_rpr(tpl);
        let base = if block_is_heading(block) && tpl_heading.is_none() { String::new() } else { base_rpr };
        let runs = self.runs_xml(&text, &base, &extra_rpr, new_rels);
        let open = start_tag(tpl, 0);
        let open = if open.starts_with("<w:p") && !open.ends_with("/>") { open.to_string() } else { "<w:p>".to_string() };
        format!("{open}{ppr}{runs}</w:p>")
    }

    fn runs_xml(&mut self, text: &str, base: &str, extra: &str, new_rels: &mut Vec<(String, String)>) -> String {
        let mut out = String::new();
        for (f, s) in parse_inline(text) {
            if let Some(n) = s.strip_prefix("\u{0}img").and_then(|n| n.parse::<usize>().ok()) {
                if let Some(d) = self.drawings.get(n.wrapping_sub(1)) {
                    out.push_str(d);
                }
                continue;
            }
            let mut rpr = String::from(base);
            rpr.push_str(extra);
            if f.b {
                rpr.push_str("<w:b/><w:bCs/>");
            }
            if f.i {
                rpr.push_str("<w:i/><w:iCs/>");
            }
            if f.s {
                rpr.push_str("<w:strike/>");
            }
            if f.u {
                rpr.push_str("<w:u w:val=\"single\"/>");
            }
            let link_id = f.link.as_deref().and_then(|u| self.link_id(u, new_rels));
            if f.link.is_some() && !f.u {
                rpr.push_str("<w:u w:val=\"single\"/><w:color w:val=\"0563C1\"/>");
            }
            let rpr = if rpr.is_empty() { String::new() } else { format!("<w:rPr>{}</w:rPr>", sort_rpr(&rpr)) };
            let mut body = String::new();
            for (i, part) in s.split("<br>").enumerate() {
                if i > 0 {
                    body.push_str("<w:br/>");
                }
                for (j, seg) in part.split('\t').enumerate() {
                    if j > 0 {
                        body.push_str("<w:tab/>");
                    }
                    if !seg.is_empty() {
                        body.push_str(&format!("<w:t xml:space=\"preserve\">{}</w:t>", escape(seg)));
                    }
                }
            }
            let r = format!("<w:r>{rpr}{body}</w:r>");
            match (link_id, &f.link) {
                (Some(id), _) => out.push_str(&format!("<w:hyperlink r:id=\"{id}\">{r}</w:hyperlink>")),
                (None, Some(a)) if a.starts_with('#') => out.push_str(&format!("<w:hyperlink w:anchor=\"{}\">{r}</w:hyperlink>", escape(&a[1..]))),
                _ => out.push_str(&r),
            }
        }
        out
    }

    fn regen_table(&mut self, tpl: &str, md: &str, new_rels: &mut Vec<(String, String)>) -> String {
        let rows = parse_table(md);
        let old_rows = elements_direct(tpl, "w:tr");
        let Some(first) = old_rows.first() else { return tpl.to_string() };
        let last = old_rows.last().unwrap().clone();
        let mut out = tpl[..first.start].to_string();
        for (ri, cells) in rows.iter().enumerate() {
            let tr_range = old_rows.get(ri).cloned().unwrap_or(last.clone());
            let tr = &tpl[tr_range];
            let tcs = elements_direct(tr, "w:tc");
            let Some(tc_first) = tcs.first() else {
                out.push_str(tr);
                continue;
            };
            let tc_last = tcs.last().unwrap().clone();
            out.push_str(&tr[..tc_first.start]);
            for (ci, text) in cells.iter().enumerate() {
                let tc = &tr[tcs.get(ci).cloned().unwrap_or(tc_last.clone())];
                let same_row = ri < old_rows.len() && ci < tcs.len();
                if same_row && self.cell_md_peek(tc) == *text {
                    out.push_str(tc);
                    continue;
                }
                let tc_open = start_tag(tc, 0);
                let tcpr = element(tc, "w:tcPr").map(|r| tc[r].to_string()).unwrap_or_default();
                let ptpl = element(tc, "w:p").map(|r| tc[r].to_string()).unwrap_or_else(|| "<w:p></w:p>".into());
                let paras: String = text.split("<br>").map(|line| self.regen_para(&ptpl, &escape_plain_block(line.trim()), new_rels)).collect();
                out.push_str(&format!("{tc_open}{tcpr}{paras}</w:tc>"));
            }
            out.push_str(&tr[tc_last.end..]);
        }
        out.push_str(&tpl[last.end..]);
        out
    }

    /// Cell Markdown without recording drawings again.
    fn cell_md_peek(&mut self, tc: &str) -> String {
        let n = self.drawings.len();
        let s = self.cell_md(tc);
        // Map the new placeholders back to the ones already shown (same document order).
        self.drawings.truncate(n);
        s
    }
}

fn block_is_heading(b: Block) -> bool {
    matches!(b, Block::Heading(_))
}

/// Cell text never contains block syntax.
fn escape_plain_block(s: &str) -> String {
    let t = s.trim_start();
    if t.starts_with('#') || t.starts_with("- ") || is_ordered_marker(t) { format!("\\{s}") } else { s.to_string() }
}

fn is_ordered_marker(t: &str) -> bool {
    let d = t.bytes().take_while(|b| b.is_ascii_digit()).count();
    d > 0 && t[d..].starts_with(". ")
}

fn num_id(p: &str) -> Option<String> {
    let ppr = &p[element(p, "w:pPr")?];
    let np = &ppr[element(ppr, "w:numPr")?];
    attr(&np[element(np, "w:numId")?], "w:val").filter(|v| v != "0")
}

fn set_ilvl(ppr: &str, level: usize) -> String {
    match element(ppr, "w:ilvl") {
        Some(r) => format!("{}<w:ilvl w:val=\"{level}\"/>{}", &ppr[..r.start], &ppr[r.end..]),
        None => ppr.to_string(),
    }
}

/// First run's rPr minus the toggles Markdown controls.
fn base_rpr(p: &str) -> String {
    let Some(r) = elements(p, "w:r").into_iter().find(|r| p[r.clone()].contains("<w:t")) else { return String::new() };
    let run = &p[r];
    let Some(rp) = element(run, "w:rPr") else { return String::new() };
    let rpr = &run[rp.clone()];
    let ir = inner(run, rp);
    let mut out = String::new();
    for (name, r) in children(rpr, 0..rpr.len()) {
        if matches!(name, "w:b" | "w:bCs" | "w:i" | "w:iCs" | "w:strike" | "w:dstrike" | "w:u" | "w:rStyle") {
            continue;
        }
        out.push_str(&rpr[r]);
    }
    let _ = ir;
    out
}

/// CT_RPr requires a fixed child order; order our additions roughly per schema.
fn sort_rpr(rpr: &str) -> String {
    const ORDER: [&str; 22] = [
        "w:rStyle", "w:rFonts", "w:b", "w:bCs", "w:i", "w:iCs", "w:caps", "w:smallCaps", "w:strike", "w:dstrike", "w:outline", "w:shadow", "w:emboss", "w:imprint",
        "w:noProof", "w:snapToGrid", "w:vanish", "w:webHidden", "w:color", "w:spacing", "w:w", "w:kern",
    ];
    const LATER: [&str; 6] = ["w:position", "w:sz", "w:szCs", "w:highlight", "w:u", "w:effect"];
    let wrapped = format!("<x>{rpr}</x>");
    let mut kids: Vec<(usize, String)> = children(&wrapped, 0..wrapped.len())
        .into_iter()
        .map(|(n, r)| {
            let rank = ORDER.iter().position(|o| *o == n).or_else(|| LATER.iter().position(|o| *o == n).map(|p| 100 + p)).unwrap_or(200);
            (rank, wrapped[r].to_string())
        })
        .collect();
    kids.sort_by_key(|k| k.0);
    // Drop duplicates (e.g. base had a color and the link adds one).
    let mut seen = std::collections::HashSet::new();
    kids.into_iter()
        .filter(|(_, x)| {
            let name: String = x[1..].chars().take_while(|c| !c.is_whitespace() && *c != '/' && *c != '>').collect();
            seen.insert(name)
        })
        .map(|(_, x)| x)
        .collect()
}

fn elements_direct(xml: &str, name: &str) -> Vec<std::ops::Range<usize>> {
    children(xml, 0..xml.len()).into_iter().filter(|(n, _)| *n == name).map(|(_, r)| r).collect()
}

fn simple_table(md: &str) -> String {
    let cols = parse_table(md).first().map(|r| r.len()).unwrap_or(1).max(1);
    let cell = "<w:tc><w:tcPr><w:tcW w:w=\"0\" w:type=\"auto\"/></w:tcPr><w:p></w:p></w:tc>".repeat(cols);
    let border = |s: &str| format!("<w:{s} w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>");
    let borders: String = ["top", "left", "bottom", "right", "insideH", "insideV"].iter().map(|s| border(s)).collect();
    format!(
        "<w:tbl><w:tblPr><w:tblW w:w=\"0\" w:type=\"auto\"/><w:tblBorders>{borders}</w:tblBorders></w:tblPr><w:tblGrid>{}</w:tblGrid><w:tr>{cell}</w:tr></w:tbl>",
        "<w:gridCol/>".repeat(cols)
    )
}

fn parse_styles(styles: &str, numbering: &str) -> Styles {
    let mut s = Styles::default();
    for r in elements(styles, "w:style") {
        let x = &styles[r];
        let tag = start_tag(x, 0);
        if attr(tag, "w:type").as_deref() != Some("paragraph") {
            continue;
        }
        let Some(id) = attr(tag, "w:styleId") else { continue };
        let name = element(x, "w:name").and_then(|r| attr(&x[r], "w:val")).unwrap_or_default().to_ascii_lowercase();
        let level = if name == "title" {
            Some(1)
        } else if let Some(n) = name.strip_prefix("heading ") {
            n.trim().parse::<u8>().ok().filter(|l| (1..=6).contains(l))
        } else {
            None
        };
        if let Some(l) = level {
            s.heading.insert(id.clone(), l);
            if name != "title" {
                s.heading_id.entry(l).or_insert(id.clone());
            }
        }
        if let Some(n) = element(x, "w:numId").and_then(|r| attr(&x[r], "w:val")).filter(|v| v != "0") {
            s.style_num.insert(id.clone(), n);
        }
        if name == "list paragraph" {
            s.list_paragraph = Some(id);
        }
    }
    // numId → abstractNumId → level 0 format.
    let mut abs: HashMap<String, bool> = HashMap::new();
    for r in elements(numbering, "w:abstractNum") {
        let x = &numbering[r];
        let Some(id) = attr(start_tag(x, 0), "w:abstractNumId") else { continue };
        let fmt = element(x, "w:numFmt").and_then(|r| attr(&x[r], "w:val")).unwrap_or_default();
        abs.insert(id, fmt != "bullet" && fmt != "none" && !fmt.is_empty());
    }
    for r in elements(numbering, "w:num") {
        let x = &numbering[r];
        let Some(id) = attr(start_tag(x, 0), "w:numId") else { continue };
        let a = element(x, "w:abstractNumId").and_then(|r| attr(&x[r], "w:val")).unwrap_or_default();
        s.ordered.insert(id, abs.get(&a).copied().unwrap_or(false));
    }
    s
}

// ---- inline formatting -------------------------------------------------------------------------

#[derive(Clone, Default, PartialEq, Debug)]
struct Fmt {
    b: bool,
    i: bool,
    s: bool,
    u: bool,
    link: Option<String>,
}

fn push_seg(segs: &mut Vec<(Fmt, String)>, f: &Fmt, text: &str) {
    if text.is_empty() {
        return;
    }
    match segs.last_mut() {
        Some((lf, lt)) if lf == f => lt.push_str(text),
        _ => segs.push((f.clone(), text.to_string())),
    }
}

fn md_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('*', "\\*").replace('~', "\\~").replace('[', "\\[")
}

fn segs_to_md(segs: &[(Fmt, String)]) -> String {
    let mut out = String::new();
    for (f, t) in segs {
        if t.starts_with("![imagen ") {
            out.push_str(t);
            continue;
        }
        // Markers can't wrap edge whitespace: keep it outside.
        let lead = &t[..t.len() - t.trim_start().len()];
        let core = t.trim();
        let trail = &t[lead.len() + core.len()..];
        out.push_str(lead);
        if core.is_empty() {
            out.push_str(trail);
            continue;
        }
        let mut s = md_escape(core).replace("\\[br\\]", "<br>");
        if f.u && f.link.is_none() {
            s = format!("<u>{s}</u>");
        }
        if f.s {
            s = format!("~~{s}~~");
        }
        s = match (f.b, f.i) {
            (true, true) => format!("***{s}***"),
            (true, false) => format!("**{s}**"),
            (false, true) => format!("*{s}*"),
            _ => s,
        };
        if let Some(l) = &f.link {
            s = format!("[{s}]({l})");
        }
        out.push_str(&s);
        out.push_str(trail);
    }
    out
}

/// Inline Markdown → formatted segments. Image placeholders come back as "\0imgN".
fn parse_inline(text: &str) -> Vec<(Fmt, String)> {
    let mut segs: Vec<(Fmt, String)> = Vec::new();
    let mut f = Fmt::default();
    let c: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut buf = String::new();
    let flush = |segs: &mut Vec<(Fmt, String)>, buf: &mut String, f: &Fmt| {
        if !buf.is_empty() {
            push_seg(segs, f, buf);
            buf.clear();
        }
    };
    let starts = |i: usize, pat: &str| -> bool { pat.chars().enumerate().all(|(k, p)| c.get(i + k) == Some(&p)) };
    while i < c.len() {
        if c[i] == '\\' && i + 1 < c.len() {
            buf.push(c[i + 1]);
            i += 2;
            continue;
        }
        if starts(i, "![imagen ") {
            if let Some(end) = (i..c.len()).find(|&j| c[j] == ']') {
                let n: String = c[i + 9..end].iter().collect();
                flush(&mut segs, &mut buf, &f);
                segs.push((Fmt::default(), format!("\u{0}img{}", n.trim())));
                i = end + 1;
                continue;
            }
        }
        if starts(i, "<u>") {
            flush(&mut segs, &mut buf, &f);
            f.u = true;
            i += 3;
            continue;
        }
        if starts(i, "</u>") {
            flush(&mut segs, &mut buf, &f);
            f.u = false;
            i += 4;
            continue;
        }
        if starts(i, "~~") {
            flush(&mut segs, &mut buf, &f);
            f.s = !f.s;
            i += 2;
            continue;
        }
        if c[i] == '*' {
            let n = c[i..].iter().take_while(|&&x| x == '*').count();
            flush(&mut segs, &mut buf, &f);
            match n {
                1 => f.i = !f.i,
                2 => f.b = !f.b,
                _ => {
                    f.b = !f.b;
                    f.i = !f.i;
                }
            }
            i += n.min(3);
            continue;
        }
        if c[i] == '[' {
            // [label](url)
            if let Some(close) = (i + 1..c.len()).find(|&j| c[j] == ']' && c[j - 1] != '\\') {
                if c.get(close + 1) == Some(&'(') {
                    if let Some(pe) = (close + 2..c.len()).find(|&j| c[j] == ')') {
                        flush(&mut segs, &mut buf, &f);
                        let label: String = c[i + 1..close].iter().collect();
                        let url: String = c[close + 2..pe].iter().collect();
                        let mut lf = f.clone();
                        lf.link = Some(url);
                        for (sf, st) in parse_inline(&label) {
                            let mut m = sf.clone();
                            m.b |= lf.b;
                            m.i |= lf.i;
                            m.s |= lf.s;
                            m.u |= lf.u;
                            m.link = lf.link.clone();
                            push_seg(&mut segs, &m, &st);
                        }
                        i = pe + 1;
                        continue;
                    }
                }
            }
        }
        buf.push(c[i]);
        i += 1;
    }
    flush(&mut segs, &mut buf, &f);
    segs
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Block {
    Plain,
    Heading(u8),
    List { ordered: bool, level: usize },
}

fn parse_block(line: &str) -> (Block, &str) {
    let indent = line.len() - line.trim_start().len();
    let t = line.trim_start();
    let hashes = t.bytes().take_while(|b| *b == b'#').count();
    if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
        return (Block::Heading(hashes as u8), t[hashes + 1..].trim_start());
    }
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(m) {
            return (Block::List { ordered: false, level: indent / 2 }, rest);
        }
    }
    if is_ordered_marker(t) {
        let d = t.find(". ").unwrap_or(0);
        return (Block::List { ordered: true, level: indent / 2 }, &t[d + 2..]);
    }
    (Block::Plain, line)
}

fn parse_table(md: &str) -> Vec<Vec<String>> {
    md.lines()
        .map(str::trim)
        .filter(|l| l.starts_with('|'))
        .filter(|l| !l.trim_matches(|c: char| c == '|' || c == '-' || c == ':' || c.is_whitespace()).is_empty() || !l.contains('-'))
        .map(|l| {
            let inner = l.trim_start_matches('|');
            let inner = inner.strip_suffix('|').filter(|s| !s.ends_with('\\')).unwrap_or(inner);
            let mut cells = Vec::new();
            let mut cur = String::new();
            let mut chars = inner.chars().peekable();
            while let Some(ch) = chars.next() {
                if ch == '\\' && chars.peek() == Some(&'|') {
                    cur.push('|');
                    chars.next();
                } else if ch == '|' {
                    cells.push(cur.trim().to_string());
                    cur.clear();
                } else {
                    cur.push(ch);
                }
            }
            cells.push(cur.trim().to_string());
            cells
        })
        .collect()
}

/// Markdown → units: consecutive `|` lines form a table, every other non-blank line a paragraph.
fn split_units(md: &str) -> Vec<(Kind, String)> {
    let mut out: Vec<(Kind, String)> = Vec::new();
    let mut table: Vec<&str> = Vec::new();
    for line in md.lines() {
        let t = line.trim();
        if t.starts_with('|') {
            table.push(line);
            continue;
        }
        if !table.is_empty() {
            out.push((Kind::Table, table.join("\n")));
            table.clear();
        }
        if !t.is_empty() {
            out.push((Kind::Para, line.trim_end().to_string()));
        }
    }
    if !table.is_empty() {
        out.push((Kind::Table, table.join("\n")));
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Keep,
    Del,
    Ins,
}

/// LCS diff (common prefix/suffix trimmed first, so typical edits are cheap).
fn diff(a: &[&str], b: &[&str]) -> Vec<Op> {
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut ops = vec![Op::Keep; pre];
    let (n, m) = (am.len(), bm.len());
    if n * m > 4_000_000 {
        ops.extend(std::iter::repeat_n(Op::Del, n));
        ops.extend(std::iter::repeat_n(Op::Ins, m));
    } else {
        let mut dp = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                dp[i * (m + 1) + j] = if am[i] == bm[j] { dp[(i + 1) * (m + 1) + j + 1] + 1 } else { dp[(i + 1) * (m + 1) + j].max(dp[i * (m + 1) + j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && am[i] == bm[j] {
                ops.push(Op::Keep);
                i += 1;
                j += 1;
            } else if j < m && (i == n || dp[i * (m + 1) + j + 1] >= dp[(i + 1) * (m + 1) + j]) {
                ops.push(Op::Ins);
                j += 1;
            } else {
                ops.push(Op::Del);
                i += 1;
            }
        }
    }
    ops.extend(std::iter::repeat_n(Op::Keep, suf));
    // Within each change run, put deletions first so pairs line up (edited paragraphs).
    let mut k = 0;
    while k < ops.len() {
        if ops[k] == Op::Keep {
            k += 1;
            continue;
        }
        let s = k;
        while k < ops.len() && ops[k] != Op::Keep {
            k += 1;
        }
        ops[s..k].sort_by_key(|o| if *o == Op::Del { 0 } else { 1 });
    }
    ops
}

/// Plain text of a .docx (used for quick previews/search).
#[allow(dead_code)]
pub fn plain_text(path: &Path) -> Res<String> {
    Ok(DocxModel::load(path)?.markdown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_roundtrip() {
        let segs = parse_inline("a **b** *c* ***d*** ~~e~~ <u>f</u> [g](http://x) \\*h ![imagen 2]");
        let md = segs_to_md(&segs.iter().filter(|(_, t)| !t.starts_with('\u{0}')).cloned().collect::<Vec<_>>());
        assert_eq!(md, "a **b** *c* ***d*** ~~e~~ <u>f</u> [g](http://x) \\*h ");
        assert!(segs.iter().any(|(_, t)| t == "\u{0}img2"));
    }

    #[test]
    fn blocks_and_diff() {
        assert_eq!(parse_block("## Hola"), (Block::Heading(2), "Hola"));
        assert_eq!(parse_block("  - x"), (Block::List { ordered: false, level: 1 }, "x"));
        assert_eq!(parse_block("3. y"), (Block::List { ordered: true, level: 0 }, "y"));
        assert_eq!(parse_block("\\# no"), (Block::Plain, "\\# no"));
        let ops = diff(&["a", "b", "c"], &["a", "B", "c", "d"]);
        assert_eq!(ops, vec![Op::Keep, Op::Del, Op::Ins, Op::Keep, Op::Ins]);
        let u = split_units("# T\n\npara\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\n- x\n- y\n");
        assert_eq!(u.len(), 5);
        assert_eq!(u[2].0, Kind::Table);
        assert_eq!(parse_table(&u[2].1), vec![vec!["a", "b"], vec!["1", "2"]]);
    }

    fn make_docx(dir: &Path, body: &str) -> PathBuf {
        use std::io::Write;
        let p = dir.join("t.docx");
        let f = std::fs::File::create(&p).unwrap();
        let mut z = zip::ZipWriter::new(f);
        let o = zip::write::SimpleFileOptions::default();
        z.start_file("[Content_Types].xml", o).unwrap();
        z.write_all(b"<Types/>").unwrap();
        z.start_file("word/document.xml", o).unwrap();
        z.write_all(format!(r#"<w:document xmlns:w="w"><w:body>{body}<w:sectPr/></w:body></w:document>"#).as_bytes()).unwrap();
        z.start_file("word/styles.xml", o).unwrap();
        z.write_all(br#"<w:styles><w:style w:type="paragraph" w:styleId="Ttulo1"><w:name w:val="heading 1"/></w:style></w:styles>"#).unwrap();
        z.finish().unwrap();
        p
    }

    #[test]
    fn docx_roundtrip() {
        let dir = std::env::temp_dir().join(format!("jt-docx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let body = r#"<w:p><w:pPr><w:pStyle w:val="Ttulo1"/></w:pPr><w:r><w:t>Title</w:t></w:r></w:p><w:p/><w:p><w:pPr><w:jc w:val="both"/></w:pPr><w:r><w:rPr><w:rFonts w:ascii="Arial"/><w:b/></w:rPr><w:t>Bold</w:t></w:r><w:r><w:t xml:space="preserve"> rest</w:t></w:r></w:p><w:tbl><w:tblPr/><w:tr><w:tc><w:p><w:r><w:t>a</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>b</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#;
        let p = make_docx(&dir, body);
        let mut m = DocxModel::load(&p).unwrap();
        assert_eq!(m.markdown, "# Title\n\n**Bold** rest\n\n| a | b |\n| --- | --- |\n");
        // No change → identical body.
        let md = m.markdown.clone();
        m.save(&md).unwrap();
        let xml = read_text(&p, DOC).unwrap().unwrap();
        assert!(xml.contains(body), "{xml}");
        // Edit paragraph, add one, edit a cell.
        m.save("# Title\n\n**Bold** changed *it*\n\nnew line\n\n| a | B |\n| --- | --- |\n").unwrap();
        let xml = read_text(&p, DOC).unwrap().unwrap();
        assert!(xml.contains(r#"<w:p><w:pPr><w:jc w:val="both"/></w:pPr><w:r><w:rPr><w:rFonts w:ascii="Arial"/><w:b/><w:bCs/></w:rPr><w:t xml:space="preserve">Bold</w:t></w:r>"#), "{xml}");
        assert!(xml.contains("new line"));
        assert!(xml.contains(r#"<w:t xml:space="preserve">B</w:t>"#));
        assert!(xml.contains("<w:sectPr/>"));
        assert_eq!(m.markdown, "# Title\n\n**Bold** changed *it*\n\nnew line\n\n| a | B |\n| --- | --- |\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
