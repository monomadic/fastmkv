//! The `Tracks` element, read for what a player or a library listing
//! shows: what each track is, its codec, and a video track's size.
//!
//! This display model is read-only. Rotation editing separately preserves
//! the original track bytes in the rotation module. Fields a file does not carry are `None`
//! rather than guessed; a track that will not parse is skipped.

use crate::ebml::{self, children, Child};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
    /// Complex, logo, buttons, control, metadata, or a value the
    /// specification does not define.
    Other(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub number: u64,
    pub kind: TrackKind,
    /// Matroska's name for the codec, e.g. `V_MPEG4/ISO/AVC`.
    pub codec_id: String,
    pub language: Option<String>,
    pub name: Option<String>,
    /// Frame duration in nanoseconds, when the track declares one.
    pub default_duration: Option<u64>,
    pub video: Option<Video>,
    pub audio: Option<Audio>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Video {
    /// Coded size.
    pub width: u64,
    pub height: u64,
    /// The size to show, when the file states one in pixels. Differs from
    /// the coded size for anamorphic video and cropped encodes.
    pub display: Option<(u64, u64)>,
    /// Display rotation in degrees, as ffprobe reports it (`rotation` in
    /// the side data): phone footage carries ±90 here. `None` when the
    /// file says nothing, which is not the same as a stated 0.
    pub rotation: Option<f64>,
    /// Bits per sample and chroma layout, from the codec's own
    /// configuration record. Known for H.264 and HEVC only; the track
    /// header itself does not carry them.
    pub bit_depth: Option<u8>,
    pub chroma: Option<Chroma>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chroma {
    Yuv420,
    Yuv422,
    Yuv444,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Audio {
    pub sample_rate: f64,
    pub channels: u64,
}

impl Track {
    /// The codec under the name ffmpeg and ffprobe give it (`h264`,
    /// `hevc`, `aac`), or `None` for one this table does not know. Callers
    /// that match on those names can use this in place of ffprobe's
    /// `codec_name`; `None` means ask something else, not "no codec".
    pub fn codec(&self) -> Option<&'static str> {
        codec_name(&self.codec_id)
    }

    /// Frames per second from the declared frame duration.
    pub fn frame_rate(&self) -> Option<f64> {
        self.default_duration
            .filter(|&d| d > 0)
            .map(|d| 1e9 / d as f64)
    }
}

impl Video {
    /// ffmpeg's name for the pixel format (`yuv420p`, `yuv420p10le`), or
    /// `None` when the depth or layout is not known. Limited-versus-full
    /// range is not distinguished, so this never says `yuvj420p` where
    /// ffprobe would.
    pub fn pix_fmt(&self) -> Option<String> {
        let layout = match self.chroma? {
            Chroma::Yuv420 => "yuv420p",
            Chroma::Yuv422 => "yuv422p",
            Chroma::Yuv444 => "yuv444p",
        };
        Some(match self.bit_depth? {
            8 => layout.to_string(),
            n @ (9 | 10 | 12 | 14 | 16) => format!("{layout}{n}le"),
            _ => return None,
        })
    }

    /// Coded size, or the display size when the file gives one.
    pub fn shown(&self) -> (u64, u64) {
        self.display.unwrap_or((self.width, self.height))
    }
}

fn find<'a>(kids: &'a [Child<'a>], id: u32) -> Option<&'a [u8]> {
    kids.iter().find(|c| c.id == id).map(|c| c.data)
}

fn uint(kids: &[Child], id: u32) -> Option<u64> {
    find(kids, id).and_then(ebml::read_uint)
}

fn text(kids: &[Child], id: u32) -> Option<String> {
    let d = find(kids, id)?;
    let end = d.iter().position(|&b| b == 0).unwrap_or(d.len());
    String::from_utf8(d[..end].to_vec())
        .ok()
        .filter(|s| !s.is_empty())
}

/// `(bit depth, chroma)` from an `avcC` record.
///
/// Baseline, Main and Extended are 8-bit 4:2:0 by definition. The High
/// profiles say so in a trailing extension, which a record from a muxer
/// that omits it leaves us without: unknown, not guessed.
fn avc_format(private: &[u8]) -> Option<(u8, Chroma)> {
    if private.len() < 7 || private[0] != 1 {
        return None;
    }
    if matches!(private[1], 66 | 77 | 88) {
        return Some((8, Chroma::Yuv420));
    }
    // The profiles whose SPS carries chroma_format_idc and bit depth.
    if !matches!(
        private[1],
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        return None;
    }
    let mut at = 6;
    for _ in 0..private[5] & 0x1F {
        at += 2 + u16::from_be_bytes([*private.get(at)?, *private.get(at + 1)?]) as usize;
    }
    let pps = *private.get(at)? as usize;
    at += 1;
    for _ in 0..pps {
        at += 2 + u16::from_be_bytes([*private.get(at)?, *private.get(at + 1)?]) as usize;
    }
    let chroma = chroma_of(private.get(at)? & 0x03)?;
    Some((8 + (private.get(at + 1)? & 0x07), chroma))
}

/// `(bit depth, chroma)` from an `hvcC` record. Monochrome is not a
/// layout this reports.
fn hevc_format(private: &[u8]) -> Option<(u8, Chroma)> {
    if private.len() < 19 || private[0] != 1 {
        return None;
    }
    // Luma and chroma depth are separate fields; a pixel format has one.
    let (luma, chroma_depth) = (private[17] & 0x07, private[18] & 0x07);
    (luma == chroma_depth).then_some(())?;
    Some((8 + luma, chroma_of(private[16] & 0x03)?))
}

fn chroma_of(idc: u8) -> Option<Chroma> {
    match idc {
        1 => Some(Chroma::Yuv420),
        2 => Some(Chroma::Yuv422),
        3 => Some(Chroma::Yuv444),
        _ => None,
    }
}

fn video(data: &[u8], codec_id: &str, private: Option<&[u8]>) -> Option<Video> {
    let k = children(data, 0).ok()?;
    let (width, height) = (uint(&k, ebml::PIXEL_WIDTH)?, uint(&k, ebml::PIXEL_HEIGHT)?);
    // DisplayUnit 0 is pixels; any other unit (cm, inches, aspect ratio)
    // is not a size a caller can use.
    let pixels = uint(&k, ebml::DISPLAY_UNIT).unwrap_or(0) == 0;
    let display = match (
        uint(&k, ebml::DISPLAY_WIDTH),
        uint(&k, ebml::DISPLAY_HEIGHT),
    ) {
        (Some(w), Some(h)) if pixels && w > 0 && h > 0 => Some((w, h)),
        _ => None,
    };
    let rotation = find(&k, ebml::PROJECTION)
        .and_then(|d| children(d, 0).ok())
        .and_then(|p| find(&p, ebml::PROJECTION_POSE_ROLL).and_then(ebml::read_float));
    let format = match codec_id {
        "V_MPEG4/ISO/AVC" => private.and_then(avc_format),
        "V_MPEGH/ISO/HEVC" => private.and_then(hevc_format),
        _ => None,
    };
    Some(Video {
        width,
        height,
        display,
        rotation,
        bit_depth: format.map(|f| f.0),
        chroma: format.map(|f| f.1),
    })
}

fn audio(data: &[u8]) -> Option<Audio> {
    let k = children(data, 0).ok()?;
    Some(Audio {
        // Matroska defaults these to 8000 Hz and one channel.
        sample_rate: find(&k, ebml::SAMPLING_FREQUENCY)
            .and_then(ebml::read_float)
            .unwrap_or(8000.0),
        channels: uint(&k, ebml::CHANNELS).unwrap_or(1),
    })
}

fn entry(data: &[u8]) -> Option<Track> {
    let k = children(data, 0).ok()?;
    let kind = match uint(&k, ebml::TRACK_TYPE)? {
        1 => TrackKind::Video,
        2 => TrackKind::Audio,
        17 => TrackKind::Subtitle,
        n => TrackKind::Other(n),
    };
    let codec_id = text(&k, ebml::CODEC_ID)?;
    let video =
        find(&k, ebml::VIDEO).and_then(|d| video(d, &codec_id, find(&k, ebml::CODEC_PRIVATE)));
    Some(Track {
        number: uint(&k, ebml::TRACK_NUMBER)?,
        kind,
        codec_id,
        // Absent means English, per the format; that is not stated here
        // because a caller showing it should not show a default as fact.
        language: text(&k, ebml::TRACK_LANGUAGE),
        name: text(&k, ebml::TRACK_NAME),
        default_duration: uint(&k, ebml::DEFAULT_DURATION),
        video,
        audio: find(&k, ebml::AUDIO).and_then(audio),
    })
}

/// The tracks in a `Tracks` element's data, in file order.
pub(crate) fn parse(data: &[u8], base: u64) -> Vec<Track> {
    let Ok(kids) = children(data, base) else {
        return Vec::new();
    };
    kids.iter()
        .filter(|c| c.id == ebml::TRACK_ENTRY)
        .filter_map(|c| entry(c.data))
        .collect()
}

fn codec_name(id: &str) -> Option<&'static str> {
    Some(match id {
        "V_MPEG4/ISO/AVC" => "h264",
        "V_MPEGH/ISO/HEVC" => "hevc",
        "V_MPEG4/ISO/ASP" | "V_MPEG4/ISO/SP" | "V_MPEG4/ISO/AP" => "mpeg4",
        "V_MPEG4/MS/V3" => "msmpeg4v3",
        "V_MPEG1" => "mpeg1video",
        "V_MPEG2" => "mpeg2video",
        "V_VP8" => "vp8",
        "V_VP9" => "vp9",
        "V_AV1" => "av1",
        "V_THEORA" => "theora",
        "V_PRORES" => "prores",
        "A_AAC" | "A_AAC/MPEG2/LC" | "A_AAC/MPEG4/LC" | "A_AAC/MPEG4/LC/SBR" => "aac",
        "A_OPUS" => "opus",
        "A_VORBIS" => "vorbis",
        "A_FLAC" => "flac",
        "A_AC3" => "ac3",
        "A_EAC3" => "eac3",
        "A_TRUEHD" => "truehd",
        "A_MPEG/L3" => "mp3",
        "A_MPEG/L2" => "mp2",
        "A_ALAC" => "alac",
        "A_DTS" | "A_DTS/EXPRESS" | "A_DTS/LOSSLESS" => "dts",
        "S_TEXT/UTF8" => "subrip",
        "S_TEXT/ASS" | "S_TEXT/SSA" => "ass",
        "S_TEXT/WEBVTT" => "webvtt",
        "S_HDMV/PGS" => "hdmv_pgs_subtitle",
        "S_VOBSUB" => "dvd_subtitle",
        _ => return None,
    })
}
