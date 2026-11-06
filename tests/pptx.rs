mod common;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use common::*;
use doc_merge::{Options, Report};

type Parts = Vec<(String, Vec<u8>)>;

const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PML: &str = "application/vnd.openxmlformats-officedocument.presentationml.";
const THEME_TYPE: &str = "application/vnd.openxmlformats-officedocument.theme+xml";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn merged_files(test: &str, paths: &[PathBuf]) -> (Report, Parts) {
    let out = temp_dir(test).join("out.pptx");
    let report = merge(paths, &out, &Options::default(), None).unwrap();
    let parts = entries(&out);
    (report, parts)
}

fn merged(test: &str, names: &[&str]) -> (Report, Parts) {
    let paths: Vec<PathBuf> = names.iter().map(|n| fixture(n)).collect();
    merged_files(test, &paths)
}

fn names<'a>(parts: &'a Parts, start: &str) -> Vec<&'a str> {
    let wanted = |n: &&str| n.starts_with(start) && n.ends_with(".xml");
    parts.iter().map(|p| p.0.as_str()).filter(wanted).collect()
}

/// The values of attribute `attr` on every element `element` of `xml`.
fn values(xml: &str, element: &str, attr: &str) -> Vec<String> {
    xml.split(&format!("<{element} "))
        .skip(1)
        .filter_map(|s| {
            let tag = &s[..s.find('>')?];
            let at = tag
                .find(&format!(" {attr}=\""))
                .map(|i| i + 1)
                .or_else(|| tag.starts_with(&format!("{attr}=\"")).then_some(0))?;
            let rest = &tag[at + attr.len() + 2..];
            Some(rest[..rest.find('"')?].to_string())
        })
        .collect()
}

/// The part a relationship of `rels_part` with this id points at, from the folder `dir`.
fn target_of(parts: &Parts, rels_part: &str, id: &str, dir: &str) -> String {
    let rels = text(parts, rels_part);
    let tag = rels
        .split("<Relationship ")
        .find(|t| t.contains(&format!("Id=\"{id}\"")))
        .unwrap_or_else(|| panic!("no {id} in {rels_part}"));
    let target = tag.split("Target=\"").nth(1).unwrap();
    let target = &target[..target.find('"').unwrap()];
    match target.strip_prefix("../") {
        Some(t) => format!("ppt/{t}"),
        None => format!("{dir}/{target}"),
    }
}

/// The slide parts of a merged deck, in show order.
fn slides(parts: &Parts) -> Vec<String> {
    let main = text(parts, "ppt/presentation.xml");
    values(&main, "p:sldId", "r:id")
        .iter()
        .map(|id| target_of(parts, "ppt/_rels/presentation.xml.rels", id, "ppt"))
        .collect()
}

fn rels_of(part: &str) -> String {
    let (dir, file) = part.rsplit_once('/').unwrap();
    format!("{dir}/_rels/{file}.rels")
}

/// What a slide's title says.
fn said(parts: &Parts, slide: &str) -> String {
    let xml = text(parts, slide);
    let at = xml.find("<a:t>").unwrap() + 5;
    xml[at..at + xml[at..].find("</a:t>").unwrap()].to_string()
}

/// A deck of `slides` slides on one master with one layout, its slide size given.
fn deck(slides: usize, size: (u64, u64)) -> Parts {
    let (cx, cy) = size;
    let root = |tag: &str, body: &str| {
        format!(r#"<p:{tag} xmlns:a="{A}" xmlns:r="{R}" xmlns:p="{P}">{body}</p:{tag}>"#)
    };
    let tree = |shapes: &str| {
        format!(
            r#"<p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>{shapes}</p:spTree></p:cSld>"#
        )
    };
    let shape = |text: &str| {
        format!(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="T"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="1000" y="2000"/><a:ext cx="3000" cy="4000"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:p><a:r><a:rPr sz="2000"/><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp>"#
        )
    };
    let mut slide_ids = String::new();
    let mut main_rels = vec![(
        "rId1".to_string(),
        format!("{R}/slideMaster"),
        "slideMasters/slideMaster1.xml".to_string(),
    )];
    let mut parts: Vec<(String, String, String)> = vec![];
    for n in 1..=slides {
        let rid = format!("rId{}", n + 1);
        slide_ids += &format!(r#"<p:sldId id="{}" r:id="{rid}"/>"#, 255 + n);
        main_rels.push((rid, format!("{R}/slide"), format!("slides/slide{n}.xml")));
        parts.push((
            format!("ppt/slides/slide{n}.xml"),
            format!("{PML}slide+xml"),
            root("sld", &tree(&shape(&format!("Slide {n}")))),
        ));
        parts.push((
            format!("ppt/slides/_rels/slide{n}.xml.rels"),
            String::new(),
            rels(&[(
                "rId1",
                &format!("{R}/slideLayout"),
                "../slideLayouts/slideLayout1.xml",
            )]),
        ));
    }
    let master = format!(
        r#"{}<p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst>"#,
        tree(&shape("Master"))
    );
    parts.push((
        "ppt/slideMasters/slideMaster1.xml".into(),
        format!("{PML}slideMaster+xml"),
        root("sldMaster", &master),
    ));
    parts.push((
        "ppt/slideMasters/_rels/slideMaster1.xml.rels".into(),
        String::new(),
        rels(&[
            (
                "rId1",
                &format!("{R}/slideLayout"),
                "../slideLayouts/slideLayout1.xml",
            ),
            ("rId2", &format!("{R}/theme"), "../theme/theme1.xml"),
        ]),
    ));
    parts.push((
        "ppt/slideLayouts/slideLayout1.xml".into(),
        format!("{PML}slideLayout+xml"),
        root("sldLayout", &tree(&shape("Layout"))),
    ));
    parts.push((
        "ppt/slideLayouts/_rels/slideLayout1.xml.rels".into(),
        String::new(),
        rels(&[(
            "rId1",
            &format!("{R}/slideMaster"),
            "../slideMasters/slideMaster1.xml",
        )]),
    ));
    parts.push((
        "ppt/theme/theme1.xml".into(),
        THEME_TYPE.into(),
        format!(r#"<a:theme xmlns:a="{A}" name="T"><a:themeElements/></a:theme>"#),
    ));
    let list = |name: &str, body: &str| {
        if body.is_empty() {
            String::new()
        } else {
            format!("<p:{name}>{body}</p:{name}>")
        }
    };
    let main = root(
        "presentation",
        &format!(
            r#"<p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>{}<p:sldSz cx="{cx}" cy="{cy}"/><p:notesSz cx="6858000" cy="9144000"/>"#,
            list("sldIdLst", &slide_ids)
        ),
    );
    let extra: Vec<(&str, &str, &str)> = parts
        .iter()
        .map(|(n, t, x)| (n.as_str(), t.as_str(), x.as_str()))
        .collect();
    let mut built = minimal("pptx", &extra);
    set(&mut built, "ppt/presentation.xml", main.as_bytes());
    let list: Vec<(&str, &str, &str)> = main_rels
        .iter()
        .map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str()))
        .collect();
    set(
        &mut built,
        "ppt/_rels/presentation.xml.rels",
        rels(&list).as_bytes(),
    );
    built
}

fn file(dir: &Path, name: &str, parts: &Parts) -> PathBuf {
    write(dir, name, &build(parts))
}

const WIDE: (u64, u64) = (12_192_000, 6_858_000);

#[test]
fn slides_follow_in_input_order_with_their_notes() {
    let (report, parts) = merged("order", &["a.pptx", "b.pptx", "c.pptx"]);
    let ranges: Vec<_> = report.inputs.iter().map(|i| i.output_slides).collect();
    assert_eq!(ranges, [Some([1, 4]), Some([5, 7]), Some([8, 10])]);
    let order: Vec<String> = slides(&parts).iter().map(|s| said(&parts, s)).collect();
    assert_eq!(
        order,
        [
            "Deck A",
            "Chart of A",
            "Detail of A",
            "Picture of A",
            "Deck B",
            "Hidden slide of B",
            "Picture of B",
            "Deck C",
            "Table of C",
            "Video of C"
        ]
    );
    let notes_masters = names(&parts, "ppt/notesMasters/notesMaster");
    assert_eq!(notes_masters, ["ppt/notesMasters/notesMaster1.xml"]);
    let notes = names(&parts, "ppt/notesSlides/notesSlide");
    assert_eq!(notes.len(), 8);
    for n in notes {
        let (dir, file) = n.rsplit_once('/').unwrap();
        let rels = text(&parts, &format!("{dir}/_rels/{file}.rels"));
        assert!(rels.contains("../notesMasters/notesMaster1.xml"), "{n}");
    }
}

#[test]
fn ids_stay_unique_and_in_range_with_masters_before_their_layouts() {
    let (_, parts) = merged("ids", &["a.pptx", "b.pptx", "c.pptx"]);
    let main = text(&parts, "ppt/presentation.xml");
    let number = |v: &String| v.parse::<u64>().unwrap();
    let slide_ids: Vec<u64> = values(&main, "p:sldId", "id").iter().map(number).collect();
    assert_eq!(slide_ids.len(), 10);
    assert_eq!(slide_ids.iter().collect::<HashSet<_>>().len(), 10);
    assert!(slide_ids.iter().all(|id| (256..1 << 31).contains(id)));
    let mut pool = HashSet::new();
    let rels = "ppt/_rels/presentation.xml.rels";
    let masters = values(&main, "p:sldMasterId", "r:id");
    for (id, rid) in values(&main, "p:sldMasterId", "id").iter().zip(&masters) {
        let id = number(id);
        assert!(id >= 1 << 31 && pool.insert(id));
        let xml = text(&parts, &target_of(&parts, rels, rid, "ppt"));
        let layouts: Vec<u64> = values(&xml, "p:sldLayoutId", "id")
            .iter()
            .map(number)
            .collect();
        assert_eq!(layouts.len(), 11);
        for layout in layouts {
            assert!(layout > id && pool.insert(layout), "{layout} after {id}");
        }
    }
}

#[test]
fn parts_left_alone_keep_their_bytes_and_two_runs_agree() {
    let (_, first) = merged("same", &["a.pptx", "b.pptx", "c.pptx"]);
    let (_, second) = merged("same", &["a.pptx", "b.pptx", "c.pptx"]);
    assert_eq!(first, second);
    let rewritten = [
        "[Content_Types].xml",
        "ppt/presentation.xml",
        "ppt/_rels/presentation.xml.rels",
        "docProps/app.xml",
        "ppt/tableStyles.xml",
    ];
    for (name, bytes) in entries(&fixture("a.pptx")) {
        if !rewritten.contains(&name.as_str()) {
            let found = first.iter().find(|p| p.0 == name).unwrap();
            assert_eq!(found.1, bytes, "{name}");
        }
    }
    let same = entries(&fixture("b.pptx"))
        .into_iter()
        .find(|p| p.0 == "ppt/slides/slide1.xml")
        .unwrap();
    let copy = slides(&first)[4].clone();
    let copied = first.iter().find(|p| p.0 == copy).unwrap();
    assert_eq!(copied.1, same.1, "b's first slide is copied as stored");
}

#[test]
fn templates_and_shows_become_plain_decks() {
    let dir = temp_dir("kinds");
    for kind in ["potx", "ppsx"] {
        let mut base = deck(1, WIDE);
        let (_, t, _) = kind_parts(kind);
        let (_, plain, _) = kind_parts("pptx");
        let types = text(&base, "[Content_Types].xml").replace(&plain, &t);
        set(&mut base, "[Content_Types].xml", types.as_bytes());
        let paths = [
            file(&dir, &format!("base.{kind}"), &base),
            file(&dir, "more.pptx", &deck(2, WIDE)),
        ];
        let (report, parts) = merged_files("kinds-out", &paths);
        assert_eq!(serde_json::to_value(report.output.kind).unwrap(), "pptx");
        assert!(text(&parts, "[Content_Types].xml").contains(&plain));
        assert_eq!(slides(&parts).len(), 3);
    }
}

#[test]
fn a_deck_without_slides_adds_none() {
    let dir = temp_dir("empty");
    let paths = [
        file(&dir, "none.pptx", &deck(0, WIDE)),
        file(&dir, "two.pptx", &deck(2, WIDE)),
        file(&dir, "none2.pptx", &deck(0, WIDE)),
    ];
    let (report, parts) = merged_files("empty-out", &paths);
    let ranges: Vec<_> = report.inputs.iter().map(|i| i.output_slides).collect();
    assert_eq!(ranges, [Some([0, 0]), Some([1, 2]), Some([0, 0])]);
    assert_eq!(slides(&parts).len(), 2);
}

#[test]
fn a_notes_master_comes_from_the_first_deck_that_has_one() {
    let dir = temp_dir("notes");
    let plain = file(&dir, "plain.pptx", &deck(1, WIDE));
    let paths = [plain, fixture("b.pptx"), fixture("a.pptx")];
    let (_, parts) = merged_files("notes-out", &paths);
    let main = text(&parts, "ppt/presentation.xml");
    let order = |s: &str| main.find(s).unwrap_or_else(|| panic!("no {s}"));
    assert!(order("<p:sldMasterIdLst") < order("<p:notesMasterIdLst"));
    assert!(order("<p:notesMasterIdLst") < order("<p:sldIdLst"));
    assert_eq!(names(&parts, "ppt/notesMasters/notesMaster").len(), 1);
    assert_eq!(names(&parts, "ppt/notesSlides/notesSlide").len(), 7);
}

#[test]
fn more_than_ten_thousand_slides_are_refused() {
    let dir = temp_dir("limit");
    let mut many = deck(1, WIDE);
    let main = text(&many, "ppt/presentation.xml");
    let one = r#"<p:sldId id="256" r:id="rId2"/>"#;
    let list = (0..5001)
        .map(|n| one.replace("256", &(256 + n).to_string()))
        .collect::<String>();
    set(
        &mut many,
        "ppt/presentation.xml",
        main.replace(one, &list).as_bytes(),
    );
    let paths = [file(&dir, "a.pptx", &many), file(&dir, "b.pptx", &many)];
    let out = dir.join("out.pptx");
    let e = merge(&paths, &out, &Options::default(), None).unwrap_err();
    assert_eq!(e.code, "limit_exceeded");
    assert!(e.message.contains("10,002"), "{}", e.message);
    assert!(!out.exists());
}

#[test]
fn a_slide_without_a_layout_is_damaged() {
    let dir = temp_dir("layout");
    let mut broken = deck(1, WIDE);
    set(
        &mut broken,
        "ppt/slides/_rels/slide1.xml.rels",
        rels(&[]).as_bytes(),
    );
    let paths = [
        file(&dir, "a.pptx", &deck(1, WIDE)),
        file(&dir, "b.pptx", &broken),
    ];
    let e = merge(&paths, &dir.join("o.pptx"), &Options::default(), None).unwrap_err();
    assert_eq!((e.code, e.input), ("bad_package", Some(2)));
}

/// The masters a merged deck ends up with, by file: how many of each kind of part.
fn counts(parts: &Parts) -> (usize, usize, usize) {
    (
        names(parts, "ppt/slideMasters/slideMaster").len(),
        names(parts, "ppt/slideLayouts/slideLayout").len(),
        names(parts, "ppt/theme/theme").len(),
    )
}

fn replace(parts: &mut Parts, name: &str, from: &str, to: &str) {
    let xml = text(parts, name);
    assert!(xml.contains(from), "{from} not in {name}");
    set(parts, name, xml.replace(from, to).as_bytes());
}

/// Gives the layout a picture.
fn with_picture(parts: &mut Parts, bytes: &[u8]) {
    let layout = "ppt/slideLayouts/_rels/slideLayout1.xml.rels";
    let picture = format!(
        r#"<Relationship Id="rId7" Type="{R}/image" Target="../media/image1.png"/></Relationships>"#
    );
    replace(parts, layout, "</Relationships>", &picture);
    set(parts, "ppt/media/image1.png", bytes);
}

fn two_decks(test: &str, first: &Parts, second: &Parts) -> (Report, Parts) {
    let dir = temp_dir(test);
    let paths = [file(&dir, "a.pptx", first), file(&dir, "b.pptx", second)];
    merged_files(&format!("{test}-out"), &paths)
}

#[test]
fn decks_of_one_template_share_their_master() {
    let (_, parts) = merged("share", &["a.pptx", "b.pptx", "c.pptx"]);
    // a's master and b's are one; c has its own. Each design has a theme, the notes master another.
    assert_eq!(counts(&parts), (2, 22, 3));
    let order = slides(&parts);
    let layout_of = |slide: &str| {
        let rels = text(&parts, &rels_of(slide));
        let tag = rels
            .split("<Relationship ")
            .find(|t| t.contains("/slideLayout\""))
            .unwrap();
        let at = tag.find("slideLayouts/").unwrap();
        tag[at..at + tag[at..].find('"').unwrap()].to_string()
    };
    assert_eq!(layout_of(&order[4]), "slideLayouts/slideLayout1.xml");
    assert_eq!(
        layout_of(&order[6]),
        layout_of(&order[3]),
        "b's and a's picture layouts"
    );
    let c = layout_of(&order[7]);
    let number: usize = c["slideLayouts/slideLayout".len()..]
        .trim_end_matches(".xml")
        .parse()
        .unwrap();
    assert!(number > 11, "c keeps a layout of its own: {c}");
}

#[test]
fn a_deck_merged_with_itself_keeps_one_master() {
    let (_, parts) = merged("itself", &["c.pptx", "c.pptx"]);
    assert_eq!(counts(&parts), (1, 11, 2));
    assert_eq!(slides(&parts).len(), 6);
}

#[test]
fn spelling_does_not_tell_masters_apart() {
    let first = deck(1, WIDE);
    let mut second = deck(1, WIDE);
    // Another relationship id, another layout id, an edit marker and other prefixes on the theme.
    let master = "ppt/slideMasters/slideMaster1.xml";
    replace(
        &mut second,
        master,
        r#"id="2147483649" r:id="rId1""#,
        r#"id="2147483700" r:id="rId5""#,
    );
    replace(
        &mut second,
        master,
        r#"<a:rPr sz="2000"/>"#,
        r#"<a:rPr sz="2000" dirty="0"/>"#,
    );
    let rels = "ppt/slideMasters/_rels/slideMaster1.xml.rels";
    replace(&mut second, rels, r#"Id="rId1""#, r#"Id="rId5""#);
    let theme = "ppt/theme/theme1.xml";
    let xml = text(&second, theme)
        .replace("a:", "d:")
        .replace("xmlns:a", "xmlns:d");
    set(&mut second, theme, xml.as_bytes());
    let (_, parts) = two_decks("spelling", &first, &second);
    assert_eq!(counts(&parts), (1, 1, 1));
}

#[test]
fn any_difference_in_the_design_keeps_both_masters() {
    let first = deck(1, WIDE);
    let changes = [
        ("ppt/slideMasters/slideMaster1.xml", "Master", "Other"),
        ("ppt/slideLayouts/slideLayout1.xml", "Layout", "Other"),
        ("ppt/theme/theme1.xml", r#"name="T""#, r#"name="U""#),
    ];
    for (part, from, to) in changes {
        let mut second = deck(1, WIDE);
        replace(&mut second, part, from, to);
        let (_, parts) = two_decks("differ", &first, &second);
        assert_eq!(counts(&parts), (2, 2, 2), "{part}");
    }
}

#[test]
fn another_slide_size_keeps_both_masters() {
    let (_, parts) = two_decks("sizes", &deck(1, WIDE), &deck(1, (9_144_000, 6_858_000)));
    assert_eq!(counts(&parts).0, 2);
}

#[test]
fn pictures_of_a_design_are_compared_by_their_bytes() {
    let picture = b"first picture bytes";
    let mut first = deck(1, WIDE);
    with_picture(&mut first, picture);
    let mut same = deck(1, WIDE);
    with_picture(&mut same, picture);
    let (_, parts) = two_decks("pictures", &first, &same);
    assert_eq!(counts(&parts), (1, 1, 1));

    let head = b"other picture b";
    let mut forged = head.to_vec();
    forged.extend(forge_crc(head, crc32(picture)));
    assert_eq!(
        (forged.len(), crc32(&forged)),
        (picture.len(), crc32(picture))
    );
    let mut other = deck(1, WIDE);
    with_picture(&mut other, &forged);
    let (_, parts) = two_decks("forged", &first, &other);
    assert_eq!(counts(&parts).0, 2, "equal size and CRC, other bytes");
}

#[test]
fn a_bad_slide_size_is_damaged() {
    let dir = temp_dir("size");
    let bad = deck(1, (0, 6_858_000));
    let paths = [
        file(&dir, "a.pptx", &deck(1, WIDE)),
        file(&dir, "b.pptx", &bad),
    ];
    let e = merge(&paths, &dir.join("o.pptx"), &Options::default(), None).unwrap_err();
    assert_eq!((e.code, e.input), ("bad_package", Some(2)));
}

#[test]
fn a_four_by_three_deck_is_centred_in_a_wide_one() {
    let (report, parts) = merged("centre", &["a.pptx", "c.pptx"]);
    let w = report.warnings.list();
    assert_eq!(w.len(), 1);
    assert_eq!((w[0].code, w[0].input), ("slides_scaled", Some(2)));
    assert_eq!(
        w[0].message,
        "Slides from c.pptx (4:3) were scaled to fit the 16:9 slide size and centred."
    );
    let source = entries(&fixture("c.pptx"));
    let table = text(&source, "ppt/slides/slide2.xml");
    let order = slides(&parts);
    let copy = text(&parts, &order[5]);
    let offsets = |xml: &str| {
        let x = values(xml, "a:off", "x");
        let x: Vec<i64> = x.iter().map(|v| v.parse().unwrap()).collect();
        (x, values(xml, "a:off", "y"))
    };
    let ((before, y_before), (after, y_after)) = (offsets(&table), offsets(&copy));
    assert_eq!(before[0], 0, "the slide's own group");
    assert_eq!(after[0], 0);
    assert!(before.len() > 1);
    for (b, a) in before.iter().zip(&after).skip(1) {
        assert_eq!(a - b, 1_524_000);
    }
    assert_eq!(y_before, y_after);
    assert_eq!(
        values(&table, "a:gridCol", "w"),
        values(&copy, "a:gridCol", "w"),
        "nothing is scaled at the same height"
    );
    // The design moves with its slides: the master's band is centred too.
    let master = "ppt/slideMasters/slideMaster2.xml";
    let band = offsets(&text(&parts, master)).0;
    let source_band = offsets(&text(&source, "ppt/slideMasters/slideMaster1.xml")).0;
    assert_eq!(
        band.last().unwrap() - source_band.last().unwrap(),
        1_524_000
    );
}

#[test]
fn notes_pages_are_not_fitted() {
    let (_, parts) = merged("notes-fit", &["a.pptx", "c.pptx"]);
    let source = entries(&fixture("c.pptx"));
    let notes = text(&source, "ppt/notesSlides/notesSlide1.xml");
    assert!(parts.iter().any(|p| p.1 == notes.as_bytes()));
}

/// Puts `shapes` after the shape of slide 1.
fn with_shapes(parts: &mut Parts, shapes: &str) {
    let closing = format!("{shapes}</p:spTree>");
    replace(parts, "ppt/slides/slide1.xml", "</p:spTree>", &closing);
}

#[test]
fn slides_of_another_size_are_scaled_by_the_smaller_ratio_then_centred() {
    let mut small = deck(1, (4_000_000, 3_000_000));
    with_shapes(
        &mut small,
        concat!(
            r#"<p:grpSp><p:nvGrpSpPr/><p:grpSpPr><a:xfrm><a:off x="100" y="200"/><a:ext cx="300" cy="400"/><a:chOff x="500" y="600"/><a:chExt cx="700" cy="800"/></a:xfrm></p:grpSpPr>"#,
            r#"<p:sp><p:nvSpPr/><p:spPr><a:xfrm><a:off x="10" y="20"/><a:ext cx="30" cy="40"/></a:xfrm></p:spPr></p:sp></p:grpSp>"#,
            r#"<p:graphicFrame><p:nvGraphicFramePr/><p:xfrm><a:off x="1000" y="1000"/><a:ext cx="2000" cy="2000"/></p:xfrm><a:graphic><a:graphicData uri="x"><a:tbl><a:tblGrid><a:gridCol w="1000"/></a:tblGrid><a:tr h="500"/></a:tbl></a:graphicData></a:graphic></p:graphicFrame>"#,
            r#"<mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><mc:Choice Requires="x"><p:sp><p:nvSpPr/><p:spPr><a:xfrm><a:off x="2" y="4"/></a:xfrm></p:spPr></p:sp></mc:Choice><mc:Fallback><p:pic><p:nvPicPr/><p:spPr><a:xfrm><a:off x="6" y="8"/></a:xfrm></p:spPr></p:pic></mc:Fallback></mc:AlternateContent>"#,
            r#"<p:sp><p:nvSpPr><p:cNvPr id="9" name="X"><a:extLst><a:ext uri="{K}"/></a:extLst></p:cNvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:p><a:pPr defTabSz="914400"/><a:r><a:rPr sz="110"/><a:t>x</a:t></a:r><a:endParaRPr sz="1000"/></a:p></p:txBody></p:sp>"#,
        ),
    );
    // Twice the width but one and a half times the height: scaled by 1.5, 1,000,000 left over
    // on each side.
    let (report, parts) = two_decks("scale", &deck(1, (8_000_000, 4_500_000)), &small);
    assert_eq!(report.warnings.list()[0].code, "slides_scaled");
    let xml = text(&parts, &slides(&parts)[1]);
    for wanted in [
        r#"<a:off x="1001500" y="3000"/><a:ext cx="4500" cy="6000"/>"#,
        r#"<a:off x="1000150" y="300"/><a:ext cx="450" cy="600"/><a:chOff x="750" y="900"/><a:chExt cx="1050" cy="1200"/>"#,
        r#"<a:off x="15" y="30"/><a:ext cx="45" cy="60"/>"#,
        r#"<p:xfrm><a:off x="1001500" y="1500"/><a:ext cx="3000" cy="3000"/></p:xfrm>"#,
        r#"<a:gridCol w="1500"/>"#,
        r#"<a:tr h="750"/>"#,
        r#"<a:off x="1000003" y="6"/>"#,
        r#"<a:off x="1000009" y="12"/>"#,
        r#"<a:ext uri="{K}"/>"#,
        r#"defTabSz="1371600""#,
        r#"<a:rPr sz="165"/>"#,
        r#"<a:endParaRPr sz="1500"/>"#,
        r#"<a:rPr sz="3000"/>"#,
        r#"<a:off x="0" y="0"/><a:ext cx="0" cy="0"/>"#,
    ] {
        assert!(xml.contains(wanted), "{wanted} not in {xml}");
    }
    let master = text(&parts, "ppt/slideMasters/slideMaster2.xml");
    assert!(
        master.contains(r#"<a:off x="1001500" y="3000"/>"#),
        "{master}"
    );
    let layout = text(&parts, "ppt/slideLayouts/slideLayout2.xml");
    assert!(
        layout.contains(r#"<a:off x="1001500" y="3000"/>"#),
        "{layout}"
    );
}

#[test]
fn small_fonts_stop_at_the_smallest_size_a_file_may_state() {
    let mut big = deck(1, (24_000_000, 13_500_000));
    let run = r#"<a:rPr sz="2000"/>"#;
    let small = r#"<a:rPr sz="150"/>"#;
    replace(&mut big, "ppt/slides/slide1.xml", run, small);
    let (_, parts) = two_decks("fonts", &deck(1, (2_400_000, 1_350_000)), &big);
    let xml = text(&parts, &slides(&parts)[1]);
    assert!(xml.contains(r#"<a:rPr sz="100"/>"#), "{xml}");
}

#[test]
fn two_decks_of_one_other_size_share_their_fitted_master() {
    let dir = temp_dir("fitted");
    let paths = [
        file(&dir, "a.pptx", &deck(1, WIDE)),
        file(&dir, "b.pptx", &deck(1, (9_144_000, 6_858_000))),
        file(&dir, "c.pptx", &deck(2, (9_144_000, 6_858_000))),
    ];
    let (report, parts) = merged_files("fitted-out", &paths);
    assert_eq!(counts(&parts), (2, 2, 2));
    assert_eq!(report.warnings.list().len(), 2);
}

#[test]
fn an_odd_slide_size_reads_as_a_decimal_ratio() {
    let (report, _) = two_decks("ratio", &deck(1, WIDE), &deck(1, (9_144_001, 6_858_000)));
    let message = &report.warnings.list()[0].message;
    assert!(message.contains("(1.33:1)"), "{message}");
}

/// Each section of a merged deck: its name and how many slides it holds.
fn sections(parts: &Parts) -> Vec<(String, usize)> {
    let main = text(parts, "ppt/presentation.xml");
    main.split("<p14:section ")
        .skip(1)
        .map(|s| {
            let at = s.find("name=\"").unwrap() + 6;
            let name = s[at..at + s[at..].find('"').unwrap()].to_string();
            (name, s.matches("<p14:sldId ").count())
        })
        .collect()
}

#[test]
fn every_deck_keeps_its_sections_or_gets_one_named_after_it() {
    let (_, parts) = merged("sections", &["a.pptx", "b.pptx", "c.pptx"]);
    let list = sections(&parts);
    let wanted = [("Opening", 2), ("Details", 2), ("b", 3), ("c", 3)];
    let wanted: Vec<(String, usize)> = wanted.map(|(n, c)| (n.to_string(), c)).to_vec();
    assert_eq!(list, wanted);
    let main = text(&parts, "ppt/presentation.xml");
    let ids = values(&main, "p:sldId", "id");
    assert_eq!(values(&main, "p14:sldId", "id"), ids, "in slide order");
}

#[test]
fn a_base_without_sections_gets_one_when_a_later_deck_has_some() {
    let (_, parts) = merged("sections-b", &["b.pptx", "a.pptx"]);
    let names: Vec<String> = sections(&parts).into_iter().map(|s| s.0).collect();
    assert_eq!(names, ["b", "Opening", "Details"]);
    let main = text(&parts, "ppt/presentation.xml");
    assert!(main.ends_with("</p:extLst></p:presentation>"));
}

#[test]
fn no_sections_are_written_when_no_deck_has_any() {
    let (_, parts) = two_decks("no-sections", &deck(1, WIDE), &deck(1, WIDE));
    assert!(!text(&parts, "ppt/presentation.xml").contains("sectionLst"));
}

#[test]
fn the_same_deck_twice_gets_new_section_and_creation_ids() {
    let (_, parts) = merged("twice", &["a.pptx", "a.pptx"]);
    let main = text(&parts, "ppt/presentation.xml");
    let guids = values(&main, "p14:section", "id");
    assert_eq!(guids.len(), 4);
    assert_eq!(guids.iter().collect::<HashSet<_>>().len(), 4);
    assert!(guids.iter().all(|g| g.len() == 38 && g.starts_with('{')));
    let creation = |slide: &String| values(&text(&parts, slide), "p14:creationId", "val");
    let all: Vec<_> = slides(&parts).iter().flat_map(creation).collect();
    assert_eq!(all.len(), 8);
    assert_eq!(all.iter().collect::<HashSet<_>>().len(), 8);
}

#[test]
fn a_section_entry_for_a_missing_slide_is_left_out() {
    let mut stale = deck(2, WIDE);
    let p14 = "http://schemas.microsoft.com/office/powerpoint/2010/main";
    let list = format!(
        r#"<p:extLst><p:ext uri="{{521415D9-36F7-43E2-AB2F-B90AF26B5E84}}"><p14:sectionLst xmlns:p14="{p14}"><p14:section name="S" id="{{00000000-0000-0000-0000-000000000001}}"><p14:sldIdLst><p14:sldId id="256"/><p14:sldId id="999"/><p14:sldId id="257"/></p14:sldIdLst></p14:section></p14:sectionLst></p:ext></p:extLst></p:presentation>"#
    );
    replace(
        &mut stale,
        "ppt/presentation.xml",
        "</p:presentation>",
        &list,
    );
    let (_, parts) = two_decks("stale", &deck(1, WIDE), &stale);
    assert_eq!(
        sections(&parts),
        [("a".to_string(), 1), ("S".to_string(), 2)]
    );
}

#[test]
fn custom_shows_are_renamed_on_a_clash_and_keep_their_slides() {
    let (report, parts) = merged("shows", &["a.pptx", "b.pptx", "c.pptx"]);
    assert_eq!(report.renames.len(), 1);
    let rename = &report.renames[0];
    assert_eq!(
        (
            rename.input,
            rename.kind,
            rename.from.as_str(),
            rename.to.as_str()
        ),
        (3, "custom_show", "Short", "Short (3)")
    );
    let main = text(&parts, "ppt/presentation.xml");
    assert_eq!(values(&main, "p:custShow", "name"), ["Short", "Short (3)"]);
    assert_eq!(values(&main, "p:custShow", "id"), ["0", "1"]);
    let list = &main[main.find(r#"name="Short (3)""#).unwrap()..];
    let rid = &values(list, "p:sld", "r:id")[0];
    let slide = target_of(&parts, "ppt/_rels/presentation.xml.rels", rid, "ppt");
    assert_eq!(said(&parts, &slide), "Table of C");
    // The button on c's table slide runs c's show, a's button still runs a's.
    let order = slides(&parts);
    assert!(text(&parts, &order[3]).contains(r#"action="ppaction://customshow?id=0""#));
    assert!(text(&parts, &order[8]).contains(r#"action="ppaction://customshow?id=1""#));
}

#[test]
fn a_show_name_taken_by_a_rename_moves_on() {
    let show = |name: &str| {
        format!(
            r#"<p:custShowLst><p:custShow name="{name}" id="4"><p:sldLst><p:sld r:id="rId2"/></p:sldLst></p:custShow></p:custShowLst></p:presentation>"#
        )
    };
    let with_show = |name: &str| {
        let mut d = deck(1, WIDE);
        replace(
            &mut d,
            "ppt/presentation.xml",
            "</p:presentation>",
            &show(name),
        );
        d
    };
    let dir = temp_dir("show-names");
    let paths = [
        file(&dir, "a.pptx", &with_show("One (2)")),
        file(&dir, "b.pptx", &with_show("one")),
        file(&dir, "c.pptx", &with_show("ONE (2)")),
    ];
    let (report, parts) = merged_files("show-names-out", &paths);
    let main = text(&parts, "ppt/presentation.xml");
    let names = values(&main, "p:custShow", "name");
    assert_eq!(names, ["One (2)", "one", "ONE (2) (3)"]);
    assert_eq!(values(&main, "p:custShow", "id"), ["4", "5", "6"]);
    assert_eq!(report.renames.len(), 1);
}

#[test]
fn slide_jump_links_land_on_the_copy_of_their_slide() {
    let (_, parts) = merged("jump", &["b.pptx", "a.pptx"]);
    let order = slides(&parts);
    assert_eq!(said(&parts, &order[3]), "Deck A");
    let xml = text(&parts, &order[3]);
    let link = xml.find("hlinksldjump").expect("a's jump");
    let rid = &values(&xml[..link + 15], "a:hlinkClick", "r:id")[0];
    let dir = order[3].rsplit_once('/').unwrap().0;
    let landed = target_of(&parts, &rels_of(&order[3]), rid, dir);
    assert_eq!(landed, order[5]);
    assert_eq!(said(&parts, &landed), "Detail of A");
}

#[test]
fn a_link_on_a_layout_follows_its_show() {
    let mut second = deck(1, WIDE);
    let main = r#"<p:custShowLst><p:custShow name="S" id="7"><p:sldLst><p:sld r:id="rId2"/></p:sldLst></p:custShow></p:custShowLst></p:presentation>"#;
    replace(
        &mut second,
        "ppt/presentation.xml",
        "</p:presentation>",
        main,
    );
    let link =
        r#"<a:hlinkClick r:id="" action="ppaction://customshow?id=7&amp;return=true"/></a:rPr>"#;
    let layout = "ppt/slideLayouts/slideLayout1.xml";
    replace(
        &mut second,
        layout,
        r#"<a:rPr sz="2000"/>"#,
        &format!(r#"<a:rPr sz="2000">{link}"#),
    );
    let (_, parts) = two_decks("layout-link", &deck(1, WIDE), &second);
    let copy = text(&parts, "ppt/slideLayouts/slideLayout2.xml");
    assert!(
        copy.contains("ppaction://customshow?id=0&amp;return=true"),
        "{copy}"
    );
}

/// Adds a legacy comment on slide 1 with its author list.
fn with_comment(parts: &mut Parts, author: (&str, &str, u32, u32), idx: u32) {
    let (name, initials, id, last) = author;
    let types = "application/vnd.openxmlformats-officedocument.presentationml.";
    let list = format!(
        r#"<p:cmAuthorLst xmlns:p="{P}"><p:cmAuthor id="{id}" name="{name}" initials="{initials}" lastIdx="{last}" clrIdx="0"/></p:cmAuthorLst>"#
    );
    let comment = format!(
        r#"<p:cmLst xmlns:p="{P}"><p:cm authorId="{id}" dt="2026-01-01T00:00:00.000" idx="{idx}"><p:pos x="1" y="2"/><p:text>x</p:text><p:extLst><p:ext uri="{{C676402C-5697-4E1C-873F-D02D1690AC5C}}"><p15:threadingInfo xmlns:p15="urn:p15" timeZoneBias="0"><p15:parentCm authorId="{id}" idx="{idx}"/></p15:threadingInfo></p:ext></p:extLst></p:cm></p:cmLst>"#
    );
    set(parts, "ppt/commentAuthors.xml", list.as_bytes());
    set(parts, "ppt/comments/comment1.xml", comment.as_bytes());
    let overrides = format!(
        r#"<Override PartName="/ppt/commentAuthors.xml" ContentType="{types}commentAuthors+xml"/><Override PartName="/ppt/comments/comment1.xml" ContentType="{types}comments+xml"/></Types>"#
    );
    replace(parts, "[Content_Types].xml", "</Types>", &overrides);
    let main = format!(
        r#"<Relationship Id="rId90" Type="{R}/commentAuthors" Target="commentAuthors.xml"/></Relationships>"#
    );
    replace(
        parts,
        "ppt/_rels/presentation.xml.rels",
        "</Relationships>",
        &main,
    );
    let slide = format!(
        r#"<Relationship Id="rId90" Type="{R}/comments" Target="../comments/comment1.xml"/></Relationships>"#
    );
    replace(
        parts,
        "ppt/slides/_rels/slide1.xml.rels",
        "</Relationships>",
        &slide,
    );
}

#[test]
fn legacy_authors_match_by_name_and_initials_and_their_numbers_go_on() {
    let mut first = deck(1, WIDE);
    with_comment(&mut first, ("Pat", "P", 3, 4), 4);
    let mut same = deck(1, WIDE);
    with_comment(&mut same, ("Pat", "P", 0, 2), 2);
    let (_, parts) = two_decks("legacy-same", &first, &same);
    let list = text(&parts, "ppt/commentAuthors.xml");
    assert_eq!(values(&list, "p:cmAuthor", "id"), ["3"]);
    assert_eq!(values(&list, "p:cmAuthor", "lastIdx"), ["6"]);
    let copy = text(&parts, "ppt/comments/comment2.xml");
    assert_eq!(values(&copy, "p:cm", "authorId"), ["3"]);
    assert_eq!(values(&copy, "p:cm", "idx"), ["6"]);
    assert_eq!(values(&copy, "p15:parentCm", "authorId"), ["3"]);
    assert_eq!(values(&copy, "p15:parentCm", "idx"), ["6"]);

    let mut other = deck(1, WIDE);
    with_comment(&mut other, ("Pat", "PX", 3, 2), 2);
    let (_, parts) = two_decks("legacy-other", &first, &other);
    let list = text(&parts, "ppt/commentAuthors.xml");
    assert_eq!(values(&list, "p:cmAuthor", "id"), ["3", "4"]);
    assert_eq!(values(&list, "p:cmAuthor", "lastIdx"), ["4", "2"]);
    let copy = text(&parts, "ppt/comments/comment2.xml");
    assert_eq!(values(&copy, "p:cm", "authorId"), ["4"]);
    assert_eq!(values(&copy, "p:cm", "idx"), ["2"]);
}

#[test]
fn comment_authors_are_created_when_only_an_appended_deck_has_them() {
    let (_, parts) = merged("authors", &["c.pptx", "b.pptx", "a.pptx"]);
    let legacy = text(&parts, "ppt/commentAuthors.xml");
    assert_eq!(values(&legacy, "p:cmAuthor", "name"), ["Author A"]);
    let modern = text(&parts, "ppt/authors.xml");
    assert_eq!(values(&modern, "p188:author", "name"), ["Author A"]);
    let rels = text(&parts, "ppt/_rels/presentation.xml.rels");
    assert!(rels.contains("/commentAuthors\" Target=\"commentAuthors.xml\""));
    assert!(rels.contains("/authors\" Target=\"authors.xml\""));
    let types = text(&parts, "[Content_Types].xml");
    assert!(types.contains("application/vnd.ms-powerpoint.authors+xml"));
}

#[test]
fn modern_comments_follow_their_slide() {
    let (_, parts) = merged("modern", &["a.pptx", "a.pptx"]);
    assert_eq!(
        values(&text(&parts, "ppt/authors.xml"), "p188:author", "id").len(),
        1
    );
    let order = slides(&parts);
    let main = text(&parts, "ppt/presentation.xml");
    let ids = values(&main, "p:sldId", "id");
    let comment_of = |slide: &str| {
        let rels = text(&parts, &rels_of(slide));
        let at = rels.find("comments/modernComment").unwrap();
        let name = &rels[at..at + rels[at..].find('"').unwrap()];
        text(&parts, &format!("ppt/{name}"))
    };
    let (first, second) = (comment_of(&order[2]), comment_of(&order[6]));
    assert_eq!(values(&second, "pc:sldMk", "sldId"), [ids[6].clone()]);
    assert_eq!(
        values(&second, "pc:sldMk", "cId"),
        values(&text(&parts, &order[6]), "p14:creationId", "val")
    );
    assert_ne!(
        values(&first, "p188:cm", "id"),
        values(&second, "p188:cm", "id")
    );
}

#[test]
fn embedded_fonts_are_added_once_by_typeface() {
    let (_, parts) = merged("fonts-add", &["a.pptx", "c.pptx", "c.pptx"]);
    let main = text(&parts, "ppt/presentation.xml");
    assert_eq!(values(&main, "p:font", "typeface"), ["Merge Box"]);
    assert!(main.contains(r#"embedTrueTypeFonts="1""#));
    let at = |s: &str| main.find(s).unwrap_or_else(|| panic!("no {s}"));
    assert!(at("<p:notesSz") < at("<p:embeddedFontLst"));
    assert!(at("<p:embeddedFontLst") < at("<p:custShowLst"));
    let rid = &values(&main, "p:regular", "r:id")[0];
    let font = target_of(&parts, "ppt/_rels/presentation.xml.rels", rid, "ppt");
    assert_eq!(font, "ppt/fonts/font1.fntdata");
    assert!(parts.iter().any(|p| p.0 == font && !p.1.is_empty()));
    let fonts = parts
        .iter()
        .filter(|p| p.0.starts_with("ppt/fonts/"))
        .count();
    assert_eq!(fonts, 1);
}

#[test]
fn table_styles_are_added_by_their_id() {
    let (_, parts) = merged("tables", &["a.pptx", "c.pptx", "c.pptx"]);
    let list = text(&parts, "ppt/tableStyles.xml");
    let names = values(&list, "a:tblStyle", "styleName");
    assert_eq!(names.iter().filter(|n| *n == "Deck C table").count(), 1);
    let source = text(&entries(&fixture("a.pptx")), "ppt/tableStyles.xml");
    assert_eq!(
        values(&list, "a:tblStyleLst", "def"),
        values(&source, "a:tblStyleLst", "def")
    );
}

#[test]
fn a_table_style_list_with_nothing_new_keeps_its_bytes() {
    let (_, parts) = merged("tables-same", &["a.pptx", "b.pptx"]);
    let source = text(&entries(&fixture("a.pptx")), "ppt/tableStyles.xml");
    assert_eq!(text(&parts, "ppt/tableStyles.xml"), source);
}

#[test]
fn the_counts_of_the_document_properties_follow_the_slides() {
    let (_, parts) = merged("counts", &["a.pptx", "b.pptx", "c.pptx"]);
    let app = text(&parts, "docProps/app.xml");
    assert!(app.contains("<Slides>10</Slides>"), "{app}");
    assert!(app.contains("<Notes>8</Notes>"), "{app}");
    assert!(app.contains("<HiddenSlides>1</HiddenSlides>"), "{app}");
}

/// A zoom object, as a slide holds it.
fn zoom(ns: &str) -> String {
    format!(
        r#"<mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><mc:Choice xmlns:z="{ns}" Requires="z"><p:graphicFrame><p:nvGraphicFramePr/><p:xfrm/><a:graphic><a:graphicData uri="{ns}"><z:sectionZm/></a:graphicData></a:graphic></p:graphicFrame></mc:Choice></mc:AlternateContent>"#
    )
}

#[test]
fn zoom_is_refused_in_an_appended_deck_and_kept_in_the_base() {
    let dir = temp_dir("zoom");
    for kind in ["sectionzoom", "slidezoom", "summaryzoom"] {
        let ns = format!("http://schemas.microsoft.com/office/powerpoint/2016/{kind}");
        let mut z = deck(1, WIDE);
        with_shapes(&mut z, &zoom(&ns));
        let z_path = file(&dir, "z.pptx", &z);
        let out = dir.join("out.pptx");
        let paths = [fixture("a.pptx"), z_path.clone()];
        let e = merge(&paths, &out, &Options::default(), None).unwrap_err();
        assert_eq!((e.code, e.input), ("unsupported_variant", Some(2)));
        assert_eq!(
            e.message,
            "'z.pptx' has slide or section zoom, which doc-merge can't merge yet."
        );
        assert!(!out.exists());
        let paths = [z_path, fixture("b.pptx")];
        merge(&paths, &out, &Options::default(), None).unwrap();
        std::fs::remove_file(&out).unwrap();
    }
}

#[test]
fn protection_and_unknown_parts_of_an_appended_deck_are_reported() {
    let mut second = deck(1, WIDE);
    let main = r#"<p:modifyVerifier cryptProviderType="rsaAES"/></p:presentation>"#;
    replace(
        &mut second,
        "ppt/presentation.xml",
        "</p:presentation>",
        main,
    );
    set(&mut second, "ppt/extra.xml", b"<x/>");
    let rel = format!(
        r#"<Relationship Id="rId91" Type="{R}/gadget" Target="extra.xml"/></Relationships>"#
    );
    replace(
        &mut second,
        "ppt/_rels/presentation.xml.rels",
        "</Relationships>",
        &rel,
    );
    let (report, parts) = two_decks("left", &deck(1, WIDE), &second);
    let codes: Vec<&str> = report.warnings.list().iter().map(|w| w.code).collect();
    assert_eq!(codes, ["protection_not_kept", "part_dropped"]);
    assert!(parts.iter().all(|p| p.0 != "ppt/extra.xml"));
    assert!(!text(&parts, "ppt/presentation.xml").contains("modifyVerifier"));
}

#[test]
fn a_data_store_reached_from_a_slide_comes_with_a_free_id() {
    let store = |parts: &mut Parts| {
        let item = r#"<root/>"#;
        let props = r#"<ds:datastoreItem xmlns:ds="http://schemas.openxmlformats.org/officeDocument/2006/customXml" ds:itemID="{11111111-2222-3333-4444-555555555555}"/>"#;
        set(parts, "customXml/item1.xml", item.as_bytes());
        set(parts, "customXml/itemProps1.xml", props.as_bytes());
        set(
            parts,
            "customXml/_rels/item1.xml.rels",
            rels(&[("rId1", &format!("{R}/customXmlProps"), "itemProps1.xml")]).as_bytes(),
        );
        let overrides = r#"<Override PartName="/customXml/itemProps1.xml" ContentType="application/vnd.openxmlformats-officedocument.customXmlProperties+xml"/></Types>"#;
        replace(parts, "[Content_Types].xml", "</Types>", overrides);
        let rel = format!(
            r#"<Relationship Id="rId92" Type="{R}/customXml" Target="../../customXml/item1.xml"/></Relationships>"#
        );
        replace(
            parts,
            "ppt/slides/_rels/slide1.xml.rels",
            "</Relationships>",
            &rel,
        );
    };
    let mut d = deck(1, WIDE);
    store(&mut d);
    let (_, parts) = two_decks("store", &d, &d);
    let ids = |name: &str| values(&text(&parts, name), "ds:datastoreItem", "ds:itemID");
    let (first, second) = (
        ids("customXml/itemProps1.xml"),
        ids("customXml/itemProps2.xml"),
    );
    assert_eq!(first, ["{11111111-2222-3333-4444-555555555555}"]);
    assert_eq!(second.len(), 1);
    assert_ne!(first, second);
}

#[test]
fn part_names_with_spaces_are_followed_and_kept_escaped() {
    let mut first = deck(1, WIDE);
    let picture = format!(
        r#"<Relationship Id="rId7" Type="{R}/image" Target="../media/my%20pic.png"/></Relationships>"#
    );
    let rels = "ppt/slides/_rels/slide1.xml.rels";
    replace(&mut first, rels, "</Relationships>", &picture);
    set(&mut first, "ppt/media/my pic.png", b"picture");
    let mut second = first.clone();
    replace(
        &mut second,
        "ppt/media/my pic.png",
        "picture",
        "other bytes",
    );
    let (_, parts) = two_decks("spaces", &first, &second);
    assert!(parts.iter().any(|p| p.0 == "ppt/media/my pic.png"));
    assert!(parts.iter().any(|p| p.0 == "ppt/media/my pic1.png"));
    let copy = rels_of(&slides(&parts)[1]);
    assert!(text(&parts, &copy).contains("../media/my%20pic1.png"));
}

#[test]
#[ignore = "known bug: DM16"]
fn a_notes_master_only_related_and_not_listed_is_still_the_decks_one() {
    let dir = temp_dir("unlisted-notes");
    let mut base = entries(&fixture("a.pptx"));
    let main = text(&base, "ppt/presentation.xml");
    let from = main.find("<p:notesMasterIdLst").unwrap();
    let to = main.find("</p:notesMasterIdLst>").unwrap() + "</p:notesMasterIdLst>".len();
    let unlisted = format!("{}{}", &main[..from], &main[to..]);
    set(&mut base, "ppt/presentation.xml", unlisted.as_bytes());
    let paths = [file(&dir, "a.pptx", &base), fixture("b.pptx")];
    let (_, parts) = merged_files("unlisted-notes-out", &paths);
    let rels = text(&parts, "ppt/_rels/presentation.xml.rels");
    assert_eq!(
        rels.matches("relationships/notesMaster\"").count(),
        1,
        "{rels}"
    );
    assert_eq!(names(&parts, "ppt/notesMasters/notesMaster").len(), 1);
    let main = text(&parts, "ppt/presentation.xml");
    assert_eq!(main.matches("<p:notesMasterId ").count(), 1, "{main}");
}

#[test]
#[ignore = "known bug: DM18"]
fn a_base_part_pointing_at_nothing_is_not_a_fault_of_ours() {
    let dir = temp_dir("dangling");
    let mut second = entries(&fixture("b.pptx"));
    let name = "ppt/slides/_rels/slide1.xml.rels";
    let rels = text(&second, name).replace(
        "</Relationships>",
        &format!(r#"<Relationship Id="rId99" Type="{R}/image" Target="/"/></Relationships>"#),
    );
    set(&mut second, name, rels.as_bytes());
    let paths = [file(&dir, "b.pptx", &second), fixture("a.pptx")];
    let merged = merge(&paths, &dir.join("o.pptx"), &Options::default(), None);
    let code = merged.err().map(|e| e.code);
    assert_ne!(code.as_deref(), Some("internal"));
}
