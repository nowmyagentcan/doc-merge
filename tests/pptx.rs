mod common;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use common::*;
use doc_merge::{Options, Report, merge};

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
