//! Against a file ffmpeg writes at test time: an independent writer's
//! checksums and layout, and ffprobe as an independent reader.
//! Skipped when ffmpeg is not installed.

mod common;
use std::process::Command;

fn ffmpeg(out: &std::path::Path, extra: &[&str]) -> bool {
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
    .arg("testsrc=size=64x64:rate=5:duration=2")
    .args(["-c:v", "mpeg4"])
    .args(extra)
    .arg(out);
    matches!(c.status(), Ok(s) if s.success())
}

#[test]
fn reads_what_ffmpeg_wrote() {
    let t = common::Temp(
        std::env::temp_dir().join(format!("fastmkv-{}-ffmpeg.mkv", std::process::id())),
    );
    if !ffmpeg(
        &t.0,
        &[
            "-metadata",
            "title=A title",
            "-metadata",
            "artist=Someone",
            "-metadata",
            "comment=ünïcode",
        ],
    ) {
        eprintln!("ffmpeg not available; skipped");
        return;
    }
    let mkv = fastmkv::open(&t.0).unwrap();
    assert_eq!(mkv.get("ARTIST"), Some("Someone"));
    assert_eq!(mkv.get("COMMENT"), Some("ünïcode"));
    assert_eq!(fastmkv::read(&t.0).unwrap().get("ARTIST"), Some("Someone"));

    // ffmpeg does not write the title as a tag: it goes in the Info
    // element's Title, which is outside what this crate models.
    assert_eq!(mkv.get("TITLE"), None);

    // ffmpeg checksums its top-level elements; agreeing with it is the
    // test of this crate's CRC against an implementation it did not write.
    let sh = mkv
        .survey
        .seek_head
        .as_ref()
        .expect("ffmpeg writes a SeekHead");
    assert!(sh.crc.is_some_and(|c| c.valid));
    assert!(mkv.tags.iter().all(|t| t.tags.crc.is_none_or(|c| c.valid)));
    assert!(mkv.tags.iter().any(|t| t.tags.crc.is_some()));

    // The per-track DURATION tags ffmpeg adds are not global.
    assert_eq!(mkv.get("DURATION"), None);
}

fn probe(path: &std::path::Path, entries: &str) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            entries,
            "-of",
            "default=nw=1",
            "--",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Decoding every frame is the test that the media is where the index
/// says it is.
fn decodes(path: &std::path::Path) -> bool {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-xerror", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    out.status.success() && out.stderr.is_empty()
}

/// An independent reader agrees with what was written, on each placement:
/// in place, back into the padding, and moved to the end.
#[test]
fn ffprobe_reads_what_was_written() {
    let t = common::Temp(
        std::env::temp_dir().join(format!("fastmkv-{}-ffmpeg-w.mkv", std::process::id())),
    );
    if !ffmpeg(
        &t.0,
        &["-metadata", "title=A title", "-metadata", "artist=Someone"],
    ) {
        eprintln!("ffmpeg not available; skipped");
        return;
    }
    let streams = probe(
        &t.0,
        "stream=index,codec_name,width,height,nb_read_packets:format=duration",
    );
    let media = |p: &std::path::Path| {
        let b = std::fs::read(p).unwrap();
        let mkv = fastmkv::open(p).unwrap();
        let first = mkv
            .survey
            .elements
            .iter()
            .find(|e| e.id == fastmkv::ebml::TAGS)
            .unwrap()
            .end();
        let cues = mkv
            .survey
            .elements
            .iter()
            .find(|e| e.id == fastmkv::ebml::CUES)
            .unwrap();
        b[first as usize..cues.end() as usize].to_vec()
    };
    let clusters = media(&t.0);

    let long = "long ".repeat(200);
    for (title, artist) in [
        ("A", "Some"),
        ("A rather longer title", "Someone else entirely, at length"),
        // Tags may go to the end; the title may not, so it stays short.
        ("A title of much the same length", long.as_str()),
        ("Back to short", "x"),
    ] {
        let mut mkv = fastmkv::open(&t.0).unwrap();
        mkv.set_title(Some(title)).unwrap();
        mkv.set("ARTIST", artist).unwrap();
        mkv.set("COMMENT", "ünïcode").unwrap();
        mkv.plan().unwrap().apply().unwrap();

        let tags = probe(&t.0, "format_tags");
        assert!(tags.contains(&format!("TAG:title={title}\n")), "{tags}");
        assert!(tags.contains(&format!("TAG:ARTIST={artist}\n")), "{tags}");
        assert!(tags.contains("TAG:COMMENT=ünïcode\n"), "{tags}");
        assert_eq!(tags.matches("TAG:ARTIST=").count(), 1);
        assert_eq!(
            probe(
                &t.0,
                "stream=index,codec_name,width,height,nb_read_packets:format=duration"
            ),
            streams
        );
        assert!(decodes(&t.0));
        assert!(fastmkv::check(&t.0).unwrap().is_none());
    }
    // Through four rewrites, the last of which moved things to the end.
    let after = std::fs::read(&t.0).unwrap();
    assert!(after
        .windows(clusters.len())
        .any(|w| w == clusters.as_slice()));
}
