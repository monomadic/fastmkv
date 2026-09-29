//! Re-seating: a copy of the file with its metadata at the front and room
//! left after it.
//!
//! A muxer leaves no padding after the tags, so the first edit that grows
//! them has to put them at the end of the file, where a reader on a slow
//! link must make a second request to find them. Re-seating is the cure,
//! paid once: every later edit fits in the padding and costs kilobytes.
//!
//! It is a copy, not a remux. Every cluster is carried byte for byte and
//! none is opened beyond its first few children. What changes is where the
//! clusters sit, so the three places that record a cluster's position are
//! corrected: the `Cues`, the `SeekHead`, and the optional `Position`
//! inside each cluster. Anything else that holds a position is a refusal,
//! not a guess. A second `SeekHead` is not carried over: the copy gets one
//! index, at the front, of everything but the clusters, which the `Cues`
//! already find.
//!
//! The source is never written to. The result is a new file, and it is
//! only left in place if it passes the same preflight as any file this
//! crate would edit.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::crc::crc32;
use crate::ebml::{self, children, element_min, encode_id, encode_uint, fit, void};
use crate::encode::{tags_body, with_crc};
use crate::error::{refuse, Error, Kind, Result};
use crate::model::split_crc;
use crate::scan::{read_at, read_data, Element};
use crate::Mkv;

const CUE_POINT: u32 = 0xBB;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const CUE_CLUSTER_POSITION: u32 = 0xF1;
const CUE_CODEC_STATE: u32 = 0xEA;
const CUE_REFERENCE: u32 = 0xDB;
const CLUSTER_TIMESTAMP: u32 = 0xE7;
const CLUSTER_POSITION: u32 = 0xA7;
const CLUSTER_PREV_SIZE: u32 = 0xAB;

/// A cluster that has to be read whole, to redo its checksum, is held in
/// memory. They are a few megabytes; this is a bound, not an expectation.
const MAX_CLUSTER: u64 = 1 << 30;

/// How much room to leave, in bytes of `Void`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Padding {
    /// After the `SeekHead`: each new entry takes about 20.
    pub seek_head: u64,
    /// After `Info`: for a longer title.
    pub info: u64,
    /// After the tags.
    pub tags: u64,
}

impl Default for Padding {
    fn default() -> Self {
        Padding {
            seek_head: 128,
            info: 512,
            tags: 4096,
        }
    }
}

/// How a file is laid out now, for deciding whether to re-seat it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seating {
    /// `Info` and every `Tags` element come before the first cluster.
    pub front: bool,
    /// Bytes of `Void` directly after the last `Tags`, or 0.
    pub tags_padding: u64,
}

enum Source {
    /// Carried from the original, from this offset.
    Copy(u64),
    Bytes(Vec<u8>),
    /// The SeekHead and the Cues hold positions, so their bytes are only
    /// known once everything has a place.
    SeekHead,
    Cues(Element),
    Cluster(u64),
}

struct Item {
    id: u32,
    /// Where it was, for the elements something may point at.
    old: Option<u64>,
    len: u64,
    source: Source,
}

fn padding(len: u64) -> Option<Item> {
    // A Void cannot be shorter than its own header.
    if len == 0 {
        return None;
    }
    Some(given(ebml::VOID, None, void(len.max(2))?))
}

fn given(id: u32, old: Option<u64>, bytes: Vec<u8>) -> Item {
    Item {
        id,
        old,
        len: bytes.len() as u64,
        source: Source::Bytes(bytes),
    }
}

fn copied(e: &Element) -> Item {
    Item {
        id: e.id,
        old: Some(e.offset),
        len: e.end() - e.offset,
        source: Source::Copy(e.offset),
    }
}

/// Old segment positions to new ones, for everything that was placed.
struct Moved(Vec<(u64, u64)>);

impl Moved {
    fn get(&self, old: u64) -> Option<u64> {
        let i = self.0.binary_search_by_key(&old, |x| x.0).ok()?;
        Some(self.0[i].1)
    }
}

/// `Cues` with every cluster position looked up in `moved`. Everything
/// else in it is carried as it was.
fn cues(data: &[u8], base: u64, moved: &Moved) -> Result<Vec<u8>> {
    fn master(id: u32, data: &[u8], base: u64, moved: &Moved, depth: usize) -> Result<Vec<u8>> {
        let kids = children(data, base)?;
        let (crc, kids) = split_crc(&kids, data);
        if crc.is_some_and(|c| !c.valid) {
            return refuse(Kind::Checksum, "the Cues fail their checksum");
        }
        let mut body = Vec::with_capacity(data.len() + 8);
        for k in kids {
            match (depth, k.id) {
                (0, CUE_POINT) | (1, CUE_TRACK_POSITIONS) => {
                    body.extend(master(k.id, k.data, base, moved, depth + 1)?)
                }
                (2, CUE_CLUSTER_POSITION) => {
                    let new = ebml::read_uint(k.data).and_then(|old| moved.get(old));
                    let Some(new) = new else {
                        return refuse(
                            Kind::Position,
                            "a cue points at something that is not a cluster",
                        );
                    };
                    body.extend(element_min(k.id, &encode_uint(new)));
                }
                // Both hold positions that this crate has no way to
                // follow. ffmpeg writes neither.
                (2, CUE_CODEC_STATE) if ebml::read_uint(k.data) != Some(0) => {
                    return refuse(Kind::Position, "the Cues hold a codec state position");
                }
                (2, CUE_REFERENCE) => {
                    return refuse(Kind::Position, "the Cues hold a cue reference");
                }
                _ => body.extend_from_slice(k.whole),
            }
        }
        Ok(element_min(id, &with_crc(crc.is_some(), body)))
    }
    master(ebml::CUES, data, base, moved, 0)
}

/// The first bytes of a cluster with its `Position` corrected, and how
/// many bytes of the original they stand for. Empty when there is nothing
/// to correct, which is every cluster ffmpeg writes.
fn cluster_head(
    src: &mut File,
    at: u64,
    end: u64,
    old_position: u64,
    new_position: u64,
) -> Result<Option<Vec<u8>>> {
    let head = read_at(src, at, 64.min(end - at) as usize)?;
    let h = ebml::header(&head, at)?;
    let mut pos = h.width() as usize;
    let mut checksummed = false;
    let mut found = None;
    while pos < head.len() {
        let Ok(c) = ebml::header(&head[pos..], at + pos as u64) else {
            break;
        };
        let (Some(size), start) = (c.size, pos + c.width() as usize) else {
            break;
        };
        let stop = start + size as usize;
        match c.id {
            ebml::CRC32 => checksummed = true,
            CLUSTER_TIMESTAMP | CLUSTER_PREV_SIZE | ebml::VOID => {}
            CLUSTER_POSITION if stop <= head.len() => found = Some((start, stop)),
            // The blocks start here, and nothing after them is a header.
            _ => break,
        }
        pos = stop;
    }
    let Some((start, stop)) = found else {
        return Ok(None);
    };
    if ebml::read_uint(&head[start..stop]) != Some(old_position) {
        // It did not say where the cluster was, so it is not corrected to
        // say where it is: it was wrong before and means nothing.
        return Ok(None);
    }
    let wide = encode_uint(new_position);
    if wide.len() > stop - start {
        return refuse(
            Kind::Position,
            "a cluster's Position has no room for where the cluster now is",
        );
    }
    let mut whole = if checksummed {
        let size = end - at;
        if size > MAX_CLUSTER {
            return refuse(
                Kind::TooLarge,
                format!("the cluster at {at} is {size} bytes"),
            );
        }
        read_at(src, at, size as usize)?
    } else {
        head[..stop].to_vec()
    };
    let fill = stop - start - wide.len();
    whole[start..start + fill].fill(0);
    whole[start + fill..stop].copy_from_slice(&wide);
    if checksummed {
        let data = h.width() as usize;
        let kids = children(&whole[data..], at)?;
        let crc = kids[0].whole.len();
        let sum = crc32(&whole[data + crc..]).to_le_bytes();
        whole[data + crc - 4..data + crc].copy_from_slice(&sum);
    }
    Ok(Some(whole))
}

impl Mkv {
    pub fn seating(&self) -> Seating {
        let s = &self.survey;
        let first = s.cluster_offsets.first().copied().unwrap_or(u64::MAX);
        let mine = |e: &&Element| matches!(e.id, ebml::INFO | ebml::TAGS);
        let front = s.elements.iter().filter(mine).all(|e| e.offset < first);
        let last = s.elements.iter().rfind(|e| e.id == ebml::TAGS);
        let tags_padding = last.map_or(0, |t| {
            let mut end = t.end();
            for v in s.elements.iter().filter(|v| v.offset >= t.end()) {
                if v.id != ebml::VOID || v.offset != end {
                    break;
                }
                end = v.end();
            }
            end - t.end()
        });
        Seating {
            front,
            tags_padding,
        }
    }

    /// Write a re-seated copy to `dest`, with any pending edits made in it.
    ///
    /// `dest` must not exist. The original is only read. If anything goes
    /// wrong, or the result does not pass the preflight, `dest` is removed.
    pub fn reseat(&self, dest: impl AsRef<Path>, padding: Padding) -> Result<()> {
        let dest = dest.as_ref();
        let mut out = OpenOptions::new().write(true).create_new(true).open(dest)?;
        let done = self
            .reseat_into(&mut out, padding)
            .and_then(|_| out.sync_all().map_err(Error::WriteFailed))
            .and_then(|_| crate::open(dest).map(|_| ()));
        if done.is_err() {
            drop(out);
            let _ = std::fs::remove_file(dest);
        }
        done
    }

    fn reseat_into(&self, out: &mut File, pad: Padding) -> Result<()> {
        let s = &self.survey;
        let mut src = File::open(&self.path)?;
        let old_start = s.segment.data_offset();
        let old_end = old_start + s.segment_size;
        let first_cluster = s.cluster_offsets.first().copied().unwrap_or(old_end);

        // Front: the index, what describes the file, the tags, and room.
        let mut items: Vec<Item> = vec![Item {
            id: ebml::SEEK_HEAD,
            old: None,
            len: 0,
            source: Source::SeekHead,
        }];
        if let Some(info) = &self.info {
            items.push(match info.dirty {
                true => given(ebml::INFO, None, element_min(ebml::INFO, &info.body())),
                false => copied(&info.element),
            });
            items.extend(padding(pad.info));
        }
        let placed_apart =
            |id: u32| matches!(id, ebml::SEEK_HEAD | ebml::VOID | ebml::INFO | ebml::TAGS);
        let carried = |e: &Element| match e.id {
            ebml::CUES => Item {
                id: e.id,
                old: Some(e.offset),
                len: e.end() - e.offset,
                source: Source::Cues(*e),
            },
            _ => copied(e),
        };
        let others = || s.elements.iter().filter(|e| !placed_apart(e.id));
        items.extend(others().filter(|e| e.offset < first_cluster).map(carried));
        for at in &self.tags {
            match (at.dirty, at.element, tags_body(&at.tags)) {
                (false, Some(e), _) => items.push(copied(&e)),
                (_, _, Some(body)) => {
                    items.push(given(ebml::TAGS, None, element_min(ebml::TAGS, &body)))
                }
                // Emptied by an edit: it is simply not written.
                (_, _, None) => {}
            }
        }
        items.extend(padding(pad.tags));

        // Then everything from the first cluster on, in the order it was.
        let mut tail: Vec<Item> = others()
            .filter(|e| e.offset >= first_cluster)
            .map(carried)
            .collect();
        tail.extend(s.cluster_offsets.iter().map(|&at| Item {
            id: ebml::CLUSTER,
            old: Some(at),
            len: 0,
            source: Source::Cluster(at),
        }));
        tail.sort_by_key(|i| i.old);
        // A cluster runs to whatever followed it in the original.
        let mut ends: Vec<u64> = s.elements.iter().map(|e| e.offset).collect();
        ends.extend(&s.cluster_offsets);
        ends.push(old_end);
        ends.sort();
        for c in tail.iter_mut().filter(|i| i.id == ebml::CLUSTER) {
            let at = c.old.unwrap_or(0);
            let next = ends.partition_point(|&e| e <= at);
            c.len = ends[next] - at;
        }
        items.extend(tail);

        // The SeekHead's region is sized for the widest positions, so its
        // length does not depend on where things land.
        let listed = |i: &Item| !matches!(i.id, ebml::SEEK_HEAD | ebml::VOID | ebml::CLUSTER);
        let checksummed = s.seek_head.as_ref().is_some_and(|h| h.crc.is_some());
        let entry = |id: u32, position: &[u8]| {
            element_min(
                ebml::SEEK,
                &[
                    element_min(ebml::SEEK_ID, &id.to_be_bytes()),
                    element_min(ebml::SEEK_POSITION, position),
                ]
                .concat(),
            )
        };
        let widest: Vec<u8> = items
            .iter()
            .filter(|i| listed(i))
            .flat_map(|i| entry(i.id, &[0xFF; 8]))
            .collect();
        let region = element_min(ebml::SEEK_HEAD, &with_crc(checksummed, widest)).len() as u64
            + pad.seek_head.max(2);
        items[0].len = region;

        // The Cues' length depends on the positions in them, and when they
        // sit in front of the clusters those depend on their length. Both
        // only grow together, so this settles.
        let cue_data: Vec<Option<Vec<u8>>> = items
            .iter()
            .map(|i| match &i.source {
                Source::Cues(e) => read_data(&mut src, e).map(Some),
                _ => Ok(None),
            })
            .collect::<Result<_>>()?;
        let mut moved;
        let mut settled = 0;
        loop {
            let mut at = 0u64;
            let mut places = Vec::with_capacity(items.len());
            for i in &items {
                if let Some(old) = i.old {
                    places.push((old - old_start, at));
                }
                at += i.len;
            }
            places.sort();
            moved = Moved(places);

            let mut changed = false;
            for (i, data) in items.iter_mut().zip(&cue_data) {
                let (Source::Cues(e), Some(data)) = (&i.source, data) else {
                    continue;
                };
                let len = cues(data, e.data_offset(), &moved)?.len() as u64;
                changed |= len != i.len;
                i.len = len;
            }
            settled += 1;
            if !changed {
                break;
            }
            if settled > 16 {
                return refuse(Kind::Malformed, "planning bug: the Cues did not settle");
            }
        }

        let total: u64 = items.iter().map(|i| i.len).sum();
        let mut segment = encode_id(ebml::SEGMENT);
        let Some(size) = ebml::encode_size(total, 8) else {
            return refuse(Kind::TooLarge, "the file is too large for a Segment");
        };
        segment.extend(size);

        let mut seek_body = Vec::new();
        let mut at = 0u64;
        for i in &items {
            if listed(i) {
                seek_body.extend(entry(i.id, &encode_uint(at)));
            }
            at += i.len;
        }
        let Some(seek_head) = fit(ebml::SEEK_HEAD, &with_crc(checksummed, seek_body), region)
        else {
            return refuse(Kind::Malformed, "planning bug: the SeekHead region");
        };

        (|| -> Result<()> {
            copy(&mut src, out, 0, s.segment.offset)?;
            out.write_all(&segment)?;
            let mut at = 0u64;
            for (i, data) in items.iter().zip(&cue_data) {
                match (&i.source, data) {
                    (Source::SeekHead, _) => out.write_all(&seek_head)?,
                    (Source::Bytes(b), _) => out.write_all(b)?,
                    (Source::Copy(from), _) => copy(&mut src, out, *from, i.len)?,
                    (Source::Cues(e), Some(data)) => {
                        out.write_all(&cues(data, e.data_offset(), &moved)?)?
                    }
                    (Source::Cues(_), None) => {}
                    (Source::Cluster(from), _) => {
                        let head =
                            cluster_head(&mut src, *from, from + i.len, from - old_start, at)?;
                        let done = head.as_ref().map_or(0, |h| h.len() as u64);
                        if let Some(h) = head {
                            out.write_all(&h)?;
                        }
                        copy(&mut src, out, from + done, i.len - done)?;
                    }
                }
                at += i.len;
            }
            Ok(())
        })()
        .map_err(|e| match e {
            Error::Io(e) => Error::WriteFailed(e),
            other => other,
        })
    }
}

fn copy(src: &mut File, out: &mut File, from: u64, len: u64) -> Result<()> {
    src.seek(SeekFrom::Start(from))?;
    let copied = std::io::copy(&mut src.take(len), out)?;
    if copied != len {
        return refuse(Kind::Malformed, "the original is shorter than it was");
    }
    Ok(())
}
