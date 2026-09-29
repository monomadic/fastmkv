//! The `Info` element, for its `Title` alone.
//!
//! ffmpeg does not write a title as a tag: it writes `Info\Title`, and that
//! is what ffprobe and players show. Everything else in `Info` -- duration,
//! timestamp scale, the segment UID -- is carried as bytes and never
//! interpreted.

use crate::ebml::{self, children, element_min};
use crate::encode::with_crc;
use crate::error::{refuse, Kind, Result};
use crate::model::{split_crc, Crc};
use crate::scan::Element;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub element: Element,
    pub crc: Option<Crc>,
    /// `(id, whole element)`, in order, checksum excluded.
    pub(crate) children: Vec<(u32, Vec<u8>)>,
    pub(crate) dirty: bool,
}

pub(crate) fn parse(element: Element, data: &[u8]) -> Result<Info> {
    let kids = children(data, element.data_offset())?;
    let (crc, kids) = split_crc(&kids, data);
    Ok(Info {
        element,
        crc,
        children: kids.iter().map(|k| (k.id, k.whole.to_vec())).collect(),
        dirty: false,
    })
}

impl Info {
    pub fn title(&self) -> Option<String> {
        let (_, whole) = self.children.iter().find(|(id, _)| *id == ebml::TITLE)?;
        let data = children(whole, 0).ok()?.first()?.data;
        let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
        String::from_utf8(data[..end].to_vec()).ok()
    }

    fn child(&self, id: u32) -> Option<Vec<u8>> {
        let (_, whole) = self.children.iter().find(|(i, _)| *i == id)?;
        Some(children(whole, 0).ok()?.first()?.data.to_vec())
    }

    /// Nanoseconds per timestamp tick; the format's default when absent.
    pub fn timestamp_scale(&self) -> u64 {
        self.child(ebml::TIMESTAMP_SCALE)
            .and_then(|d| ebml::read_uint(&d))
            .filter(|&s| s > 0)
            .unwrap_or(1_000_000)
    }

    /// The segment's length in seconds, when the muxer wrote one. Live
    /// captures and files cut short do not.
    pub fn duration(&self) -> Option<f64> {
        let ticks = ebml::read_float(&self.child(ebml::DURATION)?)?;
        let secs = ticks * self.timestamp_scale() as f64 / 1e9;
        (secs.is_finite() && secs >= 0.0).then_some(secs)
    }

    /// `None` removes the title.
    pub(crate) fn set_title(&mut self, title: Option<&str>) -> Result<()> {
        if self.title().as_deref() == title {
            return Ok(());
        }
        if self.crc.is_some_and(|c| !c.valid) {
            return refuse(Kind::Checksum, "the Info element fails its checksum");
        }
        let new = title.map(|t| (ebml::TITLE, element_min(ebml::TITLE, t.as_bytes())));
        match (
            self.children.iter().position(|(id, _)| *id == ebml::TITLE),
            new,
        ) {
            (Some(i), Some(n)) => self.children[i] = n,
            (Some(i), None) => {
                self.children.remove(i);
            }
            (None, Some(n)) => self.children.push(n),
            (None, None) => {}
        }
        // A second Title would be a stale one left to be read.
        let mut seen = false;
        self.children
            .retain(|(id, _)| *id != ebml::TITLE || !std::mem::replace(&mut seen, true));
        self.dirty = true;
        Ok(())
    }

    pub(crate) fn body(&self) -> Vec<u8> {
        let body = self.children.iter().flat_map(|(_, w)| w.clone()).collect();
        with_crc(self.crc.is_some(), body)
    }
}
