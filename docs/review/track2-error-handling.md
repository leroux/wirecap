# Track 2: Error Handling Review

**Crate**: `wirecap` (v0.1.0, ~960 lines)
**Date**: 2026-04-06
**Reviewer**: Claude Opus 4.6
**Scope**: All error types, propagation paths, panic vectors, and error swallowing

---

## 1. Error Type Inventory

The crate uses four distinct error representations with no unifying type:

| Error representation | Where used | Returned to caller? |
|---|---|---|
| `CaptureClosed` (custom struct) | `capture.rs:162-171` | Yes, from `Capture::log` |
| `std::io::Error` | `format.rs` (all read/write fns), `capture.rs` (`open_file`, `finalize_file`) | Partially -- propagated internally, but swallowed before reaching public API |
| `anyhow::Error` | `reader.rs:101`, `reader.rs:305` (`WcapReader::open`, `open_raw_wcap`) | Yes, from `WcapReader::open`; no, from `WcapTailer` (swallowed) |
| Ad-hoc `io::Error::new(InvalidData, ...)` | `format.rs:107-110`, `format.rs:115-118`, `format.rs:140-143`, `format.rs:225`, `format.rs:235` | Yes, tunneled through `std::io::Error` |

### Finding 1.1 [major] -- No unified `wirecap::Error` type

The library lacks a crate-level error enum. Format errors (bad magic, unsupported version, invalid dir byte, invalid UTF-8) are all shoehorned into `std::io::Error::new(ErrorKind::InvalidData, ...)`. This makes it impossible for callers to match on specific error conditions programmatically.

**Impact**: A consumer reading a wcap file cannot distinguish "bad magic bytes" from "unsupported version" from "corrupt dir byte" from an actual I/O failure. All arrive as `io::Error` with `ErrorKind::InvalidData`.

**Suggested fix**: Define a `wirecap::Error` enum:

```rust
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    BadMagic([u8; 4]),
    UnsupportedFileVersion(u8),
    UnsupportedRecordVersion(u8),
    InvalidDir(u8),
    InvalidUtf8(std::string::FromUtf8Error),
    /// Capture channel is closed (writer task is dead).
    Closed,
}
```

Implement `From<std::io::Error>`, `std::fmt::Display`, and `std::error::Error`. Replace `anyhow::Result` in the public API with `Result<T, wirecap::Error>`. This also eliminates the `anyhow` dependency entirely (see Finding 3.1).

---

## 2. Error Propagation Analysis

### 2.1 Write path (`Capture::log` -> `writer_task` -> `write_entry`)

```
Capture::log (capture.rs:148)
  -> mpsc::send  -- SendError mapped to CaptureClosed  [PROPAGATED]

writer_task (capture.rs:184)
  -> open_file   -- io::Error logged, dropped          [SWALLOWED]
  -> write_entry -- io::Error logged, dropped          [SWALLOWED]
  -> fsync       -- io::Error logged, dropped          [SWALLOWED]
  -> finalize    -- io::Error logged, dropped          [SWALLOWED]
  -> compress    -- io::Error logged, dropped          [SWALLOWED]
```

### 2.2 Read path (`WcapReader::open` -> `Iterator::next`)

```
WcapReader::open (reader.rs:101)
  -> File::open          -- io::Error via ?             [PROPAGATED as anyhow]
  -> zstd::Decoder::new  -- io::Error via ?             [PROPAGATED as anyhow]
  -> read_file_header    -- io::Error via ?             [PROPAGATED as anyhow]

WcapReader::next (reader.rs:131)
  -> read_record         -- io::Error logged as warn!   [SWALLOWED -> None]
```

### 2.3 Tail path (`WcapTailer::try_open` -> `read_batch`)

```
WcapTailer::try_open (reader.rs:183)
  -> find_active_file    -- io::Error logged as warn!   [SWALLOWED -> false]
  -> open_raw_wcap       -- anyhow::Error logged        [SWALLOWED -> false]

WcapTailer::read_batch (reader.rs:218)
  -> stream_position     -- io::Error                   [SWALLOWED -> break]
  -> read_record         -- io::Error seek-back, debug! [SWALLOWED -> break]
```

### Finding 2.1 [critical] -- `writer_task` is a silent black hole for I/O errors

The writer task (`capture.rs:184-293`) handles every I/O error by logging and continuing. No error is ever surfaced to the caller of `Capture::log`. The only signal a caller gets is `CaptureClosed`, which means the writer task panicked or exited -- but it never exits on I/O errors, it just keeps going with no file open.

Specifically at `capture.rs:259-268`, when `open_file` fails on rotation, the writer sets `current = None` and `continue`s. Subsequent `write_entry` calls silently drop entries because `current` is `None` (the `if let Some(of)` at `capture.rs:298` simply does nothing when there is no open file). There is no backoff, no retry, no signal to the caller. Entries disappear.

**Impact**: In production, if the filesystem fills up or becomes read-only, `Capture::log` continues to return `Ok(())` while every single entry is silently discarded. The only evidence is log lines and a `wirecap_write_errors_total` metric counter.

**Suggested fix**: At minimum, the writer task should surface errors through a shared `Arc<AtomicBool>` or `Arc<Mutex<Option<io::Error>>>` that `Capture::log` can check. Alternatively, the writer could exit on persistent I/O errors (e.g., N consecutive failures), causing the mpsc channel to close and `Capture::log` to return `CaptureClosed`. The current design of infinite silent retries is the worst option.

### Finding 2.2 [major] -- `write_entry` silently discards entries when `current` is `None`

At `capture.rs:297-310`, when there is no open file, `write_entry` does nothing. No metric is incremented, no warning is logged.

```rust
fn write_entry(current: &mut Option<OpenFile>, entry: &Entry, metrics: &MetricHandles) {
    if let Some(of) = current {
        // ...
    }
    // else: entry silently dropped, no metric, no log
}
```

**Suggested fix**: Add an `else` branch that increments `write_errors` and logs at `warn!` level. Better yet, propagate the error as described in 2.1.

---

## 3. `anyhow` Usage in a Library Crate

### Finding 3.1 [major] -- `anyhow::Result` in public API (`WcapReader::open`)

`reader.rs:101`:
```rust
pub fn open(path: &Path) -> anyhow::Result<Self> {
```

`anyhow::Error` is an opaque, type-erased error. Library consumers cannot:
- Match on specific error variants
- Convert to their own error type without `.downcast()`
- Use the `?` operator transparently with their own `Result<T, MyError>` (since there is no blanket `From<anyhow::Error>` impl)

The only error sources inside `WcapReader::open` are `std::io::Error` (from `File::open`, `zstd::Decoder::new`) and the ad-hoc `io::Error` from `read_file_header`. All of these are already `std::io::Error` -- `anyhow` adds nothing here.

**Impact**: Any consumer that defines its own error type must add an `anyhow::Error` variant or do `.downcast_ref::<io::Error>()` gymnastics. This is the anti-pattern described in the `anyhow` docs: "Use anyhow in application code, thiserror in library code."

**Suggested fix**: Change the return type to `Result<Self, std::io::Error>` (zero-cost fix, all inner errors are already `io::Error`) or `Result<Self, wirecap::Error>` if Finding 1.1 is addressed.

### Finding 3.2 [minor] -- `anyhow::Result` in private function `open_raw_wcap`

`reader.rs:305`:
```rust
fn open_raw_wcap(path: &Path) -> anyhow::Result<(BufReader<File>, String, String)> {
```

Same issue as 3.1 but internal. Every error source is `io::Error`. The `anyhow` wrapping is unnecessary -- `?` on `io::Error` already works with `io::Result`.

**Suggested fix**: Change to `std::io::Result<(BufReader<File>, String, String)>`. This also means `anyhow` can be removed from `[dependencies]` entirely (it appears nowhere else in the crate).

### Finding 3.3 [minor] -- `anyhow` dependency bloat

`Cargo.toml:11`: `anyhow = "1"` is a runtime dependency pulled into every consumer's build, yet it is used in exactly two functions, both of which can trivially use `std::io::Result` instead. Removing it shrinks the dependency tree for all downstream consumers.

---

## 4. Panic Vectors

### 4.1 `.expect()` calls in `format.rs` -- all proven safe

| Location | Expression | Verdict |
|---|---|---|
| `format.rs:152` | `rest[0..8].try_into().expect("8 bytes")` | **(a) Safe** -- slice from `[0u8; 14]`, bounds guaranteed |
| `format.rs:153` | `rest[8..12].try_into().expect("4 bytes")` | **(a) Safe** -- same |
| `format.rs:169` | `rest[0..8].try_into().expect("8 bytes")` | **(a) Safe** -- slice from `[0u8; 16]` |
| `format.rs:170` | `rest[8..10].try_into().expect("2 bytes")` | **(a) Safe** -- same |
| `format.rs:171` | `rest[10..14].try_into().expect("4 bytes")` | **(a) Safe** -- same |
| `format.rs:192` | `rest[0..8].try_into().expect("8 bytes")` | **(a) Safe** -- slice from `[0u8; 32]` |
| `format.rs:193` | `rest[8..16].try_into().expect("8 bytes")` | **(a) Safe** -- same |
| `format.rs:194` | `rest[16..24].try_into().expect("8 bytes")` | **(a) Safe** -- same |
| `format.rs:195` | `rest[24..26].try_into().expect("2 bytes")` | **(a) Safe** -- same |
| `format.rs:196` | `rest[26..30].try_into().expect("4 bytes")` | **(a) Safe** -- same |

All of these slice a fixed-size array where the index range is a compile-time constant. The `try_into()` cannot fail. These are the one correct use of `.expect()` -- the invariant is trivially verifiable. Could use `unwrap()` but `.expect()` is fine for documentation.

### 4.2 `.unwrap()` calls

| Location | Expression | Verdict |
|---|---|---|
| `capture.rs:340` | `active_path.to_str().unwrap_or_default()` | **(a) Safe** -- `unwrap_or_default`, not `unwrap` |
| `capture.rs:359` | `path.to_str().unwrap_or_default()` | **(a) Safe** -- same |
| `capture.rs:361` | `name.strip_suffix(".active").unwrap()` | **(c) Bug-adjacent** -- see Finding 4.1 |
| `reader.rs:33` | `n.to_str().unwrap_or("")` | **(a) Safe** -- fallback to empty string |
| `reader.rs:63` | `n.to_str().unwrap_or("")` | **(a) Safe** -- same |

### Finding 4.1 [minor] -- `unwrap()` in `recover_active_files` on guarded `strip_suffix`

`capture.rs:361`:
```rust
let recovered_path = PathBuf::from(name.strip_suffix(".active").unwrap())
    .with_extension("wcap.recovered");
```

This `unwrap()` is inside an `if name.ends_with(".wcap.active")` guard (line 360), so `strip_suffix(".active")` is guaranteed to succeed. However, the guard checks `".wcap.active"` while the strip is `".active"` -- this is safe (`.wcap.active` does end with `.active`), but the mismatch makes the invariant non-obvious. Would be clearer as:

```rust
if let Some(base) = name.strip_suffix(".wcap.active") {
    let recovered_path = PathBuf::from(format!("{base}.wcap.recovered"));
    // ...
}
```

### 4.3 `as` casts that could silently truncate

| Location | Expression | Risk |
|---|---|---|
| `format.rs:72` | `id_bytes.len() as u8` | **(b) Should be fallible** -- see Finding 4.2 |
| `format.rs:76` | `run_bytes.len() as u8` | **(b) Should be fallible** -- same |
| `format.rs:85` | `entry.meta.len() as u16` | **(b) Should be fallible** -- see Finding 4.3 |
| `format.rs:87` | `entry.payload.len() as u32` | **(b) Should be fallible** -- see Finding 4.3 |
| `format.rs:157` | `payload_len as usize` | **(a) Safe** -- `u32` always fits in `usize` on 32-bit+ |
| `capture.rs:301` | `n as u64` | **(a) Safe** -- `usize` to `u64` on 64-bit platform, and value represents bytes written in a single record |

### Finding 4.2 [major] -- Silent truncation of `instance_id` and `run_id` in file header

`format.rs:71-77`:
```rust
#[allow(clippy::cast_possible_truncation)]
w.write_all(&[id_bytes.len() as u8])?;
w.write_all(id_bytes)?;
// ...
#[allow(clippy::cast_possible_truncation)]
w.write_all(&[run_bytes.len() as u8])?;
```

If `instance_id` exceeds 255 bytes, the length prefix wraps around to `len % 256`, but the full string bytes are still written. The reader then reads `len % 256` bytes and returns a truncated/corrupt string, and subsequent reads will be misaligned, corrupting all following records.

The `#[allow(clippy::cast_possible_truncation)]` annotation explicitly silences the warning that would catch this.

**Impact**: Unlikely in practice (instance IDs are typically short), but this is a data corruption vector that would produce baffling, hard-to-diagnose failures.

**Suggested fix**: Add a length check that returns an error:
```rust
if id_bytes.len() > 255 {
    return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("instance_id too long: {} bytes (max 255)", id_bytes.len()),
    ));
}
```

### Finding 4.3 [minor] -- Silent truncation of `meta` and `payload` lengths

`format.rs:84-87`:
```rust
#[allow(clippy::cast_possible_truncation)]
let meta_len = entry.meta.len() as u16;
#[allow(clippy::cast_possible_truncation)]
let payload_len = entry.payload.len() as u32;
```

If `meta` exceeds 65,535 bytes or `payload` exceeds ~4 GB, the length field silently wraps. The same data corruption scenario as 4.2 applies: the full data is written but the length field is wrong, causing record misalignment.

**Impact**: `meta` exceeding 64 KB is conceivable (REST metadata with large URLs/params). Payload exceeding 4 GB is unlikely.

**Suggested fix**: Validate lengths before casting, return `io::Error::new(ErrorKind::InvalidInput, ...)` on overflow.

### 4.4 Allocation bombs from untrusted input

### Finding 4.4 [major] -- `read_record` trusts length fields for allocation

`format.rs:157`:
```rust
let mut payload = vec![0u8; payload_len as usize];
```

And similarly at `format.rs:175`, `format.rs:202`, `format.rs:209`. A corrupt or malicious wcap file can set `payload_len` to `u32::MAX` (4 GB), causing a 4 GB allocation attempt. For `meta_len` (u16), the max is only 64 KB, which is manageable. But `payload_len` as `u32::MAX` could OOM the process.

**Impact**: Reading untrusted wcap files (e.g., files from an unclean shutdown that have corrupt trailing bytes) can OOM the process.

**Suggested fix**: Add a `MAX_PAYLOAD_SIZE` constant (e.g., 64 MB) and validate before allocating:
```rust
if payload_len > MAX_PAYLOAD_SIZE {
    return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("payload too large: {payload_len} bytes (max {MAX_PAYLOAD_SIZE})"),
    ));
}
```

---

## 5. Silent Error Swallowing

### Finding 5.1 [critical] -- `WcapReader` iterator converts all errors to `None`

`reader.rs:131-148`:
```rust
impl Iterator for WcapReader {
    type Item = Entry;

    fn next(&mut self) -> Option<Entry> {
        if self.done { return None; }
        match format::read_record(&mut self.reader) {
            Ok(Some(entry)) => Some(entry),
            Ok(None) => { self.done = true; None }
            Err(e) => {
                warn!(error = %e, "wcap read error");
                self.done = true;
                None
            }
        }
    }
}
```

The `Iterator` trait forces `Item = Entry`, so there is no way to return errors. The iterator terminates on any error, silently discarding all remaining records in the file. The caller receives `None` and cannot distinguish "end of file" from "corrupt record at offset N".

**Impact**: A file with one corrupt byte at record 50 will silently return only 49 records with no indication that 1,000 more records followed. In a batch processing pipeline, this means silent data loss.

**Suggested fix**: Do not implement `Iterator` directly. Instead, provide:
```rust
pub fn next_record(&mut self) -> Result<Option<Entry>, wirecap::Error> {
    // ...
}
```

Optionally, add a convenience iterator that wraps this:
```rust
impl Iterator for WcapReader {
    type Item = Result<Entry, wirecap::Error>;
    // ...
}
```

This is the standard pattern used by `csv::Reader`, `serde_json::StreamDeserializer`, and `std::io::BufRead::lines()`.

### Finding 5.2 [major] -- `recover_active_files` errors are swallowed, no return value

`capture.rs:350-375`:

This function returns nothing (`-> ()`). All errors are logged and skipped:
- `read_dir` failure: silently returns (line 353)
- `to_str` failure: `unwrap_or_default` + the `ends_with` check fails, entry skipped
- `rename` failure: `error!` logged, `continue` (line 368-370)
- `compress_file` failure: logged internally, but caller never knows

This runs at writer startup (`capture.rs:194`). If recovery fails, the caller (writer task) has no idea. Failed-to-recover `.wcap.active` files will sit around forever.

**Suggested fix**: Return `io::Result<u32>` (count of recovered files). Let the caller decide whether to abort or continue.

### Finding 5.3 [major] -- `compress_file` errors are silently swallowed

`capture.rs:378-404`:

`compress_file` returns `()`. Compression failures are logged (`capture.rs:400`) but never returned. The function is called from:
1. Rotation path (`capture.rs:249-251`) -- via `spawn_blocking`, result ignored
2. Shutdown path (`capture.rs:282-284`) -- via `spawn_blocking`, JoinHandle result caught but inner compress error lost
3. Recovery path (`capture.rs:372`) -- result ignored

The raw file is preserved on failure (good), but the caller never knows compression failed.

**Suggested fix**: Return `io::Result<PathBuf>` (the zst path on success). At the rotation call site, log but continue. At the shutdown call site, propagate or log with higher severity.

### Finding 5.4 [minor] -- `fsync` errors in writer task are logged but not acted upon

`capture.rs:223-225` (tick-based fsync):
```rust
if let Err(e) = of.file.sync_data() {
    error!(error = %e, "fsync failed");
}
```

And `capture.rs:243-245` (rotation fsync), `capture.rs:276-278` (shutdown fsync). An fsync failure usually indicates a serious filesystem problem (disk full, device error). The writer just keeps going. Combined with Finding 2.1, this means disk errors are invisible to the application.

### Finding 5.5 [minor] -- `WcapTailer::read_batch` swallows `stream_position` error

`reader.rs:228-231`:
```rust
let pos = match reader.stream_position() {
    Ok(p) => p,
    Err(_) => break,
};
```

If `stream_position()` fails, the loop breaks with no indication of why. The returned `Vec<Entry>` may be partially filled, and the caller has no way to know whether the read stopped due to EOF, a partial record, or a seek error.

### Finding 5.6 [minor] -- `WcapTailer::try_open` and `check_rotation` swallow errors as `warn!`

`reader.rs:202-204` and `reader.rs:282-284`: Errors opening wcap files during tailing are logged at `warn!` level and treated as "no file available." This is acceptable for the polling-based tail pattern (retry later), but the caller (`try_open` returns `bool`, `check_rotation` returns nothing) has no way to distinguish "no file exists yet" from "file exists but is corrupt/unreadable."

---

## 6. Error Context

### Finding 6.1 [major] -- Format errors lack positional context

All format parsing errors (`format.rs`) report what went wrong but not where:

```rust
// format.rs:109
format!("bad magic: expected WCAP, got {magic:?}")

// format.rs:117
format!("unsupported file version: {}", ver[0])

// format.rs:141
format!("unsupported record version: {ver}")

// format.rs:225
format!("unknown dir: {b}")
```

None of these include:
- The file path being read
- The byte offset within the file
- The record number
- Which version parser was active

In production, when a `warn!` log says `"wcap read error"` with `"unsupported record version: 47"`, the operator cannot determine which file, at what offset, or whether it is corruption (partial write) vs. an actual version mismatch.

**Impact**: Debugging production failures requires reproducing the issue because the error messages are not self-sufficient.

**Suggested fix**: The format functions operate on `dyn Read` and cannot know the file path. Context should be added at call sites. A `wirecap::Error` enum (Finding 1.1) could carry an optional `path: Option<PathBuf>` and `offset: Option<u64>`. Alternatively, callers should use `.map_err()` to add context:

```rust
format::read_record(&mut reader)
    .map_err(|e| Error::ReadRecord {
        source: e,
        path: path.to_owned(),
        offset: reader.stream_position().unwrap_or(0),
    })
```

### Finding 6.2 [minor] -- `CaptureClosed` carries no diagnostic context

`capture.rs:162-171`:
```rust
pub struct CaptureClosed;

impl std::fmt::Display for CaptureClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "wirecap writer task is dead")
    }
}
```

When the caller receives `CaptureClosed`, they cannot determine:
- Which instance/run ID was affected
- Whether the writer exited cleanly (all senders dropped) or panicked
- What the last error was before the writer died

**Suggested fix**: Include optional context in `CaptureClosed`:
```rust
pub struct CaptureClosed {
    pub instance_id: Arc<str>,
    pub reason: Option<String>,
}
```

---

## 7. Test Coverage of Error Paths

### Tested error paths

| Error condition | Test | File:Line |
|---|---|---|
| Basic write-and-read roundtrip | `write_and_readback` | `tests/integration.rs:110` |
| File header roundtrip | `file_header_roundtrip` | `tests/integration.rs:141` |
| Channel and dir variant roundtrip | `all_channel_and_dir_variants` | `tests/integration.rs:179` |
| Size-based rotation | `size_based_rotation` | `tests/integration.rs:253` |
| Shutdown compresses final segment | `shutdown_compresses_final_segment` | `tests/integration.rs:316` |
| Shutdown compresses after rotation | `shutdown_compresses_all_segments_after_rotation` | `tests/integration.rs:354` |

### Finding 7.1 [major] -- No tests for any error/failure path

The test suite contains **zero** tests for error conditions. Every test is a happy-path test. Missing coverage:

**Format-level errors (none tested)**:
- Bad magic bytes (4 bytes that are not "WCAP")
- Unsupported file version (e.g., version 99)
- Unsupported record version (e.g., version 99)
- Invalid dir byte (e.g., value 5)
- Truncated file header (EOF mid-header)
- Truncated record header (EOF mid-record)
- Truncated record payload (header says 1000 bytes, only 50 available)
- Invalid UTF-8 in instance_id or run_id
- Zero-length file

**Reader-level errors (none tested)**:
- `WcapReader::open` on nonexistent file
- `WcapReader::open` on non-wcap file (e.g., a JPEG)
- `WcapReader::open` on corrupt zst file
- `WcapReader` iterator behavior on mid-file corruption (does it stop? does it skip?)

**Writer-level errors (none tested)**:
- `Capture::log` after writer task is dead (should return `CaptureClosed`)
- Write to a read-only directory
- Write when disk is full (harder to simulate, but possible with tmpfs + quota)
- Recovery of `.wcap.active` files from a simulated crash

**Tailer-level errors (none tested)**:
- `WcapTailer::try_open` on empty directory
- `WcapTailer::read_batch` on truncated/partial records
- `WcapTailer` rotation detection

**Suggested fix**: Add at minimum a `format_errors` test module that directly calls `read_file_header` and `read_record` on crafted byte buffers:

```rust
#[test]
fn bad_magic_is_rejected() {
    let data = b"NOPE\x01\x04test\x04abcd";
    let mut cursor = std::io::Cursor::new(data);
    let err = format::read_file_header(&mut cursor).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn unsupported_version_is_rejected() {
    let data = b"WCAP\x99\x04test\x04abcd";
    let mut cursor = std::io::Cursor::new(data);
    let err = format::read_file_header(&mut cursor).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}
```

---

## 8. `parse_dir` Error Type

### Finding 8.1 [minor] -- `parse_dir` returns `std::io::Error` for a domain validation failure

`format.rs:223-226`:
```rust
fn parse_dir(b: u8) -> std::io::Result<Dir> {
    Dir::from_u8(b).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("unknown dir: {b}"))
    })
}
```

This is noted in `TODO.md` (line 29) as "should be a dedicated error type."

**Analysis**: `parse_dir` is a private function called only from `read_record_v1/v2/v3`, which all return `std::io::Result`. As long as the return type of those functions is `io::Result`, `parse_dir` returning `io::Error` is the natural choice -- there is no type mismatch to fix.

The real fix is upstream: if `read_record` returns `Result<Option<Entry>, wirecap::Error>` (as suggested in Finding 1.1), then `parse_dir` should return `Result<Dir, wirecap::Error>` (or just `Option<Dir>` with the caller constructing the error). Fixing `parse_dir` in isolation without changing the error type hierarchy would be pointless churn.

**Suggested fix**: Address this as part of Finding 1.1. When `wirecap::Error` is introduced with an `InvalidDir(u8)` variant, `parse_dir` becomes:
```rust
fn parse_dir(b: u8) -> Result<Dir, Error> {
    Dir::from_u8(b).ok_or(Error::InvalidDir(b))
}
```

---

## 9. Additional Findings

### Finding 9.1 [minor] -- `discover_files` silently skips files with unreadable metadata

`reader.rs:35-39`:
```rust
if let Ok(meta) = entry.metadata() {
    if let Ok(modified) = meta.modified() {
        files.push((path, modified));
    }
}
```

Files where `metadata()` or `modified()` fails are silently excluded from the discovery result. The caller sees a shorter list with no indication that files were skipped.

### Finding 9.2 [minor] -- `find_active_file` swallows `read_dir` error as `None`

`reader.rs:53-58`:
```rust
let entries = match std::fs::read_dir(dir) {
    Ok(e) => e,
    Err(e) => {
        warn!(dir = %dir.display(), error = %e, "failed to read wcap directory");
        return None;
    }
};
```

The function returns `Option<PathBuf>`, so the caller cannot distinguish "no wcap files" from "directory unreadable." This is used by `WcapTailer::try_open`, which returns `bool` -- the information loss compounds.

### Finding 9.3 [nit] -- `finalize_file` fallback path is questionable

`capture.rs:339-347`:
```rust
fn finalize_file(active_path: &Path) -> std::io::Result<PathBuf> {
    let name = active_path.to_str().unwrap_or_default();
    let final_path = match name.strip_suffix(".active") {
        Some(base) if name.ends_with(".wcap.active") => PathBuf::from(base),
        _ => active_path.to_path_buf(),
    };
    fs::rename(active_path, &final_path)?;
    Ok(final_path)
}
```

In the fallback `_ =>` branch, the function renames the file to itself (`active_path` -> `active_path`), which is a no-op. This branch should probably return an error indicating an unexpected file extension rather than silently proceeding.

### Finding 9.4 [nit] -- `use-after-move` pattern in shutdown finalize

`capture.rs:246-256`:
```rust
if let Some(of) = current.take() {
    if let Err(e) = of.file.sync_data() {
        error!(error = %e, "fsync on rotation failed");
    }
    drop(of.file);                    // <-- `of.file` moved here
    match finalize_file(&of.path) {   // <-- `of.path` still valid (no move)
```

This compiles because `drop(of.file)` only moves `of.file`, not `of` itself. However, the explicit `drop` after the `sync_data` call is odd. `of.file` will be dropped anyway when `of` goes out of scope at the end of the `if let` block. The explicit `drop` is presumably to ensure the file handle is closed before the rename in `finalize_file`, which is correct intent, but should have a comment explaining why. More importantly, if `sync_data` fails, the function still proceeds to close the file and rename it -- the error is logged but the possibly-unsynced data is still finalized. Whether this is correct depends on the failure mode: a failed sync may mean data was lost, yet the file is renamed to look complete.

---

## Summary

### By severity

| Severity | Count | Findings |
|---|---|---|
| **Critical** | 2 | 2.1 (writer is a silent black hole), 5.1 (iterator swallows errors) |
| **Major** | 7 | 1.1 (no unified error type), 2.2 (silent discard on no file), 3.1 (anyhow in pub API), 4.2 (instance_id truncation), 4.4 (allocation bomb), 6.1 (no positional context), 7.1 (no error path tests) |
| **Minor** | 9 | 3.2, 3.3, 4.1, 4.3, 5.2, 5.3, 5.4, 5.5, 5.6, 6.2, 8.1, 9.1, 9.2 |
| **Nit** | 2 | 9.3, 9.4 |

### Recommended priority order

1. **Define `wirecap::Error`** (Finding 1.1) -- this is the foundation; most other fixes depend on it.
2. **Replace `anyhow` with `wirecap::Error` or `io::Error`** (Findings 3.1, 3.2) -- remove the dependency.
3. **Change `WcapReader` iterator to `Item = Result<Entry, Error>`** (Finding 5.1) -- stop swallowing read errors.
4. **Surface writer I/O errors to `Capture::log` callers** (Finding 2.1) -- the most impactful behavioral fix.
5. **Add length validation in `write_file_header` and `write_record`** (Findings 4.2, 4.3) -- prevent silent data corruption.
6. **Add max-size checks in `read_record`** (Finding 4.4) -- prevent OOM from corrupt files.
7. **Add error path tests** (Finding 7.1) -- validate all of the above.
8. **Add positional context to errors** (Finding 6.1) -- improve debuggability.
