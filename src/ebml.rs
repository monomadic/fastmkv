//! The EBML layer: variable-length integers and element headers (RFC 8794).
//!
//! Sizes are written with an explicit width. EBML lets a size be encoded
//! wider than it needs to be, and the writer depends on that to absorb a
//! leftover byte that is too small to hold a `Void` (PROPOSAL-2 §5.2).

use crate::error::{refuse, Kind, Result};

pub const EBML: u32 = 0x1A45_DFA3;
pub const EBML_READ_VERSION: u32 = 0x42F7;
pub const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
pub const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
pub const DOC_TYPE_VERSION: u32 = 0x4287;
pub const DOC_TYPE: u32 = 0x4282;
pub const DOC_TYPE_READ_VERSION: u32 = 0x4285;

pub const SEGMENT: u32 = 0x1853_8067;
pub const SEEK_HEAD: u32 = 0x114D_9B74;
pub const SEEK: u32 = 0x4DBB;
pub const SEEK_ID: u32 = 0x53AB;
pub const SEEK_POSITION: u32 = 0x53AC;
pub const INFO: u32 = 0x1549_A966;
pub const TITLE: u32 = 0x7BA9;
pub const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
pub const DURATION: u32 = 0x4489;
pub const TRACKS: u32 = 0x1654_AE6B;
pub const TRACK_ENTRY: u32 = 0xAE;
pub const TRACK_NUMBER: u32 = 0xD7;
pub const TRACK_TYPE: u32 = 0x83;
pub const CODEC_ID: u32 = 0x86;
pub const DEFAULT_DURATION: u32 = 0x23_E383;
pub const TRACK_LANGUAGE: u32 = 0x22_B59C;
pub const TRACK_NAME: u32 = 0x53_6E;
pub const VIDEO: u32 = 0xE0;
pub const PIXEL_WIDTH: u32 = 0xB0;
pub const PIXEL_HEIGHT: u32 = 0xBA;
pub const DISPLAY_WIDTH: u32 = 0x54B0;
pub const DISPLAY_HEIGHT: u32 = 0x54BA;
pub const DISPLAY_UNIT: u32 = 0x54B2;
pub const CODEC_PRIVATE: u32 = 0x63A2;
pub const PROJECTION_TYPE: u32 = 0x7671;
pub const PROJECTION: u32 = 0x7670;
pub const PROJECTION_POSE_ROLL: u32 = 0x7675;
pub const AUDIO: u32 = 0xE1;
pub const SAMPLING_FREQUENCY: u32 = 0xB5;
pub const CHANNELS: u32 = 0x9F;
pub const CLUSTER: u32 = 0x1F43_B675;
pub const CUES: u32 = 0x1C53_BB6B;
pub const ATTACHMENTS: u32 = 0x1941_A469;
pub const CHAPTERS: u32 = 0x1043_A770;
pub const TAGS: u32 = 0x1254_C367;

pub const TAG: u32 = 0x7373;
pub const TARGETS: u32 = 0x63C0;
pub const TARGET_TYPE_VALUE: u32 = 0x68CA;
pub const TARGET_TYPE: u32 = 0x63CA;
pub const TAG_TRACK_UID: u32 = 0x63C5;
pub const TAG_EDITION_UID: u32 = 0x63C9;
pub const TAG_CHAPTER_UID: u32 = 0x63C4;
pub const TAG_ATTACHMENT_UID: u32 = 0x63C6;
pub const SIMPLE_TAG: u32 = 0x67C8;
pub const TAG_NAME: u32 = 0x45A3;
pub const TAG_LANGUAGE: u32 = 0x447A;
pub const TAG_LANGUAGE_BCP47: u32 = 0x447B;
pub const TAG_DEFAULT: u32 = 0x4484;
pub const TAG_STRING: u32 = 0x4487;
pub const TAG_BINARY: u32 = 0x4485;

pub const VOID: u32 = 0xEC;
pub const CRC32: u32 = 0xBF;

/// The largest value a size field of `width` bytes can hold. The all-ones
/// pattern is reserved for "unknown", hence the `- 2`.
pub fn max_for_width(width: u8) -> u64 {
    (1u64 << (7 * width as u32)) - 2
}

pub fn min_width(value: u64) -> u8 {
    (1..=8).find(|&w| value <= max_for_width(w)).unwrap_or(8)
}

/// `None` when `value` cannot be encoded in `width` bytes.
pub fn encode_size(value: u64, width: u8) -> Option<Vec<u8>> {
    if !(1..=8).contains(&width) || value > max_for_width(width) {
        return None;
    }
    let marked = value | 1u64 << (7 * width as u32);
    Some(marked.to_be_bytes()[8 - width as usize..].to_vec())
}

pub fn encode_id(id: u32) -> Vec<u8> {
    let b = id.to_be_bytes();
    let skip = b.iter().take_while(|&&x| x == 0).count().min(3);
    b[skip..].to_vec()
}

pub fn encode_uint(value: u64) -> Vec<u8> {
    let b = value.to_be_bytes();
    let skip = b.iter().take_while(|&&x| x == 0).count().min(7);
    b[skip..].to_vec()
}

pub fn read_uint(data: &[u8]) -> Option<u64> {
    if data.len() > 8 {
        return None;
    }
    Some(data.iter().fold(0u64, |a, &b| a << 8 | b as u64))
}

/// An EBML float: 4 or 8 bytes, big-endian. Any other length is not one.
pub fn read_float(data: &[u8]) -> Option<f64> {
    match data.len() {
        4 => Some(f32::from_be_bytes(data.try_into().ok()?) as f64),
        8 => Some(f64::from_be_bytes(data.try_into().ok()?)),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// The ID with its length marker, as the specifications print it.
    pub id: u32,
    pub id_len: u8,
    /// `None` is the reserved unknown-size value.
    pub size: Option<u64>,
    pub size_len: u8,
}

impl Header {
    pub fn width(&self) -> u64 {
        (self.id_len + self.size_len) as u64
    }
}

/// Parse one element header from the front of `buf`. `at` is only for the
/// message.
pub fn header(buf: &[u8], at: u64) -> Result<Header> {
    let bad = |what: &str| refuse(Kind::Malformed, format!("{what} at offset {at}"));
    let Some(&first) = buf.first() else {
        return bad("truncated element");
    };
    let id_len = first.leading_zeros() as usize + 1;
    if id_len > 4 {
        return bad("element ID longer than 4 bytes");
    }
    if buf.len() < id_len + 1 {
        return bad("truncated element");
    }
    let id = buf[..id_len].iter().fold(0u32, |a, &b| a << 8 | b as u32);

    let s = &buf[id_len..];
    let size_len = s[0].leading_zeros() as usize + 1;
    if size_len > 8 {
        return bad("element size longer than 8 bytes");
    }
    if s.len() < size_len {
        return bad("truncated element");
    }
    let raw = s[..size_len].iter().fold(0u64, |a, &b| a << 8 | b as u64);
    let value = raw & !(1u64 << (7 * size_len as u32));
    let size = (value != max_for_width(size_len as u8) + 1).then_some(value);
    Ok(Header {
        id,
        id_len: id_len as u8,
        size,
        size_len: size_len as u8,
    })
}

/// One child of an in-memory master element.
#[derive(Debug, Clone, Copy)]
pub struct Child<'a> {
    pub id: u32,
    /// Header and data together: what to copy to carry the child through.
    pub whole: &'a [u8],
    pub data: &'a [u8],
}

/// Split a master element's data into its children. `base` is the file
/// offset of `data`, for messages.
pub fn children(data: &[u8], base: u64) -> Result<Vec<Child<'_>>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let at = base + pos as u64;
        let h = header(&data[pos..], at)?;
        let Some(size) = h.size else {
            return refuse(
                Kind::UnknownSize,
                format!("unknown-size element at offset {at}"),
            );
        };
        let start = pos + h.width() as usize;
        let end = start as u64 + size;
        if end > data.len() as u64 {
            return refuse(
                Kind::Malformed,
                format!("element at offset {at} overruns its parent"),
            );
        }
        let end = end as usize;
        out.push(Child {
            id: h.id,
            whole: &data[pos..end],
            data: &data[start..end],
        });
        pos = end;
    }
    Ok(out)
}

/// An element: ID, size at the given width, data.
pub fn element(id: u32, size_width: u8, data: &[u8]) -> Option<Vec<u8>> {
    let mut v = encode_id(id);
    v.extend(encode_size(data.len() as u64, size_width)?);
    v.extend_from_slice(data);
    Some(v)
}

/// A `Void` of exactly `len` bytes, header included. `None` below 2: the
/// ID and the size already take that much.
pub fn void(len: u64) -> Option<Vec<u8>> {
    (1..=8u8).find_map(|w| {
        let payload = len.checked_sub(1 + w as u64)?;
        element(VOID, w, &vec![0; usize::try_from(payload).ok()?])
    })
}

/// `body` as element `id`, filling `region` bytes exactly: the element,
/// then a `Void` over what is left. A single leftover byte cannot hold a
/// `Void`, so the size field is widened to absorb it (PROPOSAL-2 §5.2).
pub fn fit(id: u32, body: &[u8], region: u64) -> Option<Vec<u8>> {
    (min_width(body.len() as u64)..=8).find_map(|w| {
        let mut e = element(id, w, body)?;
        match region.checked_sub(e.len() as u64)? {
            0 => Some(e),
            1 => None,
            left => {
                e.extend(void(left)?);
                Some(e)
            }
        }
    })
}

/// An element with the narrowest size field that holds it.
pub fn element_min(id: u32, data: &[u8]) -> Vec<u8> {
    element(id, min_width(data.len() as u64), data).expect("min_width always fits")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_round_trip_at_every_width() {
        for w in 1..=8u8 {
            for v in [0, 1, max_for_width(w) / 2, max_for_width(w)] {
                let mut b = vec![0xEC];
                b.extend(encode_size(v, w).unwrap());
                let h = header(&b, 0).unwrap();
                assert_eq!((h.size, h.size_len), (Some(v), w), "value {v} width {w}");
            }
        }
    }

    /// 127 in one byte would be 0xFF, which means "unknown".
    #[test]
    fn the_all_ones_value_is_not_a_size() {
        assert_eq!(max_for_width(1), 126);
        assert_eq!(encode_size(127, 1), None);
        assert_eq!(min_width(126), 1);
        assert_eq!(min_width(127), 2);
        assert_eq!(min_width(16382), 2);
        assert_eq!(min_width(16383), 3);
        assert_eq!(header(&[0xEC, 0xFF], 0).unwrap().size, None);
        assert_eq!(
            header(&[0xEC, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], 0)
                .unwrap()
                .size,
            None
        );
    }

    #[test]
    fn ids_keep_their_marker() {
        for id in [VOID, TAG, 0x2A_D7B1, TAGS] {
            let mut b = encode_id(id);
            b.push(0x80);
            let h = header(&b, 0).unwrap();
            assert_eq!(h.id, id);
            assert_eq!(h.id_len as usize, b.len() - 1);
        }
    }

    #[test]
    fn a_child_that_overruns_is_refused() {
        let e = children(&[0xEC, 0x85, 0, 0], 0).unwrap_err();
        assert_eq!(e.kind(), Some(Kind::Malformed));
    }

    #[test]
    fn voids_are_exact() {
        assert_eq!(void(0), None);
        assert_eq!(void(1), None);
        for len in [2u64, 3, 127, 128, 129, 130, 16385, 16386, 16387, 70000] {
            let v = void(len).unwrap();
            assert_eq!(v.len() as u64, len);
            let h = header(&v, 0).unwrap();
            assert_eq!(h.size, Some(len - h.width()));
        }
    }

    #[test]
    fn fit_fills_the_region_exactly() {
        let body = [7u8; 10];
        // 1-byte ID + 1-byte size + 10 = 12 at the narrowest.
        assert_eq!(fit(VOID, &body, 11), None);
        for region in 12..40u64 {
            let out = fit(VOID, &body, region).unwrap();
            assert_eq!(out.len() as u64, region, "region {region}");
            let kids = children(&out, 0).unwrap();
            assert_eq!(kids[0].data, body);
            assert!(kids.len() <= 2);
        }
        // One byte over: no Void fits, so the size field takes it.
        let out = fit(VOID, &body, 13).unwrap();
        assert_eq!(header(&out, 0).unwrap().size_len, 2);
        assert_eq!(children(&out, 0).unwrap().len(), 1);
    }

    /// The width is chosen here, not inherited from the element being
    /// replaced, so a leftover byte can always be absorbed: the only body
    /// that needs all 8 bytes of size is one no file holds.
    #[test]
    fn fit_is_free_to_narrow_a_wide_size_field() {
        let body = [0u8; 4];
        let out = fit(VOID, &body, 1 + 8 + 4 + 1).unwrap();
        assert_eq!(header(&out, 0).unwrap().size_len, 1);
        assert_eq!(out.len(), 14);
    }

    #[test]
    fn uints() {
        assert_eq!(encode_uint(0), vec![0]);
        assert_eq!(encode_uint(0x1234), vec![0x12, 0x34]);
        assert_eq!(read_uint(&[0x12, 0x34]), Some(0x1234));
        assert_eq!(read_uint(&[]), Some(0));
    }
}
