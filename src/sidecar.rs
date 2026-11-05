//! Tags, rating and colour label for photos and videos, kept in XMP sidecar
//! files next to them so the media itself is never rewritten. The fields are
//! the ones digiKam, darktable, Lightroom and exiftool share, so the tags
//! work without omalens too. Everything else in a sidecar (a darktable edit
//! history, digiKam face regions) is copied through byte for byte: only the
//! five fields below are ever removed or written.

use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{QName, ResolveResult};
use quick_xml::{NsReader, Writer};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const RDF: &[u8] = b"http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const DC: &[u8] = b"http://purl.org/dc/elements/1.1/";
const LR: &[u8] = b"http://ns.adobe.com/lightroom/1.0/";
const DIGIKAM: &[u8] = b"http://www.digikam.org/ns/1.0/";
const XMP: &[u8] = b"http://ns.adobe.com/xap/1.0/";
const XMLNS: &[u8] = b"http://www.w3.org/2000/xmlns/";

/// What a new sidecar starts as; `x:xmptk` marks it as omalens' own.
const TEMPLATE: &str = r#"<?xpacket begin="﻿" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="omalens">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="">
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>
"#;

/// The XMP properties omalens owns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    /// `dc:subject`: flat keywords, the tags' last parts.
    Subject,
    /// `lr:hierarchicalSubject`: `Shot|Action`.
    Hierarchical,
    /// `digiKam:TagsList`: `Shot/Action`.
    TagsList,
    Rating,
    Label,
}

impl Field {
    fn of(namespace: &[u8], local: &[u8]) -> Option<Self> {
        match (namespace, local) {
            (DC, b"subject") => Some(Self::Subject),
            (LR, b"hierarchicalSubject") => Some(Self::Hierarchical),
            (DIGIKAM, b"TagsList") => Some(Self::TagsList),
            (XMP, b"Rating") => Some(Self::Rating),
            (XMP, b"Label") => Some(Self::Label),
            _ => None,
        }
    }
}

/// Tags, rating and colour label of one file.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Meta {
    /// Tags, `/` between levels: `Shot/Action`, `Best`.
    pub tags: BTreeSet<String>,
    /// 1 to 5 stars, 0 for none, -1 for rejected.
    pub rating: i8,
    /// Colour label as other apps write it: `Red`, `Yellow`, `Green`,
    /// `Blue` or `Purple`.
    pub label: Option<String>,
}

impl Meta {
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty() && self.rating == 0 && self.label.is_none()
    }

    /// Whether `tag` or a tag under it (`Shot` for `Shot/Action`) is set.
    pub fn has(&self, tag: &str) -> bool {
        self.tags.iter().any(|own| {
            own == tag
                || own
                    .strip_prefix(tag)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    }
}

/// Tags other apps keep for themselves (darktable's `darktable|format|rw2`):
/// kept in the sidecar, not shown.
pub fn is_internal(tag: &str) -> bool {
    tag.starts_with("darktable/")
}

/// The sidecar omalens writes for `media`: `name.ext.xmp`, the name digiKam
/// and darktable use, which keeps a RAW and a JPEG of one shot apart.
pub fn path_for(media: &Path) -> PathBuf {
    let mut name = media.as_os_str().to_owned();
    name.push(".xmp");
    PathBuf::from(name)
}

/// The existing sidecar of `media`: `name.ext.xmp`, else Lightroom's
/// `name.xmp`.
pub fn find(media: &Path) -> Option<PathBuf> {
    let full = path_for(media);
    if full.is_file() {
        return Some(full);
    }
    ["xmp", "XMP"]
        .into_iter()
        .map(|ext| media.with_extension(ext))
        .find(|stem| stem.is_file() && stem != media)
}

/// The tags of `media`, empty when it has no sidecar or an unreadable one.
pub fn read(media: &Path) -> Meta {
    let Some(sidecar) = find(media) else {
        return Meta::default();
    };
    match std::fs::read_to_string(&sidecar)
        .map_err(|error| error.to_string())
        .and_then(|text| parse(&text))
    {
        Ok(meta) => meta,
        Err(error) => {
            eprintln!("omalens: {}: {error}", sidecar.display());
            Meta::default()
        }
    }
}

/// Store `meta` for `media`, in its existing sidecar if it has one. A
/// sidecar omalens created is deleted again once it holds nothing.
pub fn write(media: &Path, meta: &Meta) -> Result<(), String> {
    let existing = find(media);
    let text = match &existing {
        Some(path) => std::fs::read_to_string(path).map_err(|error| error.to_string())?,
        None if meta.is_empty() => return Ok(()),
        None => TEMPLATE.to_string(),
    };
    let updated = update(&text, meta)?;
    let path = existing.unwrap_or_else(|| path_for(media));
    if meta.is_empty() && updated.own && !updated.foreign {
        return std::fs::remove_file(&path).map_err(|error| error.to_string());
    }
    crate::library::write_atomic(&path, |temp| {
        std::fs::write(temp, &updated.text).map_err(|error| error.to_string())
    })
}

/// Move `media`'s sidecar along when the file is renamed to `to`.
pub fn follow_rename(media: &Path, to: &Path) -> Result<(), String> {
    let Some(sidecar) = find(media) else {
        return Ok(());
    };
    let target = path_for(to);
    if target.exists() {
        return Err(format!("{} already exists", target.display()));
    }
    std::fs::rename(&sidecar, &target).map_err(|error| error.to_string())
}

fn xml_error(error: quick_xml::Error) -> String {
    format!("not valid XMP: {error}")
}

fn resolved(result: ResolveResult) -> Option<Vec<u8>> {
    match result {
        ResolveResult::Bound(namespace) => Some(namespace.into_inner().to_vec()),
        _ => None,
    }
}

/// Text of a character or entity reference (`&amp;`, `&#233;`).
fn reference(name: &str) -> String {
    match name {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" => "'".into(),
        _ => {
            let code = name
                .strip_prefix("#x")
                .map(|hex| u32::from_str_radix(hex, 16).ok())
                .unwrap_or_else(|| name.strip_prefix('#').and_then(|dec| dec.parse().ok()));
            code.and_then(char::from_u32)
                .map(String::from)
                .unwrap_or_default()
        }
    }
}

/// Read the five fields from a sidecar's XMP, as elements or attributes of
/// any `rdf:Description`.
fn parse(text: &str) -> Result<Meta, String> {
    let mut reader = NsReader::from_str(text);
    // Per element: whether it is an rdf:Description.
    let mut stack: Vec<bool> = Vec::new();
    // The field being read, the depth of its element, and its text so far.
    let mut current: Option<(Field, usize)> = None;
    let mut item = String::new();
    let (mut subject, mut hierarchical, mut tags_list) = (Vec::new(), Vec::new(), Vec::new());
    let (mut rating, mut label) = (None::<String>, None::<String>);
    let mut store = |field: Field, value: String| match field {
        Field::Subject => subject.push(value),
        Field::Hierarchical => hierarchical.push(value),
        Field::TagsList => tags_list.push(value),
        Field::Rating => rating = Some(value),
        Field::Label => label = Some(value),
    };
    loop {
        let (namespace, event) = reader.read_resolved_event().map_err(xml_error)?;
        let namespace = resolved(namespace);
        let empty = matches!(event, Event::Empty(_));
        match event {
            Event::Eof => break,
            Event::Start(start) | Event::Empty(start) => {
                let local = start.local_name().into_inner().to_vec();
                let in_description = stack.last() == Some(&true);
                let is_description = namespace.as_deref() == Some(RDF) && local == b"Description";
                if is_description {
                    for attribute in start.attributes().flatten() {
                        let (space, name) = reader.resolver().resolve_attribute(attribute.key);
                        let field =
                            resolved(space).and_then(|space| Field::of(&space, name.into_inner()));
                        if let Some(field) = field {
                            let value = attribute
                                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                .map_err(xml_error)?;
                            store(field, value.into_owned());
                        }
                    }
                }
                if current.is_none() && in_description {
                    let field = namespace
                        .as_deref()
                        .and_then(|space| Field::of(space, &local));
                    if let Some(field) = field.filter(|_| !empty) {
                        current = Some((field, stack.len() + 1));
                    }
                }
                if !empty {
                    stack.push(is_description);
                    if current.is_some() {
                        item.clear();
                    }
                }
            }
            Event::End(_) => {
                if let Some((field, depth)) = current {
                    let value = std::mem::take(&mut item);
                    let value = value.trim();
                    // A list item, or the field's own text (Rating, Label).
                    let leaf =
                        matches!(field, Field::Rating | Field::Label) && stack.len() == depth;
                    let li = stack.len() > depth && !value.is_empty();
                    if leaf || li {
                        store(field, value.to_string());
                    }
                    if stack.len() == depth {
                        current = None;
                    }
                }
                stack.pop();
            }
            Event::Text(text) if current.is_some() => {
                item.push_str(&text.decode().map_err(|error| error.to_string())?);
            }
            Event::GeneralRef(name) if current.is_some() => {
                item.push_str(&reference(
                    &name.decode().map_err(|error| error.to_string())?,
                ));
            }
            Event::CData(data) if current.is_some() => {
                item.push_str(&data.decode().map_err(|error| error.to_string())?);
            }
            _ => {}
        }
    }
    let mut tags: BTreeSet<String> = hierarchical
        .iter()
        .map(|tag| tag.replace('|', "/"))
        .chain(tags_list)
        .filter(|tag| !tag.is_empty())
        .collect();
    // Flat keywords that aren't the last part of a hierarchical tag are tags
    // of their own.
    let leaves: BTreeSet<String> = tags.iter().map(|tag| leaf(tag).to_string()).collect();
    tags.extend(
        subject
            .into_iter()
            .filter(|word| !word.is_empty() && !leaves.contains(word)),
    );
    let rating = rating
        .and_then(|rating| rating.trim().parse::<f32>().ok())
        .map_or(0, |rating| rating.round().clamp(-1.0, 5.0) as i8);
    let label = label.filter(|label| !label.trim().is_empty());
    Ok(Meta {
        tags,
        rating,
        label,
    })
}

fn leaf(tag: &str) -> &str {
    tag.rsplit('/').next().unwrap_or(tag)
}

/// A sidecar's text after `update`.
struct Updated {
    text: String,
    /// The sidecar is omalens' own (`x:xmptk="omalens"`).
    own: bool,
    /// It holds properties besides omalens' five.
    foreign: bool,
}

/// Rewrite a sidecar's XMP with `meta`: drop the five fields wherever they
/// are and write them anew into the first `rdf:Description`. Every other
/// element, attribute, comment and whitespace run is copied as it was.
fn update(text: &str, meta: &Meta) -> Result<Updated, String> {
    let mut reader = NsReader::from_str(text);
    let mut writer = Writer::new(Vec::new());
    let mut stack: Vec<bool> = Vec::new();
    // The depth of the field element being dropped.
    let mut skip: Option<usize> = None;
    // The Description that receives the fields, and the prefixes to use.
    let mut target: Option<(usize, Prefixes)> = None;
    let mut written = false;
    let (mut own, mut foreign) = (false, false);
    // Whitespace before a Description's child, held back until it is known
    // whether the child is a dropped field (then the whitespace goes too).
    let mut pending: Option<Event> = None;
    loop {
        let (namespace, event) = reader.read_resolved_event().map_err(xml_error)?;
        let namespace = resolved(namespace);
        let in_description = stack.last() == Some(&true);
        let empty = matches!(event, Event::Empty(_));
        match event {
            Event::Eof => break,
            Event::Start(start) | Event::Empty(start) => {
                if skip.is_some() {
                    if !empty {
                        stack.push(false);
                    }
                    continue;
                }
                let local = start.local_name().into_inner().to_vec();
                let field = namespace
                    .as_deref()
                    .and_then(|space| Field::of(space, &local));
                if in_description && field.is_some() {
                    pending = None;
                    if !empty {
                        skip = Some(stack.len() + 1);
                        stack.push(false);
                    }
                    continue;
                }
                if let Some(pending) = pending.take() {
                    writer
                        .write_event(pending)
                        .map_err(|error| error.to_string())?;
                }
                if in_description {
                    foreign = true;
                }
                if local == b"xmpmeta" {
                    own = start.attributes().flatten().any(|attribute| {
                        attribute.key.local_name().into_inner() == b"xmptk"
                            && attribute.value.starts_with(b"omalens")
                    });
                }
                let is_description = namespace.as_deref() == Some(RDF) && local == b"Description";
                if !is_description {
                    if !empty {
                        stack.push(false);
                    }
                    writer
                        .write_event(if empty {
                            Event::Empty(start)
                        } else {
                            Event::Start(start)
                        })
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                let first = target.is_none() && !written;
                let (start, prefixes, has_foreign) = description(&reader, &start, first)?;
                foreign |= has_foreign;
                if empty && first {
                    // `<rdf:Description .../>`: open it to hold the fields.
                    let end = start.to_end().into_owned();
                    writer
                        .write_event(Event::Start(start))
                        .map_err(|error| error.to_string())?;
                    let indent = "\n   ".to_string();
                    write_fields(&mut writer, meta, &prefixes.expect("first"), &indent);
                    writer
                        .write_event(Event::End(end))
                        .map_err(|error| error.to_string())?;
                    written = true;
                    continue;
                }
                if !empty {
                    stack.push(true);
                    if first {
                        target = prefixes.map(|prefixes| (stack.len(), prefixes));
                    }
                }
                writer
                    .write_event(if empty {
                        Event::Empty(start)
                    } else {
                        Event::Start(start)
                    })
                    .map_err(|error| error.to_string())?;
            }
            Event::End(end) => {
                if let Some(depth) = skip {
                    if stack.len() == depth {
                        skip = None;
                    }
                    stack.pop();
                    continue;
                }
                let closing = pending.take();
                if let Some((depth, prefixes)) =
                    target.as_ref().filter(|(depth, _)| *depth == stack.len())
                {
                    let _ = depth;
                    // Indent the fields one step deeper than this closing tag.
                    let close_indent = match &closing {
                        Some(Event::Text(text)) => String::from_utf8_lossy(text).into_owned(),
                        _ => "\n  ".to_string(),
                    };
                    let indent = format!("{close_indent} ");
                    write_fields(&mut writer, meta, prefixes, &indent);
                    written = true;
                    target = None;
                }
                if let Some(closing) = closing {
                    writer
                        .write_event(closing)
                        .map_err(|error| error.to_string())?;
                }
                stack.pop();
                writer
                    .write_event(Event::End(end))
                    .map_err(|error| error.to_string())?;
            }
            Event::Text(text) if skip.is_none() => {
                if let Some(held) = pending.take() {
                    writer
                        .write_event(held)
                        .map_err(|error| error.to_string())?;
                }
                if in_description && text.iter().all(u8::is_ascii_whitespace) {
                    pending = Some(Event::Text(text));
                } else {
                    writer
                        .write_event(Event::Text(text))
                        .map_err(|error| error.to_string())?;
                }
            }
            event if skip.is_none() => {
                if let Some(held) = pending.take() {
                    writer
                        .write_event(held)
                        .map_err(|error| error.to_string())?;
                }
                writer
                    .write_event(event)
                    .map_err(|error| error.to_string())?;
            }
            _ => {}
        }
    }
    if !written {
        return Err("no rdf:Description to write into".into());
    }
    let mut out = String::from_utf8(writer.into_inner()).map_err(|error| error.to_string())?;
    // The reader skips a byte order mark; keep the file's.
    if text.starts_with('\u{feff}') && !out.starts_with('\u{feff}') {
        out.insert(0, '\u{feff}');
    }
    Ok(Updated {
        text: out,
        own,
        foreign,
    })
}

/// Prefixes for the namespaces omalens writes, as usable inside the target
/// Description.
struct Prefixes {
    rdf: String,
    dc: String,
    lr: String,
    digikam: String,
    xmp: String,
}

/// An `rdf:Description` start tag without the five fields' attributes, plus,
/// for the first one, the prefixes to write the fields with (declaring any
/// that aren't in scope). Unchanged tags keep their exact bytes.
fn description<'a>(
    reader: &NsReader<&[u8]>,
    start: &BytesStart<'a>,
    first: bool,
) -> Result<(BytesStart<'a>, Option<Prefixes>, bool), String> {
    let mut dropped: Vec<Attribute> = Vec::new();
    let mut foreign = false;
    for attribute in start.attributes() {
        let attribute = attribute.map_err(|error| error.to_string())?;
        let (space, name) = reader.resolver().resolve_attribute(attribute.key);
        let space = resolved(space);
        if space
            .as_deref()
            .and_then(|space| Field::of(space, name.into_inner()))
            .is_some()
        {
            dropped.push(attribute);
            continue;
        }
        let key = attribute.key.into_inner();
        let declaration =
            key == b"xmlns" || key.starts_with(b"xmlns:") || space.as_deref() == Some(XMLNS);
        let about = space.as_deref() == Some(RDF) && name.into_inner() == b"about";
        if !declaration && !about {
            foreign = true;
        }
    }
    let mut declare: Vec<(String, &[u8])> = Vec::new();
    let prefixes = first.then(|| {
        let mut pick = |preferred: &str, uri: &'static [u8]| -> String {
            // A prefix already bound to `uri` here, else `preferred` (or a
            // numbered variant when that one means something else).
            for number in 0.. {
                let prefix = if number == 0 {
                    preferred.to_string()
                } else {
                    format!("{preferred}{number}")
                };
                let probe = format!("{prefix}:x");
                match reader.resolver().resolve_element(QName(probe.as_bytes())).0 {
                    ResolveResult::Bound(space) if space.into_inner() == uri => return prefix,
                    ResolveResult::Bound(_) => continue,
                    _ => {
                        if declare.iter().any(|(declared, _)| *declared == prefix) {
                            continue;
                        }
                        declare.push((prefix.clone(), uri));
                        return prefix;
                    }
                }
            }
            unreachable!()
        };
        let rdf = String::from_utf8_lossy(
            start
                .name()
                .prefix()
                .map_or(&b""[..], |prefix| prefix.into_inner()),
        )
        .into_owned();
        Prefixes {
            rdf,
            dc: pick("dc", DC),
            lr: pick("lr", LR),
            digikam: pick("digiKam", DIGIKAM),
            xmp: pick("xmp", XMP),
        }
    });
    if dropped.is_empty() && declare.is_empty() {
        return Ok((start.clone(), prefixes, foreign));
    }
    Ok((edit_tag(start, &dropped, &declare), prefixes, foreign))
}

/// `start` with the `dropped` attributes cut out of its text and the
/// `declare`d namespaces added after its last declaration, in the same
/// indentation and quotes, so the rest of the tag keeps its exact bytes.
fn edit_tag(
    start: &BytesStart,
    dropped: &[Attribute],
    declare: &[(String, &[u8])],
) -> BytesStart<'static> {
    let mut content = String::from_utf8_lossy(start).into_owned();
    let name_len = start.name().into_inner().len();
    // Where an attribute's text starts (after the whitespace before it) and ends.
    let span = |content: &str, key: &[u8], value: &[u8]| -> Option<(usize, usize)> {
        let (key, value) = (String::from_utf8_lossy(key), String::from_utf8_lossy(value));
        ['"', '\''].into_iter().find_map(|quote| {
            let text = format!("{key}={quote}{value}{quote}");
            content.match_indices(&text).find_map(|(at, _)| {
                let after_space = content[..at].ends_with(|c: char| c.is_ascii_whitespace());
                after_space.then_some((at, at + text.len()))
            })
        })
    };
    // The start of the whitespace run before `at`.
    let space_before = |content: &str, at: usize| {
        name_len.max(
            content[..at]
                .trim_end_matches(|c: char| c.is_ascii_whitespace())
                .len(),
        )
    };
    for attribute in dropped {
        if let Some((from, to)) = span(&content, attribute.key.into_inner(), &attribute.value) {
            let from = space_before(&content, from);
            content.replace_range(from..to, "");
        }
    }
    if !declare.is_empty() {
        // Follow the tag's last declaration: its indentation and its quotes.
        let attributes: Vec<(Vec<u8>, Vec<u8>)> =
            BytesStart::from_content(content.as_str(), name_len)
                .attributes()
                .flatten()
                .map(|attribute| {
                    (
                        attribute.key.into_inner().to_vec(),
                        attribute.value.to_vec(),
                    )
                })
                .collect();
        let last = attributes
            .iter()
            .filter(|(key, _)| key.starts_with(b"xmlns"))
            .last()
            .or(attributes.last())
            .and_then(|(key, value)| span(&content, key, value));
        let (at, indent, quote) = match last {
            Some((from, to)) => {
                let indent = content[space_before(&content, from)..from].to_string();
                (to, indent, content[..to].chars().last().unwrap_or('"'))
            }
            None => (content.len(), " ".to_string(), '"'),
        };
        let added: String = declare
            .iter()
            .map(|(prefix, uri)| {
                format!(
                    "{indent}xmlns:{prefix}={quote}{}{quote}",
                    String::from_utf8_lossy(uri)
                )
            })
            .collect();
        content.insert_str(at, &added);
    }
    BytesStart::from_content(content, name_len)
}

/// Write the five fields, each on its own line at `indent`.
fn write_fields(writer: &mut Writer<Vec<u8>>, meta: &Meta, prefixes: &Prefixes, indent: &str) {
    let rdf = |name: &str| {
        if prefixes.rdf.is_empty() {
            name.to_string()
        } else {
            format!("{}:{name}", prefixes.rdf)
        }
    };
    let out = writer.get_mut();
    let list = |out: &mut Vec<u8>, property: String, container: &str, items: Vec<String>| {
        let container = rdf(container);
        let li = rdf("li");
        out.extend(format!("{indent}<{property}>{indent} <{container}>").bytes());
        for item in items {
            let item = quick_xml::escape::escape(item.as_str());
            out.extend(format!("{indent}  <{li}>{item}</{li}>").bytes());
        }
        out.extend(format!("{indent} </{container}>{indent}</{property}>").bytes());
    };
    // Other apps' own tags (darktable's) go back only where they keep them.
    let shared = || meta.tags.iter().filter(|tag| !is_internal(tag));
    let leaves: BTreeSet<&str> = shared().map(|tag| leaf(tag)).collect();
    list(
        out,
        format!("{}:subject", prefixes.dc),
        "Bag",
        leaves.into_iter().map(String::from).collect(),
    );
    list(
        out,
        format!("{}:hierarchicalSubject", prefixes.lr),
        "Bag",
        meta.tags.iter().map(|tag| tag.replace('/', "|")).collect(),
    );
    list(
        out,
        format!("{}:TagsList", prefixes.digikam),
        "Seq",
        shared().cloned().collect(),
    );
    let xmp = &prefixes.xmp;
    out.extend(format!("{indent}<{xmp}:Rating>{}</{xmp}:Rating>", meta.rating).bytes());
    let label = quick_xml::escape::escape(meta.label.as_deref().unwrap_or(""));
    out.extend(format!("{indent}<{xmp}:Label>{label}</{xmp}:Label>").bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    const DARKTABLE: &str = include_str!("../testdata/sidecars/darktable.xmp");
    const DIGIKAM: &str = include_str!("../testdata/sidecars/digikam.xmp");
    const LIGHTROOM: &str = include_str!("../testdata/sidecars/lightroom.xmp");
    const EXIFTOOL: &str = include_str!("../testdata/sidecars/exiftool.xmp");
    const ALL: [(&str, &str); 5] = [
        ("darktable", DARKTABLE),
        ("digikam", DIGIKAM),
        ("lightroom", LIGHTROOM),
        ("exiftool", EXIFTOOL),
        ("template", TEMPLATE),
    ];

    fn meta(tags: &[&str], rating: i8, label: Option<&str>) -> Meta {
        Meta {
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            rating,
            label: label.map(String::from),
        }
    }

    /// Everything in a sidecar except omalens' five fields, namespace
    /// declarations and indentation: what other apps own.
    fn foreign(text: &str) -> Vec<String> {
        let mut reader = NsReader::from_str(text);
        let mut out = Vec::new();
        let mut stack: Vec<bool> = Vec::new();
        let mut skip: Option<usize> = None;
        loop {
            let (space, event) = reader.read_resolved_event().unwrap();
            let space = resolved(space).map(|space| String::from_utf8(space).unwrap());
            let empty = matches!(event, Event::Empty(_));
            match event {
                Event::Eof => break,
                Event::Start(start) | Event::Empty(start) => {
                    let local =
                        String::from_utf8(start.local_name().into_inner().to_vec()).unwrap();
                    let field = space
                        .as_deref()
                        .and_then(|space| Field::of(space.as_bytes(), local.as_bytes()));
                    if skip.is_some() || (stack.last() == Some(&true) && field.is_some()) {
                        if !empty {
                            skip.get_or_insert(stack.len() + 1);
                            stack.push(false);
                        }
                        continue;
                    }
                    let mut attributes: Vec<String> = start
                        .attributes()
                        .flatten()
                        .filter_map(|attribute| {
                            let key = attribute.key.into_inner();
                            if key == b"xmlns" || key.starts_with(b"xmlns:") {
                                return None;
                            }
                            let (space, name) = reader.resolver().resolve_attribute(attribute.key);
                            let space = resolved(space).unwrap_or_default();
                            if Field::of(&space, name.into_inner()).is_some() {
                                return None;
                            }
                            let value = attribute
                                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                .unwrap();
                            Some(format!(
                                "{}{}={value}",
                                String::from_utf8_lossy(&space),
                                String::from_utf8_lossy(name.into_inner())
                            ))
                        })
                        .collect();
                    attributes.sort();
                    out.push(format!(
                        "<{}{local} {}>",
                        space.unwrap_or_default(),
                        attributes.join(" ")
                    ));
                    if !empty {
                        stack.push(out.last().unwrap().contains("rdf-syntax-ns#Description"));
                    } else {
                        out.push("</>".into());
                    }
                }
                Event::End(_) => {
                    if let Some(depth) = skip {
                        if stack.len() == depth {
                            skip = None;
                        }
                        stack.pop();
                        continue;
                    }
                    stack.pop();
                    out.push("</>".into());
                }
                Event::Text(text) if skip.is_none() => {
                    let text = text.decode().unwrap();
                    if !text.trim().is_empty() {
                        out.push(text.into_owned());
                    }
                }
                Event::GeneralRef(name) if skip.is_none() => {
                    out.push(format!("&{};", name.decode().unwrap()))
                }
                Event::Comment(_) | Event::PI(_) | Event::Decl(_) if skip.is_none() => {
                    out.push(format!("{event:?}"));
                }
                _ => {}
            }
        }
        out
    }

    #[test]
    fn reads_each_apps_sidecar() {
        assert_eq!(
            parse(DARKTABLE).unwrap(),
            meta(&["Best", "darktable/format/rw2"], 1, None)
        );
        assert_eq!(parse(DIGIKAM).unwrap(), meta(&["Shot/Action"], 3, None));
        assert_eq!(
            parse(LIGHTROOM).unwrap(),
            meta(&["Kayak", "Place/Lake"], 4, Some("Red"))
        );
        assert_eq!(parse(EXIFTOOL).unwrap(), meta(&["Day 1"], 2, None));
        assert_eq!(parse(TEMPLATE).unwrap(), Meta::default());
    }

    #[test]
    fn update_changes_only_its_fields() {
        let new = meta(&["Shot/Action", "Best", "Sun & <moon>"], 5, Some("Green"));
        for (name, text) in ALL {
            let updated = update(text, &new).unwrap();
            assert_eq!(
                parse(&updated.text).unwrap(),
                new,
                "{name}: fields read back"
            );
            assert_eq!(
                foreign(&updated.text),
                foreign(text),
                "{name}: other apps' data"
            );
        }
    }

    #[test]
    fn update_keeps_other_apps_bytes() {
        let updated = update(DARKTABLE, &meta(&["Best"], 2, None)).unwrap().text;
        let history = &DARKTABLE[DARKTABLE.find("   <darktable:masks_history>").unwrap()
            ..DARKTABLE.find("   </darktable:history>").unwrap()];
        assert!(
            updated.contains(history),
            "darktable history rewritten:\n{updated}"
        );
        assert!(updated.contains(r#"darktable:import_timestamp="63869817600000000""#));
        let updated = update(DIGIKAM, &meta(&[], 0, None)).unwrap().text;
        let region = &DIGIKAM[DIGIKAM.find("   <dc:creator>").unwrap()
            ..DIGIKAM.find("  </rdf:Description>").unwrap()];
        assert!(
            updated.contains(region),
            "digiKam regions rewritten:\n{updated}"
        );
    }

    #[test]
    fn description_tag_keeps_its_text() {
        let updated = update(DARKTABLE, &meta(&["Best"], 2, None)).unwrap().text;
        let tag = |text: &str| {
            let from = text.find("<rdf:Description").unwrap();
            text[from..from + text[from..].find('>').unwrap() + 1].to_string()
        };
        let expect = tag(DARKTABLE).replace("\n   xmp:Rating=\"1\"", "").replace(
            r#"xmlns:darktable="http://darktable.sf.net/""#,
            "xmlns:darktable=\"http://darktable.sf.net/\"\n    xmlns:digiKam=\"http://www.digikam.org/ns/1.0/\"",
        );
        assert_eq!(tag(&updated), expect);
        // Single quotes stay single quotes.
        let updated = update(EXIFTOOL, &meta(&["Best"], 2, None)).unwrap().text;
        assert!(updated.contains("xmlns:dc='http://purl.org/dc/elements/1.1/'\n  xmlns:lr='http://ns.adobe.com/lightroom/1.0/'"), "{updated}");
    }

    #[test]
    fn update_is_stable() {
        let new = meta(&["Shot/Action"], 3, Some("Blue"));
        for (name, text) in ALL {
            let once = update(text, &new).unwrap().text;
            let twice = update(&once, &new).unwrap().text;
            assert_eq!(once, twice, "{name}");
        }
    }

    #[test]
    fn knows_whose_sidecar_it_is() {
        let empty = Meta::default();
        let own = update(TEMPLATE, &meta(&["Best"], 0, None)).unwrap();
        assert!(own.own && !own.foreign);
        let own_emptied = update(&own.text, &empty).unwrap();
        assert!(own_emptied.own && !own_emptied.foreign);
        for (name, text) in [
            ("darktable", DARKTABLE),
            ("digikam", DIGIKAM),
            ("lightroom", LIGHTROOM),
        ] {
            let updated = update(text, &empty).unwrap();
            assert!(!updated.own && updated.foreign, "{name}");
        }
    }

    #[test]
    fn keeps_darktables_own_tags_to_itself() {
        let new = meta(&["Best", "Shot/Action", "darktable/format/rw2"], 0, None);
        let updated = update(DARKTABLE, &new).unwrap().text;
        assert_eq!(parse(&updated).unwrap(), new);
        let subject =
            &updated[updated.find("<dc:subject>").unwrap()..updated.find("</dc:subject>").unwrap()];
        assert!(!subject.contains("rw2"), "{subject}");
        let list = &updated[updated.find("<digiKam:TagsList>").unwrap()
            ..updated.find("</digiKam:TagsList>").unwrap()];
        assert!(!list.contains("darktable"), "{list}");
        assert!(updated.contains("<rdf:li>darktable|format|rw2</rdf:li>"));
    }

    #[test]
    fn subtree_tags_match() {
        let tagged = meta(&["Shot/Action"], 0, None);
        assert!(tagged.has("Shot") && tagged.has("Shot/Action"));
        assert!(!tagged.has("Sho") && !tagged.has("Action"));
    }

    /// A scratch folder holding one fake media file.
    fn scratch(name: &str, file: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("omalens-sidecar-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let media = dir.join(file);
        std::fs::write(&media, b"media bytes").unwrap();
        media
    }

    #[test]
    fn write_creates_and_removes_its_own_sidecar() {
        let media = scratch("own", "P_S51020.MOV");
        let before = std::fs::metadata(&media).unwrap().modified().unwrap();
        let tagged = meta(&["Shot/Action"], 4, Some("Red"));
        write(&media, &tagged).unwrap();
        let sidecar = media.with_file_name("P_S51020.MOV.xmp");
        assert!(sidecar.is_file());
        assert_eq!(read(&media), tagged);
        write(&media, &Meta::default()).unwrap();
        assert!(!sidecar.exists(), "an emptied omalens sidecar is removed");
        assert_eq!(std::fs::read(&media).unwrap(), b"media bytes");
        assert_eq!(
            std::fs::metadata(&media).unwrap().modified().unwrap(),
            before
        );
        std::fs::remove_dir_all(media.parent().unwrap()).unwrap();
    }

    #[test]
    fn write_keeps_other_apps_sidecars() {
        let media = scratch("foreign", "photo.RW2");
        let sidecar = media.with_file_name("photo.RW2.xmp");
        std::fs::write(&sidecar, DARKTABLE).unwrap();
        write(&media, &Meta::default()).unwrap();
        let kept = std::fs::read_to_string(&sidecar).unwrap();
        assert_eq!(foreign(&kept), foreign(DARKTABLE));
        assert_eq!(read(&media), Meta::default());
        std::fs::remove_dir_all(media.parent().unwrap()).unwrap();
    }

    #[test]
    fn uses_lightrooms_sidecar_name_when_present() {
        let media = scratch("adobe", "photo.RW2");
        let adobe = media.with_file_name("photo.xmp");
        std::fs::write(&adobe, LIGHTROOM).unwrap();
        assert_eq!(find(&media), Some(adobe.clone()));
        let tagged = meta(&["Best"], 5, None);
        write(&media, &tagged).unwrap();
        assert!(!path_for(&media).exists(), "wrote back to photo.xmp");
        assert_eq!(read(&media), tagged);
        std::fs::remove_dir_all(media.parent().unwrap()).unwrap();
    }

    #[test]
    fn broken_sidecar_is_left_alone() {
        let media = scratch("broken", "photo.jpg");
        let sidecar = path_for(&media);
        let broken = "<x:xmpmeta><rdf:RDF><rdf:Description></x:xmpmeta>";
        std::fs::write(&sidecar, broken).unwrap();
        assert_eq!(read(&media), Meta::default());
        assert!(write(&media, &meta(&["Best"], 1, None)).is_err());
        assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), broken);
        std::fs::remove_dir_all(media.parent().unwrap()).unwrap();
    }

    #[test]
    fn reads_and_keeps_a_byte_order_mark() {
        let with_bom = format!("\u{feff}{LIGHTROOM}");
        assert_eq!(parse(&with_bom).unwrap(), parse(LIGHTROOM).unwrap());
        let updated = update(&with_bom, &meta(&["Best"], 1, None)).unwrap().text;
        assert!(updated.starts_with('\u{feff}'));
        assert_eq!(parse(&updated).unwrap(), meta(&["Best"], 1, None));
    }

    /// A shoot's worth of sidecars reads in well under the time it takes to
    /// show the first file (it runs in the background anyway).
    #[test]
    fn reads_a_shoot_quickly() {
        let first = scratch("shoot", "P_S50000.MOV");
        let dir = first.parent().unwrap().to_path_buf();
        let files: Vec<PathBuf> = (0..300)
            .map(|n| dir.join(format!("P_S5{n:04}.MOV")))
            .collect();
        for (n, file) in files.iter().enumerate() {
            std::fs::write(file, b"").unwrap();
            let tags = if n % 2 == 0 {
                vec!["Shot/Action", "Best"]
            } else {
                vec!["Day 1"]
            };
            write(file, &meta(&tags, (n % 6) as i8, None)).unwrap();
        }
        let started = std::time::Instant::now();
        let tagged = files.iter().filter(|file| !read(file).is_empty()).count();
        let elapsed = started.elapsed();
        eprintln!("read 300 sidecars in {elapsed:?}");
        assert_eq!(tagged, 300);
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "{elapsed:?}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// exiv2 (the library digiKam writes with) reads what omalens writes.
    #[test]
    fn exiv2_reads_the_fields() {
        let Ok(output) = std::process::Command::new("exiv2")
            .arg("--version")
            .output()
        else {
            eprintln!("exiv2 not installed, skipped");
            return;
        };
        assert!(output.status.success());
        for (name, text) in ALL {
            let media = scratch(&format!("exiv2-{name}"), "clip.MOV");
            std::fs::write(path_for(&media), text).unwrap();
            write(&media, &meta(&["Shot/Action", "Best"], 4, Some("Yellow"))).unwrap();
            let output = std::process::Command::new("exiv2")
                .arg("-px")
                .arg(path_for(&media))
                .output()
                .unwrap();
            let listing = String::from_utf8_lossy(&output.stdout);
            let field = |key: &str| {
                listing
                    .lines()
                    .find(|line| line.split_whitespace().next() == Some(key))
                    .map(|line| {
                        line.split_whitespace()
                            .skip(3)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
            };
            assert_eq!(
                field("Xmp.dc.subject").as_deref(),
                Some("Action, Best"),
                "{name}:\n{listing}"
            );
            assert_eq!(
                field("Xmp.lr.hierarchicalSubject").as_deref(),
                Some("Best, Shot|Action"),
                "{name}"
            );
            assert_eq!(
                field("Xmp.digiKam.TagsList").as_deref(),
                Some("Best, Shot/Action"),
                "{name}"
            );
            assert_eq!(field("Xmp.xmp.Rating").as_deref(), Some("4"), "{name}");
            assert_eq!(field("Xmp.xmp.Label").as_deref(), Some("Yellow"), "{name}");
            std::fs::remove_dir_all(media.parent().unwrap()).unwrap();
        }
    }
}
