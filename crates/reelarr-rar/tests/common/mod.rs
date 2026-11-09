//! Test-only stored archive builder, independently written from SOURCES.md.
#![allow(dead_code)]
use blake2s_simd::blake2sp;
pub const SIG4: &[u8] = b"Rar!\x1a\x07\x00";
pub const SIG5: &[u8] = b"Rar!\x1a\x07\x01\x00";
pub fn vint(mut n: u64) -> Vec<u8> {
    let mut b = Vec::new();
    loop {
        let v = (n & 127) as u8;
        n >>= 7;
        b.push(v | if n > 0 { 128 } else { 0 });
        if n == 0 {
            break;
        }
    }
    b
}
pub fn block4(typ: u8, flags: u16, body: &[u8]) -> Vec<u8> {
    let mut h = vec![typ];
    h.extend(flags.to_le_bytes());
    h.extend(((7 + body.len()) as u16).to_le_bytes());
    h.extend(body);
    let mut out = (crc32fast::hash(&h) as u16).to_le_bytes().to_vec();
    out.extend(h);
    out
}
pub fn block5(typ: u64, flags: u64, body: &[u8], extra: &[u8], data: &[u8]) -> Vec<u8> {
    let flags = flags | if extra.is_empty() { 0 } else { 1 } | if data.is_empty() { 0 } else { 2 };
    let mut h = vint(typ);
    h.extend(vint(flags));
    if flags & 1 != 0 {
        h.extend(vint(extra.len() as u64));
    }
    if flags & 2 != 0 {
        h.extend(vint(data.len() as u64));
    }
    h.extend(body);
    h.extend(extra);
    let mut encoded = vint(h.len() as u64);
    encoded.extend(h);
    let mut out = crc32fast::hash(&encoded).to_le_bytes().to_vec();
    out.extend(encoded);
    out.extend(data);
    out
}
pub fn extra(typ: u64, data: &[u8]) -> Vec<u8> {
    let mut b = vint(typ);
    b.extend(data);
    let mut out = vint(b.len() as u64);
    out.extend(b);
    out
}
#[derive(Clone)]
pub struct Spec {
    pub name: String,
    pub data: Vec<u8>,
    pub size: u64,
    pub before: bool,
    pub after: bool,
    pub directory: bool,
    pub crc: Option<u32>,
    pub hash: Option<[u8; 32]>,
    pub method: u64,
    pub flags: u64,
    pub attr: u64,
    pub host: u64,
    pub extra: Vec<u8>,
    pub version: u8,
    pub high: bool,
}
impl Spec {
    pub fn new(name: &str, data: &[u8]) -> Self {
        Self {
            name: name.into(),
            data: data.into(),
            size: data.len() as u64,
            before: false,
            after: false,
            directory: false,
            crc: Some(crc32fast::hash(data)),
            hash: None,
            method: 0,
            flags: 0,
            attr: 0o100644,
            host: 1,
            extra: Vec::new(),
            version: 29,
            high: false,
        }
    }
    pub fn hash(mut self) -> Self {
        self.hash = Some(
            blake2sp::blake2sp(&self.data)
                .as_bytes()
                .try_into()
                .unwrap(),
        );
        self
    }
    pub fn header(&self, v5: bool) -> Vec<u8> {
        if v5 {
            let mut body = vint(
                u64::from(self.directory) | if self.crc.is_some() { 4 } else { 0 } | self.flags,
            );
            body.extend(vint(self.size));
            body.extend(vint(self.attr));
            if let Some(c) = self.crc {
                body.extend(c.to_le_bytes());
            }
            body.extend(vint(self.method));
            body.extend(vint(self.host));
            body.extend(vint(self.name.len() as u64));
            body.extend(self.name.as_bytes());
            let mut extras = self.extra.clone();
            if let Some(hash) = self.hash {
                let mut b = vec![0];
                b.extend(hash);
                extras.extend(extra(2, &b));
            }
            block5(
                2,
                if self.before { 8 } else { 0 } | if self.after { 16 } else { 0 },
                &body,
                &extras,
                &self.data,
            )
        } else {
            let flags = 0x8000
                | if self.before { 1 } else { 0 }
                | if self.after { 2 } else { 0 }
                | if self.directory { 0xe0 } else { 0 }
                | if self.high { 0x100 } else { 0 }
                | self.flags as u16;
            let mut b = (self.data.len() as u32).to_le_bytes().to_vec();
            b.extend((self.size as u32).to_le_bytes());
            b.push(if self.host == 1 { 3 } else { self.host as u8 });
            b.extend(self.crc.unwrap_or(0).to_le_bytes());
            b.extend([0; 4]);
            b.push(self.version);
            b.push(0x30 + self.method as u8);
            b.extend((self.name.len() as u16).to_le_bytes());
            b.extend((self.attr as u32).to_le_bytes());
            if self.high {
                b.extend([0; 4]);
                b.extend(((self.size >> 32) as u32).to_le_bytes());
            }
            b.extend(self.name.as_bytes());
            b.extend(&self.extra);
            let mut out = block4(0x74, flags, &b);
            out.extend(&self.data);
            out
        }
    }
}
pub fn archive(v5: bool, index: usize, count: usize, new_names: bool, entries: &[Spec]) -> Vec<u8> {
    let mut out = if v5 { SIG5.to_vec() } else { SIG4.to_vec() };
    if v5 {
        let mut b = vint(if count > 1 {
            if index == 0 { 1 } else { 3 }
        } else {
            0
        });
        if index > 0 {
            b.extend(vint(index as u64));
        }
        out.extend(block5(1, 0, &b, &[], &[]));
    } else {
        out.extend(block4(
            0x73,
            if count > 1 {
                1 | if new_names { 0x10 } else { 0 }
            } else {
                0
            } | if index == 0 { 0x100 } else { 0 },
            &[0; 6],
        ));
    }
    for e in entries {
        out.extend(e.header(v5));
    }
    if v5 {
        out.extend(block5(5, 0, &vint(u64::from(index + 1 < count)), &[], &[]));
    } else {
        let mut b = crc32fast::hash(&out).to_le_bytes().to_vec();
        b.extend((index as u16).to_le_bytes());
        out.extend(block4(0x7b, 0x400a | u16::from(index + 1 < count), &b));
    }
    out
}
