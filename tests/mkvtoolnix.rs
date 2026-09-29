//! Against MKVToolNix: files mkvmerge wrote, which are laid out differently
//! from ffmpeg's, and mkvmerge and mkvpropedit as readers and editors of
//! what this crate wrote. Skipped when they are not installed.
//!
//! What mkvmerge 102 does, as found here: no checksums; 4 KB of padding
//! after the SeekHead and 1 KB after Tracks; the Cues and the Tags after
//! the clusters; per-track statistics tags; no position inside a cluster.

mod common;
use common::*;
use fastmkv::{ebml, Kind, Padding};
use std::path::Path;
use std::process::Command;

fn have(program: &str) -> bool {
    Command::new(program).arg("--version").output().is_ok()
}

/// An MP4 from ffmpeg, muxed by mkvmerge.
fn mkvmerge(name: &str, extra: &[&str]) -> Option<Temp> {
    if !have("mkvmerge") || !have("ffmpeg") {
        eprintln!("mkvmerge or ffmpeg not available; skipped");
        return None;
    }
    let src = temp(&format!("{name}-src"));
    let mp4 = src.0.with_extension("mp4");
    let made = Command::new("ffmpeg")
        .args([
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
        .args(["-c:v", "mpeg4", "-g", "5", "-c:a", "aac"])
        .arg(&mp4)
        .status()
        .unwrap();
    assert!(made.success());
    let out = temp(name);
    let muxed = Command::new("mkvmerge")
        .args(["-q", "-o"])
        .arg(&out.0)
        .args(["--title", "A title", "--cluster-length", "500ms"])
        .args(extra)
        .arg(&mp4)
        .status()
        .unwrap();
    let _ = std::fs::remove_file(&mp4);
    assert!(muxed.success());
    Some(out)
}

/// mkvmerge's own reading of a file: what it identifies, and whether it
/// had anything to say against it.
fn identify(path: &Path) -> String {
    let out = Command::new("mkvmerge")
        .arg("-J")
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "mkvmerge could not identify the file");
    let json = String::from_utf8_lossy(&out.stdout).into_owned();
    let flat: String = json.split_whitespace().collect();
    assert!(flat.contains("\"errors\":[]"), "{json}");
    assert!(flat.contains("\"warnings\":[]"), "{json}");
    flat
}

/// A full pass by mkvmerge: it reads every cluster and rebuilds the file.
/// Exit status 0 is no warnings at all, and what it rebuilt holds every
/// packet the file did.
fn remuxes_cleanly(path: &Path) {
    let out = temp("remux");
    let o = Command::new("mkvmerge")
        .arg("-o")
        .arg(&out.0)
        .arg(path)
        .output()
        .unwrap();
    assert!(
        o.status.code() == Some(0),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    assert_eq!(payloads(&out.0), payloads(path));
    let _ = std::fs::remove_file(&out.0);
}

/// For each stream, how many packets and how many bytes. mkvmerge re-cuts
/// the clusters when it remuxes, which reorders packets and recomputes
/// their timestamps and durations -- on a file this crate never touched as
/// much as on one it did -- so nothing finer survives the comparison.
fn payloads(path: &Path) -> Vec<(String, usize, usize)> {
    let mut out: Vec<(String, usize, usize)> = Vec::new();
    for line in packets(path).lines().filter(|l| l.starts_with("packet|")) {
        let field = |k: &str| {
            line.split('|')
                .find_map(|f| f.strip_prefix(k))
                .unwrap_or_default()
                .to_string()
        };
        let (stream, size) = (field("stream_index="), field("size=").parse().unwrap_or(0));
        match out.iter_mut().find(|s| s.0 == stream) {
            Some(s) => {
                s.1 += 1;
                s.2 += size;
            }
            None => out.push((stream, 1, size)),
        }
    }
    out.sort();
    assert!(!out.is_empty(), "the fixture has packets to compare");
    out
}

/// The tracks a `Tag` is aimed at, and its names and values.
type TrackTag = (Vec<u64>, Vec<(String, String)>);

fn track_tags(path: &Path) -> Vec<TrackTag> {
    let mkv = fastmkv::open(path).unwrap();
    mkv.tags
        .iter()
        .flat_map(|t| t.tags.tags())
        .filter(|t| !t.is_global())
        .map(|t| {
            let simple = t
                .simple_tags()
                .map(|s| match s.value() {
                    fastmkv::TagValue::String(v) => (s.name().to_string(), v.to_string()),
                    _ => (s.name().to_string(), String::new()),
                })
                .collect();
            (t.targets().unwrap().track_uids.clone(), simple)
        })
        .collect()
}

#[test]
fn a_file_from_mkvmerge_is_read() {
    let Some(f) = mkvmerge("read", &[]) else {
        return;
    };
    let mkv = fastmkv::open(&f.0).expect("it passes the preflight");
    assert_eq!(mkv.title().as_deref(), Some("A title"));
    assert!(mkv.survey.clusters > 10);
    assert!(
        !mkv.seating().front,
        "mkvmerge puts the tags after the clusters"
    );
    let stats = track_tags(&f.0);
    assert_eq!(stats.len(), 2);
    assert!(stats[0].1.iter().any(|(n, _)| n == "NUMBER_OF_FRAMES"));
    assert!(
        mkv.get("NUMBER_OF_FRAMES").is_none(),
        "a track's, not the file's"
    );

    // Through the SeekHead alone, the quick reader finds the same.
    let m = fastmkv::read(&f.0).unwrap();
    assert_eq!(m.title.as_deref(), Some("A title"));
    assert_eq!(
        m.global().collect::<Vec<_>>(),
        mkv.global().collect::<Vec<_>>()
    );
}

#[test]
fn a_file_from_mkvmerge_is_updated() {
    let Some(f) = mkvmerge("update", &[]) else {
        return;
    };
    let media = packets(&f.0);
    let blocks = clusters(&f.0);
    let stats = track_tags(&f.0);
    let at = fastmkv::open(&f.0).unwrap().tags[0].element.unwrap().offset;

    let mut mkv = fastmkv::open(&f.0).unwrap();
    mkv.set("ARTIST", "Someone").unwrap();
    mkv.set("COMMENT", &"grown ".repeat(100)).unwrap();
    mkv.set_title(Some("A longer title than it came with"))
        .unwrap();
    mkv.plan().unwrap().apply().unwrap();

    let mkv = fastmkv::open(&f.0).unwrap();
    assert_eq!(mkv.get("ARTIST"), Some("Someone"));
    assert_eq!(
        mkv.title().as_deref(),
        Some("A longer title than it came with")
    );
    assert_eq!(
        mkv.tags[0].element.unwrap().offset,
        at,
        "last already, so it ran on"
    );
    assert_eq!(track_tags(&f.0), stats, "the statistics are as they were");
    assert_eq!(packets(&f.0), media);
    assert_eq!(clusters(&f.0), blocks);
    assert_eq!(frames(&f.0, "7"), {
        let again = mkvmerge("update-ref", &[]).unwrap();
        frames(&again.0, "7")
    });

    let seen = identify(&f.0);
    assert!(
        seen.contains("\"title\":\"Alongertitlethanitcamewith\""),
        "{seen}"
    );
    remuxes_cleanly(&f.0);
}

#[test]
fn a_file_from_mkvmerge_is_reseated() {
    let Some(f) = mkvmerge("reseat", &[]) else {
        return;
    };
    let dst = temp("reseat-dst");
    let mut mkv = fastmkv::open(&f.0).unwrap();
    mkv.set("ARTIST", "Someone").unwrap();
    mkv.reseat(&dst.0, Padding::default()).unwrap();

    let out = fastmkv::open(&dst.0).unwrap();
    assert!(out.seating().front);
    assert_eq!(out.seating().tags_padding, Padding::default().tags);
    assert_eq!(out.get("ARTIST"), Some("Someone"));
    assert_eq!(out.title().as_deref(), Some("A title"));
    assert_eq!(track_tags(&dst.0), track_tags(&f.0));
    let first = out.survey.cluster_offsets[0];
    let cues = out
        .survey
        .elements
        .iter()
        .find(|e| e.id == ebml::CUES)
        .unwrap();
    assert!(cues.offset > first, "the Cues stay where mkvmerge put them");

    assert_eq!(packets(&dst.0), packets(&f.0));
    assert_eq!(clusters(&dst.0), clusters(&f.0));
    for from in ["0", "3.3", "7", "9.5"] {
        assert_eq!(
            frames(&dst.0, from),
            frames(&f.0, from),
            "seeking to {from}"
        );
    }
    identify(&dst.0);
    remuxes_cleanly(&dst.0);
}

/// A second SeekHead that indexes every cluster. An update leaves both
/// alone; a re-seat writes one index, and the Cues still find the clusters.
#[test]
fn a_second_seek_head_of_clusters() {
    let Some(f) = mkvmerge("second", &["--clusters-in-meta-seek"]) else {
        return;
    };
    let media = packets(&f.0);
    let mut mkv = fastmkv::open(&f.0).expect("linked from the first");
    assert_eq!(mkv.survey.seek_heads, 2);
    mkv.set("ARTIST", "Someone").unwrap();
    mkv.plan().unwrap().apply().unwrap();
    let mkv = fastmkv::open(&f.0).unwrap();
    assert_eq!(mkv.survey.seek_heads, 2);
    assert_eq!(mkv.get("ARTIST"), Some("Someone"));
    assert_eq!(packets(&f.0), media);
    identify(&f.0);
    remuxes_cleanly(&f.0);

    let dst = temp("second-dst");
    mkv.reseat(&dst.0, Padding::default()).unwrap();
    let out = fastmkv::open(&dst.0).unwrap();
    assert_eq!(out.survey.seek_heads, 1);
    assert_eq!(out.get("ARTIST"), Some("Someone"));
    assert_eq!(packets(&dst.0), media);
    assert_eq!(clusters(&dst.0), clusters(&f.0));
    for from in ["0", "3.3", "7", "9.5"] {
        assert_eq!(
            frames(&dst.0, from),
            frames(&f.0, from),
            "seeking to {from}"
        );
    }
    identify(&dst.0);
    remuxes_cleanly(&dst.0);
}

fn propedit(path: &Path, args: &[&str]) {
    let o = Command::new("mkvpropedit")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        o.status.code() == Some(0),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
}

/// The reference editor and this one, taking turns on one file. Each has
/// to accept what the other left, and neither may undo the other's work.
#[test]
fn mkvpropedit_and_fastmkv_take_turns() {
    let Some(f) = mkvmerge("turns", &[]) else {
        return;
    };
    if !have("mkvpropedit") {
        return;
    }
    let media = packets(&f.0);

    let mut mkv = fastmkv::open(&f.0).unwrap();
    mkv.set("ARTIST", "From fastmkv").unwrap();
    mkv.plan().unwrap().apply().unwrap();

    propedit(&f.0, &["--edit", "info", "--set", "title=From mkvpropedit"]);
    let mkv = fastmkv::open(&f.0).expect("what mkvpropedit leaves passes the preflight");
    assert_eq!(mkv.title().as_deref(), Some("From mkvpropedit"));
    assert_eq!(mkv.get("ARTIST"), Some("From fastmkv"));

    // Re-seated, then edited by mkvpropedit until it runs out of room at
    // the front. What it does then is write a second index at the end,
    // listing everything, and leave the first pointing at it -- which is
    // not what RFC 9559 says a second SeekHead is for, and is what files
    // in the wild look like.
    let dst = temp("turns-dst");
    mkv.reseat(&dst.0, Padding::default()).unwrap();
    let long = "longer ".repeat(30);
    propedit(
        &dst.0,
        &["--edit", "info", "--set", &format!("title={long}")],
    );
    propedit(&dst.0, &["--add-track-statistics-tags"]);
    let mut mkv = fastmkv::open(&dst.0).expect("still editable");
    assert_eq!(mkv.survey.seek_heads, 2);
    assert!(!mkv.seating().front, "it moved the tags to the end");
    assert_eq!(mkv.title().as_deref().map(str::trim), Some(long.trim()));
    assert_eq!(mkv.get("ARTIST"), Some("From fastmkv"));
    assert_eq!(track_tags(&dst.0).len(), 2);
    let read = fastmkv::read(&dst.0).unwrap();
    assert_eq!(
        read.get("ARTIST"),
        Some("From fastmkv"),
        "found through both indexes"
    );

    // An edit that has to move something is not made across two indexes.
    mkv.set("COMMENT", &"and back ".repeat(50)).unwrap();
    assert_eq!(mkv.plan().unwrap_err().kind(), Some(Kind::NeedsReseat));
    let again = temp("turns-again");
    mkv.reseat(&again.0, Padding::default()).unwrap();
    let out = fastmkv::open(&again.0).unwrap();
    assert_eq!(out.survey.seek_heads, 1);
    assert!(out.seating().front);
    assert_eq!(out.get("COMMENT").map(str::len), Some(9 * 50));
    assert_eq!(out.get("ARTIST"), Some("From fastmkv"));
    assert_eq!(packets(&again.0), media);
    identify(&again.0);
    remuxes_cleanly(&again.0);

    // And mkvpropedit accepts that in turn.
    propedit(&again.0, &["--edit", "info", "--set", "title=Last word"]);
    assert_eq!(
        fastmkv::open(&again.0).unwrap().title().as_deref(),
        Some("Last word")
    );
}

/// What this crate writes from an ffmpeg file, read by mkvmerge.
#[test]
fn mkvmerge_reads_what_fastmkv_wrote_from_an_ffmpeg_file() {
    if !have("mkvmerge") || !have("ffmpeg") {
        return;
    }
    let src = temp("ff-src");
    let made = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg("testsrc=size=64x64:rate=10:duration=10")
        .args(["-c:v", "mpeg4", "-g", "5", "-cluster_time_limit", "500"])
        .args(["-metadata", "title=A title", "-f", "matroska"])
        .arg(&src.0)
        .status()
        .unwrap();
    assert!(made.success());

    let mut mkv = fastmkv::open(&src.0).unwrap();
    mkv.set("COMMENT", &"grown ".repeat(100)).unwrap();
    mkv.plan().unwrap().apply().unwrap();
    identify(&src.0);
    remuxes_cleanly(&src.0);

    let dst = temp("ff-dst");
    fastmkv::open(&src.0)
        .unwrap()
        .reseat(&dst.0, Padding::default())
        .unwrap();
    let seen = identify(&dst.0);
    assert!(seen.contains("\"title\":\"Atitle\""), "{seen}");
    remuxes_cleanly(&dst.0);
}
