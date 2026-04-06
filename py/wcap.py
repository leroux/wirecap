"""Streaming reader for WCAP binary wirelog files.

Handles .wcap, .wcap.zst (zstandard-compressed), and all record versions
(v1, v2, v3). Streams records without loading the full file into memory.

Usage:
    from wcap import stream_records, Dir

    # Read everything
    for rec in stream_records("data/wirelog/foo.wcap.zst"):
        print(rec.ts, rec.src, rec.payload[:80])

    # Filtered read — src is a consumer-defined u8 channel tag
    for rec in stream_records("foo.wcap.zst", src=0, dir=Dir.IN):
        print(rec.ts, rec.payload[:80])
"""

import io
import struct
from dataclasses import dataclass
from enum import IntEnum
from pathlib import Path
from typing import Iterator


MAGIC = b"WCAP"
FILE_VERSION = 1

# Pre-compiled struct formats (after the 1-byte version is read separately)
_V1_HDR = struct.Struct("<QIBB")    # ts(8) + payload_len(4) + src(1) + dir(1) = 14
_V2_HDR = struct.Struct("<QHIBB")   # ts(8) + meta_len(2) + payload_len(4) + src(1) + dir(1) = 16
_V3_HDR = struct.Struct("<QQQHIBB") # ts(8) + mono_ns(8) + recv_seq(8) + meta_len(2) + payload_len(4) + src(1) + dir(1) = 32


class Dir(IntEnum):
    IN = 0
    OUT = 1


@dataclass(slots=True)
class FileHeader:
    instance_id: str
    run_id: str


@dataclass(slots=True)
class Record:
    ts: int            # nanoseconds since epoch
    mono_ns: int | None  # v3 only; None for v1/v2
    recv_seq: int | None # v3 only; None for v1/v2
    src: int
    dir: int
    meta: bytes
    payload: bytes


def _read_exact(f, n: int) -> bytes:
    buf = f.read(n)
    if len(buf) < n:
        raise EOFError
    return buf


def _read_lps(f) -> str:
    """Read a length-prefixed string (1-byte length)."""
    length = _read_exact(f, 1)[0]
    return _read_exact(f, length).decode("utf-8")


def _skip(f, n: int, seekable: bool):
    """Skip n bytes — seek if possible, otherwise read and discard."""
    if n <= 0:
        return
    if seekable:
        f.seek(n, 1)
    else:
        _read_exact(f, n)


def read_header(f) -> FileHeader:
    magic = _read_exact(f, 4)
    if magic != MAGIC:
        raise ValueError(f"bad magic: expected WCAP, got {magic!r}")
    ver = _read_exact(f, 1)[0]
    if ver != FILE_VERSION:
        raise ValueError(f"unsupported file version: {ver}")
    return FileHeader(instance_id=_read_lps(f), run_id=_read_lps(f))


def read_records(
    f,
    *,
    src: int | None = None,
    dir: int | None = None,
) -> Iterator[Record]:
    """Yield records from a file-like object positioned after the header.

    When src/dir filters are set, non-matching records skip the payload read
    entirely (seek on raw files, read-and-discard on zstd streams).
    """
    seekable = False
    try:
        seekable = f.seekable()
    except AttributeError:
        pass

    filtering = src is not None or dir is not None

    while True:
        ver_byte = f.read(1)
        if not ver_byte:
            return
        ver = ver_byte[0]

        if ver == 1:
            hdr = _read_exact(f, 14)
            ts, payload_len, rec_src, rec_dir = _V1_HDR.unpack(hdr)
            meta_len = 0
            mono_ns = None
            recv_seq = None
        elif ver == 2:
            hdr = _read_exact(f, 16)
            ts, meta_len, payload_len, rec_src, rec_dir = _V2_HDR.unpack(hdr)
            mono_ns = None
            recv_seq = None
        elif ver == 3:
            hdr = _read_exact(f, 32)
            ts, mono_ns, recv_seq, meta_len, payload_len, rec_src, rec_dir = _V3_HDR.unpack(hdr)
        else:
            raise ValueError(f"unsupported record version: {ver}")

        # Filter check — skip body bytes if record doesn't match
        if filtering and ((src is not None and rec_src != src) or (dir is not None and rec_dir != dir)):
            _skip(f, meta_len + payload_len, seekable)
            continue

        meta = _read_exact(f, meta_len) if meta_len > 0 else b""
        payload = _read_exact(f, payload_len)
        yield Record(
            ts=ts,
            mono_ns=mono_ns,
            recv_seq=recv_seq,
            src=rec_src,
            dir=rec_dir,
            meta=meta,
            payload=payload,
        )


def stream_records(
    path,
    *,
    src: int | None = None,
    dir: int | None = None,
) -> Iterator[Record]:
    """Stream records from a .wcap or .wcap.zst file.

    Optional src/dir filters skip non-matching records without reading their payload.
    """
    path = Path(path)
    if path.suffix == ".zst" or ".wcap.zst" in path.name:
        import zstandard
        with open(path, "rb") as raw:
            dctx = zstandard.ZstdDecompressor()
            with dctx.stream_reader(raw) as reader:
                f = io.BufferedReader(reader, buffer_size=1 << 20)
                read_header(f)
                yield from read_records(f, src=src, dir=dir)
    else:
        with open(path, "rb") as f:
            read_header(f)
            yield from read_records(f, src=src, dir=dir)
