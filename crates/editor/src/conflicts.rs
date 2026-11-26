//! Git merge conflict blocks (`<<<<<<<` / `|||||||` / `=======` / `>>>>>>>`) in a buffer.

use crate::Document;

/// One conflict, as line numbers (0-based) of its markers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// `<<<<<<< HEAD` line.
    pub start: usize,
    /// `||||||| base` line (diff3 / zdiff3 style), if present.
    pub base: Option<usize>,
    /// `=======` line.
    pub sep: usize,
    /// `>>>>>>> branch` line.
    pub end: usize,
    /// Text after `<<<<<<<` (usually "HEAD").
    pub current_label: String,
    /// Text after `>>>>>>>` (the incoming branch or commit).
    pub incoming_label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    Current,
    Incoming,
    Both,
}

fn marker<'a>(line: &'a str, m: &str) -> Option<&'a str> {
    let line = line.trim_end_matches(['\r', '\n']);
    let rest = line.strip_prefix(m)?;
    (rest.is_empty() || rest.starts_with(' ')).then(|| rest.trim())
}

/// Conflict blocks in the document, in order. Incomplete blocks are ignored.
pub fn find(doc: &Document) -> Vec<Conflict> {
    find_in_lines((0..doc.line_count()).map(|l| doc.line(l)))
}

pub fn find_in_lines<S: AsRef<str>>(lines: impl Iterator<Item = S>) -> Vec<Conflict> {
    let mut out = Vec::new();
    let mut open: Option<(usize, String)> = None;
    let mut base = None;
    let mut sep = None;
    for (i, l) in lines.enumerate() {
        let l = l.as_ref();
        if let Some(label) = marker(l, "<<<<<<<") {
            open = Some((i, label.to_string()));
            base = None;
            sep = None;
        } else if open.is_some() && sep.is_none() && marker(l, "|||||||").is_some() {
            base = Some(i);
        } else if open.is_some() && marker(l, "=======").is_some() {
            sep = Some(i);
        } else if let Some(label) = marker(l, ">>>>>>>") {
            if let (Some((start, current_label)), Some(s)) = (open.take(), sep) {
                out.push(Conflict { start, base, sep: s, end: i, current_label, incoming_label: label.to_string() });
            }
            base = None;
            sep = None;
        }
    }
    out
}

/// Lines (0-based, half-open) of the "current" side's content.
pub fn current_lines(c: &Conflict) -> std::ops::Range<usize> {
    c.start + 1..c.base.unwrap_or(c.sep)
}

/// Lines (0-based, half-open) of the "incoming" side's content.
pub fn incoming_lines(c: &Conflict) -> std::ops::Range<usize> {
    c.sep + 1..c.end
}

/// Replace the whole block with the chosen side(s). One undo step.
pub fn resolve(doc: &mut Document, c: &Conflict, how: Resolution) {
    let take = |r: std::ops::Range<usize>| -> String { r.map(|l| doc.line(l).into_owned() + "\n").collect() };
    let text = match how {
        Resolution::Current => take(current_lines(c)),
        Resolution::Incoming => take(incoming_lines(c)),
        Resolution::Both => take(current_lines(c)) + &take(incoming_lines(c)),
    };
    let from = doc.pos(c.start, 0);
    // Through the end of the `>>>>>>>` line, including its newline when there is one.
    let to = if c.end + 1 < doc.line_count() { doc.pos(c.end + 1, 0) } else { doc.len_chars() };
    let text = if c.end + 1 >= doc.line_count() { text.strip_suffix('\n').map(str::to_string).unwrap_or(text) } else { text };
    doc.clear_extra();
    doc.selection = crate::Selection { anchor: from, head: to };
    doc.insert(&text);
    let at = from;
    doc.set_cursor(at, false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Language;

    const SRC: &str = "a\n<<<<<<< HEAD\nmine\n=======\ntheirs\n>>>>>>> feature\nb\n<<<<<<< HEAD\nx\n||||||| base\no\n=======\ny\n>>>>>>> other\n";

    #[test]
    fn finds_blocks_with_and_without_base() {
        let c = find_in_lines(SRC.lines());
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].start, c[0].base, c[0].sep, c[0].end), (1, None, 3, 5));
        assert_eq!((c[0].current_label.as_str(), c[0].incoming_label.as_str()), ("HEAD", "feature"));
        assert_eq!((c[1].start, c[1].base, c[1].sep, c[1].end), (7, Some(9), 11, 13));
        assert_eq!(current_lines(&c[1]), 8..9);
        assert_eq!(incoming_lines(&c[1]), 12..13);
    }

    #[test]
    fn ignores_incomplete_and_plain_equals() {
        assert!(find_in_lines("=======\n>>>>>>> x\n".lines()).is_empty());
        assert!(find_in_lines("<<<<<<< HEAD\na\n".lines()).is_empty());
        assert!(find_in_lines("<<<<<<<< not a marker\n=======\n>>>>>>> x\n".lines()).is_empty());
    }

    #[test]
    fn resolves_each_way() {
        for (how, want) in [
            (Resolution::Current, "a\nmine\nb\n"),
            (Resolution::Incoming, "a\ntheirs\nb\n"),
            (Resolution::Both, "a\nmine\ntheirs\nb\n"),
        ] {
            let mut d = Document::from_str("a\n<<<<<<< HEAD\nmine\n=======\ntheirs\n>>>>>>> feature\nb\n", Language::PlainText);
            let c = find(&d);
            resolve(&mut d, &c[0], how);
            assert_eq!(d.text(), want);
            assert!(find(&d).is_empty());
        }
    }

    #[test]
    fn resolves_block_at_end_without_newline() {
        let mut d = Document::from_str("a\n<<<<<<< HEAD\nmine\n=======\ntheirs\n>>>>>>> f", Language::PlainText);
        let c = find(&d);
        resolve(&mut d, &c[0], Resolution::Incoming);
        assert_eq!(d.text(), "a\ntheirs");
    }
}
