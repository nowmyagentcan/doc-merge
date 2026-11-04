//! Excel: what an appended workbook may not hold. Each is a workbook-level registry
//! cross-linked by hidden ids, so a partial copy would corrupt the output; the base keeps its
//! own, since nothing of the base's is renamed or renumbered.

use quick_xml::events::Event;

use super::{Book, DYNAMIC_ARRAYS, S};
use crate::error::Error;
use crate::package::{MAX_PARSED, Package};
use crate::xml::{self, Reader};

/// Each variant with the folders and the last word of the part or relationship types that
/// give it away.
const VARIANTS: &[(&str, &[&str], &[&str])] = &[
    (
        "pivot tables",
        &["xl/pivottables/", "xl/pivotcache/"],
        &["pivotTable", "pivotCacheDefinition", "pivotCacheRecords"],
    ),
    (
        "slicers",
        &["xl/slicers/", "xl/slicercaches/"],
        &["slicer", "slicerCache"],
    ),
    (
        "timelines",
        &["xl/timelines/", "xl/timelinecaches/"],
        &["timeline", "timelineCache"],
    ),
    ("a data model", &["xl/model/"], &[]),
    ("data connections", &[], &["connections"]),
    ("query tables", &["xl/querytables/"], &["queryTable"]),
    ("images in cells", &["xl/richdata/"], &[]),
    ("XML maps", &[], &["xmlMaps"]),
    ("shared-workbook revision history", &["xl/revisions/"], &[]),
    ("dialog sheets", &["xl/dialogsheets/"], &["dialogsheet"]),
];

/// The last word of a content type (`….pivotTable+xml`) or relationship type (`…/slicer`).
fn last_word(kind: &str) -> &str {
    let word = kind.rsplit(['.', '/']).next().unwrap_or(kind);
    word.strip_suffix("+xml").unwrap_or(word)
}

fn is_mashup(pkg: &mut Package, part: &str) -> Result<bool, Error> {
    let bytes = pkg.read(part, MAX_PARSED)?;
    // A custom XML part never reaches the output, so a damaged one refuses nothing.
    let Ok(text) = xml::decode(&bytes, part, &pkg.origin) else {
        return Ok(false);
    };
    let mut r = Reader::new(&text, part, &pkg.origin);
    while let Ok(event) = r.event() {
        match event {
            Event::Start(e) | Event::Empty(e) => {
                return Ok(e.local_name().as_ref() == "DataMashup");
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(false)
}

fn variant(pkg: &mut Package, book: &Book) -> Result<Option<&'static str>, Error> {
    for (name, folders, kinds) in VARIANTS {
        let in_folder = |part: &str| {
            let part = part.to_ascii_lowercase();
            folders.iter().any(|f| part.starts_with(f))
        };
        let of_kind = |kind: &str| kinds.contains(&last_word(kind));
        let part = |name: &str| in_folder(name) || pkg.types.of(name).is_some_and(of_kind);
        if pkg.entries.iter().any(|e| part(&e.name)) || book.rels.iter().any(|r| of_kind(&r.kind)) {
            return Ok(Some(name));
        }
    }
    let items: Vec<String> = pkg
        .entries
        .iter()
        .map(|e| e.name.clone())
        .filter(|n| n.to_ascii_lowercase().starts_with("customxml/item"))
        .filter(|n| n.ends_with(".xml"))
        .collect();
    for part in items {
        if is_mashup(pkg, &part)? {
            return Ok(Some("Power Query queries"));
        }
    }
    let metadata = book.held(pkg, "sheetMetadata")?;
    let types = metadata
        .iter()
        .flat_map(|h| h.tree.child(S, "metadataTypes"));
    let mut names = types.flat_map(|t| t.nodes()).map(|n| n.attr("", "name"));
    if names.any(|n| n != Some(DYNAMIC_ARRAYS)) {
        return Ok(Some("cell metadata other than dynamic arrays"));
    }
    Ok(None)
}

fn date_1904(book: &Book) -> bool {
    let properties = book.tree.child(S, "workbookPr");
    let value = properties.and_then(|p| p.attr("", "date1904"));
    matches!(value, Some("1" | "true"))
}

/// Refuses an appended workbook this merge can't take, before anything is planned.
pub fn check(pkg: &mut Package, book: &Book, base: &Book, first: &str) -> Result<(), Error> {
    let origin = pkg.origin.clone();
    if let Some(name) = variant(pkg, book)? {
        return Err(Error::unsupported_variant(&origin.name, name).at(origin.input));
    }
    let own = date_1904(book);
    if own != date_1904(base) {
        return Err(Error::date_system_differs(&origin.name, first, own).at(origin.input));
    }
    Ok(())
}
