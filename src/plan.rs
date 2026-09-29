//! From an edited tree to a list of byte patches, with no I/O (PROPOSAL-2
//! §5, §6).
//!
//! Everything that can be refused is refused here, before the first byte
//! is written. What comes out is a list of `(offset, bytes)` and a file
//! length; applying it is the only step that can leave a file half done.
//!
//! No cluster ever moves. An element that outgrows its place goes to the
//! end of the segment and the `SeekHead` is pointed at it, which is what
//! keeps a write to a multi-gigabyte file a matter of kilobytes.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::ebml::{self, element_min, encode_size, encode_uint, fit, void};
use crate::encode::{tags_body, with_crc};
use crate::error::{refuse, Error, Kind, Result};
use crate::scan::{Element, SeekChild, SeekEntry};
use crate::Mkv;

/// Padding after the `SeekHead` that an element moving into it must leave
/// behind: two more entries' worth, so the next edit can still be indexed.
const RESERVE: u64 = 42;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub path: PathBuf,
    /// In the order they are applied: what is appended first, while
    /// nothing points at it yet, and the index after it (§6).
    pub patches: Vec<Patch>,
    pub old_len: u64,
    pub new_len: u64,
}

struct Work {
    id: u32,
    old: Option<Element>,
    /// `None` removes the element.
    body: Option<Vec<u8>>,
}

struct Move {
    id: u32,
    from: Option<u64>,
    to: Option<u64>,
}

struct Layout<'a> {
    elements: &'a [Element],
}

impl Layout<'_> {
    /// Where the run of `Void` directly after `e` ends.
    fn padded_end(&self, e: &Element) -> u64 {
        let mut end = e.end();
        for v in self.elements.iter().filter(|v| v.offset >= e.end()) {
            if v.id != ebml::VOID || v.offset != end {
                break;
            }
            end = v.end();
        }
        end
    }

    /// Where the run of `Void` directly before `e` starts.
    fn padded_start(&self, e: &Element) -> u64 {
        let mut start = e.offset;
        for v in self.elements.iter().rev().filter(|v| v.offset < e.offset) {
            if v.id != ebml::VOID || v.end() != start {
                break;
            }
            start = v.offset;
        }
        start
    }
}

fn internal<T>(what: &str) -> Result<T> {
    refuse(Kind::Malformed, format!("planning bug: {what}"))
}

impl Mkv {
    /// Decide every byte that will change. The file is not touched.
    pub fn plan(&self) -> Result<Plan> {
        // Moving an element into the SeekHead's padding is the better
        // placement but can starve the SeekHead; if it does, plan again
        // without it.
        match self.plan_with(true) {
            Err(e) if e.kind() == Some(Kind::NoRoom) => match self.plan_with(false) {
                // Without the padding Info has nowhere to go, which is
                // the more useful thing to say.
                Err(e) if e.kind() == Some(Kind::NeedsReseat) => Err(e),
                other => other,
            },
            other => other,
        }
    }

    fn work(&self) -> Vec<Work> {
        let mut work = Vec::new();
        if let Some(info) = self.info.as_ref().filter(|i| i.dirty) {
            work.push(Work {
                id: ebml::INFO,
                old: Some(info.element),
                body: Some(info.body()),
            });
        }
        for at in self.tags.iter().filter(|t| t.dirty) {
            let body = tags_body(&at.tags);
            if at.element.is_some() || body.is_some() {
                work.push(Work {
                    id: ebml::TAGS,
                    old: at.element,
                    body,
                });
            }
        }
        work
    }

    fn plan_with(&self, use_padding_before: bool) -> Result<Plan> {
        let s = &self.survey;
        let start = s.segment.data_offset();
        let old_end = start + s.segment_size;
        let layout = Layout {
            elements: &s.elements,
        };

        let mut end = old_end;
        let mut appended: Vec<Patch> = Vec::new();
        let mut rest: Vec<Patch> = Vec::new();
        let mut moves: Vec<Move> = Vec::new();
        // The Void left in front of an element that moved up against what
        // follows it.
        let mut gap: Option<(u64, u64)> = None;

        for w in self.work() {
            let Some(old) = w.old else {
                let bytes = element_min(w.id, w.body.as_deref().unwrap_or_default());
                moves.push(Move {
                    id: w.id,
                    from: None,
                    to: Some(end),
                });
                let offset = end;
                end += bytes.len() as u64;
                appended.push(Patch { offset, bytes });
                continue;
            };
            let region_end = layout.padded_end(&old);
            let region = region_end - old.offset;
            let Some(wiped) = void(region) else {
                return internal("an element shorter than two bytes");
            };

            let Some(body) = w.body else {
                rest.push(Patch {
                    offset: old.offset,
                    bytes: wiped,
                });
                moves.push(Move {
                    id: w.id,
                    from: Some(old.offset),
                    to: None,
                });
                continue;
            };

            // 1. Where it is.
            if let Some(bytes) = fit(w.id, &body, region) {
                rest.push(Patch {
                    offset: old.offset,
                    bytes,
                });
                continue;
            }
            let bytes = element_min(w.id, &body);
            let len = bytes.len() as u64;

            // 2. Already last: let it run on.
            if region_end == end && end == old_end {
                end = old.offset + len;
                appended.push(Patch {
                    offset: old.offset,
                    bytes,
                });
                continue;
            }

            // 3. Back into the padding in front of it, ending where it
            //    ended, so the padding that remains still follows whatever
            //    it followed.
            let region_start = layout.padded_start(&old);
            let room = region_end - region_start;
            if use_padding_before && gap.is_none() && room >= len + RESERVE {
                let to = region_end - len;
                gap = Some((region_start, to - region_start));
                rest.push(Patch { offset: to, bytes });
                moves.push(Move {
                    id: w.id,
                    from: Some(old.offset),
                    to: Some(to),
                });
                continue;
            }

            // Info holds the duration and the timestamp scale, which a
            // player needs before it can play anything. At the end of the
            // file it would be the faststart problem over again, so it
            // does not go there.
            if w.id == ebml::INFO {
                return refuse(
                    Kind::NeedsReseat,
                    "the title does not fit at the front of the file; re-seat it",
                );
            }

            // 4. To the end of the segment.
            rest.push(Patch {
                offset: old.offset,
                bytes: wiped,
            });
            moves.push(Move {
                id: w.id,
                from: Some(old.offset),
                to: Some(end),
            });
            let offset = end;
            end += len;
            appended.push(Patch { offset, bytes });
        }

        let mut index: Vec<Patch> = Vec::new();
        if !moves.is_empty() {
            index.push(self.seek_head(&layout, &moves, &mut gap, start)?);
        }
        if let Some((offset, len)) = gap {
            let Some(bytes) = void(len) else {
                return internal("a gap too small for a Void");
            };
            rest.push(Patch { offset, bytes });
        }

        let mut patches = appended;
        if end != old_end {
            let Some(bytes) = encode_size(end - start, s.segment.size_len) else {
                return refuse(
                    Kind::NoRoom,
                    "the Segment size field is too narrow for the new size",
                );
            };
            patches.push(Patch {
                offset: s.segment.offset + s.segment.id_len as u64,
                bytes,
            });
        }
        patches.extend(index);
        patches.extend(rest);

        let mut spans: Vec<(u64, u64)> = patches
            .iter()
            .map(|p| (p.offset, p.offset + p.bytes.len() as u64))
            .collect();
        spans.sort();
        if spans.windows(2).any(|w| w[0].1 > w[1].0) {
            return internal("overlapping patches");
        }

        Ok(Plan {
            path: self.path.clone(),
            patches,
            old_len: old_end,
            new_len: end,
        })
    }

    /// The first `SeekHead`, re-pointed. It may only grow into the padding
    /// that directly follows it (§5.3).
    fn seek_head(
        &self,
        layout: &Layout<'_>,
        moves: &[Move],
        gap: &mut Option<(u64, u64)>,
        start: u64,
    ) -> Result<Patch> {
        if self.survey.seek_heads > 1 {
            // The entry to change may be in either, and the two would have
            // to be kept in step. A re-seat writes one index, at the front.
            return refuse(
                Kind::NeedsReseat,
                "the index is split over two SeekHeads; re-seat the file",
            );
        }
        let Some(sh) = &self.survey.seek_head else {
            return refuse(
                Kind::SeekHead,
                "the file has no SeekHead to say where the moved element went",
            );
        };
        if sh.crc.is_some_and(|c| !c.valid) {
            return refuse(Kind::Checksum, "the SeekHead fails its checksum");
        }

        let entry = |id: u32, at: u64| {
            element_min(
                ebml::SEEK,
                &[
                    element_min(ebml::SEEK_ID, &id.to_be_bytes()),
                    element_min(ebml::SEEK_POSITION, &encode_uint(at - start)),
                ]
                .concat(),
            )
        };
        let mut listed = vec![false; moves.len()];
        let mut body = Vec::new();
        for c in &sh.children {
            match c {
                SeekChild::Void => {}
                SeekChild::Raw(bytes) => body.extend_from_slice(bytes),
                SeekChild::Entry(SeekEntry { id, position }, bytes) => {
                    let moved = moves
                        .iter()
                        .position(|m| m.id == *id && m.from == Some(start + position));
                    match moved {
                        None => body.extend_from_slice(bytes),
                        Some(i) => {
                            listed[i] = true;
                            if let Some(to) = moves[i].to {
                                body.extend(entry(*id, to));
                            }
                        }
                    }
                }
            }
        }
        for (m, _) in moves.iter().zip(&listed).filter(|(_, l)| !**l) {
            if let Some(to) = m.to {
                body.extend(entry(m.id, to));
            }
        }
        let body = with_crc(sh.crc.is_some(), body);

        let mut region_end = layout.padded_end(&sh.element);
        if let Some((at, len)) = *gap {
            // The padding after the SeekHead is the padding an element
            // moved back into: what is left of it ends where that element
            // now starts.
            if at >= sh.element.end() && at <= region_end {
                region_end = at + len;
                *gap = None;
            }
        }
        match fit(ebml::SEEK_HEAD, &body, region_end - sh.element.offset) {
            Some(bytes) => Ok(Patch {
                offset: sh.element.offset,
                bytes,
            }),
            None => refuse(
                Kind::NoRoom,
                "no padding after the SeekHead for the entry it needs",
            ),
        }
    }
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.patches.is_empty()
    }

    /// Write the patches to the file the plan was made from.
    ///
    /// A failure here can leave that file partially modified. Apply to a
    /// copy and swap it in, unless losing the file is acceptable.
    pub fn apply(&self) -> Result<()> {
        self.apply_to(&self.path)
    }

    /// Write the patches to `path`, which must hold the same bytes the
    /// plan was made from: a copy of the original.
    pub fn apply_to(&self, path: impl AsRef<Path>) -> Result<()> {
        if self.is_empty() {
            return Ok(());
        }
        let mut f = OpenOptions::new().write(true).open(path)?;
        if f.metadata()?.len() != self.old_len {
            return refuse(
                Kind::Malformed,
                "the file is not the length the plan was made for",
            );
        }
        (|| {
            for p in &self.patches {
                f.seek(SeekFrom::Start(p.offset))?;
                f.write_all(&p.bytes)?;
            }
            if self.new_len < self.old_len {
                f.set_len(self.new_len)?;
            }
            f.sync_all()
        })()
        .map_err(Error::WriteFailed)
    }
}
