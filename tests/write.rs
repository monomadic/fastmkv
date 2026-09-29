//! The write path: placement, padding, the SeekHead, checksums, and what
//! must not change (PROPOSAL-2 §5, §8).

mod common;
use common::*;
use fastmkv::ebml::{self, encode_uint};
use fastmkv::{Kind, Mkv, Plan, TagValue};

fn global_tags(simples: &[Vec<u8>]) -> Vec<u8> {
    el(ebml::TAGS, &tag(&[], simples))
}

fn pad(len: u64) -> Vec<u8> {
    ebml::void(len).unwrap()
}

struct Done {
    before: Vec<u8>,
    after: Vec<u8>,
    plan: Plan,
    mkv: Mkv,
}

/// Edit, plan, apply, and hold the result to everything that is true of
/// every write: the file still passes the preflight, its checksums hold,
/// and no byte outside the plan's patches changed.
fn write_with(name: &str, bytes: &[u8], edit: impl FnOnce(&mut Mkv)) -> Done {
    let t = write(name, bytes);
    let mut mkv = fastmkv::open(&t.0).unwrap();
    edit(&mut mkv);
    let plan = mkv.plan().unwrap();
    assert_eq!(
        std::fs::read(&t.0).unwrap(),
        bytes,
        "planning must not write"
    );
    plan.apply().unwrap();
    let after = std::fs::read(&t.0).unwrap();
    assert_eq!(after.len() as u64, plan.new_len);

    permitted(bytes, &after, &plan);

    let mut expect = bytes.to_vec();
    expect.resize(after.len(), 0);
    for p in &plan.patches {
        let at = p.offset as usize;
        expect[at..at + p.bytes.len()].copy_from_slice(&p.bytes);
    }
    assert_eq!(after, expect, "a byte outside the patches changed");

    let mkv = fastmkv::open(&t.0).expect("the result passes the preflight");
    assert!(mkv
        .survey
        .seek_head
        .iter()
        .all(|s| s.crc.is_none_or(|c| c.valid)));
    assert!(mkv.info.iter().all(|i| i.crc.is_none_or(|c| c.valid)));
    assert!(mkv.tags.iter().all(|t| t.tags.crc.is_none_or(|c| c.valid)));
    // What the reader finds through the SeekHead is what the full walk finds.
    let read = fastmkv::read(&t.0).unwrap();
    assert_eq!(
        read.global().collect::<Vec<_>>(),
        mkv.global().collect::<Vec<_>>()
    );
    assert_eq!(read.title, mkv.title());
    Done {
        before: bytes.to_vec(),
        after,
        plan,
        mkv,
    }
}

/// What a write may touch, worked out from the file as it was and without
/// reference to the plan: the elements this crate edits, padding, the
/// Segment's size field, and the space past the old end. Everything else
/// -- every cluster, Tracks, Cues, Attachments -- is compared byte for
/// byte, so a planner that patched media would fail here even though its
/// own patch list would vouch for it.
fn permitted(before: &[u8], after: &[u8], plan: &Plan) {
    let t = write("permitted", before);
    let was = fastmkv::open(&t.0).unwrap().survey;
    let editable = [ebml::SEEK_HEAD, ebml::INFO, ebml::TAGS, ebml::VOID];
    let size_field = was.segment.offset + was.segment.id_len as u64;
    let mut allowed = vec![(size_field, was.segment.data_offset())];
    allowed.extend(
        was.elements
            .iter()
            .filter(|e| editable.contains(&e.id))
            .map(|e| (e.offset, e.end())),
    );
    allowed.sort();

    let old_end = before.len() as u64;
    let mut at = 0u64;
    for &(from, to) in allowed.iter().chain([&(old_end, old_end)]) {
        let (a, b) = (at as usize, from as usize);
        assert!(
            after.len() >= b,
            "the file lost bytes it was not allowed to"
        );
        assert_eq!(after[a..b], before[a..b], "bytes {a}..{b} are not editable");
        at = to;
    }

    for p in &plan.patches {
        let (from, to) = (p.offset, p.offset + p.bytes.len() as u64);
        // Runs of editable elements are contiguous, so a patch may span
        // several; walk them.
        let mut covered = from;
        for &(a, b) in &allowed {
            if a <= covered && covered < b {
                covered = b;
            }
        }
        let ok = from >= old_end || covered >= to || covered == old_end;
        assert!(ok, "patch {from}..{to} reaches outside what may be edited");
    }
}

fn refused_with(
    name: &str,
    bytes: &[u8],
    edit: impl FnOnce(&mut Mkv) -> fastmkv::Result<()>,
) -> Kind {
    let t = write(name, bytes);
    let mut mkv = fastmkv::open(&t.0).unwrap();
    let r = edit(&mut mkv).and_then(|_| mkv.plan().map(|_| ()));
    assert_eq!(
        std::fs::read(&t.0).unwrap(),
        bytes,
        "a refusal must not write"
    );
    r.unwrap_err().kind().expect("a refusal")
}

fn count(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

#[test]
fn an_unchanged_value_plans_nothing() {
    let bytes = file(&indexed(&[
        info(),
        global_tags(&[simple("TITLE", "same")]),
        cluster(),
    ]));
    let d = write_with("noop", &bytes, |m| m.set("TITLE", "same").unwrap());
    assert!(d.plan.is_empty());
    assert_eq!(d.after, d.before);
}

/// Shorter by 0, 1, 2 and 3 bytes: exact, the size field widened, the
/// smallest Void, and one more.
#[test]
fn in_place_leftovers() {
    let bytes = file(&indexed(&[
        info(),
        global_tags(&[simple("TITLE", "abcdef")]),
        cluster(),
    ]));
    for value in ["ABCDEF", "abcde", "abcd", "abc", ""] {
        let d = write_with("leftover", &bytes, |m| m.set("TITLE", value).unwrap());
        assert_eq!(d.mkv.get("TITLE"), Some(value));
        assert_eq!(d.after.len(), d.before.len(), "{value:?} stays in place");
        assert_eq!(
            d.plan.patches.len(),
            1,
            "{value:?} leaves the SeekHead alone"
        );
        assert_eq!(
            d.mkv.tags[0].element,
            fastmkv::open(&write("l0", &bytes).0).unwrap().tags[0]
                .element
                .map(|mut e| {
                    let now = d.mkv.tags[0].element.unwrap();
                    e.size = now.size;
                    e.size_len = now.size_len;
                    e
                })
        );
    }
    let one = write_with("leftover1", &bytes, |m| m.set("TITLE", "abcde").unwrap());
    assert_eq!(
        one.mkv.tags[0].element.unwrap().size_len,
        2,
        "one byte is absorbed"
    );
    assert!(!one.mkv.survey.elements.iter().any(|e| e.id == ebml::VOID));
}

#[test]
fn growth_uses_the_padding_that_follows() {
    let parts = indexed(&[
        info(),
        global_tags(&[simple("TITLE", "a")]),
        pad(40),
        cluster(),
    ]);
    let d = write_with("pad", &file(&parts), |m| {
        m.set("TITLE", "a much longer title").unwrap()
    });
    assert_eq!(d.mkv.get("TITLE"), Some("a much longer title"));
    assert_eq!(d.after.len(), d.before.len());
    assert_eq!(d.plan.patches.len(), 1);
}

/// The common case for a file ffmpeg wrote: no room where the tags are.
#[test]
fn growth_without_padding_appends_and_reindexes() {
    let parts = [
        seek_head(&[(ebml::INFO, 0), (ebml::TAGS, 0)]),
        pad(60),
        info(),
        global_tags(&[simple("TITLE", "a")]),
        cluster(),
        cluster(),
    ];
    // Entries are fixed-width, so the offsets can be filled in afterwards.
    let l = parts[0].len() as u64 + 60;
    let i = info().len() as u64;
    let mut parts = parts.to_vec();
    parts[0] = seek_head(&[(ebml::INFO, l), (ebml::TAGS, l + i)]);
    let bytes = file(&parts);

    let d = write_with("append", &bytes, |m| {
        m.set("TITLE", "a much longer title").unwrap();
        m.set("COMMENT", "new").unwrap();
    });
    assert_eq!(d.mkv.get("TITLE"), Some("a much longer title"));
    assert_eq!(d.mkv.get("COMMENT"), Some("new"));
    assert!(d.after.len() > d.before.len());
    let at = d.mkv.tags[0].element.unwrap();
    assert_eq!(at.end(), d.after.len() as u64, "the tags are last");
    assert_eq!(d.mkv.survey.clusters, 2);
    assert_eq!(
        count(&d.after, &cluster()),
        2,
        "the clusters are as they were"
    );

    // Applied in the order that leaves the old tags readable longest.
    let offsets: Vec<u64> = d.plan.patches.iter().map(|p| p.offset).collect();
    assert_eq!(offsets[0], d.before.len() as u64, "the new tags first");
    assert!(offsets[1] < 60, "then the Segment size");

    // Now last, the next growth just runs on: no second move.
    let again = write_with("append2", &d.after, |m| {
        m.set("COMMENT", &"x".repeat(300)).unwrap()
    });
    assert_eq!(again.mkv.tags[0].element.unwrap().offset, at.offset);
    assert_eq!(again.mkv.get("COMMENT").map(str::len), Some(300));
}

#[test]
fn no_padding_for_the_seek_head_is_refused() {
    let parts = indexed(&[info(), cluster()]);
    let kind = refused_with("noroom", &file(&parts), |m| m.set("TITLE", "x"));
    assert_eq!(kind, Kind::NoRoom);
}

#[test]
fn a_move_without_a_seek_head_is_refused_but_in_place_is_not() {
    let bytes = file(&[info(), global_tags(&[simple("TITLE", "abc")]), cluster()]);
    let kind = refused_with("nosh", &bytes, |m| m.set("TITLE", "abcdefgh"));
    assert_eq!(kind, Kind::SeekHead);
    let d = write_with("nosh-ok", &bytes, |m| m.set("TITLE", "ab").unwrap());
    assert_eq!(d.mkv.get("TITLE"), Some("ab"));
}

#[test]
fn tags_are_created_where_there_were_none() {
    let l = seek_head_len(1);
    let parts = [
        seek_head(&[(ebml::INFO, l + 60)]),
        pad(60),
        info(),
        cluster(),
    ];
    let d = write_with("create", &file(&parts), |m| m.set("TITLE", "new").unwrap());
    assert_eq!(d.mkv.get("TITLE"), Some("new"));
    assert_eq!(d.mkv.tags.len(), 1);
    assert_eq!(d.mkv.survey.seek_head.as_ref().unwrap().entries.len(), 2);
}

#[test]
fn removing_the_last_tag_removes_the_element_and_its_entry() {
    let parts = indexed(&[info(), global_tags(&[simple("TITLE", "x")]), cluster()]);
    let d = write_with("empty", &file(&parts), |m| m.remove("TITLE").unwrap());
    assert!(d.mkv.tags.is_empty());
    assert_eq!(d.after.len(), d.before.len());
    let sh = d.mkv.survey.seek_head.as_ref().unwrap();
    assert!(sh.entries.iter().all(|e| e.id != ebml::TAGS));
}

/// The selection, held to on a write: everything it does not name is
/// structurally what it was.
#[test]
fn only_the_selected_tags_change() {
    let track = el(ebml::TAG_TRACK_UID, &encode_uint(77));
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
            simple("TITLE", "nested title"),
        ]),
    );
    let unknown = el(0x4DEF, b"not in any specification");
    let tags = el(
        ebml::TAGS,
        &cat(&[
            tag(
                &track,
                &[
                    simple("TITLE", "track title"),
                    simple("COMMENT", "track comment"),
                ],
            ),
            tag(
                &[],
                &[
                    simple("TITLE", "first"),
                    simple_lang("TITLE", "fra", "titre"),
                    simple("TITLE", "duplicate"),
                    simple("COMMENT", "gone"),
                    binary,
                    nested,
                    unknown.clone(),
                ],
            ),
            tag(&[], &[simple("COMMENT", "also gone")]),
        ]),
    );
    let parts = indexed(&[info(), tags, pad(64), cluster()]);
    let d = write_with("select", &file(&parts), |m| {
        m.set("TITLE", "changed").unwrap();
        m.remove("COMMENT").unwrap();
        m.set("ARTIST", "renamed").unwrap();
    });

    let tags: Vec<_> = d.mkv.tags[0].tags.tags().collect();
    assert_eq!(tags.len(), 2, "the Tag left empty is dropped");
    let of_track: Vec<_> = tags[0]
        .simple_tags()
        .map(|s| (s.name(), s.value()))
        .collect();
    assert_eq!(
        of_track,
        vec![
            ("TITLE", TagValue::String("track title")),
            ("COMMENT", TagValue::String("track comment"))
        ]
    );
    let global: Vec<_> = tags[1]
        .simple_tags()
        .map(|s| (s.name(), s.language(), s.value()))
        .collect();
    assert_eq!(
        global,
        vec![
            ("TITLE", "und", TagValue::String("changed")),
            ("TITLE", "fra", TagValue::String("titre")),
            ("COVER", "und", TagValue::Binary(&[1, 2, 3])),
            ("ARTIST", "und", TagValue::String("renamed")),
        ]
    );
    let artist = tags[1].simple_tags().last().unwrap();
    assert_eq!(
        artist.nested().next().unwrap().value(),
        TagValue::String("nested title")
    );
    assert_eq!(
        count(&d.after, &unknown),
        1,
        "an unknown child is carried through"
    );
    // The track's Tag was not re-encoded at all.
    assert_eq!(
        count(
            &d.after,
            &tag(
                &track,
                &[
                    simple("TITLE", "track title"),
                    simple("COMMENT", "track comment")
                ]
            )
        ),
        1
    );
}

/// Removing a tag removes what is inside it, and what is inside is
/// promised to survive: so it is refused, and left to the tree API.
#[test]
fn a_match_holding_nested_tags_is_not_deleted() {
    let parent = |v: &str| {
        el(
            ebml::SIMPLE_TAG,
            &cat(&[
                el(ebml::TAG_NAME, b"ARTIST"),
                el(ebml::TAG_STRING, v.as_bytes()),
                simple("SORT_WITH", "inner"),
            ]),
        )
    };
    let one = file(&indexed(&[
        info(),
        global_tags(&[parent("a")]),
        pad(64),
        cluster(),
    ]));
    assert_eq!(
        refused_with("nest-rm", &one, |m| m.remove("ARTIST")),
        Kind::HasChildren
    );
    // Changing its value keeps the children, so that is allowed.
    let d = write_with("nest-set", &one, |m| m.set("ARTIST", "b").unwrap());
    let artist = d.mkv.tags[0]
        .tags
        .tags()
        .next()
        .unwrap()
        .simple_tags()
        .next()
        .unwrap();
    assert_eq!(artist.nested().next().unwrap().name(), "SORT_WITH");

    // A duplicate that set would collapse away.
    let two = file(&indexed(&[
        info(),
        global_tags(&[simple("ARTIST", "first"), parent("second")]),
        pad(64),
        cluster(),
    ]));
    assert_eq!(
        refused_with("nest-dup", &two, |m| m.set("ARTIST", "x")),
        Kind::HasChildren
    );

    // Unknown children of the Tag or of the Tags element itself.
    let unknown = el(0x4DEF, b"unknown");
    let in_tag = el(
        ebml::TAGS,
        &tag(&[], &[simple("TITLE", "x"), unknown.clone()]),
    );
    let bytes = file(&indexed(&[info(), in_tag, cluster()]));
    assert_eq!(
        refused_with("unk-tag", &bytes, |m| m.remove("TITLE")),
        Kind::HasChildren
    );
    let in_tags = el(
        ebml::TAGS,
        &cat(&[tag(&[], &[simple("TITLE", "x")]), unknown]),
    );
    let bytes = file(&indexed(&[info(), in_tags, cluster()]));
    assert_eq!(
        refused_with("unk-tags", &bytes, |m| m.remove("TITLE")),
        Kind::HasChildren
    );
}

/// A refused edit leaves the tree as it was, so the next edit starts clean.
#[test]
fn a_refused_edit_changes_nothing_in_memory() {
    let parent = el(
        ebml::SIMPLE_TAG,
        &cat(&[
            el(ebml::TAG_NAME, b"ARTIST"),
            el(ebml::TAG_STRING, b"b"),
            simple("X", "y"),
        ]),
    );
    let bytes = file(&indexed(&[
        info(),
        global_tags(&[simple("ARTIST", "a"), parent]),
        cluster(),
    ]));
    let t = write("clean", &bytes);
    let mut mkv = fastmkv::open(&t.0).unwrap();
    assert!(mkv.set("ARTIST", "z").is_err());
    assert_eq!(mkv.get("ARTIST"), Some("a"));
    assert!(mkv.plan().unwrap().is_empty());
}

#[test]
fn a_binary_value_is_not_guessed_at() {
    let binary = el(
        ebml::SIMPLE_TAG,
        &cat(&[
            el(ebml::TAG_NAME, b"COVER"),
            el(ebml::TAG_BINARY, &[1, 2, 3]),
        ]),
    );
    let bytes = file(&indexed(&[
        info(),
        global_tags(&[binary]),
        pad(64),
        cluster(),
    ]));
    assert_eq!(
        refused_with("bin-set", &bytes, |m| m.set("COVER", "x")),
        Kind::BinaryValue
    );
    assert_eq!(
        refused_with("bin-rm", &bytes, |m| m.remove("COVER")),
        Kind::BinaryValue
    );
}

#[test]
fn checksums_are_recomputed_where_they_were() {
    let inner = el_crc(
        ebml::SIMPLE_TAG,
        &cat(&[el(ebml::TAG_NAME, b"TITLE"), el(ebml::TAG_STRING, b"old")]),
    );
    let a_tag = el_crc(ebml::TAG, &cat(&[el(ebml::TARGETS, &[]), inner]));
    let plain = tag(&[], &[simple("COMMENT", "no checksum")]);
    let tags = el_crc(ebml::TAGS, &cat(&[a_tag, plain]));
    let bytes = file(&indexed(&[info(), tags, pad(64), cluster()]));
    let d = write_with("crc", &bytes, |m| {
        m.set("TITLE", "new").unwrap();
        m.set("COMMENT", "still none").unwrap();
    });
    let tags: Vec<_> = d.mkv.tags[0].tags.tags().collect();
    assert!(d.mkv.tags[0].tags.crc.unwrap().valid);
    assert!(tags[0].crc.unwrap().valid);
    assert!(tags[0].simple_tags().next().unwrap().crc.unwrap().valid);
    assert!(tags[1].crc.is_none(), "none is added where there was none");
}

#[test]
fn a_failed_checksum_is_not_written_over() {
    let tags = el_crc(ebml::TAGS, &tag(&[], &[simple("TITLE", "x")]));
    let mut bytes = file(&[info(), tags]);
    let n = bytes.len();
    bytes[n - 1] ^= 1;
    assert_eq!(
        refused_with("badcrc", &bytes, |m| m.set("TITLE", "z")),
        Kind::Checksum
    );
    assert_eq!(
        refused_with("badcrc2", &bytes, |m| m.set("OTHER", "y")),
        Kind::Checksum
    );
}

fn titled(title: &str) -> Vec<u8> {
    el_crc(
        ebml::INFO,
        &cat(&[
            el(0x2A_D7B1, &encode_uint(1_000_000)),
            el(ebml::TITLE, title.as_bytes()),
        ]),
    )
}

/// SeekHead, padding, Info, as ffmpeg lays a file out.
fn ffmpeg_like(padding: u64, title: &str) -> Vec<u8> {
    let l = seek_head_len(1) + padding;
    file(&[
        seek_head(&[(ebml::INFO, l)]),
        pad(padding),
        titled(title),
        cluster(),
        cluster(),
    ])
}

#[test]
fn the_title_is_edited_in_info() {
    let bytes = ffmpeg_like(90, "a title");
    let d = write_with("title-short", &bytes, |m| m.set_title(Some("a")).unwrap());
    assert_eq!(d.mkv.title().as_deref(), Some("a"));
    assert_eq!(d.plan.patches.len(), 1, "shorter stays where it is");

    let d = write_with("title-none", &bytes, |m| m.set_title(None).unwrap());
    assert_eq!(d.mkv.title(), None);

    let d = write_with("title-same", &bytes, |m| {
        m.set_title(Some("a title")).unwrap()
    });
    assert!(d.plan.is_empty());
}

/// Longer, with padding in front: Info moves back into it and stays ahead
/// of the clusters.
#[test]
fn a_longer_title_moves_back_into_the_padding() {
    let bytes = ffmpeg_like(90, "a");
    let before = fastmkv::open(&write("t0", &bytes).0)
        .unwrap()
        .info
        .unwrap()
        .element;
    let d = write_with("title-long", &bytes, |m| {
        m.set_title(Some("a longer title")).unwrap()
    });
    assert_eq!(d.mkv.title().as_deref(), Some("a longer title"));
    assert_eq!(d.after.len(), d.before.len());
    let now = d.mkv.info.as_ref().unwrap().element;
    assert_eq!(now.end(), before.end());
    assert!(now.offset < before.offset);
    let els = &d.mkv.survey.elements;
    assert_eq!(
        els.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![ebml::SEEK_HEAD, ebml::VOID, ebml::INFO]
    );
}

/// Too long for the padding. Info holds what a player needs before it
/// can play, so it is not sent to the end: the caller is told to re-seat.
#[test]
fn a_title_too_long_for_the_padding_is_not_sent_to_the_end() {
    let bytes = ffmpeg_like(60, "a");
    let long = "t".repeat(200);
    let kind = refused_with("title-far", &bytes, |m| m.set_title(Some(&long)));
    assert_eq!(kind, Kind::NeedsReseat);
}

#[test]
fn title_and_tags_in_one_write() {
    let l = seek_head_len(2) + 90;
    let info = titled("a");
    let tags = global_tags(&[simple("TITLE", "a")]);
    let parts = [
        seek_head(&[(ebml::INFO, l), (ebml::TAGS, l + info.len() as u64)]),
        pad(90),
        info,
        tags,
        cluster(),
    ];
    let d = write_with("both", &file(&parts), |m| {
        m.set_title(Some("the new title")).unwrap();
        m.set("TITLE", "the new title").unwrap();
    });
    assert_eq!(d.mkv.title().as_deref(), Some("the new title"));
    assert_eq!(d.mkv.get("TITLE"), Some("the new title"));
    assert_eq!(d.mkv.survey.clusters, 1);
}

/// 126 bytes is all a one-byte size field can say.
#[test]
fn a_segment_that_cannot_say_its_new_size_is_refused() {
    let l = seek_head_len(1);
    let parts = [
        seek_head(&[(ebml::INFO, l + 60)]),
        pad(60),
        info(),
        cluster(),
    ];
    let bytes = file_narrow(&parts);
    let kind = refused_with("narrow", &bytes, |m| m.set("TITLE", &"x".repeat(100)));
    assert_eq!(kind, Kind::NoRoom);
}

#[test]
fn a_plan_is_not_applied_to_a_different_file() {
    let bytes = file(&indexed(&[
        info(),
        global_tags(&[simple("TITLE", "abc")]),
        cluster(),
    ]));
    let t = write("stale", &bytes);
    let mut mkv = fastmkv::open(&t.0).unwrap();
    mkv.set("TITLE", "ab").unwrap();
    let plan = mkv.plan().unwrap();
    let other = write("stale-other", &cat(&[bytes.clone(), vec![0]]));
    assert_eq!(
        plan.apply_to(&other.0).unwrap_err().kind(),
        Some(Kind::Malformed)
    );
}
