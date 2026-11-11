//! Zip packages and a flat XML event list for the Office and OpenDocument formats.

use std::io::{Cursor, Read, Write};

use quick_xml::events::{BytesStart, Event};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// The most one part of a package may inflate to, against zip bombs.
const MAX_PART_BYTES: u64 = 64 * 1024 * 1024;

/// The most all the parts one read inflates may add up to.
pub(super) const INFLATE_BUDGET: u64 = 256 * 1024 * 1024;

/// The most entries a package may list.
const MAX_ENTRIES: usize = 20_000;

/// A zip package being read, with one inflate budget for all its parts.
pub(super) struct Package<'a> {
    zip: ZipArchive<Cursor<&'a [u8]>>,
    /// Bytes still allowed to inflate.
    budget: u64,
}

impl<'a> Package<'a> {
    pub(super) fn open(bytes: &'a [u8]) -> Result<Self, String> {
        Self::with_budget(bytes, INFLATE_BUDGET)
    }

    pub(super) fn with_budget(bytes: &'a [u8], budget: u64) -> Result<Self, String> {
        let zip = ZipArchive::new(Cursor::new(bytes))
            .map_err(|e| format!("not a valid Office file (zip): {e}"))?;
        if zip.len() > MAX_ENTRIES {
            return Err(format!(
                "the file has too many parts ({}; the limit is {MAX_ENTRIES})",
                zip.len()
            ));
        }
        Ok(Self { zip, budget })
    }

    /// The UTF-8 text of part `name`, or `None` when the package has no such part.
    pub(super) fn text(&mut self, name: &str) -> Result<Option<String>, String> {
        let file = match self.zip.by_name(name) {
            Ok(file) => file,
            Err(zip::result::ZipError::FileNotFound) => return Ok(None),
            Err(e) => return Err(format!("{name}: {e}")),
        };
        let limit = MAX_PART_BYTES.min(self.budget);
        let mut text = String::new();
        file.take(limit + 1)
            .read_to_string(&mut text)
            .map_err(|e| format!("{name}: {e}"))?;
        let used = text.len() as u64;
        if used > MAX_PART_BYTES {
            return Err(format!("{name} is too large"));
        }
        if used > self.budget {
            return Err(too_much());
        }
        self.budget -= used;
        Ok(Some(text))
    }

    /// Inflate every part, discarding the bytes, to prove the whole
    /// package stays within the budget before a reader that has no limit
    /// of its own (calamine) inflates it again. Declared sizes are not
    /// trusted: the bytes are counted.
    pub(super) fn check_all(&mut self) -> Result<(), String> {
        for i in 0..self.zip.len() {
            let file = self
                .zip
                .by_index(i)
                .map_err(|e| format!("not a valid Office file (zip): {e}"))?;
            let used = std::io::copy(&mut file.take(self.budget + 1), &mut std::io::sink())
                .map_err(|e| format!("not a valid Office file (zip): {e}"))?;
            if used > self.budget {
                return Err(too_much());
            }
            self.budget -= used;
        }
        Ok(())
    }
}

fn too_much() -> String {
    format!(
        "the file unpacks to more than {} MB, which looks like a zip bomb",
        INFLATE_BUDGET / (1024 * 1024)
    )
}

/// A zip package of `parts`; a `mimetype` part is stored uncompressed, as
/// OpenDocument requires it first and unpacked.
pub(super) fn package(parts: &[(&str, &str)]) -> Result<Vec<u8>, String> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, contents) in parts {
        let method = if *name == "mimetype" {
            CompressionMethod::Stored
        } else {
            CompressionMethod::Deflated
        };
        zip.start_file(
            *name,
            SimpleFileOptions::default().compression_method(method),
        )
        .map_err(|e| e.to_string())?;
        zip.write_all(contents.as_bytes())
            .map_err(|e| e.to_string())?;
    }
    Ok(zip.finish().map_err(|e| e.to_string())?.into_inner())
}

/// One XML event; names are local (no namespace prefix). An empty element
/// is an `Open` followed by its `Close`.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Node {
    Open {
        name: String,
        /// Attribute names keep their prefix (`r:id`); see [`attr`].
        attrs: Vec<(String, String)>,
    },
    Close(String),
    Text(String),
}

/// The events of `xml`, one at a time, with entities and character
/// references resolved.
pub(super) fn nodes(xml: &str) -> Nodes<'_> {
    Nodes {
        reader: quick_xml::Reader::from_str(xml),
        pending: None,
        done: false,
    }
}

/// See [`nodes`]. Stops after the first error.
pub(super) struct Nodes<'a> {
    reader: quick_xml::Reader<&'a [u8]>,
    /// The `Close` of an empty element just returned as `Open`.
    pending: Option<String>,
    done: bool,
}

impl Iterator for Nodes<'_> {
    type Item = Result<Node, String>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(name) = self.pending.take() {
            return Some(Ok(Node::Close(name)));
        }
        while !self.done {
            let node = match self.reader.read_event() {
                Err(e) => Err(format!("malformed XML: {e}")),
                Ok(Event::Eof) => {
                    self.done = true;
                    return None;
                }
                Ok(Event::Start(e)) => open(&e),
                Ok(Event::Empty(e)) => open(&e).inspect(|node| {
                    if let Node::Open { name, .. } = node {
                        self.pending = Some(name.clone());
                    }
                }),
                Ok(Event::End(e)) => Ok(Node::Close(local(e.local_name().as_ref()))),
                Ok(Event::Text(e)) => Ok(Node::Text(e.xml10_content().into_owned())),
                Ok(Event::CData(e)) => Ok(Node::Text(e.xml10_content().into_owned())),
                Ok(Event::GeneralRef(e)) => {
                    let ch = match e.resolve_char_ref() {
                        Ok(Some(ch)) => Some(ch),
                        _ => match e.xml10_content().as_ref() {
                            "amp" => Some('&'),
                            "lt" => Some('<'),
                            "gt" => Some('>'),
                            "quot" => Some('"'),
                            "apos" => Some('\''),
                            _ => None,
                        },
                    };
                    match ch {
                        Some(ch) => Ok(Node::Text(ch.to_string())),
                        None => continue,
                    }
                }
                Ok(_) => continue,
            };
            if node.is_err() {
                self.done = true;
            }
            return Some(node);
        }
        None
    }
}

fn open(e: &BytesStart) -> Result<Node, String> {
    let mut attrs = Vec::new();
    for attr in e.attributes().with_checks(false) {
        let attr = attr.map_err(|e| e.to_string())?;
        let key = attr.key.as_ref().to_owned();
        let value = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|e| e.to_string())?;
        attrs.push((key, value.into_owned()));
    }
    Ok(Node::Open {
        name: local(e.local_name().as_ref()),
        attrs,
    })
}

fn local(name: &str) -> String {
    name.to_owned()
}

/// Attribute `name` exactly (`r:id`), else by local name (`val` finds `w:val`).
pub(super) fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == name)
        .or_else(|| {
            attrs
                .iter()
                .find(|(k, _)| k.rsplit(':').next() == Some(name))
        })
        .map(|(_, v)| v.as_str())
}

/// `text` escaped for XML content and attribute values, without the
/// control characters XML 1.0 forbids.
pub(super) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(ch),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// The part a relationship `target` names, relative to the folder `base`
/// (`ppt/`), or absolute when it starts with `/`.
pub(super) fn resolve(base: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_owned();
    }
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for piece in target.split('/') {
        match piece {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            p => parts.push(p),
        }
    }
    parts.join("/")
}

/// Relationship id to target from a `.rels` part.
pub(super) fn relationships(
    xml: &str,
) -> Result<std::collections::HashMap<String, String>, String> {
    let mut out = std::collections::HashMap::new();
    for node in nodes(xml) {
        if let Node::Open { name, attrs } = node? {
            if name == "Relationship" {
                if let (Some(id), Some(target)) = (attr(&attrs, "Id"), attr(&attrs, "Target")) {
                    out.insert(id.to_owned(), target.to_owned());
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_resolve_entities_and_split_empty_elements() {
        let got: Vec<Node> = nodes(r#"<w:p><w:t>a &amp; b&#33;</w:t><w:br/></w:p>"#)
            .collect::<Result<_, _>>()
            .unwrap();
        let text: String = got
            .iter()
            .filter_map(|n| match n {
                Node::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "a & b!");
        assert!(got.contains(&Node::Close("br".into())));
    }

    #[test]
    fn attr_prefers_the_exact_name() {
        let attrs = vec![
            ("id".to_owned(), "256".to_owned()),
            ("r:id".to_owned(), "rId2".to_owned()),
        ];
        assert_eq!(attr(&attrs, "r:id"), Some("rId2"));
        assert_eq!(attr(&attrs, "id"), Some("256"));
    }

    #[test]
    fn resolve_targets() {
        assert_eq!(
            resolve("ppt/", "slides/slide1.xml"),
            "ppt/slides/slide1.xml"
        );
        assert_eq!(
            resolve("ppt/slides/", "../slideLayouts/a.xml"),
            "ppt/slideLayouts/a.xml"
        );
        assert_eq!(resolve("ppt/", "/ppt/slides/s.xml"), "ppt/slides/s.xml");
    }
}
