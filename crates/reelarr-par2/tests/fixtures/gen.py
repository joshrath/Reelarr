#!/usr/bin/env python3
"""Generate reelarr-par2 test fixtures directly from the PAR2 2.0 specification.

Written for Reelarr from the public spec only (packet layout, MD5/CRC32 rules);
no par2 tool or third-party PAR2 code is used. Recovery slice packets carry
placeholder data (a fixed byte pattern), not Reed-Solomon output: verification
only counts them. Real recovery data is needed for repair (#38).

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
    recv = b"".join(
        packet(set_id, RECV, struct.pack("<I", e) + bytes([e & 0xFF]) * slice_size)
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
