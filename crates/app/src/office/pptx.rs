//! PowerPoint model: slides parsed into positioned shapes (text, fills, pictures, simple
//! tables) with placeholder geometry/text size inherited from the layout and master, plus the
//! editing operations the viewer offers (shape text, duplicate/delete/reorder slides). Edits are
//! applied to the in-memory XML parts and written back with only those parts replaced.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::{Res, attr, children, element, elements, escape, inner, parse_rels, read_parts, rels_path, resolve_target, start_tag, text_of};

const EMU_PT: f32 = 12700.;
const SLIDE_TYPE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide";

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum Anchor {
    #[default]
    Top,
    Middle,
    Bottom,
}

#[derive(Clone, Default, Debug)]
pub struct Para {
    pub text: String,
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub color: Option<u32>,
    pub align: Align,
    pub bullet: bool,
    pub level: usize,
}

#[derive(Clone, Default, Debug)]
pub struct Shape {
    pub rect: Rect,
    pub fill: Option<u32>,
    pub line: Option<u32>,
    pub ellipse: bool,
    pub paras: Vec<Para>,
    pub anchor: Anchor,
    /// Media part of a picture.
    pub image: Option<String>,
    /// Table rows (cell text).
    pub table: Vec<Vec<String>>,
    /// Index among the slide's `p:sp` elements when the text is editable.
    pub editable: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct Slide {
    pub bg: Option<u32>,
    pub shapes: Vec<Shape>,
    pub title: String,
}

/// Inherited placeholder info from layout/master.
#[derive(Clone, Default)]
struct PhInfo {
    rect: Option<Rect>,
    anchor: Option<Anchor>,
    sizes: [Option<f32>; 9],
}

pub struct PptxModel {
    pub path: PathBuf,
    pub width: f32,
    pub height: f32,
    parts: HashMap<String, String>,
    /// Slide parts in presentation order.
    order: Vec<String>,
    theme: HashMap<String, u32>,
    changed: std::collections::HashSet<String>,
    pub dirty: bool,
}

impl PptxModel {
    pub fn load(path: &Path) -> Res<Self> {
        let raw = read_parts(path, |n| n.ends_with(".xml") || n.ends_with(".rels"))?;
        let parts: HashMap<String, String> = raw.into_iter().map(|(k, v)| (k, String::from_utf8_lossy(&v).into_owned())).collect();
        let pres = parts.get("ppt/presentation.xml").ok_or("not a PowerPoint file (ppt/presentation.xml missing)")?;
        let (width, height) = element(pres, "p:sldSz")
            .map(|r| {
                let t = &pres[r];
                let f = |n| attr(t, n).and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.) / EMU_PT;
                (f("cx"), f("cy"))
            })
            .filter(|(w, h)| *w > 0. && *h > 0.)
            .unwrap_or((720., 405.));
        let theme_part = parts.keys().filter(|k| k.starts_with("ppt/theme/theme")).min().cloned();
        let theme = theme_part.and_then(|t| parts.get(&t)).map(|t| parse_theme(t)).unwrap_or_default();
        let mut m = Self { path: path.to_path_buf(), width, height, parts, order: Vec::new(), theme, changed: Default::default(), dirty: false };
        m.order = m.slide_order();
        Ok(m)
    }

    fn slide_order(&self) -> Vec<String> {
        let pres = self.parts.get("ppt/presentation.xml").cloned().unwrap_or_default();
        let rels = parse_rels(self.parts.get(&rels_path("ppt/presentation.xml")).map(|s| s.as_str()).unwrap_or(""));
        let Some(lst) = element(&pres, "p:sldIdLst") else { return Vec::new() };
        elements(&pres[lst], "p:sldId")
            .into_iter()
            .filter_map(|r| {
                let t = &pres[element(&pres, "p:sldIdLst").unwrap()][r];
                let rid = attr(t, "r:id")?;
                rels.iter().find(|x| x.0 == rid).map(|x| resolve_target("ppt/presentation.xml", &x.2))
            })
            .filter(|p| self.parts.contains_key(p))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    fn rel_target(&self, part: &str, kind_suffix: &str) -> Option<String> {
        let rels = self.parts.get(&rels_path(part))?;
        parse_rels(rels).into_iter().find(|r| r.1.ends_with(kind_suffix)).map(|r| resolve_target(part, &r.2))
    }

    fn rel_by_id(&self, part: &str, id: &str) -> Option<String> {
        let rels = self.parts.get(&rels_path(part))?;
        parse_rels(rels).into_iter().find(|r| r.0 == id).map(|r| resolve_target(part, &r.2))
    }

    fn color(&self, xml: &str) -> Option<u32> {
        // First color element inside `xml` (srgbClr / schemeClr / sysClr).
        for t in super::tags(xml) {
            let s = t.src(xml);
            match t.name {
                "a:srgbClr" => return attr(s, "val").and_then(|v| u32::from_str_radix(&v, 16).ok()),
                "a:sysClr" => return attr(s, "lastClr").and_then(|v| u32::from_str_radix(&v, 16).ok()),
                "a:schemeClr" => {
                    let v = attr(s, "val")?;
                    let key = match v.as_str() {
                        "bg1" => "lt1",
                        "tx1" => "dk1",
                        "bg2" => "lt2",
                        "tx2" => "dk2",
                        o => o,
                    };
                    return self.theme.get(key).copied();
                }
                _ => {}
            }
        }
        None
    }

    fn fill_of(&self, sppr: &str) -> Option<u32> {
        if sppr.contains("<a:noFill") && element(sppr, "a:solidFill").is_none() {
            return None;
        }
        let f = element(sppr, "a:solidFill")?;
        // Ignore the line's fill.
        if let Some(ln) = element(sppr, "a:ln") {
            if ln.start < f.start && f.end <= ln.end {
                return None;
            }
        }
        self.color(&sppr[f])
    }

    fn line_of(&self, sppr: &str) -> Option<u32> {
        let ln = &sppr[element(sppr, "a:ln")?];
        if ln.contains("<a:noFill") {
            return None;
        }
        self.color(&ln[element(ln, "a:solidFill")?])
    }

    fn bg_of(&self, part: &str) -> Option<u32> {
        let x = self.parts.get(part)?;
        let bg = &x[element(x, "p:bg")?];
        if let Some(f) = element(bg, "a:solidFill") {
            return self.color(&bg[f]);
        }
        element(bg, "p:bgRef").and_then(|r| self.color(&bg[r]))
    }

    /// Placeholder geometry/size table of a layout or master part: key "type" and "idx:N".
    fn placeholders(&self, part: &str) -> HashMap<String, PhInfo> {
        let mut out = HashMap::new();
        let Some(x) = self.parts.get(part) else { return out };
        for r in elements(x, "p:sp") {
            let sp = &x[r];
            let Some(ph) = element(sp, "p:ph").map(|r| sp[r].to_string()) else { continue };
            let mut info = PhInfo { rect: xfrm(sp), ..Default::default() };
            if let Some(bp) = element(sp, "a:bodyPr") {
                info.anchor = anchor(&sp[bp]);
            }
            if let Some(ls) = element(sp, "a:lstStyle") {
                level_sizes(&sp[ls], &mut info.sizes);
            }
            let ty = attr(&ph, "type").unwrap_or_else(|| "body".into());
            if let Some(idx) = attr(&ph, "idx") {
                out.entry(format!("idx:{idx}")).or_insert(info.clone());
            }
            out.entry(ty).or_insert(info);
        }
        out
    }

    /// Master text styles: (title sizes, body sizes, other sizes).
    fn master_styles(&self, master: &str) -> [[Option<f32>; 9]; 3] {
        let mut out = [[None; 9]; 3];
        let Some(x) = self.parts.get(master) else { return out };
        for (i, n) in ["p:titleStyle", "p:bodyStyle", "p:otherStyle"].iter().enumerate() {
            if let Some(r) = element(x, n) {
                level_sizes(&x[r], &mut out[i]);
            }
        }
        out
    }

    pub fn slide(&self, ix: usize) -> Option<Slide> {
        let part = self.order.get(ix)?.clone();
        let xml = self.parts.get(&part)?;
        let layout = self.rel_target(&part, "/slideLayout");
        let master = layout.as_ref().and_then(|l| self.rel_target(l, "/slideMaster"));
        let lay_ph = layout.as_ref().map(|l| self.placeholders(l)).unwrap_or_default();
        let mas_ph = master.as_ref().map(|m| self.placeholders(m)).unwrap_or_default();
        let styles = master.as_ref().map(|m| self.master_styles(m)).unwrap_or([[None; 9]; 3]);
        let bg = self.bg_of(&part).or_else(|| layout.as_ref().and_then(|l| self.bg_of(l))).or_else(|| master.as_ref().and_then(|m| self.bg_of(m)));

        let mut shapes = Vec::new();
        // Decorations (non-placeholder shapes) from master and layout, below the slide's own.
        let show_master = |p: &str| self.parts.get(p).is_none_or(|x| attr(start_tag(x, element(x, "p:sld").or(element(x, "p:sldLayout")).map(|r| r.start).unwrap_or(0)), "showMasterSp").as_deref() != Some("0"));
        if let Some(m) = &master {
            if show_master(&part) && layout.as_ref().is_none_or(|l| show_master(l)) {
                self.collect(m, &HashMap::new(), &HashMap::new(), &styles, true, &mut shapes);
            }
        }
        if let Some(l) = &layout {
            self.collect(l, &HashMap::new(), &HashMap::new(), &styles, true, &mut shapes);
        }
        self.collect(&part, &lay_ph, &mas_ph, &styles, false, &mut shapes);
        let title = elements(xml, "p:sp")
            .into_iter()
            .map(|r| &xml[r])
            .find(|sp| element(sp, "p:ph").is_some_and(|p| matches!(attr(&sp[p], "type").as_deref(), Some("title" | "ctrTitle"))))
            .map(|sp| text_of(sp, "a:t"))
            .or_else(|| shapes.iter().rev().find(|s| s.editable.is_some()).and_then(|s| s.paras.first().map(|p| p.text.clone())))
            .unwrap_or_default();
        Some(Slide { bg, shapes, title })
    }

    /// Shapes of a part. `deco_only` skips placeholders (layout/master decorations).
    fn collect(&self, part: &str, lay: &HashMap<String, PhInfo>, mas: &HashMap<String, PhInfo>, styles: &[[Option<f32>; 9]; 3], deco_only: bool, out: &mut Vec<Shape>) {
        let Some(x) = self.parts.get(part) else { return };
        let Some(tree) = element(x, "p:spTree") else { return };
        let sp_index: Vec<usize> = elements(x, "p:sp").into_iter().map(|r| r.start).collect();
        self.walk(x, part, tree, None, lay, mas, styles, deco_only, &sp_index, out);
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        x: &str,
        part: &str,
        range: std::ops::Range<usize>,
        map: Option<(Rect, Rect)>,
        lay: &HashMap<String, PhInfo>,
        mas: &HashMap<String, PhInfo>,
        styles: &[[Option<f32>; 9]; 3],
        deco_only: bool,
        sp_index: &[usize],
        out: &mut Vec<Shape>,
    ) {
        let tr = |r: Rect| match map {
            Some((ch, to)) if ch.w > 0. && ch.h > 0. => {
                let (sx, sy) = (to.w / ch.w, to.h / ch.h);
                Rect { x: to.x + (r.x - ch.x) * sx, y: to.y + (r.y - ch.y) * sy, w: r.w * sx, h: r.h * sy }
            }
            _ => r,
        };
        for (name, r) in children(x, range) {
            let el = &x[r.clone()];
            match name {
                "p:grpSp" => {
                    let gp = element(el, "p:grpSpPr").map(|g| &el[g]).unwrap_or("");
                    let m = group_xfrm(gp).map(|(off, ch)| (ch, tr(off)));
                    self.walk(x, part, r.clone(), m.or(map), lay, mas, styles, deco_only, sp_index, out);
                }
                "p:sp" | "p:cxnSp" => {
                    let ph = element(el, "p:ph").map(|p| el[p].to_string());
                    if deco_only && ph.is_some() {
                        continue;
                    }
                    let (ph_type, ph_idx) = match &ph {
                        Some(p) => (attr(p, "type").unwrap_or_else(|| "body".into()), attr(p, "idx")),
                        None => (String::new(), None),
                    };
                    let inherit = |m: &HashMap<String, PhInfo>| -> Option<PhInfo> {
                        if ph.is_none() {
                            return None;
                        }
                        ph_idx.as_ref().and_then(|i| m.get(&format!("idx:{i}"))).or_else(|| m.get(&ph_type)).cloned()
                    };
                    let (li, mi) = (inherit(lay), inherit(mas));
                    let Some(rect) = xfrm(el).or(li.as_ref().and_then(|i| i.rect)).or(mi.as_ref().and_then(|i| i.rect)) else { continue };
                    let sppr = element(el, "p:spPr").map(|s| &el[s]).unwrap_or("");
                    let geom = element(sppr, "a:prstGeom").and_then(|g| attr(&sppr[g], "prst")).unwrap_or_default();
                    let style_fill = || element(el, "p:style").and_then(|s| element(&el[s.clone()], "a:fillRef").map(|f| &el[s][f])).and_then(|f| self.color(f));
                    let fill = if sppr.contains("<a:noFill") && element(sppr, "a:solidFill").is_none() { None } else { self.fill_of(sppr).or_else(|| if name == "p:sp" && ph.is_none() && !sppr.contains("<a:noFill") { style_fill() } else { None }) };
                    let line = self.line_of(sppr);
                    let body = element(el, "p:txBody").map(|b| &el[b]).unwrap_or("");
                    let anchor = element(body, "a:bodyPr")
                        .and_then(|b| anchor(&body[b]))
                        .or(li.as_ref().and_then(|i| i.anchor))
                        .or(mi.as_ref().and_then(|i| i.anchor))
                        .unwrap_or(if matches!(ph_type.as_str(), "ctrTitle" | "title") { Anchor::Middle } else { Anchor::Top });
                    let style_ix = match ph_type.as_str() {
                        "title" | "ctrTitle" => 0,
                        "" => 2,
                        _ => 1,
                    };
                    let mut local = [None; 9];
                    if let Some(ls) = element(body, "a:lstStyle") {
                        level_sizes(&body[ls], &mut local);
                    }
                    let size_for = |lvl: usize| -> f32 {
                        local[lvl]
                            .or(li.as_ref().and_then(|i| i.sizes[lvl]))
                            .or(mi.as_ref().and_then(|i| i.sizes[lvl]))
                            .or(styles[style_ix][lvl])
                            .unwrap_or(if style_ix == 0 { 40. } else { 18. })
                    };
                    let paras = elements(body, "a:p").into_iter().map(|p| self.para(&body[p], &size_for, ph.is_some() && (ph_type == "body" || ph_type == "obj"))).collect();
                    let editable = (!deco_only && name == "p:sp").then(|| sp_index.iter().position(|s| *s == r.start)).flatten();
                    out.push(Shape { rect: tr(rect), fill, line, ellipse: geom == "ellipse", paras, anchor, image: None, table: Vec::new(), editable });
                }
                "p:pic" => {
                    let Some(rect) = xfrm(el) else { continue };
                    let img = element(el, "a:blip").and_then(|b| attr(&el[b], "r:embed")).and_then(|id| self.rel_by_id(part, &id));
                    out.push(Shape { rect: tr(rect), image: img, ..Default::default() });
                }
                "p:graphicFrame" => {
                    let Some(rect) = element(el, "p:xfrm").and_then(|f| xfrm_in(&el[f])) else { continue };
                    let table: Vec<Vec<String>> = elements(el, "a:tr").into_iter().map(|tr| elements(&el[tr.clone()], "a:tc").into_iter().map(|tc| text_of(&el[tr.clone()][tc], "a:t")).collect()).collect();
                    out.push(Shape { rect: tr(rect), table, line: Some(0x888888), ..Default::default() });
                }
                _ => {}
            }
        }
    }

    fn para(&self, p: &str, size_for: &dyn Fn(usize) -> f32, body_bullets: bool) -> Para {
        let ppr = element(p, "a:pPr").map(|r| start_tag(p, r.start).to_string()).unwrap_or_default();
        let level = attr(&ppr, "lvl").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0).min(8);
        let align = match attr(&ppr, "algn").as_deref() {
            Some("ctr") => Align::Center,
            Some("r") => Align::Right,
            _ => Align::Left,
        };
        let full_ppr = element(p, "a:pPr").map(|r| &p[r]).unwrap_or("");
        let bullet = !full_ppr.contains("<a:buNone") && (full_ppr.contains("<a:buChar") || full_ppr.contains("<a:buAutoNum") || body_bullets);
        let first_rpr = element(p, "a:r").and_then(|r| element(&p[r.clone()], "a:rPr").map(|x| p[r][x].to_string())).or_else(|| element(p, "a:endParaRPr").map(|r| p[r].to_string())).unwrap_or_default();
        let tag = start_tag(&first_rpr, 0);
        let size = attr(tag, "sz").and_then(|v| v.parse::<f32>().ok()).map(|v| v / 100.).unwrap_or_else(|| size_for(level));
        let mut text = String::new();
        for (n, r) in children(p, 0..p.len()) {
            match n {
                "a:r" | "a:fld" => text.push_str(&text_of(&p[r], "a:t")),
                "a:br" => text.push('\n'),
                _ => {}
            }
        }
        let has_text = !text.is_empty();
        Para {
            text,
            size,
            bold: attr(tag, "b").is_some_and(|v| v == "1" || v == "true"),
            italic: attr(tag, "i").is_some_and(|v| v == "1" || v == "true"),
            color: element(&first_rpr, "a:solidFill").and_then(|f| self.color(&first_rpr[f])),
            align,
            bullet: bullet && has_text,
            level,
        }
    }

    pub fn media(&self, part: &str) -> Option<Vec<u8>> {
        super::read_part(&self.path, part).ok().flatten()
    }

    fn set_part(&mut self, name: &str, xml: String) {
        self.parts.insert(name.to_string(), xml);
        self.changed.insert(name.to_string());
        self.dirty = true;
    }

    /// Replace the text of `p:sp` number `sp` on slide `ix` (one paragraph per line).
    pub fn set_text(&mut self, ix: usize, sp: usize, text: &str) -> Res<()> {
        let part = self.order.get(ix).ok_or("no such slide")?.clone();
        let x = self.parts.get(&part).cloned().ok_or("slide missing")?;
        let r = elements(&x, "p:sp").into_iter().nth(sp).ok_or("shape missing")?;
        let el = &x[r.clone()];
        let new_body = match element(el, "p:txBody") {
            Some(b) => {
                let body = &el[b.clone()];
                let paras = elements(body, "a:p");
                let head_end = paras.first().map(|p| p.start).unwrap_or_else(|| inner(body, 0..body.len()).end);
                let tail_start = paras.last().map(|p| p.end).unwrap_or(head_end);
                let olds: Vec<&str> = paras.iter().map(|p| &body[p.clone()]).collect();
                let mut out = body[..head_end].to_string();
                for (i, line) in text.split('\n').enumerate() {
                    let tpl = olds.get(i).or(olds.last()).copied().unwrap_or("<a:p></a:p>");
                    if olds.get(i).is_some_and(|o| para_text(o) == line) {
                        out.push_str(tpl);
                    } else {
                        out.push_str(&regen_para(tpl, line));
                    }
                }
                out.push_str(&body[tail_start..]);
                format!("{}{}{}", &el[..b.start], out, &el[b.end..])
            }
            None => {
                // Shape without text: add a body after spPr.
                let at = element(el, "p:spPr").map(|s| s.end).unwrap_or(el.len() - "</p:sp>".len());
                let paras: String = text.split('\n').map(|l| regen_para("<a:p></a:p>", l)).collect();
                format!("{}<p:txBody><a:bodyPr/><a:lstStyle/>{paras}</p:txBody>{}", &el[..at], &el[at..])
            }
        };
        let nx = format!("{}{}{}", &x[..r.start], new_body, &x[r.end..]);
        self.set_part(&part, nx);
        Ok(())
    }

    pub fn shape_text(&self, ix: usize, sp: usize) -> String {
        let Some(x) = self.order.get(ix).and_then(|p| self.parts.get(p)) else { return String::new() };
        let Some(r) = elements(x, "p:sp").into_iter().nth(sp) else { return String::new() };
        let el = &x[r];
        elements(el, "a:p").into_iter().map(|p| para_text(&el[p])).collect::<Vec<_>>().join("\n")
    }

    fn sld_ids(&self) -> (String, std::ops::Range<usize>, Vec<std::ops::Range<usize>>) {
        let pres = self.parts.get("ppt/presentation.xml").cloned().unwrap_or_default();
        let lst = element(&pres, "p:sldIdLst").unwrap_or(0..0);
        let ids = elements(&pres[lst.clone()], "p:sldId").into_iter().map(|r| r.start + lst.start..r.end + lst.start).collect();
        (pres, lst, ids)
    }

    pub fn move_slide(&mut self, ix: usize, down: bool) -> bool {
        let (pres, _, ids) = self.sld_ids();
        let j = if down { ix + 1 } else { ix.wrapping_sub(1) };
        if ix >= ids.len() || j >= ids.len() {
            return false;
        }
        let (a, b) = if ix < j { (ix, j) } else { (j, ix) };
        let (ra, rb) = (ids[a].clone(), ids[b].clone());
        let nx = format!("{}{}{}{}{}", &pres[..ra.start], &pres[rb.clone()], &pres[ra.end..rb.start], &pres[ra.clone()], &pres[rb.end..]);
        self.set_part("ppt/presentation.xml", nx);
        self.order.swap(a, b);
        true
    }

    pub fn delete_slide(&mut self, ix: usize) -> bool {
        let (pres, _, ids) = self.sld_ids();
        if ids.len() <= 1 || ix >= ids.len() {
            return false;
        }
        let r = ids[ix].clone();
        let rid = attr(&pres[r.clone()], "r:id").unwrap_or_default();
        let nx = format!("{}{}", &pres[..r.start], &pres[r.end..]);
        self.set_part("ppt/presentation.xml", nx);
        let rp = rels_path("ppt/presentation.xml");
        if let Some(rels) = self.parts.get(&rp).cloned() {
            let cleaned = super::xlsx::remove_elements_matching(&rels, "Relationship", &format!("Id=\"{rid}\""));
            self.set_part(&rp, cleaned);
        }
        self.order.remove(ix);
        true
    }

    /// Copy slide `ix` right after itself.
    pub fn duplicate_slide(&mut self, ix: usize) -> Res<usize> {
        let src = self.order.get(ix).ok_or("no such slide")?.clone();
        let mut n = 1;
        while self.parts.contains_key(&format!("ppt/slides/slide{n}.xml")) {
            n += 1;
        }
        let new = format!("ppt/slides/slide{n}.xml");
        let xml = self.parts.get(&src).cloned().unwrap_or_default();
        self.set_part(&new, xml);
        if let Some(rels) = self.parts.get(&rels_path(&src)).cloned() {
            // Notes belong to the original slide.
            let rels = super::xlsx::remove_elements_matching(&rels, "Relationship", "/notesSlide\"");
            self.set_part(&rels_path(&new), rels);
        }
        let ct = self.parts.get("[Content_Types].xml").cloned().unwrap_or_default();
        let ov = format!("<Override PartName=\"/{new}\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>");
        let ct = match ct.rfind("</Types>") {
            Some(i) => format!("{}{ov}{}", &ct[..i], &ct[i..]),
            None => ct,
        };
        self.set_part("[Content_Types].xml", ct);
        let rp = rels_path("ppt/presentation.xml");
        let rels = self.parts.get(&rp).cloned().unwrap_or_default();
        let mut k = 1;
        while rels.contains(&format!("Id=\"rId{k}\"")) {
            k += 1;
        }
        let rid = format!("rId{k}");
        let rel = format!("<Relationship Id=\"{rid}\" Type=\"{SLIDE_TYPE}\" Target=\"slides/slide{n}.xml\"/>");
        let rels = match rels.rfind("</Relationships>") {
            Some(i) => format!("{}{rel}{}", &rels[..i], &rels[i..]),
            None => rels,
        };
        self.set_part(&rp, rels);
        let (pres, _, ids) = self.sld_ids();
        let max_id = ids.iter().filter_map(|r| attr(&pres[r.clone()], "id")).filter_map(|v| v.parse::<u32>().ok()).max().unwrap_or(255);
        let at = ids.get(ix).map(|r| r.end).ok_or("sldIdLst missing")?;
        let nx = format!("{}<p:sldId id=\"{}\" r:id=\"{rid}\"/>{}", &pres[..at], max_id + 1, &pres[at..]);
        self.set_part("ppt/presentation.xml", nx);
        self.order.insert(ix + 1, new);
        Ok(ix + 1)
    }

    pub fn save(&mut self) -> Res<()> {
        let changes = self.changed.iter().map(|k| (k.clone(), self.parts.get(k).map(|v| v.clone().into_bytes()))).collect();
        super::write_package(&self.path, &changes)?;
        self.changed.clear();
        self.dirty = false;
        Ok(())
    }
}

fn para_text(p: &str) -> String {
    let mut t = String::new();
    for (n, r) in children(p, 0..p.len()) {
        match n {
            "a:r" | "a:fld" => t.push_str(&text_of(&p[r], "a:t")),
            "a:br" => t.push('\u{b}'),
            _ => {}
        }
    }
    t
}

/// New paragraph text, keeping the template's pPr and first run's rPr.
fn regen_para(tpl: &str, line: &str) -> String {
    let ppr = element(tpl, "a:pPr").map(|r| tpl[r].to_string()).unwrap_or_default();
    let rpr = element(tpl, "a:r")
        .and_then(|r| element(&tpl[r.clone()], "a:rPr").map(|x| tpl[r][x].to_string()))
        .or_else(|| element(tpl, "a:endParaRPr").map(|r| tpl[r].replace("a:endParaRPr", "a:rPr")))
        .unwrap_or_default();
    let end = element(tpl, "a:endParaRPr").map(|r| tpl[r].to_string()).unwrap_or_default();
    let runs: String = line
        .split('\u{b}')
        .enumerate()
        .map(|(i, seg)| {
            let br = if i > 0 { format!("<a:br>{rpr}</a:br>").replace("<a:br></a:br>", "<a:br/>") } else { String::new() };
            if seg.is_empty() { br } else { format!("{br}<a:r>{rpr}<a:t>{}</a:t></a:r>", escape(seg)) }
        })
        .collect();
    format!("<a:p>{ppr}{runs}{end}</a:p>")
}

fn emu(t: &str, n: &str) -> Option<f32> {
    attr(t, n).and_then(|v| v.parse::<f64>().ok()).map(|v| (v / EMU_PT as f64) as f32)
}

fn xfrm_in(x: &str) -> Option<Rect> {
    let off = start_tag(x, element(x, "a:off")?.start);
    let ext = start_tag(x, element(x, "a:ext")?.start);
    Some(Rect { x: emu(off, "x")?, y: emu(off, "y")?, w: emu(ext, "cx")?, h: emu(ext, "cy")? })
}

fn xfrm(sp: &str) -> Option<Rect> {
    let sppr = &sp[element(sp, "p:spPr")?];
    xfrm_in(&sppr[element(sppr, "a:xfrm")?])
}

/// Group transform: (offset/ext on the parent, child offset/ext).
fn group_xfrm(gp: &str) -> Option<(Rect, Rect)> {
    let x = &gp[element(gp, "a:xfrm")?];
    let off = xfrm_in(x)?;
    let co = start_tag(x, element(x, "a:chOff")?.start);
    let ce = start_tag(x, element(x, "a:chExt")?.start);
    Some((off, Rect { x: emu(co, "x")?, y: emu(co, "y")?, w: emu(ce, "cx")?, h: emu(ce, "cy")? }))
}

fn anchor(body_pr: &str) -> Option<Anchor> {
    match attr(start_tag(body_pr, 0), "anchor")?.as_str() {
        "ctr" => Some(Anchor::Middle),
        "b" => Some(Anchor::Bottom),
        _ => Some(Anchor::Top),
    }
}

/// `a:lvlNpPr/a:defRPr sz` → sizes[N-1] (points).
fn level_sizes(x: &str, out: &mut [Option<f32>; 9]) {
    for (i, slot) in out.iter_mut().enumerate() {
        if slot.is_some() {
            continue;
        }
        let Some(r) = element(x, &format!("a:lvl{}pPr", i + 1)) else { continue };
        let l = &x[r];
        if let Some(d) = element(l, "a:defRPr") {
            *slot = attr(start_tag(l, d.start), "sz").and_then(|v| v.parse::<f32>().ok()).map(|v| v / 100.);
        }
    }
}

fn parse_theme(t: &str) -> HashMap<String, u32> {
    let mut out = HashMap::new();
    let Some(cs) = element(t, "a:clrScheme") else { return out };
    let cs = &t[cs];
    for (name, r) in children(cs, 0..cs.len()) {
        let key = name.trim_start_matches("a:").to_string();
        let el = &cs[r];
        for tg in super::tags(el) {
            let s = tg.src(el);
            let v = match tg.name {
                "a:srgbClr" => attr(s, "val"),
                "a:sysClr" => attr(s, "lastClr"),
                _ => None,
            };
            if let Some(v) = v.and_then(|v| u32::from_str_radix(&v, 16).ok()) {
                out.insert(key.clone(), v);
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn para_regen() {
        let tpl = r#"<a:p><a:pPr algn="ctr"/><a:r><a:rPr lang="es" sz="2400" b="1"/><a:t>Old</a:t></a:r><a:endParaRPr lang="es"/></a:p>"#;
        let p = regen_para(tpl, "New & <x>");
        assert_eq!(p, r#"<a:p><a:pPr algn="ctr"/><a:r><a:rPr lang="es" sz="2400" b="1"/><a:t>New &amp; &lt;x&gt;</a:t></a:r><a:endParaRPr lang="es"/></a:p>"#);
        assert_eq!(para_text(tpl), "Old");
        let r = xfrm(r#"<p:sp><p:spPr><a:xfrm><a:off x="12700" y="25400"/><a:ext cx="127000" cy="254000"/></a:xfrm></p:spPr></p:sp>"#).unwrap();
        assert_eq!(r, Rect { x: 1., y: 2., w: 10., h: 20. });
    }
}
