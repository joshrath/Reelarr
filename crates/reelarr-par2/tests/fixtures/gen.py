#!/usr/bin/env python3
"""Generate reelarr-par2 test fixtures directly from the PAR2 2.0 specification.

Written for Reelarr from the public spec only (packet layout, MD5/CRC32,
GF(2^16) recovery); no par2 tool or third-party PAR2 code is used.

Usage: python3 gen.py   (writes into this directory; deterministic)
"""
import hashlib, os, struct, zlib

HERE = os.path.dirname(os.path.abspath(__file__))
MAGIC = b"PAR2\0PKT"
MAIN = b"PAR 2.0\0Main\0\0\0\0"
DESC = b"PAR 2.0\0FileDesc"
IFSC = b"PAR 2.0\0IFSC\0\0\0\0"
RECV = b"PAR 2.0\0RecvSlic"
CREATOR = b"PAR 2.0\0Creator\0"


def md5(b):
    return hashlib.md5(b).digest()


def pad4(b):
    return b + b"\0" * (-len(b) % 4)


def packet(set_id, kind, body):
    assert len(body) % 4 == 0
    length = 64 + len(body)
    tail = set_id + kind + body
    return MAGIC + struct.pack("<Q", length) + md5(tail) + tail


POLY = 0x1100B
ORDER = 65535


class Field:
    """GF(2^16) with generator 0x1100B, matching the PAR2 2.0 specification."""

    def __init__(self):
        self.exp = [0] * ORDER
        self.log = [0] * 65536
        x = 1
        for i in range(ORDER):
            self.exp[i] = x
            self.log[x] = i
            x <<= 1
            if x & 0x10000:
                x ^= POLY

    def mul(self, a, b):
        if a == 0 or b == 0:
            return 0
        return self.exp[(self.log[a] + self.log[b]) % ORDER]

    def pow(self, a, e):
        if a == 0:
            return 1 if e == 0 else 0
        return self.exp[(self.log[a] * (e % ORDER)) % ORDER]


def slice_constants(field):
    out = []
    for n in range(ORDER):
        if n % 3 and n % 5 and n % 17 and n % 257:
            out.append(field.pow(2, n))
    return out


FIELD = Field()
CONSTANTS = slice_constants(FIELD)


def encode_recovery(slices, exponent):
    slice_size = len(slices[0])
    out = bytearray(slice_size)
    factors = [FIELD.pow(CONSTANTS[i], exponent) for i in range(len(slices))]
    for off in range(0, slice_size, 2):
        acc = 0
        for i, s in enumerate(slices):
            word = s[off] | (s[off + 1] << 8)
            acc ^= FIELD.mul(word, factors[i])
        out[off] = acc & 0xFF
        out[off + 1] = (acc >> 8) & 0xFF
    return bytes(out)


def data(seed, n):
    """Deterministic pseudo-random bytes (xorshift32)."""
    x, out = seed or 1, bytearray()
    while len(out) < n:
        x ^= (x << 13) & 0xFFFFFFFF
        x ^= x >> 17
        x ^= (x << 5) & 0xFFFFFFFF
        out += struct.pack("<I", x)
    return bytes(out[:n])


def par2_set(prefix, files, slice_size, blocks):
    """files: list of (name, bytes). Returns {filename: bytes}."""
    entries = []
    for name, content in files:
        nb = name.encode()
        h16 = md5(content[:16384])
        fid = md5(h16 + struct.pack("<Q", len(content)) + nb)
        entries.append((fid, nb, content, h16))
    entries.sort(key=lambda e: e[0])  # spec: IDs sorted in the main packet
    main = struct.pack("<QI", slice_size, len(entries)) + b"".join(e[0] for e in entries)
    set_id = md5(main)
    meta = [packet(set_id, MAIN, main)]
    for fid, nb, content, h16 in entries:
        body = fid + md5(content) + h16 + struct.pack("<Q", len(content)) + pad4(nb)
        meta.append(packet(set_id, DESC, body))
        sums = b""
        for off in range(0, len(content), slice_size):
            s = content[off:off + slice_size].ljust(slice_size, b"\0")
            sums += md5(s) + struct.pack("<I", zlib.crc32(s) & 0xFFFFFFFF)
        meta.append(packet(set_id, IFSC, fid + sums))
    meta.append(packet(set_id, CREATOR, pad4(b"reelarr-par2 fixture generator")))
    index = b"".join(meta)
    out = {f"{prefix}.par2": index}
    slices = []
    for _fid, _nb, content, _h16 in entries:
        for off in range(0, len(content), slice_size):
            slices.append(content[off:off + slice_size].ljust(slice_size, b"\0"))
    recv = b"".join(
        packet(set_id, RECV, struct.pack("<I", e) + encode_recovery(slices, e))
        for e in range(blocks)
    )
    out[f"{prefix}.vol00+{blocks:02d}.par2"] = index + recv
    return out


def write(subdir, files, par2):
    d = os.path.join(HERE, subdir)
    os.makedirs(d, exist_ok=True)
    for name, content in list(files) + list(par2.items()):
        with open(os.path.join(d, name), "wb") as f:
            f.write(content)


# basic: two protected files, 4 KiB slices, 4 recovery blocks.
basic = [("movie.mkv", data(7, 100_000)), ("notes.nfo", data(9, 1234))]
write("basic", basic, par2_set("basic", basic, 4096, 4))

# other: an unrelated set for mixed-set tests.
other = [("other.bin", data(11, 9000))]
write("other", other, par2_set("other", other, 2048, 1))
