mod common;

use std::path::{Path, PathBuf};

use common::*;
use doc_merge::xlsx::a1::tokens;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// The text of every `name` element in an XML part, with the common references resolved.
fn elements(xml: &str, name: &str) -> Vec<String> {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    xml.split(&open)
        .skip(1)
        .filter(|s| s.starts_with(['>', ' ']))
        .filter_map(|s| {
            let end = s.find('>')?;
            let body = s[end + 1..].split(&close).next()?;
            (!s[..end].ends_with('/')).then(|| {
                body.replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&quot;", "\"")
                    .replace("&amp;", "&")
            })
        })
        .collect()
}

#[test]
fn every_formula_in_the_fixtures_tokenizes_back_to_itself() {
    let mut seen = 0;
    for name in ["a.xlsx", "b.xlsx", "c.xlsx", "p.xlsx"] {
        for (part, bytes) in entries(&fixture(name)) {
            if !part.ends_with(".xml") {
                continue;
            }
            let xml = String::from_utf8_lossy(&bytes);
            for element in ["f", "xm:f", "definedName", "calculatedColumnFormula", "c:f"] {
                for f in elements(&xml, element) {
                    let t = tokens(&f).unwrap_or_else(|| panic!("{name} {part}: {f}"));
                    assert_eq!(t.iter().map(|t| t.text).collect::<String>(), f);
                    seen += 1;
                }
            }
        }
    }
    assert!(seen > 25, "only {seen} formulas");
}
