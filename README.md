# fastmkv

Read and edit Matroska metadata without touching the media.

`fastmkv` changes the tags, title, and display rotation of an `.mkv` or `.webm` file and
leaves every cluster where it was, byte for byte. An edit to a
multi-gigabyte file writes a few kilobytes. It has no dependencies.

> **Unstable.** The API will change. It is developed alongside
> [tagform](https://github.com/monomadic/tagform), which is its first user.

## Why

The usual way to retag a Matroska file is to remux it: unpack every packet
and build a new container around them. That rewrites the whole file, and
keeps only what the remuxer has a model of. `fastmkv` parses the container's
top level, never opens a cluster, and carries through whatever it does not
understand.

## Three operations

| | Cost | What it does |
|---|---|---|
| `read` | a few seeks | Finds the title and tags through the file's index. Lenient: reads stream captures and damaged files as far as it can. |
| `open`, edit, `plan`, `apply` | kilobytes | Edits in place where there is room; otherwise puts the tags at the end and re-points the index. |
| `reseat` | one copy of the file | Writes a copy with the metadata at the front and padding after it, so that later edits fit in place. |

```rust
// Look.
let m = fastmkv::read("film.mkv")?;
println!("{:?} by {:?}", m.title, m.get("ARTIST"));

// Edit. Nothing is written until `apply`.
let mut mkv = fastmkv::open("film.mkv")?;
mkv.set_title(Some("A title"))?;
mkv.set("ARTIST", "Someone")?;
mkv.remove("COMMENT")?;
let plan = mkv.plan()?;          // refused here: the file is untouched
plan.apply_to("copy-of-film.mkv")?;

// Make room, once.
let mkv = fastmkv::open("film.mkv")?;
if !mkv.seating().front {
    mkv.reseat("film.seated.mkv", fastmkv::Padding::default())?;
}
```

## What it promises

- **The media is not touched.** A write may change `Tags`, `Info` (for its
  title), the first `SeekHead`, padding, and the segment's size field.
  A rotation edit may also change `Tracks` and upgrade the EBML header's
  `DocTypeVersion` to 4; encoded frames remain unchanged.
  The test suite holds every write to that list, working the permitted
  regions out from the original file and not from the writer's own plan.
- **Refusal comes before writing.** `open` checks the whole structure and
  `plan` decides every byte; both refuse without touching the file. Only
  `apply` can fail part-way, so apply to a copy and swap it in.
- **What is not selected is not changed.** `set` and `remove` act on global,
  string-valued tags in the undetermined language. Tags aimed at a track or
  chapter, other languages, nested tags, binary values and unknown elements
  are kept. An edit that would have to delete one of them is refused.
- **Checksums are recomputed where there was one**, never added where there
  was none, and a checksum that was already wrong is a refusal.
- **`Info` stays at the front.** It holds the duration and timestamp scale.
  A title that does not fit is refused with `NeedsReseat`.

## Lossless display rotation

`set_rotation` sets an absolute angle on a video track, selected by its
Matroska track number (`Track::number`, not a zero-based index). Positive
angles are counterclockwise: `-90.0` turns clockwise, `90.0` turns
counterclockwise, and `180.0` turns upside down. Finite values from -180
through 180 are accepted. `None` removes the roll field; `Some(0.0)` writes
an explicit zero.

```rust
let metadata = fastmkv::read("film.mkv")?;
let number = metadata.video().expect("a video track").number;
let mut mkv = fastmkv::open("film.mkv")?;
mkv.set_rotation(number, Some(-90.0))?;
match mkv.plan() {
    Ok(plan) => {
        // apply_to expects an existing, byte-identical copy of film.mkv.
        plan.apply_to("copy-of-film.mkv")?;
    }
    Err(e) if e.kind() == Some(fastmkv::Kind::NeedsReseat) => {
        // This destination must not exist. Pending edits are included.
        mkv.reseat("film.rotated.mkv", fastmkv::Padding::default())?;
    }
    Err(e) => return Err(e),
}
```

This writes Matroska's `ProjectionPoseRoll` field. Players must honor that
field to show the rotation; the encoded picture is unchanged. Other tracks,
codec configuration, and unrelated metadata are preserved. Non-rectangular
projections and ambiguous track headers are refused. If the track or version
header cannot grow in place, `plan` returns `NeedsReseat` before writing.

## What it refuses to edit

`fastmkv::check(path)` gives the reason. None of these stops `read`.

| | Usual cause |
|---|---|
| Segment or cluster with no size recorded | stream capture, interrupted recording |
| Data after the segment, or a second segment | concatenated files |
| An index entry that points at the wrong place | a careless editor |
| A truncated file | interrupted copy |
| A checksum over the whole segment | rare |

`ffmpeg -i in.mkv -c copy out.mkv` turns the first four into a file that
passes.

## Tested against

Files written by ffmpeg, mkvmerge and mkvpropedit. Results are read back by
ffprobe, ffmpeg (a full decode, and frame hashes after seeking) and mkvmerge,
none of which share code with this crate.

```bash
cargo test
```

The suite builds its own fixtures. Tests that need `ffmpeg` or MKVToolNix
skip when they are not installed.

```bash
cargo run --example dump -- FILE          # the layout and the tags
cargo run --example reseat -- SRC DEST    # a re-seated copy
```

## Design

[docs/PROPOSAL-2.md](docs/PROPOSAL-2.md) is the design, with the
measurements it rests on and the places where practice differs from
RFC 9559.

## Not done

- Editing through the tag tree: targeted, language-specific and nested tags
  can be read but not yet changed.
- A new `Tags` element, in a file that had none, goes to the end of the
  file even when there is padding at the front.
- Position fields inside clusters are corrected by `reseat`, but no tool
  tested here writes them, so that path runs only on hand-built fixtures.

## License

MIT. See [LICENSE](LICENSE).
