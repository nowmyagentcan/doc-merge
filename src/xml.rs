//! Reading XML parts within the limits: no DOCTYPE, only the five predefined entities and
//! character references, elements nested at most 256 deep. Names are matched by namespace
//! URI, never by prefix. Events come out exactly as read, so writing them back with
//! `quick_xml::Writer` gives the same bytes.

use std::borrow::Cow;

use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{QName, ResolveResult};
use quick_xml::{NsReader, XmlVersion};

use crate::Origin;
use crate::error::Error;

pub const MAX_DEPTH: usize = 256;

/// How many events go by between two looks at the clock.
const CLOCK_EVERY: u32 = 1 << 16;

/// A part's bytes as UTF-8 text. A UTF-8 byte-order mark is dropped; UTF-16 (with a mark, or
/// recognised from `<?`) is converted, with its declaration's encoding changed to UTF-8 so
/// that the text can be written back as it is.
pub fn decode<'b>(bytes: &'b [u8], part: &str, origin: &Origin) -> Result<Cow<'b, str>, Error> {
    let bad = || {
        origin.bad(&format!(
            "part {part} is not well-formed XML: it isn't UTF-8 or UTF-16"
        ))
    };
    let utf16 = |b: &[u8], big: bool| {
        if !b.len().is_multiple_of(2) {
            return Err(bad());
        }
        let units = b.as_chunks::<2>().0.iter().map(|&pair| {
            if big {
                u16::from_be_bytes(pair)
            } else {
                u16::from_le_bytes(pair)
            }
        });
        let text: String = char::decode_utf16(units)
            .collect::<Result<_, _>>()
            .map_err(|_| bad())?;
        Ok(Cow::Owned(utf8_declaration(text)))
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, false);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, true);
    }
    if bytes.starts_with(b"<\0?\0") || bytes.starts_with(b"\0<\0?") {
        return utf16(bytes, bytes[0] == 0);
    }
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    std::str::from_utf8(body)
        .map(Cow::Borrowed)
        .map_err(|_| bad())
}

/// `text` with the encoding in its XML declaration, if any, set to UTF-8.
fn utf8_declaration(text: String) -> String {
    let Some(end) = text.starts_with("<?xml").then(|| text.find("?>")).flatten() else {
        return text;
    };
    let Some(at) = text[..end].find("encoding") else {
        return text;
    };
    let value = text[at..end].find(['"', '\'']).map(|q| at + q);
    let close = value.and_then(|q| {
        text[q + 1..end]
            .find(text.as_bytes()[q] as char)
            .map(|c| q + 1 + c)
    });
    match (value, close) {
        (Some(q), Some(c)) => format!("{}UTF-8{}", &text[..q + 1], &text[c..]),
        _ => text,
    }
}

/// The events of one part, each checked against the limits before it is handed out.
pub struct Reader<'a> {
    inner: NsReader<&'a [u8]>,
    part: &'a str,
    origin: Origin,
    depth: usize,
    root: bool,
    events: u32,
}

impl<'a> Reader<'a> {
    pub fn new(text: &'a str, part: &'a str, origin: &Origin) -> Self {
        Self {
            inner: NsReader::from_str(text),
            part,
            origin: origin.clone(),
            depth: 0,
            root: false,
            events: 0,
        }
    }

    fn bad(&self, why: &str) -> Error {
        self.origin.bad(&format!("part {} {why}", self.part))
    }

    fn malformed(&self, detail: &str) -> Error {
        let at = self
            .inner
            .error_position()
            .max(self.inner.buffer_position());
        self.bad(&format!("is not well-formed XML: {detail} at byte {at}"))
    }

    /// The next event; `Eof` once the root element has closed.
    pub fn event(&mut self) -> Result<Event<'a>, Error> {
        self.events = self.events.wrapping_add(1);
        if self.events.is_multiple_of(CLOCK_EVERY) {
            self.origin.run.on_time()?;
        }
        let event = self
            .inner
            .read_event()
            .map_err(|e| self.malformed(&e.to_string()))?;
        match &event {
            Event::DocType(_) => return Err(self.bad("contains a DOCTYPE")),
            Event::Start(_) | Event::Empty(_) => {
                if self.depth == 0 && self.root {
                    return Err(self.malformed("a second root element"));
                }
                self.root = true;
                if self.depth == MAX_DEPTH {
                    return Err(self.bad("nests deeper than 256 elements"));
                }
                if matches!(event, Event::Start(_)) {
                    self.depth += 1;
                }
            }
            Event::End(_) => self.depth -= 1,
            Event::GeneralRef(r) => {
                let named = ["lt", "gt", "amp", "apos", "quot"].contains(&r.as_ref());
                let xml_char = |c: char| !matches!(c, '\0'..='\x08' | '\x0B' | '\x0C' | '\x0E'..='\x1F' | '\u{FFFE}' | '\u{FFFF}');
                if !named && !matches!(r.resolve_char_ref(), Ok(Some(c)) if xml_char(c)) {
                    let name: &str = r;
                    return Err(self.malformed(&format!("unknown entity &{name};")));
                }
            }
            Event::Text(t) if self.depth == 0 && !t.trim().is_empty() => {
                return Err(self.malformed("text outside the root element"));
            }
            Event::Eof if self.depth > 0 => return Err(self.malformed("unexpected end")),
            Event::Eof if !self.root => return Err(self.malformed("no root element")),
            _ => {}
        }
        Ok(event)
    }

    /// The elements open around the next event: 0 before and after the root.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// The namespace URI of an element; empty when it has none.
    pub fn element_ns(&self, e: &BytesStart) -> &str {
        uri(self.inner.resolver().resolve_element(e.name()).0)
    }

    /// The namespace URI of an attribute name on the current element; empty when it has none.
    pub fn attribute_ns(&self, name: QName) -> &str {
        uri(self.inner.resolver().resolve_attribute(name).0)
    }

    /// The namespace URI a prefix is bound to on the current element.
    fn prefix_ns(&self, prefix: &str) -> &str {
        let name = format!("{prefix}:x");
        uri(self
            .inner
            .resolver()
            .resolve_prefix(QName(&name).prefix(), false))
    }

    /// Each attribute of `e` that isn't a namespace declaration: (namespace, local name, value
    /// with references resolved and whitespace normalised).
    pub fn attributes(&self, e: &BytesStart<'a>) -> Result<Vec<(&str, String, String)>, Error> {
        let mut out = vec![];
        for a in e.attributes() {
            let a = a.map_err(|e| self.malformed(&e.to_string()))?;
            if a.key.as_namespace_binding().is_some() {
                continue;
            }
            let value = a
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|e| self.malformed(&e.to_string()))?;
            let local = a.key.local_name().as_ref().to_owned();
            out.push((self.attribute_ns(a.key), local, value.into_owned()));
        }
        Ok(out)
    }
}

fn uri(r: ResolveResult<'_>) -> &str {
    match r {
        ResolveResult::Bound(ns) => ns.0,
        _ => "",
    }
}

pub const MC: &str = "http://schemas.openxmlformats.org/markup-compatibility/2006";

/// The canonical form of XML, for telling whether two pieces mean the same: names by
/// namespace URI, attributes sorted, references resolved, whitespace-only text and comments,
/// processing instructions and the declaration left out, `<a/>` the same as `<a></a>`, and
/// the prefixes in `mc:Ignorable` resolved. Chosen elements (with everything inside them)
/// and attributes are left out too.
pub struct Canon<E, A> {
    out: String,
    text: String,
    skipping: usize,
    drop_element: E,
    drop_attribute: A,
}

impl<E, A> Canon<E, A>
where
    E: Fn(&str, &str) -> bool,
    A: Fn(&str, &str) -> bool,
{
    /// `drop_element(namespace, local)` and `drop_attribute(namespace, local)` say what to
    /// leave out.
    pub fn new(drop_element: E, drop_attribute: A) -> Self {
        Self {
            out: String::new(),
            text: String::new(),
            skipping: 0,
            drop_element,
            drop_attribute,
        }
    }

    /// Adds an event that `r` just handed out.
    pub fn push(&mut self, r: &Reader, event: &Event) -> Result<(), Error> {
        if self.skipping > 0 {
            match event {
                Event::Start(_) => self.skipping += 1,
                Event::End(_) => self.skipping -= 1,
                _ => {}
            }
            return Ok(());
        }
        match event {
            Event::Start(e) | Event::Empty(e) => {
                let ns = r.element_ns(e);
                let local = e.local_name().as_ref().to_owned();
                if (self.drop_element)(ns, &local) {
                    self.skipping = usize::from(matches!(event, Event::Start(_)));
                    return Ok(());
                }
                self.flush_text();
                let mut attrs: Vec<(String, String)> = vec![];
                for (ans, alocal, value) in r.attributes(e)? {
                    if (self.drop_attribute)(ans, &alocal) {
                        continue;
                    }
                    let value = if ans == MC {
                        let uris: Vec<&str> =
                            value.split_whitespace().map(|p| r.prefix_ns(p)).collect();
                        uris.join(" ")
                    } else {
                        value
                    };
                    attrs.push((format!("{{{ans}}}{alocal}"), value));
                }
                attrs.sort();
                self.out.push_str(&format!("<{{{ns}}}{local}"));
                for (name, value) in attrs {
                    self.out
                        .push_str(&format!(" {name}=\"{}\"", escape(&value)));
                }
                self.out.push('>');
                if matches!(event, Event::Empty(_)) {
                    self.out.push_str("</>");
                }
            }
            Event::End(_) => {
                self.flush_text();
                self.out.push_str("</>");
            }
            Event::Text(t) => self.text.push_str(&t.xml10_content()),
            Event::CData(t) => self.text.push_str(t),
            Event::GeneralRef(g) => {
                let c = match g.as_ref() {
                    "lt" => '<',
                    "gt" => '>',
                    "amp" => '&',
                    "apos" => '\'',
                    "quot" => '"',
                    _ => g.resolve_char_ref().ok().flatten().unwrap_or('\u{FFFD}'),
                };
                self.text.push(c);
            }
            _ => {}
        }
        Ok(())
    }

    fn flush_text(&mut self) {
        if !self.text.trim().is_empty() {
            self.out.push_str(&escape(&self.text));
        }
        self.text.clear();
    }

    pub fn finish(mut self) -> String {
        self.flush_text();
        self.out
    }
}

fn escape(s: &str) -> Cow<'_, str> {
    if s.contains(['<', '&', '"']) {
        Cow::Owned(
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('"', "&quot;"),
        )
    } else {
        Cow::Borrowed(s)
    }
}

/// The canonical form of a whole part (see [`Canon`]).
pub fn canonical(
    text: &str,
    part: &str,
    origin: &Origin,
    drop_element: impl Fn(&str, &str) -> bool,
    drop_attribute: impl Fn(&str, &str) -> bool,
) -> Result<String, Error> {
    let mut r = Reader::new(text, part, origin);
    let mut canon = Canon::new(drop_element, drop_attribute);
    loop {
        let event = r.event()?;
        if matches!(event, Event::Eof) {
            return Ok(canon.finish());
        }
        canon.push(&r, &event)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> Origin {
        Origin::test("a.docx")
    }

    fn events(text: &str) -> Result<usize, Error> {
        let o = origin();
        let mut r = Reader::new(text, "word/document.xml", &o);
        let mut n = 0;
        while !matches!(r.event()?, Event::Eof) {
            n += 1;
        }
        Ok(n)
    }

    fn message(text: &str) -> String {
        events(text).unwrap_err().message
    }

    #[test]
    fn refuses_doctype_and_unknown_entities() {
        let laughs = r#"<?xml version="1.0"?><!DOCTYPE lolz [<!ENTITY lol "lol">]><a>&lol;</a>"#;
        assert_eq!(
            message(laughs),
            "'a.docx' is damaged or unsafe: part word/document.xml contains a DOCTYPE."
        );
        assert!(message("<a>&lol;</a>").contains("not well-formed XML: unknown entity &lol;"));
        assert!(message("<a>&#0;</a>").contains("not well-formed"));
        assert_eq!(
            events("<a>&lt;&gt;&amp;&apos;&quot;&#x41;&#66;</a>").unwrap(),
            9
        );
    }

    #[test]
    fn depth_is_bounded() {
        let deep = |n| format!("{}{}", "<a>".repeat(n), "</a>".repeat(n));
        assert!(events(&deep(MAX_DEPTH)).is_ok());
        let e = events(&deep(MAX_DEPTH + 1)).unwrap_err();
        assert_eq!(e.code, "bad_package");
        assert!(
            e.message
                .ends_with("part word/document.xml nests deeper than 256 elements.")
        );
        let empty = format!(
            "{}<b/>{}",
            "<a>".repeat(MAX_DEPTH),
            "</a>".repeat(MAX_DEPTH)
        );
        assert!(events(&empty).is_err());
    }

    #[test]
    fn a_passed_deadline_stops_long_parts() {
        let o = Origin {
            run: crate::Run::new(Some(std::time::Instant::now())),
            ..origin()
        };
        let text = format!("<a>{}</a>", "<b/>".repeat(1 << 16));
        let mut r = Reader::new(&text, "x.xml", &o);
        let e = loop {
            if let Err(e) = r.event() {
                break e;
            }
        };
        assert_eq!(e.code, "timeout");
    }

    #[test]
    fn malformed_xml_is_reported_with_detail() {
        for bad in [
            "<a>",
            "<a></b>",
            "<a/><b/>",
            "",
            "text",
            "<a/>text",
            "<a x='1/>",
        ] {
            let e = events(bad).unwrap_err();
            assert!(
                e.message.contains("is not well-formed XML: "),
                "{bad}: {}",
                e.message
            );
            assert!(e.message.contains(" at byte "), "{bad}");
        }
    }

    #[test]
    fn unchanged_events_write_back_byte_identical() {
        let text = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" xmlns:mc=\"http://schemas.openxmlformats.org/markup-compatibility/2006\" mc:Ignorable=\"w14\"><!-- c --><w:body><w:p><w:r><w:t xml:space=\"preserve\"> a &amp; b &#x2014; &quot;c&quot; </w:t></w:r></w:p><![CDATA[x<y]]><?pi data?><w:sectPr  w:rsidR='00AB'/></w:body></w:document>";
        let o = origin();
        let mut r = Reader::new(text, "word/document.xml", &o);
        let mut w = quick_xml::Writer::new(Vec::new());
        loop {
            match r.event().unwrap() {
                Event::Eof => break,
                e => w.write_event(e).unwrap(),
            }
        }
        assert_eq!(String::from_utf8(w.into_inner()).unwrap(), text);
    }

    #[test]
    fn utf16_parts_become_utf8_with_their_declaration_fixed() {
        let text = "<?xml version=\"1.0\" encoding=\"UTF-16\" standalone=\"yes\"?><a>é</a>";
        let mut le = vec![0xFF, 0xFE];
        le.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        let o = origin();
        assert_eq!(
            decode(&le, "x.xml", &o).unwrap(),
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><a>é</a>"
        );
        let be: Vec<u8> = "<?xml version='1.0'?><a/>"
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();
        assert_eq!(
            decode(&be, "x.xml", &o).unwrap(),
            "<?xml version='1.0'?><a/>"
        );
        assert_eq!(decode(b"\xEF\xBB\xBF<a/>", "x.xml", &o).unwrap(), "<a/>");
        assert!(decode(b"<a>\xFF</a>", "x.xml", &o).is_err());
    }

    fn canon(text: &str) -> String {
        canonical(
            text,
            "x.xml",
            &origin(),
            |_, local| local == "name",
            |_, local| local == "rsid",
        )
        .unwrap()
    }

    #[test]
    fn canonical_form_ignores_spelling_but_not_meaning() {
        let a = r#"<w:style xmlns:w="urn:w" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:x="urn:x" mc:Ignorable="x" w:type="paragraph" w:styleId="H"><w:name w:val="heading 1"/><w:rPr>
            <w:b/><w:sz w:val="28" w:rsid="1"/></w:rPr></w:style>"#;
        let b = r#"<s:style xmlns:s="urn:w" xmlns:m="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:y="urn:x" s:styleId='H' s:type="paragraph" m:Ignorable="y"><s:rPr><s:b></s:b><s:sz s:val="&#50;8"/></s:rPr></s:style>"#;
        assert_eq!(canon(a), canon(b));
        assert_eq!(
            canon(b),
            r#"<{urn:w}style {http://schemas.openxmlformats.org/markup-compatibility/2006}Ignorable="urn:x" {urn:w}styleId="H" {urn:w}type="paragraph"><{urn:w}rPr><{urn:w}b></><{urn:w}sz {urn:w}val="28"></></></>"#
        );
        let changed = b.replace(r#"s:val="&#50;8""#, r#"s:val="24""#);
        assert_ne!(canon(b), canon(&changed));
        assert_ne!(canon("<a>x</a>"), canon("<a>y</a>"));
        assert_eq!(
            canon("<a> x &amp; <!-- c -->y</a>"),
            canon("<a> x &amp; y</a>")
        );
        assert_ne!(
            canon(r#"<a xmlns="urn:1"/>"#),
            canon(r#"<a xmlns="urn:2"/>"#)
        );
    }
}
