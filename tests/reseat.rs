//! Re-seating: the media is where the index says it is, and it is the
//! same media. ffmpeg and ffprobe are the judges, since they share no code
//! with this crate. Skipped when ffmpeg is not installed.

mod common;
use common::*;
use fastmkv::crc::crc32;
use fastmkv::ebml::{self, encode_uint};
use fastmkv::{Kind, Padding};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Ten seconds, a keyframe every five frames and a cluster every half
/// second: enough clusters and cues that a wrong position would be hit.
fn ffmpeg(out: &Path, extra: &[&str]) -> bool {
    let mut c = Command::new("ffmpeg");
    c.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
    ])
    .arg("testsrc=size=64x64:rate=10:duration=10")
    .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=10"])
    .args([
        "-c:v",
        "mpeg4",
        "-g",
        "5",
        "-c:a",
        "aac",
        "-cluster_time_limit",
        "500",
    ])
    .args(["-metadata", "title=A title", "-metadata", "artist=Someone"])
    .args(extra)
    .arg(out);
    matches!(c.status(), Ok(s) if s.success())
}

struct Case {
    _src: Temp,
    _dst: Temp,
    src: PathBuf,
    dst: PathBuf,
}

fn reseated(name: &str, extra: &[&str], edit: impl FnOnce(&mut fastmkv::Mkv)) -> Option<Case> {
    let src = temp(&format!("{name}-src"));
    let dst = temp(&format!("{name}-dst"));
    if !ffmpeg(&src.0, extra) {
        eprintln!("ffmpeg not available; skipped");
        return None;
    }
    let before = std::fs::read(&src.0).unwrap();
    let mut mkv = fastmkv::open(&src.0).unwrap();
    edit(&mut mkv);
    mkv.reseat(&dst.0, Padding::default()).unwrap();
    assert_eq!(
        std::fs::read(&src.0).unwrap(),
        before,
        "the original is only read"
    );
    Some(Case {
        src: src.0.clone(),
        dst: dst.0.clone(),
        _src: src,
        _dst: dst,
    })
}

fn same_media(c: &Case) {
    assert_eq!(packets(&c.dst), packets(&c.src));
    assert_eq!(clusters(&c.dst), clusters(&c.src));
    assert!(
        clusters(&c.src).len() > 10,
        "the fixture has clusters to get wrong"
    );
    for from in ["0", "3.3", "7", "9.5"] {
        assert_eq!(
            frames(&c.dst, from),
            frames(&c.src, from),
            "seeking to {from}"
        );
    }
    run(
        "ffmpeg",
        &["-v", "error", "-xerror", "-f", "null", "-", "-i"],
        &c.dst,
    );
}

fn seated(path: &Path) {
    let mkv = fastmkv::open(path).unwrap();
    let seat = mkv.seating();
    assert!(seat.front);
    assert_eq!(seat.tags_padding, Padding::default().tags);
    assert!(fastmkv::read(path).unwrap().complete || mkv.survey.clusters > 0);
    assert!(mkv
        .survey
        .seek_head
        .as_ref()
        .unwrap()
        .crc
        .is_none_or(|c| c.valid));
}

#[test]
fn a_file_from_ffmpeg() {
    let Some(c) = reseated("plain", &[], |_| {}) else {
        return;
    };
    same_media(&c);
    seated(&c.dst);
    let src = fastmkv::open(&c.src).unwrap();
    assert_eq!(src.seating().tags_padding, 0, "ffmpeg leaves none");
    let m = fastmkv::read(&c.dst).unwrap();
    assert_eq!(m.title.as_deref(), Some("A title"));
    assert_eq!(m.get("ARTIST"), Some("Someone"));
}

/// The Cues in front of the clusters: their length moves the clusters,
/// and the clusters' positions set their length.
#[test]
fn cues_at_the_front() {
    let Some(c) = reseated("front", &["-reserve_index_space", "4096"], |_| {}) else {
        return;
    };
    let src = fastmkv::open(&c.src).unwrap();
    let cues = src
        .survey
        .elements
        .iter()
        .find(|e| e.id == ebml::CUES)
        .unwrap();
    assert!(
        cues.offset < src.survey.cluster_offsets[0],
        "the fixture is what it claims"
    );
    same_media(&c);
    seated(&c.dst);
}

#[test]
fn edits_are_made_in_the_copy() {
    let long = "a title far too long for the front of the file ".repeat(8);
    let Some(c) = reseated("edit", &[], |m| {
        m.set_title(Some(&long)).unwrap();
        m.set("ARTIST", "Someone else").unwrap();
        m.set("COMMENT", "new").unwrap();
    }) else {
        return;
    };
    same_media(&c);
    seated(&c.dst);
    let m = fastmkv::open(&c.dst).unwrap();
    assert_eq!(m.title().as_deref(), Some(long.as_str()));
    assert_eq!(m.get("ARTIST"), Some("Someone else"));
    assert_eq!(m.get("COMMENT"), Some("new"));
    let tags = run(
        "ffprobe",
        &[
            "-v",
            "error",
            "-show_entries",
            "format_tags",
            "-of",
            "default=nw=1",
            "--",
        ],
        &c.dst,
    );
    assert!(tags.contains("TAG:ARTIST=Someone else\n"), "{tags}");
    assert!(tags.contains(&format!("TAG:title={long}\n")), "{tags}");
}

/// The whole point: tags an earlier edit sent to the end come back, and
/// the edits after that stay at the front and cost nothing.
#[test]
fn tags_at_the_end_come_back_to_the_front() {
    let src = temp("back-src");
    let dst = temp("back-dst");
    if !ffmpeg(&src.0, &[]) {
        eprintln!("ffmpeg not available; skipped");
        return;
    }
    let media = packets(&src.0);
    let mut mkv = fastmkv::open(&src.0).unwrap();
    mkv.set("COMMENT", &"grown ".repeat(100)).unwrap();
    mkv.plan().unwrap().apply().unwrap();
    let mkv = fastmkv::open(&src.0).unwrap();
    assert!(!mkv.seating().front, "the append put them at the end");

    mkv.reseat(&dst.0, Padding::default()).unwrap();
    seated(&dst.0);
    assert_eq!(packets(&dst.0), media);

    let size = std::fs::metadata(&dst.0).unwrap().len();
    let mut mkv = fastmkv::open(&dst.0).unwrap();
    mkv.set("COMMENT", &"grown again ".repeat(200)).unwrap();
    mkv.set_title(Some(&"a longer title ".repeat(20))).unwrap();
    let plan = mkv.plan().unwrap();
    plan.apply().unwrap();
    assert_eq!(
        std::fs::metadata(&dst.0).unwrap().len(),
        size,
        "in the padding"
    );
    assert!(fastmkv::open(&dst.0).unwrap().seating().front);
    assert_eq!(packets(&dst.0), media);
    assert_eq!(frames(&dst.0, "7"), frames(&src.0, "7"));
}

#[test]
fn a_title_that_does_not_fit_asks_for_a_reseat() {
    let src = temp("ask");
    if !ffmpeg(&src.0, &[]) {
        eprintln!("ffmpeg not available; skipped");
        return;
    }
    let before = std::fs::read(&src.0).unwrap();
    let mut mkv = fastmkv::open(&src.0).unwrap();
    mkv.set_title(Some(&"long ".repeat(100))).unwrap();
    assert_eq!(mkv.plan().unwrap_err().kind(), Some(Kind::NeedsReseat));
    assert_eq!(std::fs::read(&src.0).unwrap(), before);
}

#[test]
fn the_destination_is_never_overwritten() {
    let bytes = file(&indexed(&[info(), cluster()]));
    let src = write("exists-src", &bytes);
    let dst = write("exists-dst", b"something the user has");
    let mkv = fastmkv::open(&src.0).unwrap();
    assert!(mkv.reseat(&dst.0, Padding::default()).is_err());
    assert_eq!(std::fs::read(&dst.0).unwrap(), b"something the user has");
}

/// A cluster that records its own position, under a checksum: mkvmerge's
/// habit, not ffmpeg's, so it is built by hand.
fn positioned(at: u64, checksummed: bool) -> Vec<u8> {
    let body = cat(&[
        el(0xE7, &[0]),
        el(0xA7, &at.to_be_bytes()[4..]),
        el(0xA3, &[0x81, 0, 0, 0x80, 1, 2, 3, 4]),
    ]);
    match checksummed {
        true => el_crc(ebml::CLUSTER, &body),
        false => el(ebml::CLUSTER, &body),
    }
}

fn cue(time: u64, at: u64) -> Vec<u8> {
    el(
        0xBB,
        &cat(&[
            el(0xB3, &encode_uint(time)),
            el(0xB7, &cat(&[el(0xF7, &[1]), el(0xF1, &encode_uint(at))])),
        ]),
    )
}

#[test]
fn positions_inside_the_file_are_corrected() {
    for checksummed in [false, true] {
        let l = seek_head_len(2);
        let i = info().len() as u64;
        let c = positioned(0, checksummed).len() as u64;
        let first = l + i;
        let parts = [
            seek_head(&[(ebml::INFO, l), (ebml::CUES, first + 2 * c)]),
            info(),
            positioned(first, checksummed),
            positioned(first + c, checksummed),
            el_crc(ebml::CUES, &cat(&[cue(0, first), cue(1, first + c)])),
        ];
        let src = write("pos-src", &file(&parts));
        let dst = temp("pos-dst");
        fastmkv::open(&src.0)
            .unwrap()
            .reseat(&dst.0, Padding::default())
            .unwrap();

        let out = std::fs::read(&dst.0).unwrap();
        let mkv = fastmkv::open(&dst.0).unwrap();
        let start = mkv.survey.segment.data_offset();
        assert_eq!(mkv.survey.cluster_offsets.len(), 2);
        assert!(mkv.survey.cluster_offsets[0] - start > first, "they moved");
        for &at in &mkv.survey.cluster_offsets {
            let whole = &out[at as usize..at as usize + c as usize];
            let kids = ebml::children(whole, 0).unwrap();
            let inner = ebml::children(kids[0].data, 0).unwrap();
            let position = inner.iter().find(|k| k.id == 0xA7).unwrap();
            assert_eq!(ebml::read_uint(position.data), Some(at - start));
            assert_eq!(position.data.len(), 4, "its width is kept");
            if checksummed {
                let sum = u32::from_le_bytes(inner[0].data.try_into().unwrap());
                assert_eq!(sum, crc32(&kids[0].data[inner[0].whole.len()..]));
            }
            // The block, which is the media, is as it was.
            assert_eq!(inner.last().unwrap().data, [0x81, 0, 0, 0x80, 1, 2, 3, 4]);
        }

        let cues = mkv
            .survey
            .elements
            .iter()
            .find(|e| e.id == ebml::CUES)
            .unwrap();
        let data = &out[cues.data_offset() as usize..cues.end() as usize];
        let kids = ebml::children(data, 0).unwrap();
        assert_eq!(
            u32::from_le_bytes(kids[0].data.try_into().unwrap()),
            crc32(&data[kids[0].whole.len()..])
        );
        let pointed: Vec<u64> = kids[1..]
            .iter()
            .map(|p| {
                let tp = ebml::children(p.data, 0).unwrap();
                let inner = ebml::children(tp[1].data, 0).unwrap();
                ebml::read_uint(inner[1].data).unwrap() + start
            })
            .collect();
        assert_eq!(pointed, mkv.survey.cluster_offsets);
    }
}

#[test]
fn what_cannot_be_corrected_is_refused() {
    let l = seek_head_len(2);
    let i = info().len() as u64;
    let c = cluster().len() as u64;
    let with = |cues: Vec<u8>| {
        file(&[
            seek_head(&[(ebml::INFO, l), (ebml::CUES, l + i + c)]),
            info(),
            cluster(),
            cues,
        ])
    };
    let refused = |name: &str, bytes: &[u8]| {
        let src = write(name, bytes);
        let dst = temp(&format!("{name}-dst"));
        let e = fastmkv::open(&src.0)
            .unwrap()
            .reseat(&dst.0, Padding::default())
            .unwrap_err();
        assert!(!dst.0.exists(), "nothing is left behind");
        e.kind()
    };

    // A cue that points between clusters.
    let stale = with(el(ebml::CUES, &cue(0, l + i + 1)));
    assert_eq!(refused("stale", &stale), Some(Kind::Position));

    // A position of a kind this crate does not follow.
    let state = el(
        0xBB,
        &cat(&[
            el(0xB3, &[0]),
            el(
                0xB7,
                &cat(&[
                    el(0xF7, &[1]),
                    el(0xF1, &encode_uint(l + i)),
                    el(0xEA, &[9]),
                ]),
            ),
        ]),
    );
    assert_eq!(
        refused("state", &with(el(ebml::CUES, &state))),
        Some(Kind::Position)
    );
}
