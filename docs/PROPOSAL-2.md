# fastmkv: native Matroska tag writer — proposal 2

Supersedes PROPOSAL-1. Changes from it are marked **(new)** or **(changed)**.
Spec references: EBML is RFC 8794, Matroska is RFC 9559.

## 0. Status

Steps 1–4 of §11 are built: EBML layer, preflight, tag tree, patch planner,
apply, and the convenience API, plus `Info\Title` and re-seating (§12). Not built: the tree API
for edits (§7.1 is read-only so far), field mapping, tagform integration.

Entry points: `read` (lenient, follows the `SeekHead`, never walks
clusters), `open` (strict, this document's preflight), `check` (the refusal
reason without opening for edit).

Where the code differs from the text below:

- **Title** is edited in `Info` (`set_title`), separately from a `TITLE`
  tag. What a caller should write is in §14, and it is not "both".
- **`read` reports whether it saw everything** (`Metadata::complete`).
- **Convenience edits refuse to delete a node that holds nested tags or
  unknown children** (§7.2).
- **§5.1 has a fourth placement**, tried before appending: an element with
  padding directly *before* it moves back into that padding, ending where
  it ended. In an ffmpeg file that is `Info` and the padding after the
  `SeekHead`. 42 bytes are always left for the `SeekHead` to grow.
- **§5.2's "size field already 8 wide" case cannot occur.** A rewritten
  element's size width is chosen afresh, never inherited.
- **`Info` is never moved to the end.** It holds the duration and the
  timestamp scale. A title that outgrows the front is refused with
  `NeedsReseat` (§12).

## 1. Promise (changed)

Preserve all media and metadata; modify only what is necessary.

- **Does:** read and edit Matroska tags.
- **May modify:** `Tags` elements, `Info` (for its `Title` only), the first
  `SeekHead`, `Void` padding, the `Segment` size field, and any `CRC-32`
  whose parent was modified. Nothing else. The test suite holds every
  write to this list (§8).
- **Doesn't:** demux, remux, touch codecs, edit chapters or attachments.
  Clusters are never parsed.
- **Location:** standalone crate at `~/src/fastmkv`, path dependency of
  tagform. No dependencies beyond std (CRC-32 is a table and a loop).

## 2. Supported-file boundary (new)

A file is accepted only if all of these hold. Anything else is a preflight
refusal (§6) and the file is not touched.

| Check | Accept |
|---|---|
| EBML header | parses; `EBMLMaxIDLength` ≤ 4, `EBMLMaxSizeLength` ≤ 8 |
| `DocType` | `matroska` or `webm` |
| `DocTypeReadVersion` | a version this crate knows (list fixed in code) |
| Segments | exactly one, with a known size |
| Every top-level element | known size, including every `Cluster` |
| Top-level chain | ends exactly at the segment end; segment ends at EOF |
| Segment-level `CRC-32` | absent |
| `SeekHead` | the conditions in §5.3 |

The magic-number check in tagform stays as the *rejection* test. *Selecting*
the Matroska writer requires this full preflight to pass.

## 3. EBML layer

- Vint read/write for IDs and sizes, with explicit width control on write:
  sizes may be encoded wider than minimal, and an all-ones payload is the
  reserved "unknown" marker, so e.g. 127 cannot be written in one byte.
- Element header walker: ID, size, size-field width, payload offset.
- Top-level scan skips each element by its size. It never descends into
  `Cluster`.

## 4. File model (changed)

- Top-level index: `SeekHead`, `Info`, `Tracks`, `Tags`, `Attachments`,
  `Cues`, `Void`, each with offset, header width and size.
- **All** `Tags` elements are modelled, not just the first.
- `Tags → Tag → { Targets, SimpleTag* }`, with nested `SimpleTag`s.
- `Targets` keeps `TargetTypeValue`, `TargetType` and every UID list
  (track, edition, chapter, attachment).
- `SimpleTag` keeps name, language (both forms), default flag, and the value
  as either string or binary.
- Unknown children are kept as raw bytes in their original position.
- **`CRC-32` is never treated as an unknown child.** It is dropped on parse
  and recorded as a flag on its parent; on write it is recomputed for every
  master element that had one and whose bytes changed (§5.4).

## 5. Write strategy (changed)

### 5.1 Placement

Tried in order; the first that is *encodable* wins.

1. **In place.** The new element starts where the old one started and
   occupies its region plus any `Void` directly *following* it. Its
   position is unchanged, so the `SeekHead` is not touched.
2. **Grow at end.** The old `Tags` is already the last element: extend it and
   the segment size.
3. **Append.** Write the new `Tags` at the segment end, turn the old one into
   `Void`, update the `SeekHead` entry and the segment size.

With several `Tags` elements, only those containing a changed `Tag` are
rewritten. They are not merged.

### 5.2 "Fits" means encodable

For a region of `R` bytes and new content of `N` bytes, leftover `L = R − N`:

| L | Action |
|---|---|
| 0 | exact fit |
| 1 | widen the new `Tags` size field by one byte; if already 8 wide, this placement fails |
| ≥ 2 | `Void` of exactly `L` bytes, choosing the size-field width that makes header + payload = `L` |

The same rule applies wherever padding is produced (voiding the old `Tags`,
`SeekHead` slack).

### 5.3 SeekHead

Checked against RFC 9559 §5.1.1, §6.1, §6.3, §16 and §25.2. Quoted rules are
the spec's; "v1" lines are this crate's choices.

**What the spec requires**

| Rule | Source |
|---|---|
| At most two `SeekHead` elements | §5.1.1 (`maxOccurs: 2`) |
| If used, the first MUST be the first non-`CRC-32` child of the `Segment` | §6.3 |
| If there is a second, the first MUST reference it | §6.3 |
| The second MUST only reference `Cluster` elements | §6.3 |
| The second MAY sit anywhere among the top-level elements | §6.3 |
| Together they MUST reference all top-level elements except the first `SeekHead` | §6.3 |
| When top-level elements are added, the `SeekHead`(s) MUST be updated | §6.1 |
| A `SeekHead` is RECOMMENDED, not required | §4.5 |
| A `Void` after the first `SeekHead` is RECOMMENDED, for expansion | §25.2 |
| `SeekID` is binary, always 4 bytes; `SeekPosition` is a variable-width uinteger | §5.1.1.1 |
| `SeekPosition` counts from the first byte of `Segment` data to the first byte of the target's ID | §16 |

**Consequences**

- A new `SeekHead` cannot go into arbitrary `Void` space: the first one has a
  fixed position, and a second one may not reference `Tags` at all. So a
  `Tags` entry can only ever live in the first `SeekHead`.
- An in-place edit does not move the `Tags` element's first byte, so its
  `SeekPosition` is unchanged and the `SeekHead` is not touched.
- Every other referenced element keeps its position in all three placements,
  so only `Tags` entries ever change.

**Preflight additions (§2)**

| Found | Result |
|---|---|
| More than two `SeekHead`s | refuse |
| A `SeekHead` exists but the first is not the first non-`CRC-32` child | refuse |
| Second `SeekHead` references anything but `Cluster` | **accepted** (changed; see below) |
| Second `SeekHead` not referenced by the first | refuse |
| An entry whose `SeekPosition` does not land on an element with that `SeekID` | refuse |
| `Tags` present but missing from the first `SeekHead` | accepted; treated as "needs an entry" if the tags move |

**Where practice differs from the spec.** RFC 9559 has a second
`SeekHead` list clusters only. mkvpropedit, the reference editor, does
otherwise: when the first has no room it writes a complete new index at
the end of the file and cuts the first down to one entry pointing at it.
Refusing that shape, as this document first specified, made every file
mkvpropedit had run out of room on uneditable. So a second `SeekHead` is
held to the same rule as the first, that its entries land, and:

| Operation | With two `SeekHead`s |
|---|---|
| `read` | follows both |
| An edit that moves nothing | made; neither index is touched |
| An edit that has to move an element | refused with `NeedsReseat` |
| `reseat` | writes one index at the front; the second is not carried over |

mkvmerge's `--clusters-in-meta-seek`, the shape the RFC describes, is
handled the same way. The cluster index it holds is dropped by a re-seat;
the `Cues` still find the clusters.

**v1 behaviour**

- Only the first `SeekHead` is edited. None is ever created or moved.
- One entry per `Tags` element, matched by `SeekID` and `SeekPosition`.
- An entry costs 13 bytes plus the `SeekPosition` width (1–8). Moving tags
  toward the end of a large file can widen it; adding an entry costs the
  whole amount.
- Growth is taken, in order, from: `Void` inside the `SeekHead`, then `Void`
  directly following it. Leftover follows the §5.2 rule. Not enough: refuse.
- The `SeekHead` size field may change width only if the bytes come from
  that same padding, so that nothing after the padding moves.
- A removed `Tags` element (all tags deleted) has its entry removed and the
  space returned as `Void`.

**Two deliberate choices, not spec requirements**

- *No `SeekHead`, tags must move: refuse.* The spec allows a file without
  one, so appending would be valid, but a reader would have to scan past
  every cluster to find the tags and many will not.
- *`Void` elements are not indexed.* Read literally, §6.3 covers every
  top-level element. Existing `SeekHead`s are left as found and no entries
  are added for padding this crate creates. To be checked in step 1 against
  what ffmpeg writes.

### 5.4 Checksums

- Recompute `CRC-32` for each modified master that carried one: `Tags`,
  `Tag`, `SimpleTag`, `SeekHead`, at whatever level it appeared.
- A modified element that had no checksum does not gain one.
- Segment-level checksum: refused in preflight (§2), since it covers the
  whole file. The spec says a `Segment` SHOULD NOT have one (RFC 9559 §6.2).
- An existing checksum on an element about to be modified is verified
  first. A mismatch is a refusal: recomputing would certify damaged data.
- Algorithm per RFC 8794 §11.3.1: IEEE CRC-32, initial value 0xFFFFFFFF,
  stored little-endian, first child of its parent, covering all the
  parent's data except itself.

### 5.5 Segment size

Grows on append. If the new size does not fit the existing size-field
width: refuse. The field is never widened, because that would shift every
byte after it.

## 6. Preflight vs write failure (new)

Two phases with different guarantees.

**Plan (no mutation).** Parse, apply edits in memory, choose placement,
compute `SeekHead` capacity, checksums and segment growth. Output is a patch
list: `(offset, bytes)` pairs plus a new file length. Any failure here is a
`Refused` error and the file is byte-identical.

**Apply.** Write the patch list. An I/O failure here returns `WriteFailed`
and **the path may be partially modified.** Patches are ordered so that an
interrupted write is least harmful:

1. appended `Tags` (past the segment end, invisible to readers)
2. segment size
3. `SeekHead`
4. voiding the old `Tags`

tagform always applies to a temp copy, verifies, then swaps, so its original
is never at risk. Standalone callers must do the same or accept the risk;
the crate docs say so on `write()`.

## 7. API (changed)

Two levels.

### 7.1 Tree API — exact

Full access to the `Tag` tree: iterate, inspect targets, edit or delete a
specific `SimpleTag`. No selection rules; the caller chooses.

### 7.2 Convenience API — defined selection

```rust
let mut f = fastmkv::open(path)?;       // runs the §2 preflight
f.get("TITLE");                          // Option<&str>
f.set("TITLE", "…")?;
f.remove("COMMENT")?;
let plan = f.plan()?;                    // Refused here, file untouched
plan.apply()?;                           // WriteFailed here, see §6
```

These operate on the **global scope** only:

| Term | Definition |
|---|---|
| Global `Tag` | `Targets` absent or empty of non-zero UIDs, and `TargetTypeValue` absent or 50 |
| Match | a top-level `SimpleTag` in a global `Tag`, exact name match, language undetermined (absent, `und`), string-valued |

| Call | Effect |
|---|---|
| `get` | first match in file order |
| `set` | first match gets the new string; every other match is removed; if none, a `SimpleTag` is added to the first global `Tag`, creating `Tags`/`Tag` if needed |
| `remove` | every match is removed |

Never changed by these calls: tags with UID targets, other target levels,
other languages, nested `SimpleTag`s, binary-valued tags, unknown children.

Two refusals keep that promise where an edit would break it:

| Situation | Result |
|---|---|
| The name has a binary-valued tag in global scope | refused: no guessing |
| A match that would be *removed* holds nested tags or unknown children | refused |
| Removal would leave a `Tag` or `Tags` holding only unknown children | refused |

The second covers both `remove` and the duplicates `set` collapses. A match
whose value is *changed* keeps its children, so that is allowed. A refused
edit leaves the in-memory tree as it was. Deleting such a node is a
decision for the tree API, where the caller names the node.

## 8. Tests (changed)

**Unit**
- Vint round-trips at every width, including the reserved all-ones values.
- CRC-32 against known vectors.
- Hand-built EBML buffers for the parser.

**Placement boundaries**
- Exact fit, 1-byte leftover, 2-byte leftover.
- Size-width transitions (content crossing 126/127, 16382/16383 bytes).
- 1-byte leftover with an 8-wide size field → falls through to append.
- `SeekPosition` needing one more byte, with and without padding available.

**Preservation** — three separate assertions, on every write in the suite
- *Against the plan:* every byte outside the patch list is identical.
- *Independent of the plan:* the editable regions are derived from the
  file as it was (`SeekHead`, `Info`, `Tags`, `Void`, the segment size
  field, space past the old end). Every byte outside them is compared, and
  every patch must lie inside them. A planner that patched a cluster fails
  this even though its own patch list would vouch for it.
- *Structural:* every `Tag`/`SimpleTag` not selected by the edit is
  identical after re-parse (targets, language, value, children, order).

**Fixtures**
- ffmpeg-generated: no tags, tags with padding, tags without padding,
  attachment present.
- Hand-built (ffmpeg won't produce these): checksums at each level, several
  `Tags` elements, language and target variants, duplicate names, binary
  values.

**Refusals** — each asserts the file is byte-identical afterwards
- Unknown-size segment, unknown-size cluster, segment checksum, wrong
  `DocType`, two segments, malformed lengths, element overrunning its
  parent, no `SeekHead` room, segment size field too narrow.
- Misplaced first `SeekHead`, three `SeekHead`s, second `SeekHead`
  referencing `Tags`, entry pointing at the wrong element, bad existing
  checksum on `Tags`.

**Cross-check**
- ffprobe reads back the same global tags, stream count and duration.
- `mkvinfo`/`mkvalidator` if installed; skipped otherwise.

## 9. tagform projection (new, moved ahead of integration)

tagform's reader collapses container tags into a lowercase key/value map.
Matroska is richer, so the projection is explicit.

**Discovery**

- Form state comes from `open`, never from `read`. `read` can miss tags
  that sit after the clusters and are not in the `SeekHead`; a field that
  looked empty would then be filled and the real value overwritten.
  Sharing selection rules is not enough when discovery differs.
- A file that fails the preflight is shown read-only, from `read`, with
  `complete` surfaced. It cannot be written, so nothing can be overwritten.
- `read` is for switchblade and abner, which only display.

**Names**

- Each projected entry keeps its original name beside the display key:
  `{ key: "my_key", name: "MY_KEY", value }`. Writes use `name`, because
  the convenience API matches exactly. Writing the display key back would
  create a second tag instead of updating the first.
- A new key, typed by the user, is written in upper case, which is the
  Matroska convention.
- **Last match wins** when building the map, as in ffprobe, which
  tagform already reads MP4 through. `get` in the crate returns the first;
  tagform's projection does not use it.
- **Collisions:** names that differ only in case (`Artist`, `ARTIST`)
  share a display key. The last in file order is shown and edited, which
  is what ffprobe does (measured: `Artist`, `ARTIST`, `artist` in that
  order reads back as the third). On
  write the others are removed, as duplicates are, so that the value shown
  is the value every reader gets. This is reported in the write plan.

**Scope**

- Only §7.2 matches are projected.
- Everything else (targeted, other-language, nested, binary) is invisible
  to the form and preserved in the file. It is not placed in
  `Report::custom`, because that map cannot express it.
- Global string tags that no field claims go to `Report::custom` as usual.
- `FIELDS` gains an `mkv` column: the names written, and the wider set read.
- Title follows §14.

Still to decide: the actual name mapping per field.

## 10. tagform integration

- `Writer::Matroska` in `plan.rs`, selected when the fastmkv preflight passes.
- A file that fails preflight gets the refusal reason as its write error.
- Temp copy (APFS clone where available) → apply → verify → swap.
- Replace the blanket Matroska refusal in `write.rs`.
- Update DESIGN §1 non-goals and §9.

## 11. Order of work (changed)

1. EBML layer, preflight, read-only tag tree
2. Patch planner: placement, padding, `SeekHead`, checksums — no I/O
3. Apply, plus preservation and refusal tests
4. Convenience API
5. Field mapping and projection (§9)
6. tagform integration (§10)

## 12. Re-seating (built)

A muxer leaves no padding after the tags, so the first edit that grows
them sends them to the end of the file. A reader on a slow link then needs
a second request to find them: the faststart problem in a milder form.
Re-seating is the cure, paid once.

**It is a copy, not a remux.** Every cluster is carried byte for byte.
What changes is where the clusters sit, so what records their position is
corrected.

| Holds a position | Treatment |
|---|---|
| `Cues` → `CueClusterPosition` | looked up in the old-to-new map; checksum redone |
| `SeekHead` | rebuilt from the new layout |
| `Cluster` → `Position` (optional; ffmpeg writes none) | corrected in its existing width; the cluster's checksum redone if it has one |
| `CueCodecState`, `CueReference` | refused |
| A cue that points at no cluster | refused |
| A second `SeekHead` | not carried over; the copy has one index |

**Layout written**

```
SeekHead · Void · Info · Void · Tracks, Chapters, Attachments, (Cues) · Tags · Void · clusters and the rest
```

Default padding: 128 bytes after the `SeekHead`, 512 after `Info`, 4096
after the tags. `Void` elements of the original are dropped.

**Rules**

| | |
|---|---|
| Source | only read |
| Destination | must not exist; removed if anything fails |
| Accepted only if | the result passes the same preflight as any editable file |
| Pending edits | made in the copy |
| `Info` | never placed after the clusters, by any write. An in-place edit whose title does not fit is refused with `NeedsReseat` |

**The three operations**

| Operation | Cost | When |
|---|---|---|
| Update (`plan` + `apply`) | kilobytes | every edit; tags may go to the end if there is no room |
| Re-seat (`reseat`) | one full copy | first write to an unpadded file; archiving; a title that does not fit |
| Check (`seating`) | none | tells the caller which of the two it needs |

**In tagform:** automatic, behind a toggle that replaces the faststart
switch for `.mkv`. With it on, a file whose tags are not at the front or
have no padding is re-seated on its first write. With it off, writes are
updates, and only a title that does not fit forces the question.

**MP4 to MKV.** When tagform writes a new `.mkv` from an MP4, ffmpeg does
the conversion (`-c copy -f matroska`) and the result is re-seated before
it is handed over, so a file tagform creates is padded from the start.
ffmpeg has no option to leave this padding itself.

**Verified by**, on ffmpeg-made files and on a real 39 MB download:
identical packet lists (stream, timestamps, size, flags), identical
cluster bytes, identical decoded frames after seeking to several points,
a clean full decode, and mpv opening it at a seek point.

## 13. Defects a Matroska file can have

`check` reports the first four today, as refusal reasons.

| Defect | Cause | Effect | Seen by `check` |
|---|---|---|---|
| Unknown-size segment or clusters | Stream capture; writer could not seek back | Often no duration, no index | yes |
| Trailing data or a second segment | Concatenated or appended files | Players stop at the first segment | yes |
| Stale or wrong `SeekHead` entries | A careless editor | Elements not found | yes |
| Truncated file | Interrupted copy or download | Plays until the cut | yes |
| No `Cues` | Capture, or interrupted recording | Seeking is slow or refused | not yet; cheap to add |
| No duration in `Info` | Same | Players show no length | not yet; cheap to add |
| No `SeekHead` | Minimal muxers | Slow open; tags after the clusters go unseen | not yet; cheap to add |
| `Cues` at the end | Normal for ffmpeg | Extra round trip when streaming | not a defect locally |

Unlike MP4, none of these makes the file unplayable: an interrupted
recording plays up to where it stopped. All of the real ones are repaired
by a remux (`ffmpeg -c copy`), which rebuilds sizes, index and duration.

## 14. Title: measured, and what to write

Proposal 2 first concluded "write both" from one observation (ffmpeg
writes `Info\Title`). That was not enough to conclude it. Measured since,
on a file carrying each combination:

| File has | ffprobe | mpv | mediainfo | exiftool |
|---|---|---|---|---|
| `Info\Title` only | `title=INFO` | INFO | INFO | not measured |
| `TITLE` tag only | `TITLE=TAG` | TAG | TAG | not measured |
| Both, same value | `TITLE=SAME` | **SAME / SAME** | SAME | SAME |
| Both, different | `TITLE=TAG` | **INFO / TAG** | **INFO / TAG** | whichever comes later in the file |

Versions: ffprobe/Lavf 63.1, and the mpv, mediainfo and exiftool installed
here. VLC, browsers and QuickTime were not measured.

**Findings**

- Every reader measured shows a title from either place alone.
- Writing both is harmful: mpv shows the title twice even when the two
  agree. When they disagree, no two readers need agree on the answer.
- ffprobe lets the tag win; exiftool depends on file order.

**Policy for tagform**

| | Rule |
|---|---|
| Write | `Info\Title` only, and remove any global `TITLE` tag in the same write |
| Read | `Info\Title`; if absent, the global `TITLE` tag |
| Both present and different | show `Info\Title`, and mark the field as conflicting; saving resolves it |
| Clear | remove both |

A file always leaves tagform with exactly one title. fastmkv keeps the two
as separate calls (`set_title`, `set("TITLE", …)`); the policy is the
caller's.

**What supporting `Info` changed in the crate**

| Area | Change |
|---|---|
| Modification whitelist | `Info` added (§1), for `Title` only; its other children are carried as bytes |
| Seek entries | an `Info` that moves is re-indexed like `Tags`; an unlisted one gains an entry |
| Placement | can move back into the padding before it, or to the end (§0) |
| Checksums | `Info`'s is verified before and recomputed after |
| Deletion | clearing the title removes the `Title` child; `Info` itself is never removed |
| Duplicates | a second `Title` child is dropped on write, so no stale one is left |

## Measured in step 1

From files written by ffmpeg (Lavf 63.1), including one yt-dlp download.

| Question | Finding |
|---|---|
| Where are `Tags`? | Before the first cluster, after `Tracks`/`Attachments` |
| Padding after `Tags`? | None. Any growth takes the append path |
| Padding after the `SeekHead`? | Yes, a `Void` of 74–92 bytes: room for several entries |
| Are `Void` elements indexed? | No, and neither are clusters |
| Checksums? | On every top-level master. This crate's CRC agrees with all of them |
| Segment size field | 8 bytes wide, so growth never overflows it |
| Per-track tags | `DURATION`, `HANDLER_NAME`, `ENCODER`, in `Tag`s targeted by track UID |
| `DocTypeReadVersion` | 2 |
| Title | ffmpeg writes it to `Info\Title`. What readers do with it is in §14 |

## Measured against MKVToolNix 102

| Question | Finding |
|---|---|
| mkvmerge: checksums | none |
| mkvmerge: padding | 4 KB after the `SeekHead`, 1 KB after `Tracks` |
| mkvmerge: where are `Tags` and `Cues`? | after the clusters, both |
| mkvmerge: position inside a cluster | not written; that path is still only tested on hand-built fixtures |
| mkvmerge: tags | per-track statistics (`BPS`, `NUMBER_OF_FRAMES`, ...), targeted by track UID, so not global |
| mkvpropedit: a title that does not fit | moves `Info` to the end of the file |
| mkvpropedit: no room in the `SeekHead` | a second, complete index at the end (§5.3) |
| mkvmerge reading this crate's output | identifies it with no errors or warnings, and remuxes it with exit status 0, after an update, an append and a re-seat |
| mkvpropedit editing this crate's output | accepted, including a re-seated file; the result passes the preflight |

mkvmerge's own remux reorders packets and recomputes timestamps even on a
file nothing else has touched, so against it only packet counts and byte
totals per stream are compared. Packet-exact comparison is against ffprobe
on the file itself, before and after.

Two of mkvpropedit's habits are ones this crate declines for itself:
it sends `Info` to the end, and it splits the index. Both are read here;
neither is written.

## Open questions

- Title policy: §14 recommends `Info\Title` only. Awaiting confirmation,
  since it reverses an earlier decision.
- A new `Tags` element, in a file that had none, is appended at the end
  even when there is padding at the front. Re-seating fixes it; the
  planner could place it in the padding directly.
- Per-field tag names (§9).
- Which `DocTypeReadVersion` values to accept.
