//! Finding things in the file without reading the media.
//!
//! Two walks, for two callers. `locate` is for reading: it stops at the
//! first `Cluster` and lets the `SeekHead` say where everything else is, so
//! it costs a handful of seeks however large the file. `survey` is for
//! writing: it visits every top-level element, because a write is only
//! planned against a file whose whole structure is accounted for
//! (PROPOSAL-2 §2).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::ebml::{self, children, header, Header};
use crate::error::{refuse, Kind, Result};
use crate::model::{split_crc, Crc};

/// Small master elements are read whole. Anything claiming to be larger
/// than this is not metadata.
pub(crate) const MAX_MASTER: u64 = 64 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Element {
    pub id: u32,
    /// File offset of the first byte of the ID.
    pub offset: u64,
    pub id_len: u8,
    pub size_len: u8,
    pub size: u64,
}

impl Element {
    pub fn data_offset(&self) -> u64 {
        self.offset + (self.id_len + self.size_len) as u64
    }
    pub fn end(&self) -> u64 {
        self.data_offset() + self.size
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocHeader {
    pub doc_type: String,
    pub doc_type_read_version: u64,
    /// Where the EBML header ends and the next element starts.
    pub end: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub offset: u64,
    pub id_len: u8,
    pub size_len: u8,
    /// `None` for an unknown-size segment, which only the reader accepts.
    pub size: Option<u64>,
}

impl Segment {
    /// What every Segment Position is measured from (RFC 9559 §16).
    pub fn data_offset(&self) -> u64 {
        self.offset + (self.id_len + self.size_len) as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeekEntry {
    pub id: u32,
    /// A Segment Position, not a file offset.
    pub position: u64,
}

/// A child of the SeekHead as it was found, so a rewrite carries through
/// what it does not change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SeekChild {
    Entry(SeekEntry, Vec<u8>),
    Void,
    Raw(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeekHead {
    pub element: Element,
    pub crc: Option<Crc>,
    pub entries: Vec<SeekEntry>,
    pub(crate) children: Vec<SeekChild>,
    /// Bytes of `Void` among its children: room to grow without moving.
    pub void_inside: u64,
}

pub(crate) fn read_at(f: &mut File, offset: u64, len: usize) -> Result<Vec<u8>> {
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    let mut got = 0;
    while got < len {
        match f.read(&mut buf[got..])? {
            0 => break,
            n => got += n,
        }
    }
    buf.truncate(got);
    Ok(buf)
}

/// A header is at most 4 + 8 bytes.
fn header_at(f: &mut File, offset: u64, limit: u64) -> Result<Header> {
    let want = (limit.saturating_sub(offset)).min(12) as usize;
    let buf = read_at(f, offset, want)?;
    header(&buf, offset)
}

pub(crate) fn read_data(f: &mut File, e: &Element) -> Result<Vec<u8>> {
    if e.size > MAX_MASTER {
        return refuse(
            Kind::TooLarge,
            format!("element at offset {} is {} bytes", e.offset, e.size),
        );
    }
    let data = read_at(f, e.data_offset(), e.size as usize)?;
    if data.len() as u64 != e.size {
        return refuse(
            Kind::Malformed,
            format!("element at offset {} is truncated", e.offset),
        );
    }
    Ok(data)
}

/// RFC 9559 DocTypeReadVersion values this crate has been written against.
const READ_VERSIONS: std::ops::RangeInclusive<u64> = 1..=4;

pub fn doc_header(f: &mut File, file_len: u64) -> Result<DocHeader> {
    let head = read_at(f, 0, 12)?;
    if head.len() < 5 || head[..4] != ebml::EBML.to_be_bytes() {
        return refuse(Kind::NotMatroska, "not a Matroska file");
    }
    let h = header(&head, 0)?;
    let Some(size) = h.size else {
        return refuse(Kind::Malformed, "EBML header has no size");
    };
    let e = Element {
        id: h.id,
        offset: 0,
        id_len: h.id_len,
        size_len: h.size_len,
        size,
    };
    if e.end() > file_len {
        return refuse(Kind::Malformed, "EBML header is truncated");
    }
    let data = read_data(f, &e)?;

    let (mut doc_type, mut read_version) = (None, 1u64);
    let (mut ebml_read, mut max_id, mut max_size) = (1u64, 4u64, 8u64);
    for c in children(&data, e.data_offset())? {
        let n = ebml::read_uint(c.data);
        match (c.id, n) {
            (ebml::DOC_TYPE, _) => {
                let end = c.data.iter().position(|&b| b == 0).unwrap_or(c.data.len());
                doc_type = Some(String::from_utf8_lossy(&c.data[..end]).into_owned());
            }
            (ebml::DOC_TYPE_READ_VERSION, Some(n)) => read_version = n,
            (ebml::EBML_READ_VERSION, Some(n)) => ebml_read = n,
            (ebml::EBML_MAX_ID_LENGTH, Some(n)) => max_id = n,
            (ebml::EBML_MAX_SIZE_LENGTH, Some(n)) => max_size = n,
            _ => {}
        }
    }
    let doc_type = doc_type.unwrap_or_default();
    if !matches!(doc_type.as_str(), "matroska" | "webm") {
        return refuse(Kind::DocType, format!("unsupported DocType {doc_type:?}"));
    }
    if ebml_read != 1 || max_id > 4 || max_size > 8 {
        return refuse(Kind::Version, "unsupported EBML version or field lengths");
    }
    if !READ_VERSIONS.contains(&read_version) {
        return refuse(
            Kind::Version,
            format!("unsupported DocTypeReadVersion {read_version}"),
        );
    }
    Ok(DocHeader {
        doc_type,
        doc_type_read_version: read_version,
        end: e.end(),
    })
}

/// The segment header. `Void` between the EBML header and the segment is
/// stepped over.
pub fn segment(f: &mut File, mut at: u64, file_len: u64) -> Result<Segment> {
    loop {
        let h = header_at(f, at, file_len)?;
        match (h.id, h.size) {
            (ebml::SEGMENT, size) => {
                return Ok(Segment {
                    offset: at,
                    id_len: h.id_len,
                    size_len: h.size_len,
                    size,
                })
            }
            (ebml::VOID, Some(n)) => at += h.width() + n,
            _ => return refuse(Kind::Malformed, format!("no Segment at offset {at}")),
        }
    }
}

fn element_at(f: &mut File, at: u64, limit: u64) -> Result<Element> {
    let h = header_at(f, at, limit)?;
    let Some(size) = h.size else {
        return refuse(
            Kind::UnknownSize,
            format!("element {:#X} at offset {at} has an unknown size", h.id),
        );
    };
    let e = Element {
        id: h.id,
        offset: at,
        id_len: h.id_len,
        size_len: h.size_len,
        size,
    };
    if e.end() > limit {
        return refuse(
            Kind::Malformed,
            format!("element {:#X} at offset {at} runs past the end", h.id),
        );
    }
    Ok(e)
}

pub fn seek_head(f: &mut File, e: Element) -> Result<SeekHead> {
    let data = read_data(f, &e)?;
    let kids = children(&data, e.data_offset())?;
    let (crc, kids) = split_crc(&kids, &data);
    let mut entries = Vec::new();
    let mut kept = Vec::new();
    let mut void_inside = 0;
    for k in kids {
        match k.id {
            ebml::VOID => {
                void_inside += k.whole.len() as u64;
                kept.push(SeekChild::Void);
            }
            ebml::SEEK => {
                let (mut id, mut position) = (None, None);
                for s in children(k.data, e.data_offset())? {
                    match s.id {
                        ebml::SEEK_ID if s.data.len() <= 4 => {
                            id = ebml::read_uint(s.data).map(|n| n as u32)
                        }
                        ebml::SEEK_POSITION => position = ebml::read_uint(s.data),
                        _ => {}
                    }
                }
                let (Some(id), Some(position)) = (id, position) else {
                    return refuse(
                        Kind::SeekHead,
                        format!(
                            "incomplete Seek entry in the SeekHead at offset {}",
                            e.offset
                        ),
                    );
                };
                entries.push(SeekEntry { id, position });
                kept.push(SeekChild::Entry(
                    SeekEntry { id, position },
                    k.whole.to_vec(),
                ));
            }
            _ => kept.push(SeekChild::Raw(k.whole.to_vec())),
        }
    }
    Ok(SeekHead {
        element: e,
        crc,
        entries,
        children: kept,
        void_inside,
    })
}

/// What the reader needs: the elements before the first cluster, plus
/// whatever the `SeekHead`s point to.
#[derive(Debug, Clone)]
pub struct Located {
    pub doc: DocHeader,
    pub segment: Segment,
    pub seek_heads: Vec<SeekHead>,
    /// Deduplicated, in file order. Clusters are never listed.
    pub elements: Vec<Element>,
    /// Whether every top-level element was visited. `false` whenever the
    /// file has a cluster: what lies beyond it is known only through the
    /// SeekHead, and an element the SeekHead does not list is not seen.
    pub complete: bool,
}

pub fn locate(f: &mut File) -> Result<Located> {
    let file_len = f.metadata()?.len();
    let doc = doc_header(f, file_len)?;
    let segment = segment(f, doc.end, file_len)?;
    // An unknown-size or truncated segment is read as far as the file goes.
    let limit = segment
        .size
        .map_or(file_len, |s| (segment.data_offset() + s).min(file_len));

    let mut elements: Vec<Element> = Vec::new();
    let mut at = segment.data_offset();
    while at < limit {
        let Ok(h) = header_at(f, at, limit) else {
            break;
        };
        if h.id == ebml::CLUSTER {
            break;
        }
        let Ok(e) = element_at(f, at, limit) else {
            break;
        };
        elements.push(e);
        at = e.end();
    }
    let complete = at == limit;

    let mut seek_heads = Vec::new();
    let mut i = 0;
    // Entries can name a second SeekHead, so the list grows while it is
    // walked; two is the most the format allows.
    while i < elements.len() && seek_heads.len() < 2 {
        let e = elements[i];
        i += 1;
        if e.id != ebml::SEEK_HEAD {
            continue;
        }
        let Ok(sh) = seek_head(f, e) else { continue };
        for entry in &sh.entries {
            if entry.id == ebml::CLUSTER {
                continue;
            }
            let at = segment.data_offset().saturating_add(entry.position);
            if at >= limit || elements.iter().any(|x| x.offset == at) {
                continue;
            }
            // A stale entry is ignored rather than fatal: this is the
            // lenient path.
            if let Ok(found) = element_at(f, at, limit) {
                if found.id == entry.id {
                    elements.push(found);
                }
            }
        }
        seek_heads.push(sh);
    }
    elements.sort_by_key(|e| e.offset);
    Ok(Located {
        doc,
        segment,
        seek_heads,
        elements,
        complete,
    })
}

/// The whole top-level structure, every rule of the supported-file
/// boundary enforced.
#[derive(Debug, Clone)]
pub struct Survey {
    pub doc: DocHeader,
    pub segment: Segment,
    pub segment_size: u64,
    /// At most one: the first. A second, if present, is validated and then
    /// left alone.
    pub seek_head: Option<SeekHead>,
    /// Every top-level element except clusters, in file order.
    pub elements: Vec<Element>,
    pub clusters: u64,
    /// How many SeekHeads the file has: one more than `seek_head` holds
    /// when there is a second.
    pub seek_heads: usize,
    /// Where each cluster starts, in file order. Its extent is not kept:
    /// a cluster runs to whatever comes next.
    pub cluster_offsets: Vec<u64>,
}

pub fn survey(f: &mut File) -> Result<Survey> {
    let file_len = f.metadata()?.len();
    let doc = doc_header(f, file_len)?;
    let segment = segment(f, doc.end, file_len)?;
    let Some(segment_size) = segment.size else {
        return refuse(Kind::UnknownSize, "the Segment has an unknown size");
    };
    let start = segment.data_offset();
    let end = start + segment_size;
    if end > file_len {
        return refuse(Kind::Malformed, "the Segment runs past the end of the file");
    }
    if end < file_len {
        let next = read_at(f, end, 4)?;
        return if next == ebml::SEGMENT.to_be_bytes() || next == ebml::EBML.to_be_bytes() {
            refuse(
                Kind::MultipleSegments,
                "the file holds more than one Segment",
            )
        } else {
            refuse(
                Kind::TrailingData,
                format!("{} bytes follow the Segment", file_len - end),
            )
        };
    }

    let mut elements = Vec::new();
    // Offsets of every element, clusters included, for checking where the
    // SeekHead entries land. 12 bytes a cluster, so a long film is a few
    // hundred kilobytes.
    let mut all: Vec<(u64, u32)> = Vec::new();
    let mut clusters = 0;
    let mut at = start;
    while at < end {
        let e = element_at(f, at, end)?;
        if e.id == ebml::CRC32 && at == start {
            return refuse(Kind::SegmentChecksum, "the Segment carries a checksum");
        }
        all.push((e.offset, e.id));
        if e.id == ebml::CLUSTER {
            clusters += 1;
        } else {
            elements.push(e);
        }
        at = e.end();
    }

    let heads: Vec<Element> = elements
        .iter()
        .filter(|e| e.id == ebml::SEEK_HEAD)
        .copied()
        .collect();
    if heads.len() > 2 {
        return refuse(Kind::SeekHead, "more than two SeekHead elements");
    }
    if let Some(first) = heads.first() {
        if first.offset != start {
            return refuse(
                Kind::SeekHead,
                "the first SeekHead is not the first element of the Segment",
            );
        }
    }
    let mut parsed: Vec<SeekHead> = Vec::new();
    for (i, e) in heads.iter().enumerate() {
        let sh = seek_head(f, *e)?;
        if i == 1 {
            let linked = parsed[0].entries.iter().any(|x| {
                x.id == ebml::SEEK_HEAD && start.checked_add(x.position) == Some(e.offset)
            });
            if !linked {
                return refuse(
                    Kind::SeekHead,
                    "the first SeekHead does not reference the second",
                );
            }
        }
        // RFC 9559 has the second SeekHead list clusters and nothing
        // else. mkvpropedit does otherwise: out of room in the first, it
        // writes a whole new index at the end and leaves the first with one
        // entry, pointing at it. Such files are everywhere, so what is
        // asked of a second SeekHead is what is asked of the first: that
        // its entries land.
        for entry in &sh.entries {
            let target = start.checked_add(entry.position);
            let lands = target
                .and_then(|t| all.binary_search_by_key(&t, |x| x.0).ok())
                .is_some_and(|k| all[k].1 == entry.id);
            if !lands {
                return refuse(
                    Kind::SeekHead,
                    format!(
                        "a SeekHead entry for {:#X} does not point at one (position {})",
                        entry.id, entry.position
                    ),
                );
            }
        }
        parsed.push(sh);
    }

    Ok(Survey {
        doc,
        segment,
        segment_size,
        seek_heads: parsed.len(),
        seek_head: parsed.into_iter().next(),
        elements,
        clusters,
        cluster_offsets: all
            .iter()
            .filter(|x| x.1 == ebml::CLUSTER)
            .map(|x| x.0)
            .collect(),
    })
}
