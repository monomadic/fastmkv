//! Read and edit Matroska tags without touching the media.
//!
//! Two ways in, with different promises (docs/PROPOSAL-2.md):
//!
//! - [`read`] is for looking. It is lenient and cheap: it follows the
//!   `SeekHead` to the tags and never walks the clusters, so a stream
//!   capture with no sizes or a file with a stale index still yields what
//!   can be found.
//! - [`open`] is for editing. It accounts for every top-level element and
//!   refuses a file that falls outside the supported boundary, before
//!   anything is written.

pub mod crc;
pub mod ebml;
mod edit;
mod encode;
pub mod error;
pub mod info;
pub mod model;
pub mod plan;
pub mod reseat;
pub mod scan;
pub mod stream;

use std::fs::File;
use std::path::{Path, PathBuf};

pub use error::{Error, Kind, Refusal, Result};
pub use info::Info;
pub use model::{SimpleTag, Tag, TagValue, Tags, Targets};
pub use plan::{Patch, Plan};
pub use reseat::{Padding, Seating};
pub use scan::{Element, SeekHead};
pub use stream::{Audio, Chroma, Track, TrackKind, Video};

/// One `Tags` element and where it was found.
#[derive(Debug, Clone)]
pub struct TagsAt {
    /// `None` for one an edit created, which is not in the file yet.
    pub element: Option<Element>,
    pub tags: Tags,
    pub(crate) dirty: bool,
}

/// What [`read`] found.
#[derive(Debug, Clone)]
pub struct Metadata {
    pub doc_type: String,
    /// `Info\Title`: where ffmpeg puts a title, and what players show.
    pub title: Option<String>,
    /// Segment length in seconds, when the file states one. Absent from
    /// live captures and from files whose muxer never finished.
    pub duration: Option<f64>,
    /// Every track, in file order. Empty when the file has no `Tracks`
    /// element this read could find (see `complete`).
    pub tracks: Vec<Track>,
    pub tags: Vec<TagsAt>,
    /// `false` when tags may exist that this read did not find: the file
    /// has clusters, and anything after them that its SeekHead does not
    /// list goes unseen. An absent value is then not proof of absence.
    /// Form state that will be written back must come from [`open`].
    pub complete: bool,
}

/// A file that passed the preflight, ready to be edited.
#[derive(Debug, Clone)]
pub struct Mkv {
    pub path: PathBuf,
    pub survey: scan::Survey,
    pub info: Option<Info>,
    pub tags: Vec<TagsAt>,
}

fn load_info(f: &mut File, elements: &[Element], strict: bool) -> Result<Option<Info>> {
    let Some(e) = elements.iter().find(|e| e.id == ebml::INFO) else {
        return Ok(None);
    };
    match scan::read_data(f, e).and_then(|d| info::parse(*e, &d)) {
        Ok(i) => Ok(Some(i)),
        Err(err) if strict => Err(err),
        Err(_) => Ok(None),
    }
}

fn load_tracks(f: &mut File, elements: &[Element]) -> Vec<Track> {
    let mut out = Vec::new();
    for e in elements.iter().filter(|e| e.id == ebml::TRACKS) {
        if let Ok(d) = scan::read_data(f, e) {
            out.extend(stream::parse(&d, e.data_offset()));
        }
    }
    out
}

fn load_tags(f: &mut File, elements: &[Element], strict: bool) -> Result<Vec<TagsAt>> {
    let mut out = Vec::new();
    for e in elements.iter().filter(|e| e.id == ebml::TAGS) {
        let parsed = scan::read_data(f, e).and_then(|d| model::parse_tags(&d, e.data_offset()));
        match parsed {
            Ok(tags) => out.push(TagsAt {
                element: Some(*e),
                tags,
                dirty: false,
            }),
            Err(err) if strict => return Err(err),
            Err(_) => {}
        }
    }
    Ok(out)
}

/// The global, string-valued tags in the undetermined language: the ones
/// the convenience calls select (PROPOSAL-2 §7.2). Nested tags are not
/// descended into.
fn global(tags: &[TagsAt]) -> impl Iterator<Item = (&str, &str)> {
    tags.iter()
        .flat_map(|t| t.tags.tags())
        .filter(|t| t.is_global())
        .flat_map(|t| t.simple_tags())
        .filter(|s| s.is_undetermined_language())
        .filter_map(|s| match s.value() {
            TagValue::String(v) => Some((s.name(), v)),
            _ => None,
        })
}

impl Metadata {
    /// `(name, value)` for every string tag about the segment as a whole,
    /// at any target level (album, collection, and so on), in file order.
    /// This is what ffprobe lists as format tags. It is wider than
    /// [`global`](Self::global), which is the narrower set an edit may
    /// touch; use this one to display, never to decide what to write.
    pub fn segment(&self) -> impl Iterator<Item = (&str, &str)> {
        self.tags
            .iter()
            .flat_map(|t| t.tags.tags())
            .filter(|t| t.is_segment_wide())
            .flat_map(|t| t.simple_tags())
            .filter(|s| s.is_undetermined_language())
            .filter_map(|s| match s.value() {
                TagValue::String(v) => Some((s.name(), v)),
                _ => None,
            })
    }

    /// The first of `names` that has a non-blank [`segment`](Self::segment)
    /// tag, in the caller's order of preference. Names match without
    /// regard to case, because muxers disagree (`TITLE`, `title`) and a
    /// file that has passed through several carries whichever the last
    /// one wrote. The value is trimmed; a blank one is no value.
    pub fn first_of(&self, names: &[&str]) -> Option<String> {
        names.iter().find_map(|want| {
            self.segment()
                .filter(|(n, _)| n.eq_ignore_ascii_case(want))
                .map(|(_, v)| v.trim())
                .find(|v| !v.is_empty())
                .map(String::from)
        })
    }

    /// The first video track.
    pub fn video(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == TrackKind::Video)
    }

    /// The first audio track.
    pub fn audio(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == TrackKind::Audio)
    }

    /// `(name, value)` in file order. A name can repeat.
    pub fn global(&self) -> impl Iterator<Item = (&str, &str)> {
        global(&self.tags)
    }

    /// The first value under `name`, matched exactly.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.global().find(|(n, _)| *n == name).map(|(_, v)| v)
    }
}

impl Mkv {
    pub fn global(&self) -> impl Iterator<Item = (&str, &str)> {
        global(&self.tags)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.global().find(|(n, _)| *n == name).map(|(_, v)| v)
    }

    /// Give `name` this one value. The first match takes it and any other
    /// match is removed, so no stale duplicate is left to be read instead.
    /// Nothing outside the selection is changed (PROPOSAL-2 §7.2).
    pub fn set(&mut self, name: &str, value: &str) -> Result<()> {
        edit::set(&mut self.tags, name, value)
    }

    /// Remove every match for `name`.
    pub fn remove(&mut self, name: &str) -> Result<()> {
        edit::remove(&mut self.tags, name)
    }

    pub fn title(&self) -> Option<String> {
        self.info.as_ref().and_then(Info::title)
    }

    /// Set or clear `Info\Title`. This is separate from a `TITLE` tag;
    /// a caller that wants both sets both.
    pub fn set_title(&mut self, title: Option<&str>) -> Result<()> {
        match self.info.as_mut() {
            Some(info) => info.set_title(title),
            None => error::refuse(Kind::Malformed, "the file has no Info element"),
        }
    }
}

/// Read the tags. Refuses only what is not Matroska at all.
///
/// For display. It is fast because it does not visit the clusters, and for
/// the same reason it can miss tags: see [`Metadata::complete`]. Never
/// decide what to write from what this did not find.
pub fn read(path: impl AsRef<Path>) -> Result<Metadata> {
    let mut f = File::open(path)?;
    let found = scan::locate(&mut f)?;
    let tags = load_tags(&mut f, &found.elements, false)?;
    let info = load_info(&mut f, &found.elements, false)?;
    let tracks = load_tracks(&mut f, &found.elements);
    Ok(Metadata {
        doc_type: found.doc.doc_type,
        title: info.as_ref().and_then(|i| i.title()),
        duration: info.as_ref().and_then(Info::duration),
        tracks,
        tags,
        complete: found.complete,
    })
}

/// Run the preflight and load the tag tree for editing.
pub fn open(path: impl AsRef<Path>) -> Result<Mkv> {
    let path = path.as_ref();
    let mut f = File::open(path)?;
    let survey = scan::survey(&mut f)?;
    let tags = load_tags(&mut f, &survey.elements, true)?;
    let info = load_info(&mut f, &survey.elements, true)?;
    Ok(Mkv {
        path: path.to_path_buf(),
        survey,
        info,
        tags,
    })
}

/// Why this file could not be edited, or `None` if it can. The diagnosis
/// without the intent to write.
pub fn check(path: impl AsRef<Path>) -> Result<Option<Refusal>> {
    match open(path) {
        Ok(_) => Ok(None),
        Err(Error::Refused(r)) => Ok(Some(r)),
        Err(e) => Err(e),
    }
}
