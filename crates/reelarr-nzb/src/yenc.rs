//! Independent scalar decoder based solely on Juergen Helbing's yEnc 1.3 draft
//! (2002-03-05), mirrored at
//! <https://yence.sourceforge.net/docs/protocol/version1_3_draft.html>.
//!
//! Feed arbitrarily fragmented bytes to [`Decoder::push`], then call
//! [`Decoder::finish`] to require a complete trailer. Only a bounded encoded
//! line and decoded line are buffered. Output is provisional until finish
//! succeeds: callers must discard it on *any* error. CRC32 detects corruption,
//! not authenticity. Multipart whole-file CRC is returned for disk-order checking.
use crc32fast::Hasher;
use std::{
    collections::BTreeMap,
    io::{self, Write},
};
use thiserror::Error;

/// Explicit transport contract; never dot-unstuff an already unstuffed BODY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyEncoding {
    /// NNTP wire BODY lines, with leading dots doubled (terminator optional).
    DotStuffed,
    /// NNTP layer has already removed dot-stuffing and the terminator.
    Unstuffed,
}

/// Hard resource ceilings, independent of untrusted declarations.
#[derive(Debug, Clone, Copy)]
pub struct DecodeLimits {
    /// Maximum encoded physical line, including optional CR (default 8192).
    pub max_line_bytes: usize,
    /// Maximum total article bytes including ignored preamble/epilogue (128 MiB).
    pub max_input_bytes: u64,
    /// Maximum decoded part (64 MiB).
    pub max_part_bytes: u64,
    /// Maximum logical file length, also used by the assembler (1 TiB).
    pub max_file_bytes: u64,
}
impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_line_bytes: 8192,
            max_input_bytes: 128 * 1024 * 1024,
            max_part_bytes: 64 * 1024 * 1024,
            max_file_bytes: 1024 * 1024 * 1024 * 1024,
        }
    }
}

/// Fail-closed errors; input text and filenames are not included in messages.
#[derive(Debug, Error)]
pub enum DecodeError {
    /// Malformed headers, escapes, ordering or transport framing.
    #[error("invalid yEnc: {0}")]
    Invalid(&'static str),
    /// A resource ceiling was exceeded.
    #[error("yEnc limit exceeded: {0}")]
    Limit(&'static str),
    /// Header/trailer/range disagrees with decoded length.
    #[error("yEnc size mismatch")]
    Size,
    /// An advertised checksum did not match.
    #[error("yEnc checksum mismatch")]
    Crc,
    /// The stream ended without a complete attachment.
    #[error("truncated yEnc attachment")]
    Truncated,
    /// Sink/input failure.
    #[error("yEnc I/O failed")]
    Io(#[from] io::Error),
    /// Decoder cannot be reused after an error.
    #[error("yEnc decoder previously failed")]
    Failed,
}

/// Validated attachment metadata. Offsets are zero-based, half-open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartInfo {
    /// Safe, single-component UTF-8 filename (unsafe names are rejected).
    pub name: String,
    /// Typical encoded line length advertised by the sender.
    pub line: usize,
    /// Whole logical file size.
    pub file_size: u64,
    /// One-based part number; single-part articles use 1.
    pub part: u32,
    /// Optional advertised part count; single-part articles use Some(1).
    pub total: Option<u32>,
    /// Inclusive zero-based first byte offset.
    pub start: u64,
    /// Exclusive byte offset.
    pub end: u64,
    /// Whether this is a multipart article.
    pub multipart: bool,
    /// Part checksum supplied and verified, if any.
    pub part_crc32: Option<u32>,
    /// Whole-file checksum to verify after assembly (already verified for single).
    pub file_crc32: Option<u32>,
}

/// Reject traversal, platform-special characters, reserved device names and
/// ambiguous trailing dots/spaces instead of lossy replacement/collisions.
pub fn validate_filename(name: &str) -> Result<(), DecodeError> {
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let device = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|n| {
            matches!(
                n,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    if name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || name.trim() != name
        || name.ends_with('.')
        || device
        || name
            .chars()
            .any(|c| c.is_control() || "/\\<>:\"|?*".contains(c))
    {
        return Err(DecodeError::Invalid("unsafe filename"));
    }
    Ok(())
}

fn fields(line: &str) -> Result<BTreeMap<&str, &str>, DecodeError> {
    let mut out = BTreeMap::new();
    for field in line.split_ascii_whitespace() {
        let (k, v) = field
            .split_once('=')
            .ok_or(DecodeError::Invalid("header field"))?;
        if k.is_empty() || v.is_empty() || out.insert(k, v).is_some() {
            return Err(DecodeError::Invalid("duplicate or empty field"));
        }
    }
    Ok(out)
}
fn value<'a>(f: &BTreeMap<&str, &'a str>, key: &str) -> Result<&'a str, DecodeError> {
    f.get(key)
        .copied()
        .ok_or(DecodeError::Invalid("missing field"))
}
fn number<T: std::str::FromStr>(s: &str) -> Result<T, DecodeError> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(DecodeError::Invalid("number"));
    }
    s.parse()
        .map_err(|_| DecodeError::Invalid("number overflow"))
}
fn crc(f: &BTreeMap<&str, &str>, key: &str) -> Result<Option<u32>, DecodeError> {
    f.get(key)
        .map(|v| {
            if v.len() != 8 || !v.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(DecodeError::Invalid("checksum field"));
            }
            u32::from_str_radix(v, 16).map_err(|_| DecodeError::Invalid("checksum field"))
        })
        .transpose()
}

#[derive(PartialEq, Eq)]
enum State {
    Before,
    Range,
    Data,
    Done,
}

/// Incremental decoder. A failed instance is permanently poisoned. Encoded
/// escapes can cross push boundaries but may not cross physical lines (1.3).
/// A single attachment is decoded; prose outside its block is ignored.
///
/// ```
/// use reelarr_nzb::yenc::{BodyEncoding, DecodeLimits, Decoder};
/// # fn main() -> Result<(), reelarr_nzb::yenc::DecodeError> {
/// let mut decoder = Decoder::new(Vec::new(), DecodeLimits::default(), BodyEncoding::Unstuffed);
/// decoder.push(b"=ybegin line=128 size=1 name=example.bin\n")?;
/// decoder.push(b"*\n=yend size=1 crc32=d202ef8d\n")?;
/// let (bytes, info) = decoder.finish()?;
/// assert_eq!(bytes, [0]);
/// assert_eq!(info.file_size, 1);
/// # Ok(()) }
/// ```
pub struct Decoder<W> {
    sink: W,
    limits: DecodeLimits,
    encoding: BodyEncoding,
    line: Vec<u8>,
    state: State,
    info: Option<PartInfo>,
    hasher: Hasher,
    decoded: u64,
    input: u64,
    failed: bool,
    terminated: bool,
}
impl<W: Write> Decoder<W> {
    /// Create a decoder with explicit limits and NNTP framing mode.
    pub fn new(sink: W, limits: DecodeLimits, encoding: BodyEncoding) -> Self {
        Self {
            sink,
            limits,
            encoding,
            line: Vec::new(),
            state: State::Before,
            info: None,
            hasher: Hasher::new(),
            decoded: 0,
            input: 0,
            failed: false,
            terminated: false,
        }
    }
    /// Feed any chunk of article bytes; output remains untrusted until finish.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), DecodeError> {
        if self.failed {
            return Err(DecodeError::Failed);
        }
        let result = self.push_inner(bytes);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn push_inner(&mut self, bytes: &[u8]) -> Result<(), DecodeError> {
        self.input = self
            .input
            .checked_add(bytes.len() as u64)
            .filter(|n| *n <= self.limits.max_input_bytes)
            .ok_or(DecodeError::Limit("input"))?;
        for &b in bytes {
            if b == b'\n' {
                let mut line = std::mem::take(&mut self.line);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.process_line(&line)?;
                line.clear();
                self.line = line;
            } else {
                if self.line.len() >= self.limits.max_line_bytes {
                    return Err(DecodeError::Limit("line"));
                }
                self.line.push(b);
            }
        }
        Ok(())
    }
    /// Require valid trailer, checksums and sizes, returning the sink and metadata.
    /// A last control line without LF is accepted. No flush/durability is implied.
    pub fn finish(mut self) -> Result<(W, PartInfo), DecodeError> {
        if self.failed {
            return Err(DecodeError::Failed);
        }
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.process_line(&line)?;
        }
        if self.state != State::Done {
            return Err(DecodeError::Truncated);
        }
        Ok((self.sink, self.info.ok_or(DecodeError::Truncated)?))
    }
    fn process_line(&mut self, mut line: &[u8]) -> Result<(), DecodeError> {
        if self.terminated {
            return Err(DecodeError::Invalid("bytes after NNTP terminator"));
        }
        if self.encoding == BodyEncoding::DotStuffed && line.starts_with(b".") {
            if line == b"." {
                if self.state != State::Done {
                    return Err(DecodeError::Truncated);
                }
                self.terminated = true;
                return Ok(());
            }
            if !line.starts_with(b"..") {
                return Err(DecodeError::Invalid("dot stuffing"));
            }
            line = &line[1..];
        }
        match self.state {
            State::Before => {
                if let Some(header) = line.strip_prefix(b"=ybegin ") {
                    self.begin(header)?;
                }
            }
            State::Range => self.range(line)?,
            State::Data => {
                if let Some(trailer) = line.strip_prefix(b"=yend ") {
                    self.end(trailer)?;
                } else {
                    self.data(line)?;
                }
            }
            State::Done => {
                if line.starts_with(b"=ybegin ") {
                    return Err(DecodeError::Invalid("multiple attachments"));
                }
            }
        }
        Ok(())
    }
    fn begin(&mut self, header: &[u8]) -> Result<(), DecodeError> {
        let header =
            std::str::from_utf8(header).map_err(|_| DecodeError::Invalid("header encoding"))?;
        // name is last and may itself contain spaces or strings like size=.
        let (prefix, name) = header
            .split_once("name=")
            .ok_or(DecodeError::Invalid("missing name"))?;
        if !prefix.ends_with(' ') {
            return Err(DecodeError::Invalid("name field"));
        }
        let f = fields(prefix)?;
        let name = name.trim();
        validate_filename(name)?;
        let line: usize = number(value(&f, "line")?)?;
        if line == 0 || line >= self.limits.max_line_bytes {
            return Err(DecodeError::Limit("declared line"));
        }
        let file_size = number(value(&f, "size")?)?;
        if file_size > self.limits.max_file_bytes {
            return Err(DecodeError::Limit("file"));
        }
        let part = f.get("part").map(|s| number::<u32>(s)).transpose()?;
        let total = f.get("total").map(|s| number::<u32>(s)).transpose()?;
        if part == Some(0)
            || total == Some(0)
            || total.zip(part).is_some_and(|(t, p)| p > t)
            || (part.is_none() && total.is_some())
        {
            return Err(DecodeError::Invalid("part count"));
        }
        if part.is_none() && file_size > self.limits.max_part_bytes {
            return Err(DecodeError::Limit("part"));
        }
        self.info = Some(PartInfo {
            name: name.to_owned(),
            line,
            file_size,
            part: part.unwrap_or(1),
            total: if part.is_none() { Some(1) } else { total },
            start: 0,
            end: file_size,
            multipart: part.is_some(),
            part_crc32: None,
            file_crc32: None,
        });
        self.state = if part.is_some() {
            State::Range
        } else {
            State::Data
        };
        Ok(())
    }
    fn range(&mut self, line: &[u8]) -> Result<(), DecodeError> {
        let line = line
            .strip_prefix(b"=ypart ")
            .ok_or(DecodeError::Invalid("missing range"))?;
        let f =
            fields(std::str::from_utf8(line).map_err(|_| DecodeError::Invalid("range encoding"))?)?;
        let begin: u64 = number(value(&f, "begin")?)?;
        let end: u64 = number(value(&f, "end")?)?;
        let info = self
            .info
            .as_mut()
            .ok_or(DecodeError::Invalid("missing header"))?;
        if begin == 0 || begin > end || end > info.file_size {
            return Err(DecodeError::Size);
        }
        if end - (begin - 1) > self.limits.max_part_bytes {
            return Err(DecodeError::Limit("part"));
        }
        info.start = begin - 1;
        info.end = end;
        self.state = State::Data;
        Ok(())
    }
    fn data(&mut self, line: &[u8]) -> Result<(), DecodeError> {
        let info = self
            .info
            .as_ref()
            .ok_or(DecodeError::Invalid("missing header"))?;
        if line.len() > info.line.saturating_add(1) {
            return Err(DecodeError::Limit("data line"));
        }
        let mut decoded = Vec::with_capacity(line.len());
        let mut escaped = false;
        for &b in line {
            if matches!(b, b'\r' | b'\n') || (!escaped && b == 0) {
                return Err(DecodeError::Invalid("unescaped critical byte"));
            }
            if !escaped && b == b'=' {
                escaped = true;
                continue;
            }
            decoded.push(if escaped { b.wrapping_sub(64) } else { b }.wrapping_sub(42));
            escaped = false;
        }
        if escaped {
            return Err(DecodeError::Invalid("dangling escape"));
        }
        self.decoded = self
            .decoded
            .checked_add(decoded.len() as u64)
            .ok_or(DecodeError::Size)?;
        if self.decoded > self.limits.max_part_bytes {
            return Err(DecodeError::Limit("part"));
        }
        if self.decoded > info.end - info.start {
            return Err(DecodeError::Size);
        }
        self.hasher.update(&decoded);
        self.sink.write_all(&decoded)?;
        Ok(())
    }
    fn end(&mut self, trailer: &[u8]) -> Result<(), DecodeError> {
        let f = fields(
            std::str::from_utf8(trailer).map_err(|_| DecodeError::Invalid("trailer encoding"))?,
        )?;
        let info = self
            .info
            .as_mut()
            .ok_or(DecodeError::Invalid("missing header"))?;
        if number::<u64>(value(&f, "size")?)? != self.decoded
            || self.decoded != info.end - info.start
        {
            return Err(DecodeError::Size);
        }
        if info.multipart {
            if number::<u32>(value(&f, "part")?)? != info.part {
                return Err(DecodeError::Invalid("trailer part"));
            }
            if f.contains_key("total") && Some(number::<u32>(value(&f, "total")?)?) != info.total {
                return Err(DecodeError::Invalid("trailer total"));
            }
        } else if f.contains_key("part") || f.contains_key("total") {
            return Err(DecodeError::Invalid("unexpected part"));
        }
        info.part_crc32 = crc(&f, "pcrc32")?;
        info.file_crc32 = crc(&f, "crc32")?;
        if info.multipart && info.part_crc32.is_none() {
            return Err(DecodeError::Invalid("missing part CRC"));
        }
        let actual = self.hasher.clone().finalize();
        if info.part_crc32.is_some_and(|v| v != actual)
            || (!info.multipart && info.file_crc32.is_some_and(|v| v != actual))
        {
            return Err(DecodeError::Crc);
        }
        self.state = State::Done;
        Ok(())
    }
}
