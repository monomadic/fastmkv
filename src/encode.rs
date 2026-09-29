//! The tag tree back to bytes.
//!
//! A node that still has the bytes it was parsed from is written from
//! them. Only what an edit touched is re-encoded, and a checksum is
//! recomputed exactly where one was (PROPOSAL-2 §5.4).

use crate::crc::crc32;
use crate::ebml::{self, element_min, encode_uint};
use crate::model::{Raw, SimpleChild, SimpleTag, Tag, TagChild, Tags, TagsChild};

pub(crate) fn with_crc(had: bool, body: Vec<u8>) -> Vec<u8> {
    if !had {
        return body;
    }
    let mut out = element_min(ebml::CRC32, &crc32(&body).to_le_bytes());
    out.extend(body);
    out
}

fn raw(r: &Raw) -> Vec<u8> {
    r.bytes.clone()
}

pub(crate) fn simple(s: &SimpleTag) -> Vec<u8> {
    if let Some(o) = &s.original {
        return o.clone();
    }
    let body: Vec<u8> = s
        .children
        .iter()
        .flat_map(|c| match c {
            SimpleChild::Name(v) => element_min(ebml::TAG_NAME, v.as_bytes()),
            SimpleChild::Language(v) => element_min(ebml::TAG_LANGUAGE, v.as_bytes()),
            SimpleChild::LanguageBcp47(v) => element_min(ebml::TAG_LANGUAGE_BCP47, v.as_bytes()),
            SimpleChild::Default(n) => element_min(ebml::TAG_DEFAULT, &encode_uint(*n)),
            SimpleChild::String(v) => element_min(ebml::TAG_STRING, v.as_bytes()),
            SimpleChild::Binary(b) => element_min(ebml::TAG_BINARY, b),
            SimpleChild::Nested(n) => simple(n),
            SimpleChild::Raw(r) => raw(r),
        })
        .collect();
    element_min(ebml::SIMPLE_TAG, &with_crc(s.crc.is_some(), body))
}

pub(crate) fn tag(t: &Tag) -> Vec<u8> {
    if let Some(o) = &t.original {
        return o.clone();
    }
    let body: Vec<u8> = t
        .children
        .iter()
        .flat_map(|c| match c {
            TagChild::Targets(t) => t.original.clone(),
            TagChild::Simple(s) => simple(s),
            TagChild::Raw(r) => raw(r),
        })
        .collect();
    element_min(ebml::TAG, &with_crc(t.crc.is_some(), body))
}

/// The data of a `Tags` element, checksum included. `None` when nothing is
/// left to write: a `Tags` must hold at least one `Tag`.
pub(crate) fn tags_body(t: &Tags) -> Option<Vec<u8>> {
    t.tags().next()?;
    let body: Vec<u8> = t
        .children
        .iter()
        .flat_map(|c| match c {
            TagsChild::Tag(t) => tag(t),
            TagsChild::Raw(r) => raw(r),
        })
        .collect();
    Some(with_crc(t.crc.is_some(), body))
}
