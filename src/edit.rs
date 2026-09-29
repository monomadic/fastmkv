//! The convenience edits and exactly what they select (PROPOSAL-2 §7.2).
//!
//! A match is a top-level `SimpleTag` of a global `Tag`, with this exact
//! name, in the undetermined language, holding a string. Tags aimed at a
//! track or a chapter, other languages, nested tags and binary values are
//! never selected, so they are never changed.

use crate::ebml::{self, encode_uint};
use crate::error::{refuse, Kind, Result};
use crate::model::{
    Crc, SimpleChild, SimpleTag, Tag, TagChild, TagValue, Tags, TagsChild, Targets,
};
use crate::TagsAt;

/// Recomputing a checksum that was already wrong would certify whatever
/// damage made it wrong.
fn sound(crc: Option<Crc>, what: &str) -> Result<()> {
    match crc {
        Some(c) if !c.valid => refuse(
            Kind::Checksum,
            format!("the {what} to be changed fails its checksum"),
        ),
        _ => Ok(()),
    }
}

fn candidate(s: &SimpleTag, name: &str) -> bool {
    s.name() == name && s.is_undetermined_language()
}

fn is_match(s: &SimpleTag, name: &str) -> bool {
    candidate(s, name) && matches!(s.value(), TagValue::String(_))
}

fn globals(tags: &[TagsAt]) -> impl Iterator<Item = &SimpleTag> {
    tags.iter()
        .flat_map(|t| t.tags.tags())
        .filter(|t| t.is_global())
        .flat_map(|t| t.simple_tags())
}

fn refuse_binary(tags: &[TagsAt], name: &str) -> Result<()> {
    let binary =
        globals(tags).any(|s| candidate(s, name) && matches!(s.value(), TagValue::Binary(_)));
    if binary {
        return refuse(Kind::BinaryValue, format!("{name} holds a binary value"));
    }
    Ok(())
}

fn new_simple(name: &str, value: &str) -> SimpleTag {
    SimpleTag {
        crc: None,
        children: vec![
            SimpleChild::Name(name.into()),
            SimpleChild::String(value.into()),
        ],
        original: None,
    }
}

fn new_tag(name: &str, value: &str) -> Tag {
    let targets = ebml::element_min(
        ebml::TARGETS,
        &ebml::element_min(ebml::TARGET_TYPE_VALUE, &encode_uint(50)),
    );
    Tag {
        crc: None,
        children: vec![
            TagChild::Targets(Targets {
                type_value: 50,
                type_name: None,
                track_uids: Vec::new(),
                edition_uids: Vec::new(),
                chapter_uids: Vec::new(),
                attachment_uids: Vec::new(),
                original: targets,
            }),
            TagChild::Simple(new_simple(name, value)),
        ],
        original: None,
    }
}

/// Deleting a match deletes what is inside it. Nested tags and unknown
/// children are promised to survive a convenience edit, so an edit that
/// cannot keep the promise is refused rather than quietly breaking it.
fn disposable(s: &SimpleTag, name: &str) -> Result<()> {
    let holds = s
        .children
        .iter()
        .any(|c| matches!(c, SimpleChild::Nested(_) | SimpleChild::Raw(_)));
    if holds {
        return refuse(
            Kind::HasChildren,
            format!("a {name} tag that would be removed holds nested tags"),
        );
    }
    Ok(())
}

/// Apply `f` to every match in file order; `f` returns whether to keep it.
/// A `Tag` left with no `SimpleTag` is dropped: the format requires one.
fn for_matches(
    tags: &mut [TagsAt],
    name: &str,
    mut f: impl FnMut(&mut SimpleTag) -> Result<bool>,
) -> Result<()> {
    for at in tags.iter_mut() {
        let mut emptied = Vec::new();
        let mut touched_any = false;
        for (i, child) in at.tags.children.iter_mut().enumerate() {
            let TagsChild::Tag(tag) = child else { continue };
            if !tag.is_global() || !tag.simple_tags().any(|s| is_match(s, name)) {
                continue;
            }
            let before = tag.children.clone();
            let mut kept = Vec::with_capacity(tag.children.len());
            for c in std::mem::take(&mut tag.children) {
                match c {
                    TagChild::Simple(mut s) if is_match(&s, name) => {
                        if f(&mut s)? {
                            kept.push(TagChild::Simple(s));
                        } else {
                            disposable(&s, name)?;
                        }
                    }
                    other => kept.push(other),
                }
            }
            tag.children = kept;
            if tag.children == before {
                continue;
            }
            sound(tag.crc, "Tag")?;
            tag.original = None;
            touched_any = true;
            if tag.simple_tags().next().is_none() {
                if tag.children.iter().any(|c| matches!(c, TagChild::Raw(_))) {
                    return refuse(
                        Kind::HasChildren,
                        format!("removing {name} would leave a Tag with only unknown children"),
                    );
                }
                emptied.push(i);
            }
        }
        if touched_any {
            sound(at.tags.crc, "Tags element")?;
            for i in emptied.into_iter().rev() {
                at.tags.children.remove(i);
            }
            if at.tags.tags().next().is_none() && !at.tags.children.is_empty() {
                return refuse(
                    Kind::HasChildren,
                    format!(
                        "removing {name} would leave a Tags element with only unknown children"
                    ),
                );
            }
            at.dirty = true;
        }
    }
    Ok(())
}

fn set_string(s: &mut SimpleTag, value: &str) -> Result<()> {
    if s.value() == TagValue::String(value) {
        return Ok(());
    }
    sound(s.crc, "SimpleTag")?;
    for c in s.children.iter_mut() {
        if let SimpleChild::String(v) = c {
            *v = value.into();
            break;
        }
    }
    s.original = None;
    Ok(())
}

pub(crate) fn set(tags: &mut Vec<TagsAt>, name: &str, value: &str) -> Result<()> {
    refuse_binary(tags, name)?;
    // Edits are made on a copy so a refusal half-way leaves the tree as it was.
    let mut work = tags.clone();
    if globals(&work).any(|s| is_match(s, name)) {
        let mut first = true;
        for_matches(&mut work, name, |s| {
            if !std::mem::take(&mut first) {
                return Ok(false);
            }
            set_string(s, value).map(|_| true)
        })?;
    } else {
        add(&mut work, name, value)?;
    }
    *tags = work;
    Ok(())
}

pub(crate) fn remove(tags: &mut Vec<TagsAt>, name: &str) -> Result<()> {
    refuse_binary(tags, name)?;
    let mut work = tags.clone();
    for_matches(&mut work, name, |_| Ok(false))?;
    *tags = work;
    Ok(())
}

fn add(tags: &mut Vec<TagsAt>, name: &str, value: &str) -> Result<()> {
    // The first global Tag takes it; failing that the first Tags element
    // gains a Tag; failing that the file gains a Tags element.
    for at in tags.iter_mut() {
        for child in at.tags.children.iter_mut() {
            let TagsChild::Tag(tag) = child else { continue };
            if !tag.is_global() {
                continue;
            }
            sound(tag.crc, "Tag")?;
            sound(at.tags.crc, "Tags element")?;
            tag.children.push(TagChild::Simple(new_simple(name, value)));
            tag.original = None;
            at.dirty = true;
            return Ok(());
        }
    }
    if let Some(at) = tags.first_mut() {
        sound(at.tags.crc, "Tags element")?;
        at.tags.children.push(TagsChild::Tag(new_tag(name, value)));
        at.dirty = true;
        return Ok(());
    }
    tags.push(TagsAt {
        element: None,
        tags: Tags {
            crc: None,
            children: vec![TagsChild::Tag(new_tag(name, value))],
        },
        dirty: true,
    });
    Ok(())
}
