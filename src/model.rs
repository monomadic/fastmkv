//! The tag tree (PROPOSAL-2 §4).
//!
//! Every node keeps the bytes it was parsed from. A node nobody edits is
//! written back from those bytes, so an untouched tag survives exactly --
//! including a size field wider than it needed to be, which re-encoding
//! would quietly narrow.

use crate::crc::crc32;
use crate::ebml::{self, children, Child};
use crate::error::Result;

/// A `CRC-32` found as the first child of a master element. Never carried
/// through as an unknown child: an edit to its parent makes it wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crc {
    pub stored: u32,
    pub valid: bool,
}

/// Take the checksum off the front of a master's children, if it has one.
pub(crate) fn split_crc<'a>(kids: &'a [Child<'a>], data: &[u8]) -> (Option<Crc>, &'a [Child<'a>]) {
    match kids.first() {
        Some(c) if c.id == ebml::CRC32 && c.data.len() == 4 => {
            let stored = u32::from_le_bytes(c.data.try_into().unwrap());
            let valid = crc32(&data[c.whole.len()..]) == stored;
            (Some(Crc { stored, valid }), &kids[1..])
        }
        _ => (None, kids),
    }
}

/// A child this crate has no model of, kept verbatim and in position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raw {
    pub id: u32,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tags {
    pub crc: Option<Crc>,
    pub children: Vec<TagsChild>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagsChild {
    Tag(Tag),
    Raw(Raw),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub crc: Option<Crc>,
    pub children: Vec<TagChild>,
    pub(crate) original: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagChild {
    Targets(Targets),
    Simple(SimpleTag),
    Raw(Raw),
}

/// Read-only: nothing here edits what a tag applies to, so the element is
/// carried as bytes and only interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Targets {
    pub type_value: u64,
    pub type_name: Option<String>,
    pub track_uids: Vec<u64>,
    pub edition_uids: Vec<u64>,
    pub chapter_uids: Vec<u64>,
    pub attachment_uids: Vec<u64>,
    pub(crate) original: Vec<u8>,
}

impl Targets {
    /// Applies to the segment as a whole: no non-zero UID of any kind, at
    /// the default level. A UID of 0 means "all of them", which is the same
    /// thing (PROPOSAL-2 §7.2).
    pub fn is_global(&self) -> bool {
        self.type_value == 50
            && [
                &self.track_uids,
                &self.edition_uids,
                &self.chapter_uids,
                &self.attachment_uids,
            ]
            .iter()
            .all(|u| u.iter().all(|&x| x == 0))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleTag {
    pub crc: Option<Crc>,
    pub children: Vec<SimpleChild>,
    pub(crate) original: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimpleChild {
    Name(String),
    Language(String),
    LanguageBcp47(String),
    Default(u64),
    String(String),
    Binary(Vec<u8>),
    Nested(SimpleTag),
    Raw(Raw),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagValue<'a> {
    String(&'a str),
    Binary(&'a [u8]),
    /// A `SimpleTag` with neither: legal, and used as a parent for nested tags.
    None,
}

impl Tag {
    pub fn targets(&self) -> Option<&Targets> {
        self.children.iter().find_map(|c| match c {
            TagChild::Targets(t) => Some(t),
            _ => None,
        })
    }

    /// `Targets` absent or empty means the whole segment.
    pub fn is_global(&self) -> bool {
        self.targets().is_none_or(Targets::is_global)
    }

    pub fn simple_tags(&self) -> impl Iterator<Item = &SimpleTag> {
        self.children.iter().filter_map(|c| match c {
            TagChild::Simple(s) => Some(s),
            _ => None,
        })
    }
}

impl SimpleTag {
    pub fn name(&self) -> &str {
        self.children
            .iter()
            .find_map(|c| match c {
                SimpleChild::Name(n) => Some(n.as_str()),
                _ => None,
            })
            .unwrap_or("")
    }

    pub fn value(&self) -> TagValue<'_> {
        self.children
            .iter()
            .find_map(|c| match c {
                SimpleChild::String(s) => Some(TagValue::String(s)),
                SimpleChild::Binary(b) => Some(TagValue::Binary(b)),
                _ => None,
            })
            .unwrap_or(TagValue::None)
    }

    /// The BCP 47 form wins when both are present (RFC 9559 §5.1.8.1.2.3);
    /// the default is "und".
    pub fn language(&self) -> &str {
        let find = |bcp: bool| {
            self.children.iter().find_map(|c| match c {
                SimpleChild::LanguageBcp47(l) if bcp => Some(l.as_str()),
                SimpleChild::Language(l) if !bcp => Some(l.as_str()),
                _ => None,
            })
        };
        find(true).or_else(|| find(false)).unwrap_or("und")
    }

    pub fn is_undetermined_language(&self) -> bool {
        matches!(self.language(), "und" | "")
    }

    pub fn nested(&self) -> impl Iterator<Item = &SimpleTag> {
        self.children.iter().filter_map(|c| match c {
            SimpleChild::Nested(s) => Some(s),
            _ => None,
        })
    }
}

impl Tags {
    pub fn tags(&self) -> impl Iterator<Item = &Tag> {
        self.children.iter().filter_map(|c| match c {
            TagsChild::Tag(t) => Some(t),
            _ => None,
        })
    }
}

fn raw(c: &Child<'_>) -> Raw {
    Raw {
        id: c.id,
        bytes: c.whole.to_vec(),
    }
}

/// EBML strings may be padded with NULs after the text (RFC 8794 §7.4).
/// `None` when what is left is not UTF-8: the caller keeps such a child raw
/// rather than guess at it.
fn text(data: &[u8]) -> Option<String> {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8(data[..end].to_vec()).ok()
}

/// `data` is the payload of a `Tags` element at file offset `base`.
pub fn parse_tags(data: &[u8], base: u64) -> Result<Tags> {
    let kids = children(data, base)?;
    let (crc, kids) = split_crc(&kids, data);
    let mut out = Vec::new();
    for c in kids {
        let at = base + offset_in(data, c.data);
        out.push(match c.id {
            ebml::TAG => TagsChild::Tag(parse_tag(c, at)?),
            _ => TagsChild::Raw(raw(c)),
        });
    }
    Ok(Tags { crc, children: out })
}

fn offset_in(parent: &[u8], child: &[u8]) -> u64 {
    (child.as_ptr() as usize - parent.as_ptr() as usize) as u64
}

fn parse_tag(c: &Child<'_>, base: u64) -> Result<Tag> {
    let kids = children(c.data, base)?;
    let (crc, kids) = split_crc(&kids, c.data);
    let mut out = Vec::new();
    for k in kids {
        let at = base + offset_in(c.data, k.data);
        out.push(match k.id {
            ebml::TARGETS => match parse_targets(k, at)? {
                Some(t) => TagChild::Targets(t),
                None => TagChild::Raw(raw(k)),
            },
            ebml::SIMPLE_TAG => TagChild::Simple(parse_simple(k, at, 0)?),
            _ => TagChild::Raw(raw(k)),
        });
    }
    Ok(Tag {
        crc,
        children: out,
        original: Some(c.whole.to_vec()),
    })
}

fn parse_targets(c: &Child<'_>, base: u64) -> Result<Option<Targets>> {
    let kids = children(c.data, base)?;
    let (_, kids) = split_crc(&kids, c.data);
    let mut t = Targets {
        type_value: 50,
        type_name: None,
        track_uids: Vec::new(),
        edition_uids: Vec::new(),
        chapter_uids: Vec::new(),
        attachment_uids: Vec::new(),
        original: c.whole.to_vec(),
    };
    for k in kids {
        let n = ebml::read_uint(k.data);
        match (k.id, n) {
            (ebml::TARGET_TYPE, _) => t.type_name = text(k.data),
            (ebml::TARGET_TYPE_VALUE, Some(n)) => t.type_value = n,
            (ebml::TAG_TRACK_UID, Some(n)) => t.track_uids.push(n),
            (ebml::TAG_EDITION_UID, Some(n)) => t.edition_uids.push(n),
            (ebml::TAG_CHAPTER_UID, Some(n)) => t.chapter_uids.push(n),
            (ebml::TAG_ATTACHMENT_UID, Some(n)) => t.attachment_uids.push(n),
            // An integer too wide to read: what this applies to is not
            // known, so it must not be mistaken for global.
            (
                ebml::TARGET_TYPE_VALUE
                | ebml::TAG_TRACK_UID
                | ebml::TAG_EDITION_UID
                | ebml::TAG_CHAPTER_UID
                | ebml::TAG_ATTACHMENT_UID,
                None,
            ) => return Ok(None),
            _ => {}
        }
    }
    Ok(Some(t))
}

/// Nesting is bounded so a crafted file cannot recurse without limit.
const MAX_DEPTH: usize = 32;

fn parse_simple(c: &Child<'_>, base: u64, depth: usize) -> Result<SimpleTag> {
    let kids = children(c.data, base)?;
    let (crc, kids) = split_crc(&kids, c.data);
    let mut out = Vec::new();
    for k in kids {
        let at = base + offset_in(c.data, k.data);
        let as_text =
            |f: fn(String) -> SimpleChild| text(k.data).map(f).unwrap_or(SimpleChild::Raw(raw(k)));
        out.push(match k.id {
            ebml::TAG_NAME => as_text(SimpleChild::Name),
            ebml::TAG_LANGUAGE => as_text(SimpleChild::Language),
            ebml::TAG_LANGUAGE_BCP47 => as_text(SimpleChild::LanguageBcp47),
            ebml::TAG_STRING => as_text(SimpleChild::String),
            ebml::TAG_DEFAULT => match ebml::read_uint(k.data) {
                Some(n) => SimpleChild::Default(n),
                None => SimpleChild::Raw(raw(k)),
            },
            ebml::TAG_BINARY => SimpleChild::Binary(k.data.to_vec()),
            ebml::SIMPLE_TAG if depth < MAX_DEPTH => {
                SimpleChild::Nested(parse_simple(k, at, depth + 1)?)
            }
            _ => SimpleChild::Raw(raw(k)),
        });
    }
    Ok(SimpleTag {
        crc,
        children: out,
        original: Some(c.whole.to_vec()),
    })
}
