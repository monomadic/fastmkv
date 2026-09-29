//! Reading, and the supported-file boundary (PROPOSAL-2 §2, §5.3).

mod common;
use common::*;
use fastmkv::ebml::{self, encode_uint};
use fastmkv::{Kind, TagValue};

fn global_tag(simples: &[Vec<u8>]) -> Vec<u8> {
    el(ebml::TAGS, &tag(&[], simples))
}

fn refused(name: &str, bytes: &[u8]) -> Kind {
    let t = write(name, bytes);
    let before = std::fs::read(&t.0).unwrap();
    let kind = fastmkv::open(&t.0).unwrap_err().kind().expect("a refusal");
    assert_eq!(fastmkv::check(&t.0).unwrap().map(|r| r.kind), Some(kind));
    assert_eq!(
        std::fs::read(&t.0).unwrap(),
        before,
        "a refusal must not write"
    );
    kind
}

#[test]
fn a_plain_file_opens_and_reads_the_same() {
    let parts = indexed(&[
        info(),
        global_tag(&[simple("TITLE", "one"), simple("COMMENT", "two")]),
        cluster(),
        cluster(),
    ]);
    let t = write("plain", &file(&parts));
    let mkv = fastmkv::open(&t.0).unwrap();
    assert_eq!(mkv.survey.clusters, 2);
    assert_eq!(mkv.get("TITLE"), Some("one"));
    let m = fastmkv::read(&t.0).unwrap();
    assert_eq!(
        m.global().collect::<Vec<_>>(),
        vec![("TITLE", "one"), ("COMMENT", "two")]
    );
}

/// Tags after the clusters are only reachable through the SeekHead when
/// the clusters are not walked.
#[test]
fn read_follows_the_seek_head_past_the_clusters() {
    let parts = indexed(&[
        info(),
        cluster(),
        cluster(),
        global_tag(&[simple("TITLE", "end")]),
    ]);
    let t = write("tail", &file(&parts));
    assert_eq!(fastmkv::read(&t.0).unwrap().get("TITLE"), Some("end"));
    assert_eq!(fastmkv::open(&t.0).unwrap().get("TITLE"), Some("end"));
}

#[test]
fn only_global_undetermined_strings_are_selected() {
    let track = el(ebml::TAG_TRACK_UID, &encode_uint(77));
    let album = el(ebml::TARGET_TYPE_VALUE, &encode_uint(70));
    let all_tracks = el(ebml::TAG_TRACK_UID, &encode_uint(0));
    let binary = el(
        ebml::SIMPLE_TAG,
        &cat(&[
            el(ebml::TAG_NAME, b"COVER"),
            el(ebml::TAG_BINARY, &[1, 2, 3]),
        ]),
    );
    let nested = el(
        ebml::SIMPLE_TAG,
        &cat(&[
            el(ebml::TAG_NAME, b"ARTIST"),
            el(ebml::TAG_STRING, b"outer"),
            simple("SORT_WITH", "inner"),
        ]),
    );
    let tags = el(
        ebml::TAGS,
        &cat(&[
            tag(&track, &[simple("TITLE", "track title")]),
            tag(&album, &[simple("TITLE", "album title")]),
            tag(&all_tracks, &[simple("TITLE", "global title")]),
            tag(
                &[],
                &[
                    simple_lang("TITLE", "fra", "titre"),
                    simple_lang("COMMENT", "und", "plain"),
                    binary,
                    nested,
                ],
            ),
        ]),
    );
    let t = write("select", &file(&indexed(&[info(), tags, cluster()])));
    let m = fastmkv::read(&t.0).unwrap();
    assert_eq!(
        m.global().collect::<Vec<_>>(),
        vec![
            ("TITLE", "global title"),
            ("COMMENT", "plain"),
            ("ARTIST", "outer")
        ]
    );

    // The rest is still in the tree, which is what lets a write keep it.
    let all: Vec<_> = m.tags[0].tags.tags().collect();
    assert_eq!(all.len(), 4);
    assert_eq!(all[0].targets().unwrap().track_uids, vec![77]);
    assert_eq!(all[1].targets().unwrap().type_value, 70);
    let last: Vec<_> = all[3].simple_tags().collect();
    assert_eq!(last[0].language(), "fra");
    assert_eq!(last[2].value(), TagValue::Binary(&[1, 2, 3]));
    assert_eq!(last[3].nested().next().unwrap().name(), "SORT_WITH");
}

#[test]
fn several_tags_elements_are_all_read() {
    let parts = indexed(&[
        info(),
        global_tag(&[simple("TITLE", "first")]),
        cluster(),
        global_tag(&[simple("COMMENT", "second")]),
    ]);
    let t = write("several", &file(&parts));
    let mkv = fastmkv::open(&t.0).unwrap();
    assert_eq!(mkv.tags.len(), 2);
    assert_eq!(mkv.get("COMMENT"), Some("second"));
}

#[test]
fn checksums_are_verified_not_carried() {
    let body = tag(&[], &[simple("TITLE", "x")]);
    let good = write(
        "crc-good",
        &file(&indexed(&[info(), el_crc(ebml::TAGS, &body)])),
    );
    let crc = fastmkv::open(&good.0).unwrap().tags[0].tags.crc.unwrap();
    assert!(crc.valid);

    let mut bytes = std::fs::read(&good.0).unwrap();
    let n = bytes.len();
    bytes[n - 1] ^= 1; // the last byte of the tag's value
    let bad = write("crc-bad", &bytes);
    let mkv = fastmkv::open(&bad.0).unwrap();
    assert!(!mkv.tags[0].tags.crc.unwrap().valid);
    assert_eq!(
        mkv.tags[0].tags.tags().count(),
        1,
        "the checksum is not a child"
    );
}

/// A stream capture: nothing has a size. Readable, not editable.
#[test]
fn an_unknown_size_segment_reads_but_does_not_open() {
    let body = cat(&[info(), global_tag(&[simple("TITLE", "live")]), cluster()]);
    let bytes = cat(&[doc("matroska", 2), unknown_size(ebml::SEGMENT, &body)]);
    let t = write("live", &bytes);
    assert_eq!(fastmkv::read(&t.0).unwrap().get("TITLE"), Some("live"));
    assert_eq!(refused("live2", &bytes), Kind::UnknownSize);
}

#[test]
fn an_unknown_size_cluster_is_refused() {
    let body = cat(&[info(), unknown_size(ebml::CLUSTER, &[0xE7, 0x81, 0])]);
    assert_eq!(refused("cluster", &file(&[body])), Kind::UnknownSize);
}

#[test]
fn the_document_type_is_checked() {
    assert_eq!(
        refused("not", b"this is not matroska at all"),
        Kind::NotMatroska
    );
    let other = cat(&[doc("other", 2), el(ebml::SEGMENT, &info())]);
    assert_eq!(refused("doctype", &other), Kind::DocType);
    let future = cat(&[doc("matroska", 9), el(ebml::SEGMENT, &info())]);
    assert_eq!(refused("version", &future), Kind::Version);
    let webm = cat(&[doc("webm", 2), el(ebml::SEGMENT, &info())]);
    assert!(fastmkv::open(&write("webm", &webm).0).is_ok());
}

#[test]
fn what_follows_the_segment_is_refused() {
    let one = file(&[info()]);
    let two = cat(&[one.clone(), el(ebml::SEGMENT, &info())]);
    assert_eq!(refused("two", &two), Kind::MultipleSegments);
    let junk = cat(&[one.clone(), vec![0; 16]]);
    assert_eq!(refused("junk", &junk), Kind::TrailingData);
    let short = &one[..one.len() - 1];
    assert_eq!(refused("short", short), Kind::Malformed);
}

#[test]
fn a_segment_checksum_is_refused() {
    let bytes = cat(&[doc("matroska", 2), el_crc(ebml::SEGMENT, &info())]);
    assert_eq!(refused("segcrc", &bytes), Kind::SegmentChecksum);
}

#[test]
fn an_element_that_overruns_is_refused() {
    let mut inner = info();
    inner[4] += 1; // Info now claims one byte more than the segment holds
    assert_eq!(refused("overrun", &file(&[inner])), Kind::Malformed);
}

#[test]
fn seek_head_rules() {
    // Not first.
    let sh = seek_head(&[(ebml::INFO, 0)]);
    assert_eq!(refused("sh-late", &file(&[info(), sh])), Kind::SeekHead);

    // Three of them.
    let l = seek_head_len(1);
    let three = [
        seek_head(&[(ebml::INFO, 3 * l)]),
        seek_head(&[(ebml::INFO, 3 * l)]),
        seek_head(&[(ebml::INFO, 3 * l)]),
        info(),
    ];
    assert_eq!(refused("sh-three", &file(&three)), Kind::SeekHead);

    // A second one the first does not point at.
    let i = info().len() as u64;
    let c = cluster().len() as u64;
    let second = [
        seek_head(&[(ebml::INFO, 2 * l)]),
        seek_head(&[(ebml::INFO, 2 * l)]),
        info(),
    ];
    assert_eq!(refused("sh-second", &file(&second)), Kind::SeekHead);
    // The first has to say where the second is.
    let unlinked = [
        seek_head(&[(ebml::INFO, 2 * l)]),
        seek_head(&[(ebml::CLUSTER, 2 * l + i)]),
        info(),
        cluster(),
    ];
    assert_eq!(refused("sh-unlinked", &file(&unlinked)), Kind::SeekHead);
    let l2 = seek_head_len(2);
    let linked = [
        seek_head(&[(ebml::INFO, l2 + l), (ebml::SEEK_HEAD, l2)]),
        seek_head(&[(ebml::CLUSTER, l2 + l + i)]),
        info(),
        cluster(),
    ];
    assert!(fastmkv::open(&write("sh-linked", &file(&linked)).0).is_ok());
    // What mkvpropedit leaves: the first points at the second and nothing
    // else, and the second lists everything.
    let l1 = seek_head_len(1);
    let moved = [
        seek_head(&[(ebml::SEEK_HEAD, l1 + i + c)]),
        info(),
        cluster(),
        seek_head(&[(ebml::INFO, l1)]),
    ];
    let mkv = fastmkv::open(&write("sh-propedit", &file(&moved)).0).unwrap();
    assert_eq!(mkv.survey.seek_heads, 2);

    // An entry that lands on something else, or on nothing.
    let wrong = [seek_head(&[(ebml::TAGS, l)]), info()];
    assert_eq!(refused("sh-wrong", &file(&wrong)), Kind::SeekHead);
    let nowhere = [seek_head(&[(ebml::INFO, l + 1)]), info()];
    assert_eq!(refused("sh-nowhere", &file(&nowhere)), Kind::SeekHead);
}

/// Tags the SeekHead does not list are accepted: the full walk finds them.
#[test]
fn unlisted_tags_are_found_by_open() {
    let l = seek_head_len(1);
    let parts = [
        seek_head(&[(ebml::INFO, l)]),
        info(),
        cluster(),
        global_tag(&[simple("TITLE", "unlisted")]),
    ];
    let t = write("unlisted", &file(&parts));
    assert_eq!(fastmkv::open(&t.0).unwrap().get("TITLE"), Some("unlisted"));
    // The reader does not walk clusters, so it cannot -- and says so.
    let m = fastmkv::read(&t.0).unwrap();
    assert_eq!(m.get("TITLE"), None);
    assert!(!m.complete);
}

#[test]
fn a_read_that_saw_everything_says_so() {
    let t = write(
        "complete",
        &file(&indexed(&[info(), global_tag(&[simple("TITLE", "x")])])),
    );
    assert!(fastmkv::read(&t.0).unwrap().complete);
}
