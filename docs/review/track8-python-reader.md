# Track 8: Python Reader Review

Review of `py/wcap.py` (159 lines) for correctness, spec compliance, parity
with the Rust reader (`src/format.rs`, `src/reader.rs`), and code quality.

---

## Finding 1: No v3 record support — reader is broken for current files [critical]

**Location:** `py/wcap.py` lines 123-124

The `read_records` function handles record versions 1 and 2 but silently
returns on any other version:

```python
else:
    return
```

The Rust writer (`format.rs` line 88) writes `RECORD_VERSION = 3` for all new
records. This means the Python reader **silently yields zero records** from any
file produced by the current Rust writer. It encounters the version byte `3`,
hits the `else` branch, and terminates the generator without error.

This is the highest-severity issue: the reader appears to work (no exception)
but produces no output for modern files.

**Fix required:** Add a v3 parsing branch. The v3 header (after the version
byte) is 32 bytes:

```
ts(8) + mono_ns(8) + recv_seq(8) + meta_len(2) + payload_len(4) + src(1) + dir(1) = 32
```

The `Record` dataclass also needs `mono_ns` and `recv_seq` fields (see
Finding 3).

**Behavior comparison with Rust:** The Rust `read_record` function
(`format.rs` line 140) returns an `Err` with `InvalidData` for unknown
versions. The Python reader silently returns — it should at minimum raise an
exception for unknown versions rather than silently ending iteration.

---

## Finding 2: v1 record header size disagrees with SPEC [major]

**Location:** `py/wcap.py` lines 30, 113

The Python reader reads 14 bytes for the v1 header (after the version byte):

```python
_V1_HDR = struct.Struct("<QIBB")   # ts(8) + payload_len(4) + src(1) + dir(1) = 14
```

The SPEC.md says the v1 record header is **15 bytes total** (including the
version byte), meaning 14 bytes after the version byte:

| Offset | Size | Field |
|--------|------|-------|
| 0 | 1 | version |
| 1 | 8 | ts |
| 9 | 4 | payload_len |
| 13 | 1 | src |
| 14 | 1 | dir |

The Rust reader (`format.rs` line 149) also reads 14 bytes after the version
byte. So the Python code **matches the Rust implementation** and the spec
(15 total - 1 version byte = 14 remaining bytes). The struct format `<QIBB`
decodes to `u64 + u32 + u8 + u8 = 14 bytes`. This is correct.

Similarly for v2: SPEC says 17 total, minus 1 version byte = 16 remaining.
`<QHIBB` = `u64 + u16 + u32 + u8 + u8 = 16 bytes`. Matches the Rust reader
at `format.rs` line 165. Also correct.

**Conclusion:** Both v1 and v2 header parsing is correct and matches the spec
and Rust implementation. No bug here, despite the slightly confusing comments
(the comments document the size without the version byte, which is accurate for
the struct since the version byte is read separately).

---

## Finding 3: `Record` dataclass missing `mono_ns` and `recv_seq` fields [major]

**Location:** `py/wcap.py` lines 45-51

The `Record` dataclass has five fields:

```python
@dataclass(slots=True)
class Record:
    ts: int
    src: int
    dir: int
    meta: bytes
    payload: bytes
```

The Rust `Entry` struct (`format.rs` lines 40-60) has seven fields, including:

- `mono_ns: Option<u64>` — monotonic timestamp for ordering
- `recv_seq: Option<u64>` — process-global receive sequence number

These are critical for v3 records where the data is present on the wire. Even
when adding v3 support, there would be no way to expose these values to callers
without updating the dataclass.

**Fix required:** Add `mono_ns: int | None = None` and
`recv_seq: int | None = None` to the `Record` dataclass. For v1 and v2
records, these should be `None`.

---

## Finding 4: `_skip` ignores short reads on non-seekable streams [minor]

**Location:** `py/wcap.py` lines 67-74

```python
def _skip(f, n: int, seekable: bool):
    if n <= 0:
        return
    if seekable:
        f.seek(n, 1)
    else:
        f.read(n)
```

On the non-seekable path, `f.read(n)` may return fewer than `n` bytes (e.g.,
the zstd stream has less data buffered). The return value is discarded without
checking length. If this happens partway through a record's body, the next
iteration will misparse the following bytes as a version byte, leading to
either a silent `return` (unknown version) or corrupt field values.

The seekable path has a subtler issue: `f.seek(n, 1)` can seek past the end
of the file without error on most platforms, which would cause the next
`f.read(1)` to return empty bytes and cleanly end iteration. This is benign.

**Fix:** Use `_read_exact(f, n)` for the non-seekable case, or at least check
the return length. Note that `_read_exact` already exists and raises `EOFError`
on short reads.

---

## Finding 5: Short reads on payload not detected [minor]

**Location:** `py/wcap.py` lines 131-132

```python
meta = f.read(meta_len) if meta_len > 0 else b""
payload = f.read(payload_len)
```

Neither `meta` nor `payload` reads check for short returns. If the file is
truncated mid-record, `f.read(payload_len)` will return fewer bytes than
requested. The `Record` will be yielded with a truncated `payload` and no
indication of data loss.

By contrast, the Rust reader uses `read_exact` everywhere (`format.rs` lines
158, 176, 209), which returns an error on short reads.

**Fix:** Use `_read_exact(f, meta_len)` and `_read_exact(f, payload_len)`.
The function already exists and does the right thing.

---

## Finding 6: Unknown record version silently terminates iteration [minor]

**Location:** `py/wcap.py` lines 123-124

```python
else:
    return
```

When an unknown record version is encountered, the generator silently
terminates. No exception, no warning, no way for the caller to distinguish
"file ended normally" from "file contained data we couldn't parse."

The Rust reader returns `Err(InvalidData, "unsupported record version: {ver}")`
in the equivalent case (`format.rs` line 142). This is a meaningful difference
in behavior — Rust callers know something went wrong.

**Fix:** Raise `ValueError(f"unsupported record version: {ver}")`. Callers who
want to skip unknown versions can catch the exception.

---

## Finding 7: `FileHeader` is not exposed to `stream_records` callers [minor]

**Location:** `py/wcap.py` lines 153, 157

`stream_records` calls `read_header(f)` but discards the result. Callers
cannot access the `instance_id` or `run_id` from the file header.

The Rust `WcapReader` exposes `instance_id` and `run_id` as public fields
(`reader.rs` lines 93-94). The Rust `WcapTailer` also exposes them
(`reader.rs` lines 166-167).

This matters for tools that process multiple files and need to identify or
group them by instance/run.

**Potential fix:** Either return a `(FileHeader, Iterator[Record])` tuple, or
add a separate `open_wcap` function that returns a reader object with header
access. The current function signature makes this a breaking change to add
later.

---

## Finding 8: `Dir` enum not validated on record read [nit]

**Location:** `py/wcap.py` line 133

The `rec_dir` value from the struct unpack is stored as a plain `int` in the
`Record`. The `Dir` IntEnum exists but is never used for validation during
reading. A corrupt `dir` value of `5` would be silently stored.

The Rust reader validates via `parse_dir()` (`format.rs` line 223) and returns
an error for unknown direction values.

**Impact:** Low. The `Dir` enum is only used for filtering (`dir=Dir.IN`), and
comparison would still work correctly since `Dir.IN == 0`. But storing
unvalidated values is inconsistent with the Rust reader's behavior.

---

## Finding 9: Missing Rust reader features — no `WcapTailer` or discovery [minor]

**Location:** Rust `reader.rs` (entire file)

The Rust crate provides substantial functionality beyond basic record reading
that has no Python equivalent:

| Rust feature | Python equivalent |
|---|---|
| `WcapReader` (iterator over records) | `stream_records` (partial equivalent) |
| `WcapTailer` (follow live files) | None |
| `discover_files` (find wcap files in dir) | None |
| `find_active_file` (find current active) | None |
| `.wcap.active` file handling | None |
| `.wcap.recovered` file handling | None |
| Partial-record rewind on EOF | None |
| File rotation detection | None |

The Python reader only handles `.wcap` and `.wcap.zst` files. It cannot tail
active files, discover files in a directory, or handle the `.wcap.active` and
`.wcap.recovered` extensions.

Whether these are needed in Python depends on use cases. If the Python reader
is only used for offline analysis of sealed/compressed files, the current scope
is adequate. If it needs to support live monitoring, significant work is needed.

---

## Finding 10: `.wcap.zst` detection is redundant [nit]

**Location:** `py/wcap.py` line 147

```python
if path.suffix == ".zst" or ".wcap.zst" in path.name:
```

`path.suffix` returns the final extension, so for a file named `foo.wcap.zst`,
`path.suffix == ".zst"` is `True`. The second condition `".wcap.zst" in
path.name` is therefore always redundant when the first is true for wcap
files. The only case the second condition would catch that the first does not
is a file named something like `foo.wcap.zst.bak` — but that would also be
wrong since the file is no longer zst-compressed at the outer layer.

The check should simply be:

```python
if path.suffix == ".zst":
```

Or, for maximum clarity:

```python
if path.name.endswith(".wcap.zst"):
```

---

## Finding 11: Zstd import is conditional but not error-handled [nit]

**Location:** `py/wcap.py` line 148

```python
import zstandard
```

The `zstandard` package is imported inline only when needed, which is good for
making the module usable without it for raw `.wcap` files. However, if the
import fails (package not installed), the user gets a bare `ModuleNotFoundError`
with no guidance.

**Suggestion:** Wrap in a try/except with a more helpful message, or document
the dependency requirement clearly.

---

## Finding 12: Buffer size for zstd is reasonable [nit]

**Location:** `py/wcap.py` line 152

```python
f = io.BufferedReader(reader, buffer_size=1 << 20)
```

The 1 MB buffer for the zstd `BufferedReader` is appropriate. Zstd frames are
typically 128 KB, so a 1 MB buffer amortizes syscalls well without excessive
memory use. No issue here.

---

## Summary

| Severity | Count | Key items |
|----------|-------|-----------|
| Critical | 1 | No v3 support — reader yields zero records for current files |
| Major | 2 | Missing `mono_ns`/`recv_seq` fields; v1/v2 sizes verified correct |
| Minor | 4 | Silent truncation on short reads; unknown version silently exits; no header exposure; no tailing |
| Nit | 4 | Dir not validated; redundant zst check; zstandard import; buffer size OK |

The Python reader is well-structured, has good performance characteristics
(pre-compiled structs, seekable skip optimization, streaming without full-file
load), and handles zstd transparently. However, it is **effectively non-functional
for current files** due to the missing v3 support. The silent failure mode
(no exception, just zero records) makes this particularly dangerous — callers
may not realize they are getting no data.

Priority fixes: (1) add v3 record parsing, (2) add `mono_ns`/`recv_seq` to
`Record`, (3) use `_read_exact` for meta/payload reads, (4) raise on unknown
record version instead of silent return.
