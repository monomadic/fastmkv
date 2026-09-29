//! Duration and tracks, from `read`.

mod common;
use common::*;
use fastmkv::ebml::{self, encode_uint};
use fastmkv::TrackKind;
use std::process::Command;

fn float(id: u32, v: f64) -> Vec<u8> {
    el(id, &v.to_be_bytes())
}

fn uint(id: u32, v: u64) -> Vec<u8> {
    el(id, &encode_uint(v))
}

fn info_with(duration: Option<f64>, scale: u64) -> Vec<u8> {
    let mut kids = vec![uint(ebml::TIMESTAMP_SCALE, scale)];
    kids.extend(duration.map(|d| float(ebml::DURATION, d)));
    el(ebml::INFO, &cat(&kids))
}

fn video_track(number: u64, codec: &str, video: &[Vec<u8>]) -> Vec<u8> {
    el(
        ebml::TRACK_ENTRY,
        &cat(&[
            uint(ebml::TRACK_NUMBER, number),
            uint(ebml::TRACK_TYPE, 1),
            el(ebml::CODEC_ID, codec.as_bytes()),
            uint(ebml::DEFAULT_DURATION, 40_000_000),
            el(ebml::VIDEO, &cat(video)),
        ]),
    )
}

fn audio_track(number: u64, codec: &str) -> Vec<u8> {
    el(
        ebml::TRACK_ENTRY,
        &cat(&[
            uint(ebml::TRACK_NUMBER, number),
            uint(ebml::TRACK_TYPE, 2),
            el(ebml::CODEC_ID, codec.as_bytes()),
            el(ebml::TRACK_LANGUAGE, b"jpn"),
            el(
                ebml::AUDIO,
                &cat(&[
                    float(ebml::SAMPLING_FREQUENCY, 48000.0),
                    uint(ebml::CHANNELS, 2),
                ]),
            ),
        ]),
    )
}

#[test]
fn reads_duration_and_tracks() {
    let tracks = el(
        ebml::TRACKS,
        &cat(&[
            video_track(
                1,
                "V_MPEG4/ISO/AVC",
                &[
                    uint(ebml::PIXEL_WIDTH, 1920),
                    uint(ebml::PIXEL_HEIGHT, 1080),
                ],
            ),
            audio_track(2, "A_OPUS"),
        ]),
    );
    let parts = indexed(&[info_with(Some(90_500.0), 1_000_000), tracks, cluster()]);
    let t = write("tracks", &file(&parts));
    let m = fastmkv::read(&t.0).unwrap();

    assert_eq!(m.duration, Some(90.5));
    assert_eq!(m.tracks.len(), 2);
    let v = m.video().unwrap();
    assert_eq!(v.kind, TrackKind::Video);
    assert_eq!(v.codec(), Some("h264"));
    assert_eq!(v.video.unwrap().shown(), (1920, 1080));
    assert_eq!(v.frame_rate(), Some(25.0));
    let a = m.audio().unwrap();
    assert_eq!(a.codec(), Some("opus"));
    assert_eq!(a.language.as_deref(), Some("jpn"));
    assert_eq!(a.audio.unwrap().channels, 2);
}

#[test]
fn the_timestamp_scale_is_applied() {
    // 2500 ticks of 10 ms.
    let parts = indexed(&[info_with(Some(2500.0), 10_000_000), cluster()]);
    let t = write("scale", &file(&parts));
    assert_eq!(fastmkv::read(&t.0).unwrap().duration, Some(25.0));
}

#[test]
fn absent_facts_are_none_not_guesses() {
    let parts = indexed(&[info_with(None, 1_000_000), cluster()]);
    let t = write("bare", &file(&parts));
    let m = fastmkv::read(&t.0).unwrap();
    assert_eq!((m.duration, m.tracks.len()), (None, 0));
    assert!(m.video().is_none());
}

#[test]
fn display_size_wins_only_when_in_pixels() {
    let dims = |unit: Option<u64>| {
        let mut v = vec![
            uint(ebml::PIXEL_WIDTH, 720),
            uint(ebml::PIXEL_HEIGHT, 480),
            uint(ebml::DISPLAY_WIDTH, 853),
            uint(ebml::DISPLAY_HEIGHT, 480),
        ];
        v.extend(unit.map(|u| uint(ebml::DISPLAY_UNIT, u)));
        let tracks = el(ebml::TRACKS, &video_track(1, "V_VP9", &v));
        let t = write("display", &file(&indexed(&[info(), tracks, cluster()])));
        fastmkv::read(&t.0).unwrap().video().unwrap().video.unwrap()
    };
    assert_eq!(dims(None).shown(), (853, 480));
    assert_eq!(dims(Some(0)).shown(), (853, 480));
    // Display units of aspect ratio are not a size.
    assert_eq!(dims(Some(3)).shown(), (720, 480));
}

#[test]
fn an_unknown_codec_id_is_kept_and_unnamed() {
    let tracks = el(ebml::TRACKS, &audio_track(1, "A_MADE_UP"));
    let t = write("codec", &file(&indexed(&[info(), tracks, cluster()])));
    let m = fastmkv::read(&t.0).unwrap();
    assert_eq!(m.tracks[0].codec_id, "A_MADE_UP");
    assert_eq!(m.tracks[0].codec(), None);
}

#[test]
fn a_broken_track_is_skipped_and_the_rest_read() {
    // No TrackType: not a track this reader can classify.
    let broken = el(ebml::TRACK_ENTRY, &uint(ebml::TRACK_NUMBER, 9));
    let tracks = el(ebml::TRACKS, &cat(&[broken, audio_track(2, "A_FLAC")]));
    let t = write("broken", &file(&indexed(&[info(), tracks, cluster()])));
    let m = fastmkv::read(&t.0).unwrap();
    assert_eq!(m.tracks.len(), 1);
    assert_eq!(m.tracks[0].number, 2);
}

/// ffprobe as the independent reader, on a file ffmpeg wrote.
#[test]
fn agrees_with_ffprobe_on_an_ffmpeg_file() {
    let t = Temp(std::env::temp_dir().join(format!("fastmkv-{}-probe.mkv", std::process::id())));
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
        .arg("testsrc=size=160x90:rate=25:duration=3")
        .args(["-f", "lavfi", "-i", "sine=duration=3"])
        .args(["-c:v", "mpeg4", "-c:a", "flac"])
        .arg(&t.0)
        .status();
    if !matches!(made, Ok(s) if s.success()) {
        eprintln!("ffmpeg not available; skipped");
        return;
    }
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries"])
        .arg("format=duration:stream=codec_name,codec_type,width,height")
        .args(["-of", "json"])
        .arg(&t.0)
        .output();
    let Ok(out) = out else {
        eprintln!("ffprobe not available; skipped");
        return;
    };
    let text = String::from_utf8(out.stdout).unwrap();
    let field = |key: &str| -> String {
        let at = text.find(&format!("\"{key}\"")).unwrap();
        text[at..].split('"').nth(3).unwrap().to_string()
    };

    let m = fastmkv::read(&t.0).unwrap();
    let probed: f64 = field("duration").parse().unwrap();
    let ours = m.duration.expect("ffmpeg writes a duration");
    assert!((ours - probed).abs() < 0.05, "{ours} vs {probed}");
    let v = m.video().unwrap();
    assert_eq!(v.codec(), Some("mpeg4"));
    assert_eq!(v.video.unwrap().shown(), (160, 90));
    assert_eq!(v.frame_rate(), Some(25.0));
    assert_eq!(m.audio().unwrap().codec(), Some("flac"));
}

fn probe_field(path: &std::path::Path, entries: &str, key: &str) -> Option<String> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            entries,
        ])
        .args(["-of", "json"])
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let at = text.find(&format!("\"{key}\""))?;
    let rest = &text[at + key.len() + 2..];
    let rest = rest.trim_start_matches([':', ' ']);
    let v = if let Some(q) = rest.strip_prefix('"') {
        q.split('"').next()?
    } else {
        rest.split([',', '\n']).next()?.trim()
    };
    Some(v.to_string())
}

/// pixel format and rotation, per encoder setting, against ffprobe.
#[test]
fn pixel_format_and_rotation_agree_with_ffprobe() {
    let cases: &[(&str, &str, &str, Option<&str>)] = &[
        ("libx264", "yuv420p", "h264", None),
        ("libx264", "yuv420p10le", "h264", None),
        ("libx264", "yuv422p", "h264", None),
        ("libx264", "yuv444p", "h264", None),
        ("libx265", "yuv420p", "hevc", None),
        ("libx265", "yuv420p10le", "hevc", None),
        ("libx264", "yuv420p", "h264", Some("90")),
        ("libx264", "yuv420p", "h264", Some("-90")),
        ("libx264", "yuv420p", "h264", Some("180")),
    ];
    let mut ran = 0;
    for (n, (enc, pix, name, rot)) in cases.iter().enumerate() {
        let base = std::env::temp_dir().join(format!("fastmkv-{}-pf{n}.mp4", std::process::id()));
        let t =
            Temp(std::env::temp_dir().join(format!("fastmkv-{}-pf{n}.mkv", std::process::id())));
        let _base = Temp(base.clone());
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
            .arg("testsrc=size=160x96:rate=25:duration=1")
            .args(["-c:v", enc, "-pix_fmt", pix])
            .arg(&base)
            .status();
        if !matches!(made, Ok(s) if s.success()) {
            continue;
        }
        let mut c = Command::new("ffmpeg");
        c.args(["-hide_banner", "-loglevel", "error", "-y"]);
        if let Some(r) = rot {
            c.args(["-display_rotation", r]);
        }
        c.arg("-i").arg(&base).args(["-c", "copy"]).arg(&t.0);
        assert!(c.status().unwrap().success());

        let m = fastmkv::read(&t.0).unwrap();
        let v = m.video().unwrap();
        let video = v.video.unwrap();
        assert_eq!(v.codec(), Some(*name));
        assert_eq!(video.pix_fmt().as_deref(), Some(*pix), "{enc} {pix}");
        assert_eq!(
            probe_field(&t.0, "stream=pix_fmt", "pix_fmt").as_deref(),
            Some(*pix),
            "ffprobe disagrees about the fixture"
        );
        let want = probe_field(&t.0, "stream_side_data=rotation", "rotation")
            .and_then(|r| r.parse::<f64>().ok());
        // +180 and -180 are one rotation; ffprobe picks a sign, the file
        // stores another.
        let turn = |r: Option<f64>| r.map(|r| r.rem_euclid(360.0));
        assert_eq!(turn(video.rotation), turn(want), "{enc} {pix} rot {rot:?}");
        ran += 1;
    }
    if ran == 0 {
        eprintln!("ffmpeg not available; skipped");
    }
}

#[test]
fn segment_tags_span_levels_and_first_of_ignores_case() {
    use fastmkv::ebml::TARGET_TYPE_VALUE;
    let global = tag(&[], &[simple("Channel", "  Chan "), simple("ARTIST", "  ")]);
    let level70 = tag(&uint(TARGET_TYPE_VALUE, 70), &[simple("ACTORS", "A, B")]);
    let track = tag(
        &el(ebml::TAG_TRACK_UID, &encode_uint(7)),
        &[simple("ACTORS", "not the file")],
    );
    let tags = el(ebml::TAGS, &cat(&[global, level70, track]));
    let t = write("segtags", &file(&indexed(&[info(), tags, cluster()])));
    let m = fastmkv::read(&t.0).unwrap();

    assert_eq!(m.first_of(&["channel"]).as_deref(), Some("Chan"));
    assert_eq!(m.first_of(&["artist"]), None, "blank is no value");
    assert_eq!(m.first_of(&["missing", "Actors"]).as_deref(), Some("A, B"));
    // `global` stays the narrow set an edit may touch.
    assert_eq!(m.get("ACTORS"), None);
}
