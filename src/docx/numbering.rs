//! Lists: every list an appended document uses is copied with new ids and a new `w:nsid`, so it
//! restarts as it did in its source and Word never joins it to another list.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;

use crate::docx::W;
use crate::docx::styles::reference;
use crate::error::Error;
use crate::xml::{Attr, Child, Node};

fn id(n: &Node, local: &str) -> u64 {
    n.attr(W, local).and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// The lists of one document.
#[derive(Default)]
pub struct Lists {
    pub bullets: Vec<Node>,
    pub abstracts: Vec<Node>,
    pub nums: Vec<Node>,
}

impl Lists {
    pub fn new(root: Option<&Node>) -> Self {
        let mut lists = Lists::default();
        for n in root.iter().flat_map(|r| r.nodes()) {
            if n.is(W, "numPicBullet") {
                lists.bullets.push(n.clone());
            } else if n.is(W, "abstractNum") {
                lists.abstracts.push(n.clone());
            } else if n.is(W, "num") {
                lists.nums.push(n.clone());
            }
        }
        lists
    }

    /// The `w:numId` of every list.
    pub fn ids(&self) -> HashSet<String> {
        let ids = self.nums.iter().filter_map(|n| n.attr(W, "numId"));
        ids.map(String::from).collect()
    }

    fn abstract_of(&self, num: &str) -> Option<&Node> {
        let n = self.nums.iter().find(|n| n.attr(W, "numId") == Some(num))?;
        let wanted = reference(n, "abstractNumId")?;
        self.abstracts
            .iter()
            .find(|a| a.attr(W, "abstractNumId") == Some(wanted))
    }

    /// What a list looks like, for comparing the styles that use it: its abstract numbering
    /// without what only identifies it.
    pub fn form(&self, num: &str) -> String {
        let mut out = String::new();
        if let Some(a) = self.abstract_of(num) {
            let mut a = a.clone();
            a.attrs.retain(|x| x.local != "abstractNumId");
            a.children
                .retain(|c| !matches!(c, Child::Node(n) if n.is(W, "nsid") || n.is(W, "tmpl")));
            a.canonical(&mut out);
        }
        out
    }

    /// The numbering styles a list is tied to.
    pub fn links(&self, num: &str) -> Vec<String> {
        let list = self.abstract_of(num);
        ["styleLink", "numStyleLink"]
            .iter()
            .filter_map(|l| list.and_then(|a| reference(a, l)).map(String::from))
            .collect()
    }
}

/// The output's lists: the base's and, after them, the copies.
#[derive(Default)]
pub struct Output {
    pub lists: Lists,
    nsids: HashSet<String>,
    base: [usize; 3],
}

/// How one document's lists landed in the output.
#[derive(Default)]
pub struct Copied {
    /// Old to new `w:numId`s.
    pub nums: HashMap<String, String>,
    pub bullets: Range<usize>,
    pub abstracts: Range<usize>,
    pub added_nums: Range<usize>,
}

impl Output {
    pub fn new(lists: Lists) -> Self {
        let base = [lists.bullets.len(), lists.abstracts.len(), lists.nums.len()];
        let nsids = lists
            .abstracts
            .iter()
            .filter_map(|a| reference(a, "nsid").map(str::to_ascii_uppercase))
            .collect();
        Self { lists, nsids, base }
    }

    /// The copies so far: picture bullets, abstract numberings and `w:num`s.
    pub fn added(&self) -> [&[Node]; 3] {
        [
            &self.lists.bullets[self.base[0]..],
            &self.lists.abstracts[self.base[1]..],
            &self.lists.nums[self.base[2]..],
        ]
    }

    /// An `nsid` no list in the output has.
    fn new_nsid(&mut self, input: usize, old: u64) -> String {
        let free = (0u32..).find_map(|attempt| {
            let mut h = DefaultHasher::new();
            (input, old, attempt).hash(&mut h);
            let nsid = format!("{:08X}", h.finish() as u32);
            (!self.nsids.contains(&nsid)).then_some(nsid)
        });
        let nsid = free.expect("a free value");
        self.nsids.insert(nsid.clone());
        nsid
    }

    /// Copies the `w:num`s in `used` with the abstract numberings and picture bullets they
    /// reach. `input` is the document's position, for new `nsid`s; `rel` copies what a picture
    /// bullet's relationship points to and returns the new id.
    pub fn copy(
        &mut self,
        doc: &Lists,
        used: &BTreeSet<String>,
        input: usize,
        rel: &mut dyn FnMut(&str) -> Result<String, Error>,
    ) -> Result<Copied, Error> {
        let starts = [
            self.lists.bullets.len(),
            self.lists.abstracts.len(),
            self.lists.nums.len(),
        ];
        let mut copied = Copied::default();
        let mut abstracts: HashMap<String, String> = HashMap::new();
        let mut bullets: HashMap<String, String> = HashMap::new();
        for n in &doc.nums {
            let old = n.attr(W, "numId").unwrap_or_default();
            if !used.contains(old) {
                continue;
            }
            let Some(a) = doc.abstract_of(old) else {
                continue;
            };
            let old_abstract = a.attr(W, "abstractNumId").unwrap_or_default().to_string();
            let new_abstract = match abstracts.get(&old_abstract) {
                Some(new) => new.clone(),
                None => {
                    let new = self.copy_abstract(a, doc, input, &mut bullets, rel)?;
                    abstracts.insert(old_abstract, new.clone());
                    new
                }
            };
            let next = self.lists.nums.iter().map(|n| id(n, "numId")).max();
            let new_num = next.unwrap_or(0) + 1;
            let mut num = n.clone();
            num.set_attr(W, "w", "numId", &new_num.to_string());
            if let Some(c) = num.child_mut(W, "abstractNumId") {
                c.set_attr(W, "w", "val", &new_abstract);
            }
            self.lists.nums.push(num);
            copied.nums.insert(old.to_string(), new_num.to_string());
        }
        copied.bullets = starts[0]..self.lists.bullets.len();
        copied.abstracts = starts[1]..self.lists.abstracts.len();
        copied.added_nums = starts[2]..self.lists.nums.len();
        Ok(copied)
    }

    fn copy_abstract(
        &mut self,
        a: &Node,
        doc: &Lists,
        input: usize,
        bullets: &mut HashMap<String, String>,
        rel: &mut dyn FnMut(&str) -> Result<String, Error>,
    ) -> Result<String, Error> {
        let new = self
            .lists
            .abstracts
            .iter()
            .map(|a| id(a, "abstractNumId"))
            .max()
            .map_or(0, |m| m + 1)
            .to_string();
        let mut a = a.clone();
        let old = id(&a, "abstractNumId");
        a.set_attr(W, "w", "abstractNumId", &new);
        let nsid = self.new_nsid(input, old);
        match a.child_mut(W, "nsid") {
            Some(n) => n.set_attr(W, "w", "val", &nsid),
            None => {
                let attr = Attr::new("w:val", W, &nsid);
                a.children
                    .insert(0, Child::Node(Node::new("w:nsid", W, vec![attr])));
            }
        }
        let mut wanted = vec![];
        a.walk(&mut |n| {
            if n.is(W, "lvlPicBulletId")
                && let Some(v) = n.attr(W, "val")
            {
                wanted.push(v.to_string());
            }
        });
        for old in wanted {
            if bullets.contains_key(&old) {
                continue;
            }
            let found = doc
                .bullets
                .iter()
                .find(|b| b.attr(W, "numPicBulletId") == Some(old.as_str()));
            let Some(b) = found else { continue };
            let next = self.lists.bullets.iter().map(|b| id(b, "numPicBulletId"));
            let new = next.max().map_or(0, |m| m + 1).to_string();
            let mut b = b.clone();
            b.set_attr(W, "w", "numPicBulletId", &new);
            let mut rids = vec![];
            b.walk(&mut |n| {
                rids.extend(
                    n.attrs
                        .iter()
                        .filter(|x| super::is_rid(x))
                        .map(|x| x.value.clone()),
                );
            });
            let mut map = HashMap::new();
            for rid in rids {
                let new = rel(&rid)?;
                map.insert(rid, new);
            }
            b.walk_mut(&mut |n| {
                for x in n.attrs.iter_mut().filter(|x| super::is_rid(x)) {
                    if let Some(new) = map.get(&x.value) {
                        x.value = new.clone();
                    }
                }
            });
            self.lists.bullets.push(b);
            bullets.insert(old, new);
        }
        a.walk_mut(&mut |n| {
            if n.is(W, "lvlPicBulletId")
                && let Some(new) = n.attr(W, "val").and_then(|v| bullets.get(v)).cloned()
            {
                n.set_attr(W, "w", "val", &new);
            }
        });
        self.lists.abstracts.push(a);
        Ok(new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Origin, xml};

    const NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    fn lists(body: &str) -> Lists {
        let text = format!("<w:numbering {NS}>{body}</w:numbering>");
        let root = xml::tree(&text, "n.xml", &Origin::test("x.docx")).unwrap();
        Lists::new(Some(&root))
    }

    const ONE: &str = r#"<w:abstractNum w:abstractNumId="0"><w:nsid w:val="AAAA0000"/><w:tmpl w:val="1"/><w:lvl w:ilvl="0"/></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#;

    fn no_rel(_: &str) -> Result<String, Error> {
        panic!("no relationship expected")
    }

    #[test]
    fn copies_get_new_ids_and_nsids() {
        let mut out = Output::new(lists(ONE));
        let doc = lists(ONE);
        let used = BTreeSet::from(["1".to_string()]);
        let c = out.copy(&doc, &used, 2, &mut no_rel).unwrap();
        assert_eq!(c.nums["1"], "2");
        let [_, abstracts, nums] = out.added();
        assert_eq!(abstracts[0].attr(W, "abstractNumId"), Some("1"));
        assert_eq!(reference(&nums[0], "abstractNumId"), Some("1"));
        assert_ne!(reference(&abstracts[0], "nsid"), Some("AAAA0000"));
        assert_eq!(reference(&abstracts[0], "tmpl"), Some("1"));
        let again = out.copy(&doc, &used, 2, &mut no_rel).unwrap();
        assert_eq!(again.nums["1"], "3");
        let nsids: HashSet<_> = out
            .lists
            .abstracts
            .iter()
            .map(|a| reference(a, "nsid"))
            .collect();
        assert_eq!(nsids.len(), 3, "every nsid differs");
    }

    #[test]
    fn nums_sharing_an_abstract_share_its_copy_and_keep_overrides() {
        let two = format!(
            r#"{ONE}<w:num w:numId="2"><w:abstractNumId w:val="0"/><w:lvlOverride w:ilvl="0"><w:startOverride w:val="5"/></w:lvlOverride></w:num><w:num w:numId="3"><w:abstractNumId w:val="0"/></w:num>"#
        );
        let mut out = Output::new(lists(""));
        let used = BTreeSet::from(["1".to_string(), "2".to_string()]);
        let c = out.copy(&lists(&two), &used, 2, &mut no_rel).unwrap();
        assert_eq!(c.nums.len(), 2);
        let [_, abstracts, nums] = out.added();
        assert_eq!(abstracts.len(), 1);
        assert_eq!(abstracts[0].attr(W, "abstractNumId"), Some("0"));
        assert!(nums[1].child(W, "lvlOverride").is_some());
    }

    #[test]
    fn picture_bullets_come_with_their_relationship() {
        let body = r#"<w:numPicBullet w:numPicBulletId="4" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:drawing r:embed="rId3"/></w:numPicBullet><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:lvlPicBulletId w:val="4"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#;
        let base = r#"<w:numPicBullet w:numPicBulletId="0"/>"#;
        let mut out = Output::new(lists(base));
        let used = BTreeSet::from(["1".to_string()]);
        let mut rel = |old: &str| {
            assert_eq!(old, "rId3");
            Ok("rId9".to_string())
        };
        out.copy(&lists(body), &used, 2, &mut rel).unwrap();
        let [bullets, abstracts, _] = out.added();
        assert_eq!(bullets[0].attr(W, "numPicBulletId"), Some("1"));
        let mut text = String::new();
        bullets[0].write(&mut text);
        assert!(text.contains(r#"r:embed="rId9""#), "{text}");
        abstracts[0].write(&mut text);
        assert!(text.contains(r#"<w:lvlPicBulletId w:val="1"/>"#), "{text}");
    }

    #[test]
    fn forms_ignore_what_only_identifies_a_list() {
        let other = ONE
            .replace("AAAA0000", "BBBB0000")
            .replace(r#"w:abstractNumId="0""#, r#"w:abstractNumId="7""#)
            .replace(
                r#"<w:abstractNumId w:val="0"/>"#,
                r#"<w:abstractNumId w:val="7"/>"#,
            )
            .replace(r#"w:tmpl w:val="1""#, r#"w:tmpl w:val="2""#);
        assert_eq!(lists(ONE).form("1"), lists(&other).form("1"));
        let changed = ONE.replace(r#"w:ilvl="0""#, r#"w:ilvl="1""#);
        assert_ne!(lists(ONE).form("1"), lists(&changed).form("1"));
        assert_eq!(lists(ONE).form("9"), "");
    }
}
