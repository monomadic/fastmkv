//! `cargo run --example dump -- FILE`: the top-level layout and the tags.

use fastmkv::{ebml, TagValue};

fn name(id: u32) -> &'static str {
    match id {
        ebml::SEEK_HEAD => "SeekHead",
        ebml::INFO => "Info",
        ebml::TRACKS => "Tracks",
        ebml::CUES => "Cues",
        ebml::ATTACHMENTS => "Attachments",
        ebml::CHAPTERS => "Chapters",
        ebml::TAGS => "Tags",
        ebml::VOID => "Void",
        ebml::CRC32 => "CRC-32",
        _ => "?",
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: dump FILE");
    match fastmkv::open(&path) {
        Err(e) => println!("not editable: {e}"),
        Ok(mkv) => {
            let s = &mkv.survey;
            println!(
                "{} v{}, segment at {} (size field {} bytes), {} clusters",
                s.doc.doc_type,
                s.doc.doc_type_read_version,
                s.segment.offset,
                s.segment.size_len,
                s.clusters
            );
            for e in &s.elements {
                println!(
                    "  {:>12}  {:<12} {:>10} bytes  size field {}",
                    e.offset,
                    name(e.id),
                    e.size,
                    e.size_len
                );
            }
            if let Some(sh) = &s.seek_head {
                println!(
                    "SeekHead: crc {:?}, {} bytes of Void inside",
                    sh.crc, sh.void_inside
                );
                for en in &sh.entries {
                    println!(
                        "  {:<12} -> {}",
                        name(en.id),
                        s.segment.data_offset() + en.position
                    );
                }
            }
        }
    }
    match fastmkv::read(&path) {
        Err(e) => println!("not readable: {e}"),
        Ok(m) => {
            println!("Title: {:?}", m.title);
            for t in &m.tags {
                println!(
                    "Tags at {:?}: crc {:?}",
                    t.element.map(|e| e.offset),
                    t.tags.crc
                );
                for tag in t.tags.tags() {
                    let tg = tag.targets();
                    println!(
                        "  Tag level {} tracks {:?} global {}",
                        tg.map_or(50, |t| t.type_value),
                        tg.map(|t| t.track_uids.clone()).unwrap_or_default(),
                        tag.is_global()
                    );
                    for s in tag.simple_tags() {
                        match s.value() {
                            TagValue::String(v) => {
                                println!("    {} [{}] = {v}", s.name(), s.language())
                            }
                            TagValue::Binary(b) => {
                                println!("    {} = <{} bytes>", s.name(), b.len())
                            }
                            TagValue::None => println!("    {} (no value)", s.name()),
                        }
                    }
                }
            }
        }
    }
}
