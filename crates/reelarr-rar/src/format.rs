//! Container parsing from the documentation listed in SOURCES.md; no decompressor.
use crate::{Error, Limits, Result};
use std::io::Read;

pub(crate) const RAR4: &[u8] = b"Rar!\x1a\x07\x00";
pub(crate) const RAR5: &[u8] = b"Rar!\x1a\x07\x01\x00";

pub(crate) struct Fields<'a> {
    pub data: &'a [u8],
}
impl<'a> Fields<'a> {
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.data.len() {
            return Err(Error::Truncated);
        }
        let (a, b) = self.data.split_at(n);
        self.data = b;
        Ok(a)
    }
    pub fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn vint(&mut self) -> Result<u64> {
        let mut n = 0;
        for i in 0..10 {
            let b = self.byte()?;
            if i == 9 && b > 1 {
                return Err(Error::Corrupt("vint overflow"));
            }
            n |= u64::from(b & 127) << (i * 7);
            if b & 128 == 0 {
                return Ok(n);
            }
        }
        Err(Error::Corrupt("unterminated vint"))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub name: String,
    pub packed: u64,
    pub size: u64,
    pub directory: bool,
    pub before: bool,
    pub after: bool,
    pub crc: Option<u32>,
    pub hash: Option<[u8; 32]>,
    pub identity: (u64, u64, u64),
}
pub(crate) enum Block {
    Main {
        volume: bool,
        number: u64,
        new_names: Option<bool>,
    },
    File(Entry),
    Skip(u64),
    End {
        more: bool,
        crc: Option<u32>,
        number: Option<u64>,
    },
}
fn unsupported(reason: &'static str) -> Error {
    Error::Unsupported { reason }
}
fn name(bytes: &[u8], ascii: bool) -> Result<String> {
    if ascii && !bytes.is_ascii() {
        return Err(unsupported("legacy RAR4 name encoding"));
    }
    let s = std::str::from_utf8(bytes).map_err(|_| unsupported("filename encoding"))?;
    if s.contains('\u{fffe}') {
        return Err(unsupported("mapped non-Unicode filename"));
    }
    Ok(s.into())
}
fn object(host: u64, attr: u64, directory: bool, unix: u64, windows_max: u64) -> Result<()> {
    if host == unix {
        let kind = attr & 0o170000;
        if kind != 0 && kind != if directory { 0o040000 } else { 0o100000 } {
            return Err(unsupported("link, device or inconsistent Unix object"));
        }
    } else if host <= windows_max {
        if attr & 0x440 != 0 {
            return Err(unsupported("device or reparse point"));
        }
        if (attr & 0x10 != 0) != directory {
            return Err(Error::Corrupt("directory attributes"));
        }
    } else {
        return Err(unsupported("host OS"));
    }
    Ok(())
}

pub(crate) fn read4(r: &mut impl Read, limits: &Limits) -> Result<Block> {
    let mut prefix = [0; 7];
    r.read_exact(&mut prefix)?;
    let mut f = Fields { data: &prefix };
    let crc = f.u16()?;
    let typ = f.byte()?;
    let flags = f.u16()?;
    let n = usize::from(f.u16()?);
    if n < 7 {
        return Err(Error::Corrupt("short RAR4 header"));
    }
    if n > limits.max_header_bytes {
        return Err(Error::Limit("header bytes"));
    }
    let mut header = vec![0; n];
    header[..7].copy_from_slice(&prefix);
    r.read_exact(&mut header[7..])?;
    if crc32fast::hash(&header[2..]) as u16 != crc {
        return Err(Error::Checksum("RAR4 header"));
    }
    let mut f = Fields { data: &header[7..] };
    match typ {
        0x73 => {
            if flags & (0x2 | 0x8 | 0x80) != 0 {
                return Err(unsupported(
                    "RAR4 embedded comment, solid or encrypted archive",
                ));
            }
            if flags & !0x157 != 0 {
                return Err(unsupported("RAR4 main flags"));
            }
            f.take(6)?;
            Ok(Block::Main {
                volume: flags & 1 != 0,
                number: if flags & 0x100 != 0 { 0 } else { u64::MAX },
                new_names: Some(flags & 0x10 != 0),
            })
        }
        0x74 => {
            if flags & (4 | 8 | 0x10 | 0x400 | 0x800) != 0 {
                return Err(unsupported(
                    "RAR4 encryption, solid, comments or file versions",
                ));
            }
            if flags & 0x8000 == 0 || flags & !0x93e3 != 0 {
                return Err(unsupported("RAR4 file flags"));
            }
            let mut packed = u64::from(f.u32()?);
            let mut size = u64::from(f.u32()?);
            let host = u64::from(f.byte()?);
            let crc = f.u32()?;
            f.take(4)?;
            let version = f.byte()?;
            if !(20..=50).contains(&version) {
                return Err(unsupported("RAR4 unpack version"));
            }
            if f.byte()? != 0x30 {
                return Err(unsupported("compressed RAR4 entry"));
            }
            let n = usize::from(f.u16()?);
            let attr = u64::from(f.u32()?);
            if flags & 0x100 != 0 {
                packed |= u64::from(f.u32()?) << 32;
                size |= u64::from(f.u32()?) << 32;
            }
            let raw = f.take(n)?;
            if flags & 0x200 != 0 && raw.contains(&0) {
                return Err(unsupported("encoded RAR4 Unicode filename"));
            }
            let name = name(raw, flags & 0x200 == 0)?.replace('\\', "/");
            let directory = flags & 0xe0 == 0xe0;
            object(host, attr, directory, 3, 2)?;
            // Extended times and bounded optional metadata are intentionally not restored.
            Ok(Block::File(Entry {
                name,
                packed,
                size,
                directory,
                before: flags & 1 != 0,
                after: flags & 2 != 0,
                crc: Some(crc),
                hash: None,
                identity: (host, attr, u64::from(version)),
            }))
        }
        0x7b => {
            if flags & !0x400f != 0 {
                return Err(unsupported("RAR4 end flags"));
            }
            let crc = if flags & 2 != 0 { Some(f.u32()?) } else { None };
            let number = if flags & 8 != 0 {
                Some(u64::from(f.u16()?))
            } else {
                None
            };
            Ok(Block::End {
                more: flags & 1 != 0,
                crc,
                number,
            })
        }
        0x75..=0x7a => Ok(Block::Skip(if flags & 0x8000 != 0 {
            u64::from(f.u32()?)
        } else {
            0
        })),
        _ => Err(unsupported("RAR4 block type")),
    }
}

pub(crate) fn read5(r: &mut impl Read, limits: &Limits) -> Result<Block> {
    let mut checksum = [0; 4];
    r.read_exact(&mut checksum)?;
    let mut encoded = Vec::with_capacity(3);
    for _ in 0..3 {
        let mut b = [0];
        r.read_exact(&mut b)?;
        encoded.push(b[0]);
        if b[0] & 128 == 0 {
            break;
        }
    }
    if encoded.last().is_none_or(|b| b & 128 != 0) {
        return Err(Error::Corrupt("RAR5 header size vint"));
    }
    let n = Fields { data: &encoded }.vint()?;
    if n > limits.max_header_bytes as u64 {
        return Err(Error::Limit("header bytes"));
    }
    let start = encoded.len();
    encoded.resize(start + n as usize, 0);
    r.read_exact(&mut encoded[start..])?;
    if crc32fast::hash(&encoded) != u32::from_le_bytes(checksum) {
        return Err(Error::Checksum("RAR5 header"));
    }
    let mut f = Fields {
        data: &encoded[start..],
    };
    let typ = f.vint()?;
    let flags = f.vint()?;
    if flags & !0x7f != 0 {
        return Err(unsupported("RAR5 common flags"));
    }
    let extra = if flags & 1 != 0 { f.vint()? } else { 0 };
    let packed = if flags & 2 != 0 { f.vint()? } else { 0 };
    if extra > f.data.len() as u64 {
        return Err(Error::Truncated);
    }
    let split = f.data.len() - extra as usize;
    let extras = &f.data[split..];
    f.data = &f.data[..split];
    match typ {
        1 => {
            if flags & !5 != 0 {
                return Err(Error::Corrupt("RAR5 main common flags"));
            }
            let archive = f.vint()?;
            if archive & !0x1b != 0 {
                return Err(unsupported("solid RAR5 archive or archive flags"));
            }
            if archive & 2 != 0 && archive & 1 == 0 {
                return Err(Error::Corrupt("volume number without volume flag"));
            }
            let number = if archive & 2 != 0 { f.vint()? } else { 0 };
            validate_extras(extras, false)?;
            Ok(Block::Main {
                volume: archive & 1 != 0,
                number,
                new_names: None,
            })
        }
        2 => {
            if flags & !0x1f != 0 {
                return Err(unsupported("RAR5 dependent file"));
            }
            let file_flags = f.vint()?;
            if file_flags & !7 != 0 {
                return Err(unsupported("unknown size or RAR5 file flags"));
            }
            let size = f.vint()?;
            let attr = f.vint()?;
            if file_flags & 2 != 0 {
                f.take(4)?;
            }
            let crc = if file_flags & 4 != 0 {
                Some(f.u32()?)
            } else {
                None
            };
            let compression = f.vint()?;
            if compression & 0x3f > 1 || compression & 0x3c0 != 0 || compression & !0x1fffff != 0 {
                return Err(unsupported("RAR5 compression, solid or algorithm version"));
            }
            let host = f.vint()?;
            let n = f.vint()?;
            if n > f.data.len() as u64 {
                return Err(Error::Truncated);
            }
            let name = name(f.take(n as usize)?, false)?;
            let directory = file_flags & 1 != 0;
            object(host, attr, directory, 1, 0)?;
            if !f.data.is_empty() {
                return Err(unsupported("RAR5 trailing file fields"));
            }
            let hash = validate_extras(extras, true)?;
            Ok(Block::File(Entry {
                name,
                packed,
                size,
                directory,
                before: flags & 8 != 0,
                after: flags & 16 != 0,
                crc,
                hash,
                identity: (host, attr, compression),
            }))
        }
        // Service streams (including QO) are never used to list or extract files.
        3 => {
            validate_extras(extras, false)?;
            Ok(Block::Skip(packed))
        }
        4 => Err(unsupported("encrypted RAR5 headers")),
        5 => {
            if flags & !4 != 0 || packed != 0 {
                return Err(Error::Corrupt("RAR5 end common flags"));
            }
            let end = f.vint()?;
            if end & !1 != 0 {
                return Err(unsupported("RAR5 end flags"));
            }
            Ok(Block::End {
                more: end & 1 != 0,
                crc: None,
                number: None,
            })
        }
        _ if flags & 4 != 0 => Ok(Block::Skip(packed)),
        _ => Err(unsupported("RAR5 required block type")),
    }
}
fn validate_extras(data: &[u8], file: bool) -> Result<Option<[u8; 32]>> {
    let mut f = Fields { data };
    let mut hash = None;
    while !f.data.is_empty() {
        let n = f.vint()?;
        if n == 0 || n > f.data.len() as u64 {
            return Err(Error::Corrupt("extra area size"));
        }
        let mut e = Fields {
            data: f.take(n as usize)?,
        };
        let typ = e.vint()?;
        if file {
            match typ {
                1 => return Err(unsupported("encrypted RAR5 file")),
                2 => {
                    if hash.is_some() || e.vint()? != 0 {
                        return Err(unsupported("unknown or duplicate file hash"));
                    }
                    hash = Some(e.take(32)?.try_into().unwrap());
                    if !e.data.is_empty() {
                        return Err(Error::Corrupt("hash record size"));
                    }
                }
                4 | 5 => return Err(unsupported("file version or filesystem redirection")),
                _ => {}
            }
        }
    }
    Ok(hash)
}
