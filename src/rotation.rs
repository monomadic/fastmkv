//! Lossless display rotation. Only the selected track's ancestor elements
//! are rebuilt; every other child is carried verbatim.
use std::fs::File;

use crate::ebml::{self, children, element_min, Child};
use crate::encode::with_crc;
use crate::error::{refuse, Kind, Result};
use crate::model::split_crc;
use crate::scan::{read_at, read_data, Element};
use crate::Mkv;

#[derive(Debug, Clone)]
pub(crate) struct TracksEdit {
    pub element: Element,
    pub body: Vec<u8>,
}

// Refuse ambiguous singleton fields rather than discard one of them.
fn single<'a>(kids: &[Child<'a>], id: u32) -> Result<Option<Child<'a>>> {
    let mut found = kids.iter().filter(|c| c.id == id);
    let first = found.next().copied();
    if found.next().is_some() {
        return refuse(
            Kind::Malformed,
            format!("duplicate rotation-related field {id:#x}"),
        );
    }
    Ok(first)
}

fn rebuild(data: &[u8], id: u32, replacement: Option<Vec<u8>>) -> Result<Vec<u8>> {
    let kids = children(data, 0)?;
    let (crc, kids) = split_crc(&kids, data);
    if crc.is_some_and(|c| !c.valid) {
        return refuse(Kind::Checksum, "a rotation ancestor fails its checksum");
    }
    // A misplaced or malformed checksum must not survive a changed parent.
    if kids.iter().any(|c| c.id == ebml::CRC32) {
        return refuse(
            Kind::Checksum,
            "unsupported checksum layout in a rotation ancestor",
        );
    }
    let mut body = Vec::new();
    let mut found = false;
    for c in kids {
        if c.id == id {
            if found {
                return refuse(Kind::Malformed, "ambiguous rotation ancestor");
            }
            found = true;
            if let Some(bytes) = &replacement {
                body.extend_from_slice(bytes);
            }
        } else {
            body.extend_from_slice(c.whole);
        }
    }
    if !found {
        if let Some(bytes) = replacement {
            body.extend(bytes);
        }
    }
    Ok(with_crc(crc.is_some(), body))
}

fn wrap(old: Option<&Child<'_>>, id: u32, body: &[u8]) -> Vec<u8> {
    old.and_then(|c| ebml::header(c.whole, 0).ok())
        .and_then(|h| ebml::element(id, h.size_len, body))
        .unwrap_or_else(|| element_min(id, body))
}

fn rotated(entry: &[u8], degrees: Option<f64>) -> Result<Option<Vec<u8>>> {
    let kids = children(entry, 0)?;
    let kind = single(&kids, ebml::TRACK_TYPE)?;
    if kind.and_then(|c| ebml::read_uint(c.data)) != Some(1) {
        return refuse(Kind::Malformed, "rotation requires a video track");
    }
    let Some(video) = single(&kids, ebml::VIDEO)? else {
        return refuse(Kind::Malformed, "the video track has no Video element");
    };
    let video_kids = children(video.data, 0)?;
    let projection = single(&video_kids, ebml::PROJECTION)?;
    let projection_data = projection.as_ref().map_or(&[][..], |c| c.data);
    let pose = children(projection_data, 0)?;
    if let Some(kind) = single(&pose, ebml::PROJECTION_TYPE)? {
        if ebml::read_uint(kind.data) != Some(0) {
            return refuse(
                Kind::Malformed,
                "display rotation requires a rectangular projection",
            );
        }
    }
    let roll = single(&pose, ebml::PROJECTION_POSE_ROLL)?;
    let current = roll.as_ref().map(|c| ebml::read_float(c.data));
    if current == degrees.map(Some) {
        return Ok(None);
    }
    let replacement = degrees.map(|d| {
        let bytes = match roll.as_ref().map(|c| c.data.len()) {
            Some(4) if (d as f32) as f64 == d => (d as f32).to_be_bytes().to_vec(),
            _ => d.to_be_bytes().to_vec(),
        };
        wrap(roll.as_ref(), ebml::PROJECTION_POSE_ROLL, &bytes)
    });
    let projection_body = rebuild(projection_data, ebml::PROJECTION_POSE_ROLL, replacement)?;
    let video_body = rebuild(
        video.data,
        ebml::PROJECTION,
        Some(wrap(
            projection.as_ref(),
            ebml::PROJECTION,
            &projection_body,
        )),
    )?;
    let entry_body = rebuild(
        entry,
        ebml::VIDEO,
        Some(wrap(Some(&video), ebml::VIDEO, &video_body)),
    )?;
    Ok(Some(entry_body))
}

impl Mkv {
    /// Set an absolute display rotation for a video `Track::number` (not
    /// its zero-based index). Positive degrees rotate counterclockwise.
    /// Accepts finite angles in -180..=180; `None` removes the roll field.
    /// Encoded frames are unchanged. Playback requires player support.
    ///
    /// Changes are staged until `plan()?.apply()` or `reseat()`. If the
    /// headers cannot grow in place, `plan()` returns `Kind::NeedsReseat`.
    /// Non-rectangular projections and ambiguous track headers are refused.
    pub fn set_rotation(&mut self, track_number: u64, degrees: Option<f64>) -> Result<()> {
        if degrees.is_some_and(|d| !d.is_finite() || !(-180.0..=180.0).contains(&d)) {
            return refuse(
                Kind::Malformed,
                "rotation must be finite and between -180 and 180 degrees",
            );
        }
        let mut file = File::open(&self.path)?;
        let mut selected = None;
        for e in self.survey.elements.iter().filter(|e| e.id == ebml::TRACKS) {
            let data = match self.track_edits.iter().find(|t| t.element == *e) {
                Some(t) => t.body.clone(),
                None => read_data(&mut file, e)?,
            };
            let kids = children(&data, e.data_offset())?;
            for (i, c) in kids
                .iter()
                .enumerate()
                .filter(|(_, c)| c.id == ebml::TRACK_ENTRY)
            {
                let fields = children(c.data, 0)?;
                let number = single(&fields, ebml::TRACK_NUMBER)?;
                if number.and_then(|c| ebml::read_uint(c.data)) == Some(track_number) {
                    if selected.is_some() {
                        return refuse(Kind::Malformed, "duplicate track number");
                    }
                    selected = Some((*e, data.clone(), i));
                }
            }
        }
        let Some((element, data, index)) = selected else {
            return refuse(Kind::Malformed, "the requested track number does not exist");
        };
        let kids = children(&data, 0)?;
        let entry = &kids[index];
        let Some(body) = rotated(entry.data, degrees)? else {
            return Ok(());
        };
        // Tracks has repeated TrackEntry children; replace only the selected one.
        let (crc, rest) = split_crc(&kids, &data);
        if crc.is_some_and(|c| !c.valid) || rest.iter().any(|c| c.id == ebml::CRC32) {
            return refuse(
                Kind::Checksum,
                "the Tracks element fails its checksum or has an unsupported checksum layout",
            );
        }
        let replacement = wrap(Some(entry), ebml::TRACK_ENTRY, &body);
        let mut body = Vec::new();
        for c in rest {
            if c.whole.as_ptr() == entry.whole.as_ptr() {
                body.extend_from_slice(&replacement);
            } else {
                body.extend_from_slice(c.whole);
            }
        }
        let body = with_crc(crc.is_some(), body);
        // ProjectionPoseRoll is a v4 element. Stage the header upgrade
        // atomically with the track edit, without raising ReadVersion.
        let header = if degrees.is_some() {
            self.rotation_header(&mut file)?
        } else {
            self.rotation_header.clone()
        };
        self.rotation_header = header;
        if let Some(t) = self.track_edits.iter_mut().find(|t| t.element == element) {
            t.body = body;
        } else {
            self.track_edits.push(TracksEdit { element, body });
        }
        Ok(())
    }

    fn rotation_header(&self, file: &mut File) -> Result<Option<Vec<u8>>> {
        if self.rotation_header.is_some() {
            return Ok(self.rotation_header.clone());
        }
        let bytes = read_at(file, 0, self.survey.doc.end as usize)?;
        let root = children(&bytes, 0)?;
        let data = root[0].data;
        let kids = children(data, 0)?;
        let version = single(&kids, ebml::DOC_TYPE_VERSION)?;
        let value = version
            .as_ref()
            .map(|c| ebml::read_uint(c.data))
            .unwrap_or(Some(1));
        let Some(value) = value else {
            return refuse(Kind::Malformed, "invalid DocTypeVersion");
        };
        if value >= 4 {
            return Ok(None);
        }
        let mut value = vec![0; version.as_ref().map_or(1, |c| c.data.len().max(1))];
        *value.last_mut().unwrap() = 4;
        rebuild(
            data,
            ebml::DOC_TYPE_VERSION,
            Some(wrap(version.as_ref(), ebml::DOC_TYPE_VERSION, &value)),
        )
        .map(Some)
    }
}
