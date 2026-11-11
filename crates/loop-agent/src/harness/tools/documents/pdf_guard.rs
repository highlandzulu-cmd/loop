//! Checks on a loaded PDF before pdf-extract walks it. They are best
//! effort; the reader child's limits are the guarantee.
//!
//! - pdf-extract draws a form XObject (`Do`) by recursing with no depth
//!   limit, so a form that draws itself overflows the stack. The forms each
//!   page can reach are walked here the way pdf-extract resolves them (a
//!   form without `/Resources` uses the resources it was drawn with), and
//!   a cycle or a chain deeper than [`MAX_DEPTH`] is refused.
//! - lopdf inflates Flate streams with no limit. Every Flate stream is
//!   inflated here once, counting and discarding the bytes, against
//!   [`MAX_STREAM_BYTES`] each and [`MAX_TOTAL_BYTES`] in all. lopdf has
//!   already inflated object streams while loading; those are bounded only
//!   by the child's limits.

use std::collections::HashSet;
use std::io::Read as _;

use pdf_extract::{Dictionary, Document, Object, ObjectId};

/// The deepest chain of forms drawing forms.
const MAX_DEPTH: usize = 24;
/// The most form visits for the whole document.
const MAX_VISITS: usize = 200_000;
/// The most one stream may inflate to.
pub(super) const MAX_STREAM_BYTES: u64 = 256 * 1024 * 1024;
/// The most all Flate streams may inflate to together.
pub(super) const MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;

pub(super) fn check(doc: &Document) -> Result<(), String> {
    check_streams(doc)?;
    check_forms(doc)
}

fn check_streams(doc: &Document) -> Result<(), String> {
    let mut total: u64 = 0;
    for object in doc.objects.values() {
        let Object::Stream(stream) = object else {
            continue;
        };
        if !is_flate(doc, &stream.dict) {
            continue;
        }
        let limit = MAX_STREAM_BYTES.min(MAX_TOTAL_BYTES - total);
        let used = inflated_size(&stream.content, limit + 1);
        if used > limit {
            return Err(format!(
                "the PDF holds compressed data that unpacks to too much (over {} MB), which looks \
                 like a decompression bomb",
                limit / (1024 * 1024)
            ));
        }
        total += used;
    }
    Ok(())
}

fn is_flate(doc: &Document, dict: &Dictionary) -> bool {
    let Ok(filter) = dict.get(b"Filter") else {
        return false;
    };
    let filter = doc.dereference(filter).map(|(_, f)| f).unwrap_or(filter);
    let flate = |o: &Object| matches!(o.as_name(), Ok(b"FlateDecode" | b"Fl"));
    match filter {
        Object::Array(filters) => filters.iter().any(flate),
        other => flate(other),
    }
}

/// The bytes `data` inflates to (zlib, else raw deflate, as lopdf tries),
/// reading at most `cap`.
fn inflated_size(data: &[u8], cap: u64) -> u64 {
    let mut sink = std::io::sink();
    let zlib = flate2::read::ZlibDecoder::new(data);
    let (used, ok) = copy_counted(zlib.take(cap), &mut sink);
    if ok || used > 0 || data.len() <= 2 {
        return used;
    }
    let raw = flate2::read::DeflateDecoder::new(&data[2..]);
    copy_counted(raw.take(cap), &mut sink).0
}

/// Bytes copied, and whether the stream ended cleanly.
fn copy_counted(mut from: impl std::io::Read, to: &mut std::io::Sink) -> (u64, bool) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut used = 0u64;
    loop {
        match from.read(&mut buf) {
            Ok(0) => return (used, true),
            Ok(n) => {
                used += n as u64;
                let _ = std::io::Write::write_all(to, &buf[..n]);
            }
            Err(_) => return (used, false),
        }
    }
}

struct Walk<'a> {
    doc: &'a Document,
    /// The forms being drawn, outermost first.
    stack: Vec<ObjectId>,
    visits: usize,
    /// (form, resources it inherited) pairs already found safe.
    done: HashSet<(ObjectId, usize)>,
}

fn check_forms(doc: &Document) -> Result<(), String> {
    let mut walk = Walk {
        doc,
        stack: Vec::new(),
        visits: 0,
        done: HashSet::new(),
    };
    for page in doc.get_pages().into_values() {
        let Ok(page) = doc.get_dictionary(page) else {
            continue;
        };
        if let Some(resources) = page_resources(doc, page) {
            walk.resources(resources)?;
        }
    }
    Ok(())
}

/// The page's `/Resources`, inherited from its parents when missing.
fn page_resources<'a>(doc: &'a Document, mut node: &'a Dictionary) -> Option<&'a Dictionary> {
    for _ in 0..32 {
        if let Some(resources) = dict_at(doc, node, b"Resources") {
            return Some(resources);
        }
        let parent = node.get(b"Parent").ok()?.as_reference().ok()?;
        node = doc.get_dictionary(parent).ok()?;
    }
    None
}

fn dict_at<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a Dictionary> {
    let value = dict.get(key).ok()?;
    let (_, value) = doc.dereference(value).ok()?;
    value.as_dict().ok()
}

impl Walk<'_> {
    /// Every form `resources` names, drawn with `resources`.
    fn resources(&mut self, resources: &Dictionary) -> Result<(), String> {
        let Some(xobjects) = dict_at(self.doc, resources, b"XObject") else {
            return Ok(());
        };
        for (_, value) in xobjects.iter() {
            if let Ok(id) = value.as_reference() {
                self.form(id, resources)?;
            }
        }
        Ok(())
    }

    fn form(&mut self, id: ObjectId, inherited: &Dictionary) -> Result<(), String> {
        if self.stack.contains(&id) {
            return Err(
                "the PDF has a form that draws itself, which the reader cannot handle".into(),
            );
        }
        if self.stack.len() >= MAX_DEPTH {
            return Err(format!(
                "the PDF nests forms more than {MAX_DEPTH} deep, which the reader cannot handle"
            ));
        }
        self.visits += 1;
        if self.visits > MAX_VISITS {
            return Err("the PDF's forms are too complex to read".into());
        }
        let Ok(Object::Stream(stream)) = self.doc.get_object(id) else {
            return Ok(());
        };
        let own = dict_at(self.doc, &stream.dict, b"Resources");
        // A form with its own resources is safe in any context once checked.
        let key = (
            id,
            own.map_or(inherited as *const Dictionary as usize, |_| 0),
        );
        if self.done.contains(&key) {
            return Ok(());
        }
        self.stack.push(id);
        let result = self.resources(own.unwrap_or(inherited));
        self.stack.pop();
        result?;
        self.done.insert(key);
        Ok(())
    }
}
