//! Streaming XML reader with global input and structural limits.
use crate::{NzbDocument, NzbFile, Segment, validate};
use quick_xml::{
    Reader,
    events::{BytesStart, Event},
};
use std::{
    cell::Cell,
    collections::HashMap,
    io::{self, BufReader, Read},
    rc::Rc,
};
use thiserror::Error;

/// Resource ceilings. All lengths are UTF-8 bytes, not character counts.
/// The input cap also bounds quick-xml's temporary single-event allocation;
/// field limits apply after an event has been decoded.
#[derive(Debug, Clone, Copy)]
pub struct ParseLimits {
    /// Maximum XML input bytes, including comments and markup (default 32 MiB).
    pub max_bytes: u64,
    /// Maximum open elements (default 32).
    pub max_depth: usize,
    /// Maximum individual attribute or accumulated text field (default 64 KiB).
    pub max_field_bytes: usize,
    /// Maximum files (default 10,000).
    pub max_files: usize,
    /// Maximum segments across all files (default 250,000).
    pub max_segments: usize,
}
impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            max_bytes: 32 * 1024 * 1024,
            max_depth: 32,
            max_field_bytes: 64 * 1024,
            max_files: 10_000,
            max_segments: 250_000,
        }
    }
}

/// Fail-closed parser errors; messages never include input/password values.
#[derive(Debug, Error)]
pub enum ParseError {
    /// A configured resource ceiling was reached.
    #[error("NZB limit exceeded: {0}")]
    Limit(&'static str),
    /// XML is malformed, not UTF-8, or uses an unsupported construct.
    #[error("invalid NZB XML")]
    Xml,
    /// Input could not be read.
    #[error("could not read NZB input")]
    Io(#[source] io::Error),
    /// DTD declarations and custom entity references are forbidden.
    #[error("DOCTYPE and custom entities are forbidden")]
    ForbiddenEntity,
    /// Domain structure or a numeric value is invalid.
    #[error("invalid NZB: {0}")]
    Invalid(&'static str),
}

struct Bounded<R> {
    inner: R,
    left: u64,
    exceeded: Rc<Cell<bool>>,
}
impl<R: Read> Read for Bounded<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let mut probe = [0];
            if self.inner.read(&mut probe)? != 0 {
                self.exceeded.set(true);
                return Err(io::Error::other("NZB byte limit"));
            }
            return Ok(0);
        }
        let size = out
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut out[..size])?;
        self.left -= n as u64;
        Ok(n)
    }
}
struct Frame {
    name: String,
    attrs: HashMap<String, String>,
    text: String,
}
fn attrs(
    event: &BytesStart<'_>,
    limits: ParseLimits,
) -> Result<HashMap<String, String>, ParseError> {
    let mut result = HashMap::new();
    for attr in event.attributes() {
        let attr = attr.map_err(|_| ParseError::Xml)?;
        let key = attr.key.as_ref();
        if attr.value.len() > limits.max_field_bytes || key.len() > limits.max_field_bytes {
            return Err(ParseError::Limit("field"));
        }
        // Only predefined XML entities and numeric references are resolved.
        let value = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|_| ParseError::ForbiddenEntity)?
            .into_owned();
        if value.len() > limits.max_field_bytes {
            return Err(ParseError::Limit("field"));
        }
        check_chars(&value)?;
        result.insert(key.to_owned(), value);
    }
    Ok(result)
}
fn required<'a>(attrs: &'a HashMap<String, String>, key: &str) -> Result<&'a str, ParseError> {
    attrs
        .get(key)
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(ParseError::Invalid("required attribute missing"))
}
fn number<T: std::str::FromStr>(s: &str) -> Result<T, ParseError> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ParseError::Invalid("invalid numeric attribute"));
    }
    s.parse()
        .map_err(|_| ParseError::Invalid("numeric attribute overflow"))
}
fn check_chars(text: &str) -> Result<(), ParseError> {
    if text.chars().all(|c| matches!(c, '\u{9}' | '\u{a}' | '\u{d}' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')) { Ok(()) } else { Err(ParseError::Xml) }
}
fn append(stack: &mut [Frame], text: &str, limits: ParseLimits) -> Result<(), ParseError> {
    check_chars(text)?;
    if let Some(frame) = stack.last_mut() {
        if matches!(frame.name.as_str(), "meta" | "group" | "segment") {
            if text.len() > limits.max_field_bytes.saturating_sub(frame.text.len()) {
                return Err(ParseError::Limit("field"));
            }
            frame.text.push_str(text);
        } else if !text.trim().is_empty() {
            return Err(ParseError::Invalid("unexpected text"));
        }
    } else if !text.trim().is_empty() {
        return Err(ParseError::Invalid("text outside root"));
    }
    Ok(())
}

/// Parse from a reader without loading the XML document as a tree.
///
/// Accepts the standard default NZB namespace (or unqualified NZB), UTF-8,
/// predefined entities and character references. Rejects unknown structure,
/// empty documents/files, noncontiguous segment numbers and unsafe message IDs.
/// No DTD is loaded and no network or filesystem references are followed.
pub fn parse(input: impl Read, limits: ParseLimits) -> Result<NzbDocument, ParseError> {
    let exceeded = Rc::new(Cell::new(false));
    let mut reader = Reader::from_reader(BufReader::new(Bounded {
        inner: input,
        left: limits.max_bytes,
        exceeded: exceeded.clone(),
    }));
    reader.config_mut().expand_empty_elements = true;
    reader.config_mut().check_comments = true;
    let mut stack: Vec<Frame> = Vec::new();
    let mut doc = NzbDocument::default();
    let mut file: Option<NzbFile> = None;
    let mut root_seen = false;
    let mut declaration_seen = false;
    let mut head_seen = false;
    let mut groups_seen = false;
    let mut segments_seen = false;
    let mut segment_count = 0usize;
    let mut buffer = Vec::new();
    loop {
        let event = reader.read_event_into(&mut buffer).map_err(|e| {
            if exceeded.get() {
                ParseError::Limit("input bytes")
            } else {
                match e {
                    quick_xml::Error::Io(e) => {
                        ParseError::Io(io::Error::new(e.kind(), "NZB read failed"))
                    }
                    _ => ParseError::Xml,
                }
            }
        })?;
        match event {
            Event::Start(e) => {
                if stack.len() >= limits.max_depth {
                    return Err(ParseError::Limit("depth"));
                }
                let name = e.name().as_ref().to_owned();
                if name.len() > limits.max_field_bytes {
                    return Err(ParseError::Limit("field"));
                }
                let attrs = attrs(&e, limits)?;
                let parent = stack.last().map(|f| f.name.as_str());
                match (parent, name.as_str()) {
                    (None, "nzb") if !root_seen => {
                        root_seen = true;
                        if attrs
                            .get("xmlns")
                            .is_some_and(|v| v != "http://www.newzbin.com/DTD/2003/nzb")
                        {
                            return Err(ParseError::Invalid("unsupported namespace"));
                        }
                    }
                    (Some("nzb"), "head") if !head_seen && doc.files.is_empty() => {
                        head_seen = true;
                    }
                    (Some("head"), "meta") => {
                        required(&attrs, "type")?;
                    }
                    (Some("nzb"), "file") => {
                        if doc.files.len() >= limits.max_files {
                            return Err(ParseError::Limit("files"));
                        }
                        file = Some(NzbFile {
                            subject: required(&attrs, "subject")?.to_owned(),
                            poster: required(&attrs, "poster")?.to_owned(),
                            date: number(required(&attrs, "date")?)?,
                            groups: Vec::new(),
                            segments: Vec::new(),
                        });
                        groups_seen = false;
                        segments_seen = false;
                    }
                    (Some("file"), "groups") if !groups_seen => {
                        groups_seen = true;
                    }
                    (Some("file"), "segments") if !segments_seen => {
                        segments_seen = true;
                    }
                    (Some("groups"), "group") => {}
                    (Some("segments"), "segment") => {
                        if segment_count >= limits.max_segments {
                            return Err(ParseError::Limit("segments"));
                        }
                        segment_count += 1;
                    }
                    _ => return Err(ParseError::Invalid("unexpected element")),
                }
                // Namespace rebinding would change the meaning of descendants.
                if parent.is_some()
                    && attrs
                        .keys()
                        .any(|k| k == "xmlns" || k.starts_with("xmlns:"))
                {
                    return Err(ParseError::Invalid("namespace rebinding"));
                }
                stack.push(Frame {
                    name,
                    attrs,
                    text: String::new(),
                });
            }
            Event::End(_) => {
                let frame = stack.pop().ok_or(ParseError::Xml)?;
                match frame.name.as_str() {
                    "meta" => {
                        let target = match required(&frame.attrs, "type")? {
                            "title" => Some(&mut doc.title),
                            "password" => Some(&mut doc.password),
                            "category" => Some(&mut doc.category),
                            _ => None,
                        };
                        if let Some(target) = target {
                            if target.is_some() {
                                return Err(ParseError::Invalid("duplicate metadata"));
                            }
                            *target = Some(frame.text);
                        }
                    }
                    "group" => file
                        .as_mut()
                        .ok_or(ParseError::Xml)?
                        .groups
                        .push(frame.text.trim().to_owned()),
                    "segment" => file
                        .as_mut()
                        .ok_or(ParseError::Xml)?
                        .segments
                        .push(Segment {
                            number: number(required(&frame.attrs, "number")?)?,
                            bytes: number(required(&frame.attrs, "bytes")?)?,
                            message_id: frame.text.trim().to_owned(),
                        }),
                    "file" => {
                        let mut finished = file.take().ok_or(ParseError::Xml)?;
                        finished.segments.sort_unstable_by_key(|s| s.number);
                        doc.files.push(finished);
                    }
                    _ => {}
                }
            }
            Event::Text(e) => append(&mut stack, &e.xml10_content(), limits)?,
            Event::CData(e) => {
                if stack.is_empty() {
                    return Err(ParseError::Xml);
                }
                append(&mut stack, &e.xml10_content(), limits)?;
            }
            Event::GeneralRef(e) => {
                if stack.is_empty() {
                    return Err(ParseError::Xml);
                }
                let text = match e.as_ref() {
                    "amp" => "&".to_owned(),
                    "lt" => "<".to_owned(),
                    "gt" => ">".to_owned(),
                    "quot" => "\"".to_owned(),
                    "apos" => "'".to_owned(),
                    _ => e
                        .resolve_char_ref()
                        .map_err(|_| ParseError::Xml)?
                        .ok_or(ParseError::ForbiddenEntity)?
                        .to_string(),
                };
                append(&mut stack, &text, limits)?;
            }
            Event::DocType(_) => return Err(ParseError::ForbiddenEntity),
            Event::Decl(e) => {
                if declaration_seen
                    || root_seen
                    || e.version().map_err(|_| ParseError::Xml)?.as_ref() != "1.0"
                {
                    return Err(ParseError::Xml);
                }
                declaration_seen = true;
                if let Some(enc) = e.encoding()
                    && !enc
                        .map_err(|_| ParseError::Xml)?
                        .eq_ignore_ascii_case("UTF-8")
                {
                    return Err(ParseError::Xml);
                }
            }
            Event::Eof => break,
            Event::Comment(_) | Event::PI(_) => {}
            Event::Empty(_) => return Err(ParseError::Xml),
        }
        buffer.clear();
    }
    if !root_seen || !stack.is_empty() {
        return Err(ParseError::Xml);
    }
    validate(&doc)?;
    Ok(doc)
}
