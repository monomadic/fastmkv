//! Hand-built files. ffmpeg will not produce most of the shapes the
//! preflight has to refuse, so they are assembled here byte by byte.
#![allow(dead_code)]

use fastmkv::crc::crc32;
use fastmkv::ebml::{self, element_min, encode_id, encode_uint};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn el(id: u32, data: &[u8]) -> Vec<u8> {
    element_min(id, data)
}

pub fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

/// A master with a checksum as its first child.
pub fn el_crc(id: u32, data: &[u8]) -> Vec<u8> {
    let crc = el(ebml::CRC32, &crc32(data).to_le_bytes());
    el(id, &cat(&[crc, data.to_vec()]))
}

pub fn unknown_size(id: u32, data: &[u8]) -> Vec<u8> {
    cat(&[encode_id(id), vec![0xFF], data.to_vec()])
}

pub fn doc(doc_type: &str, read_version: u64) -> Vec<u8> {
    el(
        ebml::EBML,
        &cat(&[
            el(ebml::DOC_TYPE, doc_type.as_bytes()),
            el(ebml::DOC_TYPE_READ_VERSION, &encode_uint(read_version)),
        ]),
    )
}

pub fn info() -> Vec<u8> {
    el(ebml::INFO, &el(0x2A_D7B1, &encode_uint(1_000_000)))
}

pub fn cluster() -> Vec<u8> {
    el(ebml::CLUSTER, &el(0xE7, &[0]))
}

pub fn simple(name: &str, value: &str) -> Vec<u8> {
    el(
        ebml::SIMPLE_TAG,
        &cat(&[
            el(ebml::TAG_NAME, name.as_bytes()),
            el(ebml::TAG_STRING, value.as_bytes()),
        ]),
    )
}

pub fn simple_lang(name: &str, lang: &str, value: &str) -> Vec<u8> {
    el(
        ebml::SIMPLE_TAG,
        &cat(&[
            el(ebml::TAG_NAME, name.as_bytes()),
            el(ebml::TAG_LANGUAGE, lang.as_bytes()),
            el(ebml::TAG_STRING, value.as_bytes()),
        ]),
    )
}

pub fn tag(targets: &[u8], simples: &[Vec<u8>]) -> Vec<u8> {
    el(
        ebml::TAG,
        &cat(&[el(ebml::TARGETS, targets), simples.concat()]),
    )
}

/// A SeekHead whose entries all use 8-byte positions, so its length does
/// not depend on what it points at and offsets can be worked out up front.
pub fn seek_head(entries: &[(u32, u64)]) -> Vec<u8> {
    let body: Vec<u8> = entries
        .iter()
        .flat_map(|&(id, pos)| {
            el(
                ebml::SEEK,
                &cat(&[
                    el(ebml::SEEK_ID, &id.to_be_bytes()),
                    el(ebml::SEEK_POSITION, &pos.to_be_bytes()),
                ]),
            )
        })
        .collect();
    el(ebml::SEEK_HEAD, &body)
}

pub fn seek_head_len(n: usize) -> u64 {
    seek_head(&vec![(ebml::TAGS, 0); n]).len() as u64
}

/// A SeekHead first, indexing every part that is not a cluster or a Void.
pub fn indexed(parts: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let listed = |p: &Vec<u8>| p[0] != 0xEC && p[..4] != ebml::CLUSTER.to_be_bytes();
    let n = parts.iter().filter(|p| listed(p)).count();
    let mut at = seek_head_len(n);
    let mut entries = Vec::new();
    for p in parts {
        if listed(p) {
            entries.push((u32::from_be_bytes(p[..4].try_into().unwrap()), at));
        }
        at += p.len() as u64;
    }
    let mut out = vec![seek_head(&entries)];
    out.extend_from_slice(parts);
    out
}

/// The segment's size field is 8 bytes wide, as muxers write it: they do
/// not know the size until the end, so they reserve the widest.
pub fn file(parts: &[Vec<u8>]) -> Vec<u8> {
    let segment = ebml::element(ebml::SEGMENT, 8, &parts.concat()).unwrap();
    cat(&[doc("matroska", 2), segment])
}

/// The same with the narrowest size field that holds it: no room to grow.
pub fn file_narrow(parts: &[Vec<u8>]) -> Vec<u8> {
    cat(&[doc("matroska", 2), el(ebml::SEGMENT, &parts.concat())])
}

pub struct Temp(pub PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Tests run in parallel and reuse names, so every file gets its own.
pub fn write(name: &str, bytes: &[u8]) -> Temp {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("fastmkv-{}-{n}-{name}.mkv", std::process::id()));
    std::fs::write(&p, bytes).unwrap();
    Temp(p)
}

// ---------------------------------------------------------------------------
// judging media with tools that share no code with this crate
// ---------------------------------------------------------------------------

/// A path for a file that does not exist yet.
pub fn temp(name: &str) -> Temp {
    let t = write(name, b"");
    std::fs::remove_file(&t.0).unwrap();
    t
}

pub fn run(program: &str, args: &[&str], path: &Path) -> String {
    let out = Command::new(program).args(args).arg(path).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Every packet's stream, timestamps, size and flags, but not its
/// position: that is the one thing meant to change.
pub fn packets(path: &Path) -> String {
    let fields = "packet=stream_index,pts,dts,duration,size,flags:stream=index,codec_name,width,height,sample_rate:format=duration";
    run(
        "ffprobe",
        &[
            "-v",
            "error",
            "-show_entries",
            fields,
            "-of",
            "compact",
            "--",
        ],
        path,
    )
}

/// A hash of every decoded frame from `from` seconds on. Seeking is what
/// the Cues are for, so this is what a wrong cue would break.
pub fn frames(path: &Path, from: &str) -> String {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-ss", from, "-i"])
        .arg(path)
        .args(["-map", "0:v", "-f", "framemd5", "-"])
        .output()
        .unwrap();
    assert!(out.status.success() && out.stderr.is_empty());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The bytes of every cluster, in order, by this test's own walk of the
/// top level.
pub fn clusters(path: &Path) -> Vec<Vec<u8>> {
    let bytes = std::fs::read(path).unwrap();
    let mkv = fastmkv::open(path).unwrap();
    let s = &mkv.survey;
    let mut ends: Vec<u64> = s.elements.iter().map(|e| e.offset).collect();
    ends.extend(&s.cluster_offsets);
    ends.push(bytes.len() as u64);
    ends.sort();
    s.cluster_offsets
        .iter()
        .map(|&at| {
            let end = ends[ends.partition_point(|&e| e <= at)];
            bytes[at as usize..end as usize].to_vec()
        })
        .collect()
}
