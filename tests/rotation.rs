mod common;
use common::*;
use fastmkv::ebml::{self, children, encode_uint};
use fastmkv::{Kind, Padding};
use std::process::Command;

fn track(number: u64, kind: u64, projection: Option<Vec<u8>>) -> Vec<u8> {
    let video = cat(&[
        el(ebml::PIXEL_WIDTH, &encode_uint(160)),
        el(ebml::PIXEL_HEIGHT, &encode_uint(96)),
        projection.unwrap_or_default(),
        el(0x55B0, b"unknown-video-child"),
    ]);
    el_crc(
        ebml::TRACK_ENTRY,
        &cat(&[
            el(ebml::TRACK_NUMBER, &encode_uint(number)),
            el(ebml::TRACK_TYPE, &encode_uint(kind)),
            el(ebml::CODEC_ID, b"V_MPEG4/ISO/AVC"),
            el_crc(ebml::VIDEO, &video),
            el(ebml::CODEC_PRIVATE, b"keep-codec-config"),
        ]),
    )
}

fn projection(degrees: f64) -> Vec<u8> {
    el_crc(
        ebml::PROJECTION,
        &cat(&[
            el(ebml::PROJECTION_TYPE, &[0]),
            el(ebml::PROJECTION_POSE_ROLL, &degrees.to_be_bytes()),
            el(0x7673, &0f64.to_be_bytes()),
            el(0x7672, b"preserve-private"),
        ]),
    )
}

fn fixture(tracks: Vec<u8>, pad: u64, version: Option<u8>) -> Vec<u8> {
    let head = el(
        ebml::EBML,
        &cat(&[
            el(ebml::DOC_TYPE, b"matroska"),
            el(ebml::DOC_TYPE_READ_VERSION, &[2]),
            version
                .map(|v| el(ebml::DOC_TYPE_VERSION, &[v]))
                .unwrap_or_default(),
        ]),
    );
    let mut parts = vec![info(), tracks];
    if let Some(padding) = ebml::void(pad) {
        parts.push(padding);
    }
    parts.push(cluster());
    let parts = indexed(&parts);
    cat(&[
        head,
        ebml::element(ebml::SEGMENT, 8, &parts.concat()).unwrap(),
    ])
}

fn rotation(path: &std::path::Path, n: u64) -> Option<f64> {
    fastmkv::read(path)
        .unwrap()
        .tracks
        .iter()
        .find(|t| t.number == n)
        .unwrap()
        .video
        .unwrap()
        .rotation
}

fn assert_crcs(data: &[u8]) {
    let kids = children(data, 0).unwrap();
    for (i, c) in kids.iter().enumerate() {
        if c.id == ebml::CRC32 {
            assert_eq!(i, 0);
            assert_eq!(
                c.data,
                fastmkv::crc::crc32(&data[c.whole.len()..]).to_le_bytes()
            );
        }
        if matches!(
            c.id,
            ebml::EBML
                | ebml::SEGMENT
                | ebml::TRACKS
                | ebml::TRACK_ENTRY
                | ebml::VIDEO
                | ebml::PROJECTION
        ) {
            assert_crcs(c.data);
        }
    }
}

#[test]
fn add_replace_remove_preserve_media_other_tracks_and_unknown_fields() {
    let other = track(9, 1, Some(projection(30.0)));
    let tracks = el_crc(ebml::TRACKS, &cat(&[track(3, 1, None), other.clone()]));
    let before = fixture(tracks, 256, Some(2));
    let t = write("rotate", &before);
    let original = fastmkv::open(&t.0).unwrap();
    let tracks = original
        .survey
        .elements
        .iter()
        .find(|e| e.id == ebml::TRACKS)
        .unwrap();
    let region_end = tracks.end() as usize + 256;
    for degrees in [Some(-90.0), Some(90.0), Some(180.0), Some(0.0), None] {
        let mut m = fastmkv::open(&t.0).unwrap();
        m.set_rotation(3, degrees).unwrap();
        let plan = m.plan().unwrap();
        plan.apply().unwrap();
        assert_eq!(rotation(&t.0, 3), degrees);
        assert_eq!(rotation(&t.0, 9), Some(30.0));
        let after = std::fs::read(&t.0).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(&before[region_end..], &after[region_end..]);
        // Independently restrict changes to the document header and the
        // original Tracks + its padding. Info and media must not change.
        for i in original.survey.doc.end as usize..before.len() {
            if !(tracks.offset as usize..region_end).contains(&i) {
                assert_eq!(before[i], after[i], "unexpected change at {i}");
            }
        }
        for bytes in [
            other.as_slice(),
            b"unknown-video-child",
            b"keep-codec-config",
        ] {
            assert!(after.windows(bytes.len()).any(|w| w == bytes));
        }
        assert_crcs(&after);
        let head = children(&after, 0).unwrap();
        let fields = children(head[0].data, 0).unwrap();
        assert_eq!(
            fields
                .iter()
                .find(|c| c.id == ebml::DOC_TYPE_VERSION)
                .unwrap()
                .data,
            &[4]
        );
        assert_eq!(
            fields
                .iter()
                .find(|c| c.id == ebml::DOC_TYPE_READ_VERSION)
                .unwrap()
                .data,
            &[2]
        );
        let mut again = fastmkv::open(&t.0).unwrap();
        again.set_rotation(3, degrees).unwrap();
        assert!(again.plan().unwrap().is_empty());
    }
}

#[test]
fn pending_edits_compose_and_reseat_handles_growth_in_both_headers() {
    let bytes = fixture(
        el_crc(ebml::TRACKS, &cat(&[track(3, 1, None), track(9, 1, None)])),
        0,
        None,
    );
    let t = write("rotate-tight", &bytes);
    let out = temp("rotate-seated");
    let mut m = fastmkv::open(&t.0).unwrap();
    m.set_rotation(3, Some(90.0)).unwrap();
    m.set_rotation(9, Some(-90.0)).unwrap();
    m.set_title(Some("rotated")).unwrap();
    m.set("ARTIST", "unchanged frames").unwrap();
    assert_eq!(m.plan().unwrap_err().kind(), Some(Kind::NeedsReseat));
    assert_eq!(std::fs::read(&t.0).unwrap(), bytes);
    m.reseat(&out.0, Padding::default()).unwrap();
    assert_eq!(rotation(&out.0, 3), Some(90.0));
    assert_eq!(rotation(&out.0, 9), Some(-90.0));
    let read = fastmkv::read(&out.0).unwrap();
    assert_eq!(read.title.as_deref(), Some("rotated"));
    assert_eq!(read.get("ARTIST"), Some("unchanged frames"));
    assert_crcs(&std::fs::read(&out.0).unwrap());
}

#[test]
fn invalid_requests_are_atomic() {
    let t = write(
        "rotate-invalid",
        &fixture(
            el(ebml::TRACKS, &cat(&[track(3, 1, None), track(4, 2, None)])),
            128,
            Some(4),
        ),
    );
    let mut m = fastmkv::open(&t.0).unwrap();
    for (n, d) in [
        (3, f64::NAN),
        (3, f64::INFINITY),
        (3, 181.0),
        (3, -181.0),
        (99, 90.0),
        (4, 90.0),
    ] {
        assert!(m.set_rotation(n, Some(d)).is_err());
        assert!(m.plan().unwrap().is_empty());
    }
    m.set_rotation(3, Some(90.0)).unwrap();
    let plan = m.plan().unwrap();
    assert!(m.set_rotation(4, Some(90.0)).is_err());
    assert_eq!(m.plan().unwrap().patches, plan.patches);
}

#[test]
fn refuse_bad_checksums_duplicates_and_spherical_projection() {
    let mut bad_crc = projection(0.0);
    // Alter the CRC value, leaving the EBML structure intact.
    let root = children(&bad_crc, 0).unwrap();
    let c = children(root[0].data, 0).unwrap()[0];
    let at = c.data.as_ptr() as usize - bad_crc.as_ptr() as usize;
    bad_crc[at] ^= 1;
    let cases = [
        el(ebml::TRACKS, &track(3, 1, Some(bad_crc))),
        el(ebml::TRACKS, &cat(&[track(3, 1, None), track(3, 1, None)])),
        el(
            ebml::TRACKS,
            &track(
                3,
                1,
                Some(el(
                    ebml::PROJECTION,
                    &cat(&[
                        el(ebml::PROJECTION_POSE_ROLL, &0f64.to_be_bytes()),
                        el(ebml::PROJECTION_POSE_ROLL, &0f64.to_be_bytes()),
                    ]),
                )),
            ),
        ),
        el(
            ebml::TRACKS,
            &track(
                3,
                1,
                Some(el(ebml::PROJECTION, &el(ebml::PROJECTION_TYPE, &[1]))),
            ),
        ),
    ];
    for tracks in cases {
        let bytes = fixture(tracks, 128, Some(4));
        let t = write("rotate-refused", &bytes);
        let mut m = fastmkv::open(&t.0).unwrap();
        assert!(m.set_rotation(3, Some(90.0)).is_err());
        assert!(m.plan().unwrap().is_empty());
        assert_eq!(std::fs::read(&t.0).unwrap(), bytes);
    }
}

#[test]
fn existing_float32_roll_does_not_need_more_room() {
    let pose = el(
        ebml::PROJECTION,
        &el(ebml::PROJECTION_POSE_ROLL, &0f32.to_be_bytes()),
    );
    let t = write(
        "rotate-f32",
        &fixture(el_crc(ebml::TRACKS, &track(3, 1, Some(pose))), 0, Some(4)),
    );
    let mut m = fastmkv::open(&t.0).unwrap();
    m.set_rotation(3, Some(-90.0)).unwrap();
    m.plan().unwrap().apply().unwrap();
    assert_eq!(rotation(&t.0, 3), Some(-90.0));
}

#[test]
fn ffmpeg_reads_rotation_and_decoded_frames_are_identical() {
    if !["ffmpeg", "ffprobe"].iter().all(|p| {
        Command::new(p)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    }) {
        eprintln!("ffmpeg/ffprobe unavailable; skipped");
        return;
    }
    let src = temp("rotate-ffmpeg");
    assert!(Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x96:rate=10:duration=1",
            "-c:v",
            "libx264"
        ])
        .arg(&src.0)
        .status()
        .unwrap()
        .success());
    let hash = |p: &std::path::Path| {
        let out = Command::new("ffmpeg")
            .args(["-v", "error", "-noautorotate", "-i"])
            .arg(p)
            .args(["-f", "framemd5", "-"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    };
    let before = hash(&src.0);
    for degrees in [-90.0, 90.0, 180.0, 0.0] {
        let dst = temp("rotate-ffmpeg-out");
        let mut m = fastmkv::open(&src.0).unwrap();
        let n = fastmkv::read(&src.0).unwrap().video().unwrap().number;
        m.set_rotation(n, Some(degrees)).unwrap();
        // Exercise both placement paths against a real muxer's output.
        m.reseat(&dst.0, Padding::default()).unwrap();
        let probe = run(
            "ffprobe",
            &[
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream_side_data=rotation",
                "-of",
                "default=nw=1:nk=1",
            ],
            &dst.0,
        );
        let reported: f64 = probe.trim().parse().unwrap_or(0.0);
        assert_eq!(reported.rem_euclid(360.0), degrees.rem_euclid(360.0));
        assert_eq!(before, hash(&dst.0));
        let only_packets = |p: &std::path::Path| {
            run(
                "ffprobe",
                &[
                    "-v",
                    "error",
                    "-show_packets",
                    "-show_data_hash",
                    "sha256",
                    "-show_entries",
                    "packet=stream_index,pts,dts,duration,size,flags,data_hash",
                    "-of",
                    "compact",
                ],
                p,
            )
        };
        assert_eq!(only_packets(&src.0), only_packets(&dst.0));
        let mut again = fastmkv::open(&dst.0).unwrap();
        again.set_rotation(n, Some(-degrees)).unwrap();
        again.plan().unwrap().apply().unwrap();
        assert_eq!(rotation(&dst.0, n), Some(-degrees));
        assert_eq!(before, hash(&dst.0));
    }
}

#[test]
fn rotation_can_grow_back_into_padding_and_reindex_tracks() {
    let tracks = el_crc(ebml::TRACKS, &track(3, 1, None));
    let raw = fixture(tracks.clone(), 0, Some(4));
    let root = children(&raw, 0).unwrap();
    let parts = indexed(&[info(), ebml::void(128).unwrap(), tracks, cluster()]);
    let bytes = cat(&[
        root[0].whole.to_vec(),
        ebml::element(ebml::SEGMENT, 8, &parts.concat()).unwrap(),
    ]);
    let t = write("rotation-move", &bytes);
    let mut m = fastmkv::open(&t.0).unwrap();
    let cluster = m.survey.cluster_offsets[0] as usize;
    let old_tracks = m
        .survey
        .elements
        .iter()
        .find(|e| e.id == ebml::TRACKS)
        .unwrap()
        .offset;
    m.set_rotation(3, Some(90.0)).unwrap();
    m.plan().unwrap().apply().unwrap();
    let after = fastmkv::open(&t.0).unwrap();
    assert!(
        after
            .survey
            .elements
            .iter()
            .find(|e| e.id == ebml::TRACKS)
            .unwrap()
            .offset
            < old_tracks
    );
    assert_eq!(rotation(&t.0, 3), Some(90.0));
    assert_eq!(&bytes[cluster..], &std::fs::read(&t.0).unwrap()[cluster..]);
}

#[test]
fn missing_version_requires_reseat_even_when_tracks_fit() {
    let bytes = fixture(el(ebml::TRACKS, &track(3, 1, None)), 128, None);
    let t = write("rotation-header", &bytes);
    let out = temp("rotation-header-copy");
    let mut m = fastmkv::open(&t.0).unwrap();
    m.set_rotation(3, Some(90.0)).unwrap();
    let err = m.plan().unwrap_err();
    assert_eq!(err.kind(), Some(Kind::NeedsReseat));
    assert!(err.to_string().contains("version header"));
    m.reseat(&out.0, Padding::default()).unwrap();
    assert_eq!(rotation(&out.0, 3), Some(90.0));
    let after = std::fs::read(&out.0).unwrap();
    let root = children(&after, 0).unwrap();
    assert_eq!(
        children(root[0].data, 0)
            .unwrap()
            .iter()
            .find(|c| c.id == ebml::DOC_TYPE_VERSION)
            .unwrap()
            .data,
        &[4]
    );
}
