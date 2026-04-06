# Track 3: Safety & Correctness Review

**Crate**: `wirecap` (v0.1.0, ~960 lines)
**Date**: 2026-04-06
**Reviewer**: Claude Opus 4.6
**Scope**: Data integrity, concurrency correctness, file lifecycle, format parsing edge cases, filesystem edge cases, clock/timing, drop/shutdown, spec compliance

**Prior findings referenced but not duplicated**: Track 2 covered silent `as u8`/`as u16`/`as u32` truncation in write_file_header and write_record (T2-4.2, T2-4.3), allocation bombs from untrusted payload_len (T2-4.4), and the use-after-move pattern in shutdown finalize (T2-5.x). This track goes deeper on correctness dimensions those findings did not cover.

---

## 1. Data Integrity

### Finding 1.1 [critical] -- Use-after-move of `OpenFile` in rotation path

`capture.rs:242-256`:
```rust
if let Some(of) = current.take() {
    if let Err(e) = of.file.sync_data() {       // borrows of.file
        error!(error = %e, "fsync on rotation failed");
    }
    drop(of.file);                                // MOVES of.file
    match finalize_file(&of.path) {               // borrows of.path (partial move OK)
        Ok(final_path) => { ... }
        Err(e) => {
            error!(... path = %of.path.display(), "failed to finalize file");
        }
    }
}
```

While Rust allows partial moves (so `of.path` is still accessible after `drop(of.file)`), the `drop(of.file)` on line 246 is unnecessary and misleading. The file handle would be dropped at the end of the `if let` block regardless. More importantly, this creates a semantic trap: the `sync_data()` call on line 243 borrows `of.file`, and if someone refactors to add code between `drop(of.file)` and the `finalize_file` call, they might try to use `of.file` again and get a compile error they don't understand, or worse, they might restructure the code in a way that drops `of` entirely before `of.path` is used.

The identical pattern exists on the shutdown path at `capture.rs:275-289`.

**Impact**: Not a runtime bug today (Rust's borrow checker prevents misuse), but the explicit `drop` of a field followed by use of a sibling field is unusual Rust and a maintenance hazard. A future refactor could easily introduce actual bugs if the struct is restructured.

**Suggested fix**: Remove the explicit `drop(of.file)` calls. The `File` will be closed when `of` goes out of scope at the end of the block. If early close is desired for correctness (ensuring the fd is released before rename), restructure with destructuring:
```rust
let OpenFile { file, path, .. } = of;
file.sync_data()?;
drop(file);
finalize_file(&path)?;
```

### Finding 1.2 [major] -- `bytes_written` does not include file header size

`capture.rs:330-334`:
```rust
Ok(OpenFile {
    file,
    path,
    bytes_written: 0,    // header bytes not counted
    opened_at: Instant::now(),
})
```

The file header is written at `capture.rs:328` via `format::write_file_header`, but `bytes_written` is initialized to 0. The file header size is variable (6 + instance_id.len() + run_id.len() bytes). The rotation check at `capture.rs:237` compares `bytes_written >= max_file_bytes`.

**Impact**: When `max_file_bytes` is small (as in tests with 512 or 1024), the actual file on disk is larger than `max_file_bytes` by the header size. For a typical instance_id of 10 bytes and run_id of 8 bytes, that is 24 bytes -- negligible for the default 100MB limit, but technically incorrect. More importantly, this means the metric `wirecap_bytes_total` undercounts total bytes written to disk by the header size of every file opened.

**Suggested fix**: Either initialize `bytes_written` to the return value of `write_file_header` (which currently returns `()` in `Ok` -- would need to return the byte count), or compute the header size and initialize accordingly.

### Finding 1.3 [major] -- `write_record` returns byte count as `usize`, but `bytes_written` field is `u64`

`capture.rs:301`:
```rust
of.bytes_written += n as u64;
```

`format::write_record` at `format.rs:99` returns `Ok(RECORD_HEADER_SIZE + entry.meta.len() + entry.payload.len())`. This is a `usize`. On 64-bit platforms, `usize` fits in `u64`, so the `as u64` cast is safe. However, the return value only accounts for the bytes the function *intended* to write, not what was actually flushed to the OS. If a partial write occurs inside `write_all` (which should not happen for `File` unless interrupted by signals), the byte count would be wrong.

More critically: the return value at `format.rs:99` computes `RECORD_HEADER_SIZE + entry.meta.len() + entry.payload.len()`. But Track 2 already identified that `entry.meta.len()` and `entry.payload.len()` can exceed `u16::MAX` and `u32::MAX` respectively, in which case `write_record` writes the full data but records a truncated length in the header. The returned byte count would reflect the *actual* bytes written (correct), but the file is now corrupt because the recorded lengths don't match. The returned count being "correct" masks the corruption.

**Impact**: If truncation occurs per T2-4.3, the byte count is accurate for rotation purposes, but the file is already corrupt and unreadable. The two bugs compound: the file won't rotate early enough to limit damage, and recovery tools will fail to parse the corrupt records.

**Suggested fix**: Address T2-4.3 first (reject oversized meta/payload). Then the byte count is inherently correct.

### Finding 1.4 [minor] -- No checksum or record framing for corruption detection

The file format has no per-record CRC, no magic bytes between records, and no record length prefix that could be used for forward scanning. If a single byte is corrupted in a record header (e.g., `payload_len` is wrong by 1 byte), all subsequent records are unrecoverable because the reader cannot find the next record boundary.

**Impact**: A single-bit flip anywhere in a record header permanently loses all subsequent records in that file. The tailer (`read_batch`) would rewind to the corrupt position and retry forever.

**Suggested fix (post-v1)**: Consider adding a 4-byte CRC32 at the end of each record, or at minimum a 2-byte magic (e.g., `0xCA 0xFE`) at the start of each record header to enable forward scanning for recovery. This is a format change (v4).

### Finding 1.5 [nit] -- Endianness is consistent but not documented in code

All integer serialization in `format.rs` uses `to_le_bytes()` / `from_le_bytes()` (little-endian). This matches `SPEC.md` ("All multi-byte integers are little-endian"). The code is consistent, but no constant or doc comment in `format.rs` states the byte order convention. If someone adds a new field, they might use `to_be_bytes()` by mistake.

**Suggested fix**: Add a module-level doc comment: `//! All multi-byte integers are serialized as little-endian.`

---

## 2. Concurrency Correctness

### Finding 2.1 [minor] -- `channel_depth` gauge is approximate under concurrent senders

`capture.rs:149-154`:
```rust
pub async fn log(&self, entry: Entry) -> Result<(), CaptureClosed> {
    self.metrics.channel_depth.increment(1.0);    // (A)

    self.tx.send(entry).await.map_err(|_| {       // (B)
        self.metrics.channel_depth.decrement(1.0);
        CaptureClosed
    })
}
```

And the writer side at `capture.rs:233`:
```rust
metrics.channel_depth.decrement(1.0);             // (C)
```

The increment at (A) happens *before* the entry is actually enqueued into the channel. If the channel is full, `send().await` suspends, and during that suspension `channel_depth` reports a value higher than the actual queue length. Conversely, the decrement at (C) happens after `recv()` returns, which is correct for the dequeue side.

Under heavy concurrent load with N senders blocked on a full channel, `channel_depth` can report `actual_queue_len + N_blocked_senders`. This is not a correctness bug, but the metric label "Current capture channel queue depth" is misleading -- it really tracks "entries in flight" (queued + pending send).

**Impact**: Monitoring dashboards may show misleading spikes in channel depth when backpressure is active, potentially triggering false alerts.

**Suggested fix**: Either (a) move the increment to after `send` succeeds, or (b) rename the metric to `wirecap_entries_in_flight` and update the description to "Entries queued or pending send."

### Finding 2.2 [minor] -- No entries can be lost between channel close and drain

The channel shutdown sequence is correct:

1. All `Capture` clones are dropped, which drops all `mpsc::Sender` handles.
2. The `rx.recv()` in the `loop` returns `None`, breaking the loop.
3. The code falls through to the shutdown block at line 274.
4. The shutdown block fsyncs and finalizes the current file.

However, the `loop` at `capture.rs:212-272` uses `tokio::select! { biased; }` which prioritizes `rx.recv()` over `fsync_interval.tick()`. This means all channel entries are processed before fsync ticks, which is correct for throughput but means the final batch of entries may not be fsynced until the shutdown block at line 276. If the process crashes between the last `write_entry` in the loop and the `sync_data()` at line 276, those entries are lost.

This is inherent to buffered I/O and not really fixable without per-entry fsync (which would destroy throughput), but worth noting.

**Impact**: On crash (SIGKILL, power loss), up to 1 second of entries (the fsync interval) can be lost. This is an acceptable tradeoff documented by "Decision 4" but should be explicitly documented in the public API.

### Finding 2.3 [major] -- Race in rotation: entry processed after failed `open_file` is silently dropped

`capture.rs:259-268`:
```rust
match open_file(output_dir, instance_id, run_id) {
    Ok(of) => {
        info!(path = %of.path.display(), "wirecap file opened");
        current = Some(of);
    }
    Err(e) => {
        error!(error = %e, "failed to open wirecap file");
        continue;    // <-- skips write_entry for the current entry
    }
}
```

When `open_file` fails during rotation, the `continue` statement skips the `write_entry` call at line 271. The entry that triggered the rotation is silently lost. On the next iteration, a new entry arrives and the code checks `current.is_none()` (line 241), triggering another `open_file` attempt. If that also fails, another entry is lost.

This was partially noted in Track 2 (Finding 2.2) but the specific detail that the *triggering* entry is lost (not just subsequent ones) was not highlighted.

**Impact**: During filesystem failures, entries are lost one-per-retry rather than being buffered. The entry that caused the rotation check is always lost, even if the next `open_file` succeeds.

**Suggested fix**: Do not `continue` after a failed `open_file`. Instead, fall through to `write_entry` (which will handle `current = None` via its `if let Some` guard -- still drops the entry, but at least consistently). Better: retry `open_file` immediately with backoff, or buffer the current entry.

### Finding 2.4 [minor] -- `writer_healthy` gauge only set in fsync tick, not on each write

`capture.rs:228`:
```rust
metrics.writer_healthy.set(1.0);
```

The `writer_healthy` gauge is only set to 1.0 inside the fsync interval branch. If the writer is processing entries so fast that the `biased` select always takes the `rx.recv()` branch, the fsync tick never fires, and `writer_healthy` is never set to 1.0. On a very busy system, the health gauge could remain at its initial value (likely 0.0 per the `metrics` crate default) for an extended period.

**Impact**: The health check may report the writer as unhealthy during high-throughput bursts, even though it is actively writing.

**Suggested fix**: Set `writer_healthy` to 1.0 once at writer task startup, not just in the fsync branch.

---

## 3. File Lifecycle Correctness

### Finding 3.1 [critical] -- `recover_active_files` corrupts the filename via double extension replacement

`capture.rs:361-362`:
```rust
let recovered_path = PathBuf::from(name.strip_suffix(".active").unwrap())
    .with_extension("wcap.recovered");
```

Consider an input path like `/data/svc_2026-04-06T143022.517Z_a1b2c3d4.wcap.active`.

Step 1: `strip_suffix(".active")` yields `/data/svc_2026-04-06T143022.517Z_a1b2c3d4.wcap`.
Step 2: `.with_extension("wcap.recovered")` replaces the current extension (`.wcap`) with `wcap.recovered`, yielding `/data/svc_2026-04-06T143022.517Z_a1b2c3d4.wcap.recovered`.

This happens to produce the correct result because `with_extension` replaces only the last extension (`.wcap`), and the replacement `"wcap.recovered"` includes the `wcap.` prefix. But this is accidental correctness. If the filename contained an additional dot (e.g., `svc.prod_...wcap.active`), `with_extension` would replace the wrong extension:

- Input: `svc.prod_2026-04-06T143022.517Z_a1b2c3d4.wcap.active`
- After `strip_suffix(".active")`: `svc.prod_2026-04-06T143022.517Z_a1b2c3d4.wcap`
- `with_extension("wcap.recovered")`: `svc.prod_2026-04-06T143022.517Z_a1b2c3d4.wcap.recovered`

Wait -- this is actually still correct because `with_extension` replaces the part after the last `.` only. The `.wcap` after the run_id is the last extension. So `.with_extension("wcap.recovered")` replaces `.wcap` with `.wcap.recovered`. This is correct.

However, the approach is fragile and non-obvious. A simpler and more robust approach:

**Suggested fix**:
```rust
if let Some(base) = name.strip_suffix(".wcap.active") {
    let recovered_path = format!("{base}.wcap.recovered");
}
```

This is explicit about the full suffix being replaced and doesn't rely on `Path::with_extension` semantics.

**Severity revised to [minor]** -- the current code produces correct output for all realistic inputs, but the indirection through `with_extension` is unnecessarily subtle.

### Finding 3.2 [major] -- Recovery path compresses synchronously on the writer task startup

`capture.rs:194`:
```rust
recover_active_files(output_dir);
```

Inside `recover_active_files` at `capture.rs:372`:
```rust
compress_file(&recovered_path);
```

Recovery runs synchronously at writer task startup. If there are multiple large `.wcap.active` files from a previous crash, `compress_file` runs sequentially for each one, blocking the writer task. During this time, the mpsc channel is filling up, and if it reaches capacity, all `Capture::log` calls block (backpressure). In the worst case (many large crash-recovery files), the system could be unresponsive for minutes at startup.

**Impact**: Startup latency proportional to the total size of crash-leftover files. With the default 100MB max file size and zstd level 3, compressing a single file takes ~1-2 seconds. Multiple files could stall startup for 10+ seconds.

**Suggested fix**: Use `tokio::task::spawn_blocking` for recovery compression (as is already done for rotation compression), or defer recovery to a background task after the writer is actively processing entries.

### Finding 3.3 [minor] -- No atomic write for compressed files

`capture.rs:383-388`:
```rust
let input = File::open(path)?;
let output = File::create(&zst_path)?;
let mut encoder = zstd::Encoder::new(output, 3)?;
std::io::copy(&mut std::io::BufReader::new(input), &mut encoder)?;
encoder.finish()?;
```

If the process crashes during compression, a partially-written `.zst` file remains on disk. The original `.wcap` file is still present (it is only deleted on success at line 393), so no data is lost. However, the partial `.zst` file is a confusing artifact that will fail to decompress if someone tries to read it.

On the next startup, `recover_active_files` only handles `.wcap.active` files, not orphaned `.zst` files. The partial `.zst` coexists alongside the original `.wcap` indefinitely.

**Impact**: No data loss, but confusing filesystem state after a crash during compression.

**Suggested fix**: Write to a temporary file (e.g., `.wcap.zst.tmp`), then atomically rename to `.wcap.zst` on success. Delete the `.tmp` file on failure. On startup, clean up any `.zst.tmp` files.

### Finding 3.4 [minor] -- `finalize_file` fallback path returns the active path unchanged

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

In the `_` fallback arm (when the path does not end with `.wcap.active`), `final_path == active_path`. The `fs::rename(active_path, &final_path)` call renames a file to itself. On most Unix systems, `rename(2)` with identical source and destination is a no-op and succeeds. On some edge cases (NFS, certain FUSE filesystems), this could fail.

This fallback should never be reached in normal operation (the writer always creates `.wcap.active` files), but if it is reached, the silent self-rename is confusing.

**Impact**: No practical impact (the path is unreachable in normal operation), but defensive code should either `debug_assert!` or return an error rather than performing a no-op rename.

**Suggested fix**: Add a `debug_assert!(name.ends_with(".wcap.active"))` or return an error in the fallback arm.

---

## 4. Edge Cases in Format Parsing

### Finding 4.1 [major] -- Zero-length payload produces valid but potentially surprising records

`format.rs:98`:
```rust
w.write_all(&entry.payload)?;
```

When `entry.payload` is empty, `write_all` writes zero bytes. The reader at `format.rs:209`:
```rust
let mut payload = vec![0u8; payload_len as usize];
r.read_exact(&mut payload)?;
```

When `payload_len` is 0, this allocates an empty `Vec` and `read_exact` with a zero-length buffer succeeds immediately. This is correct behavior.

However, the Entry struct has no validation that payload is non-empty, and there is no documentation about whether zero-length payloads are semantically valid. A consumer expecting every entry to have payload data could be surprised.

**Impact**: Semantic rather than mechanical. No bug, but worth documenting.

### Finding 4.2 [minor] -- `read_record` returns `Ok(None)` only on clean EOF at version byte boundary

`format.rs:129-132`:
```rust
match r.read_exact(&mut ver_buf) {
    Ok(()) => {}
    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
    Err(e) => return Err(e),
}
```

`Ok(None)` (signaling end-of-records) is returned only when `read_exact` on the 1-byte version field gets `UnexpectedEof`. Any `UnexpectedEof` in subsequent `read_exact` calls (reading the rest of the header, meta, or payload) propagates as `Err`. This is correct for distinguishing "end of file" from "truncated record."

However, there is a subtle edge case: if the file ends with exactly 1 garbage byte after the last valid record, `read_record` will read that byte as a version number, attempt to dispatch to `read_record_v1`/`v2`/`v3`, and either fail with "unsupported record version" (if the byte is not 1, 2, or 3) or fail with `UnexpectedEof` inside the version-specific reader. In neither case does `read_record` return `Ok(None)` -- it returns `Err`.

The tailer handles this correctly (seeks back to retry), but `WcapReader` treats any `Err` as terminal (sets `done = true` and returns `None`, per Track 2 Finding 5.1). This means a file with trailing garbage loses zero records but also provides no way for the caller to know the file was truncated.

**Impact**: Minor -- trailing garbage is only possible after a crash, and the data before the garbage is correctly read. The error is logged via `warn!`.

### Finding 4.3 [minor] -- Record version 0 or version 255 produces a clear error

`format.rs:140-143`:
```rust
_ => Err(std::io::Error::new(
    std::io::ErrorKind::InvalidData,
    format!("unsupported record version: {ver}"),
)),
```

This correctly rejects any version outside {1, 2, 3}. No issue here.

### Finding 4.4 [minor] -- Maximum-length strings (255 bytes) are handled correctly

`format.rs:229-236`:
```rust
fn read_length_prefixed_string(r: &mut dyn std::io::Read) -> std::io::Result<String> {
    let mut len_buf = [0u8; 1];
    r.read_exact(&mut len_buf)?;
    let len = len_buf[0] as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
```

A 255-byte string is the maximum (u8 length prefix). This is handled correctly. A zero-length string (len = 0) allocates an empty Vec, `read_exact` on an empty buffer succeeds, and `String::from_utf8(vec![])` returns `Ok(String::new())`. All correct.

### Finding 4.5 [minor] -- Empty file (header only, no records) is handled correctly

If a file contains only the header and no records, `read_record` will encounter EOF on the first version byte read and return `Ok(None)`. `WcapReader` will yield zero entries. `WcapTailer` will treat it as an empty batch. Both are correct.

### Finding 4.6 [major] -- `read_record_v3` returns `mono_ns: Some(0)` for entries that had `mono_ns: None`

`format.rs:90`:
```rust
w.write_all(&entry.mono_ns.unwrap_or(0).to_le_bytes())?;
```

And `format.rs:212-213`:
```rust
mono_ns: Some(mono_ns),
recv_seq: Some(recv_seq),
```

When writing, `None` is serialized as `0`. When reading v3, the value is always wrapped in `Some(...)`. This means a roundtrip of `mono_ns: None` through write+read produces `mono_ns: Some(0)`. The same applies to `recv_seq`.

This is a lossy roundtrip: the distinction between "not set" (`None`) and "set to zero" (`Some(0)`) is lost. For `mono_ns` this may be acceptable (a monotonic timestamp of 0 is unlikely), but for `recv_seq`, sequence number 0 is the natural first value.

**Impact**: Any code that checks `entry.mono_ns.is_none()` to detect v1/v2-origin records will get false negatives after the entry passes through a v3 write+read cycle. The SPEC.md says "`None` when read from v1/v2 files that predate this field," but once a v2-origin entry is written to a v3 file and read back, it appears to have `mono_ns: Some(0)`.

**Suggested fix**: Either (a) use a sentinel value (e.g., `u64::MAX`) for "not set" and document it, or (b) accept the lossy roundtrip and document that `Some(0)` should be treated as "possibly not set," or (c) add an `Option`-aware encoding: write a 1-byte flag before each optional field (format change, v4).

---

## 5. Filesystem Edge Cases

### Finding 5.1 [minor] -- `output_dir` that doesn't exist is handled correctly

`capture.rs:314`:
```rust
fs::create_dir_all(dir)?;
```

`open_file` calls `create_dir_all` before creating the file. This handles the case where the directory doesn't exist. If `output_dir` is a file (not a directory), `create_dir_all` will fail with an appropriate error.

### Finding 5.2 [major] -- No validation of `instance_id` for path safety

`capture.rs:319`:
```rust
let filename = format!("{instance_id}_{timestamp}.{millis:03}Z_{run_id}.wcap.active");
let path = dir.join(&filename);
```

`instance_id` is user-provided (via `CaptureConfig`) and is interpolated directly into the filename. There is no validation that `instance_id`:
- Does not contain path separators (`/`, `\`)
- Does not contain null bytes
- Does not equal `.` or `..`
- Does not contain characters illegal on Windows (`<>:"|?*`)
- Is not excessively long (filesystem limits, typically 255 bytes for filename)

A malicious or careless `instance_id` like `../../../etc/evil` would cause `dir.join(filename)` to create a file outside `output_dir`. While `Path::join` with a relative component stays within the base on most platforms, an absolute path in `instance_id` (e.g., `/tmp/pwned`) would cause `join` to discard the base entirely on Unix.

Wait -- `instance_id` is part of a larger format string, so `format!("{instance_id}_...")` produces a string, and `dir.join(&filename)` joins it. The `instance_id` with `/` would create subdirectories, not escape the parent. But an `instance_id` starting with `/` would make the entire `filename` start with `/`, and `Path::join` with an absolute path discards the base. Let me verify:

Actually, `format!("{instance_id}_...")` with `instance_id = "/tmp/pwned"` produces `/tmp/pwned_2026...`, and `dir.join("/tmp/pwned_2026...")` returns `/tmp/pwned_2026...` on Unix (join discards base for absolute paths). This is a path traversal vulnerability.

**Impact**: An attacker who controls `instance_id` can write files to arbitrary locations on the filesystem. In practice, `instance_id` typically comes from application configuration (not user input), so the attack surface is limited to configuration injection.

**Suggested fix**: Validate `instance_id` in `CaptureConfig::new`:
```rust
assert!(
    !instance_id.contains('/') && !instance_id.contains('\\')
        && instance_id != "." && instance_id != "..",
    "instance_id must be a simple filename component"
);
```
Or use `Path::file_name()` to extract only the filename component.

### Finding 5.3 [minor] -- `run_id` is safe (generated internally)

`capture.rs:406-409`:
```rust
fn generate_run_id() -> String {
    let n: u32 = rand::Rng::r#gen(&mut rand::thread_rng());
    format!("{n:08x}")
}
```

`run_id` is always 8 hex characters, no path safety concern.

### Finding 5.4 [minor] -- Permissions change mid-run handled via existing error logging

If filesystem permissions change after the writer starts (e.g., directory becomes read-only), `write_all` calls will fail with `io::Error`, which is logged and counted as `write_errors`. The writer continues with `current` still set (the file handle is still open), so subsequent writes to the same file handle will also fail. The writer does not attempt to close and reopen the file, nor does it set `current = None`. This means the writer will log errors on every entry until rotation triggers a new `open_file` call.

If the permission issue is on the directory (preventing new file creation), rotation will fail at `open_file`, and the `continue` at line 266 drops the entry (per Finding 2.3).

**Impact**: Transient permission failures cause error storms in logs but no data loss beyond the already-documented entry-drop on failed rotation.

### Finding 5.5 [nit] -- Symlinks in `output_dir` are followed without restriction

`fs::create_dir_all` and `OpenOptions::open` follow symlinks. If `output_dir` is a symlink to another location, files are written there. This is standard Unix behavior and not inherently a bug, but worth noting for security-conscious deployments.

### Finding 5.6 [nit] -- Non-UTF-8 paths cause silent fallback behavior

`capture.rs:340`:
```rust
let name = active_path.to_str().unwrap_or_default();
```

If the path contains non-UTF-8 characters, `to_str()` returns `None`, `unwrap_or_default()` returns `""`, and the `strip_suffix` check fails, causing `finalize_file` to rename the file to itself (Finding 3.4). Similarly in `recover_active_files` at line 359. While non-UTF-8 paths are rare on modern systems, the silent fallback behavior is surprising.

---

## 6. Clock and Timing

### Finding 6.1 [minor] -- File rotation uses `Instant` (monotonic) correctly

`capture.rs:4`:
```rust
use std::time::Instant;
```

`capture.rs:238-239`:
```rust
of.opened_at.elapsed().as_secs() >= max_file_secs
```

`OpenFile.opened_at` is `Instant::now()` (monotonic clock), so file age is immune to wall-clock jumps. This is correct.

### Finding 6.2 [minor] -- Filename timestamp uses wall-clock, which can produce non-monotonic filenames

`capture.rs:316-317`:
```rust
let now = Utc::now();
let timestamp = now.format("%Y-%m-%dT%H%M%S");
```

The filename includes a wall-clock timestamp. If the system clock jumps backward (NTP correction, manual adjustment), two sequentially-created files can have timestamps that are out of order. `discover_files` in `reader.rs:43` sorts by modification time, not by filename timestamp, so this doesn't affect read order. But `find_active_file` at `reader.rs:75` also uses modification time, which is also wall-clock-based and subject to the same concern.

**Impact**: Mostly cosmetic. File naming order may not match creation order after a clock jump, but the reader sorts by mtime, which is usually correct. In the unlikely event that mtime is also affected by the clock jump (it would be on most systems), the reader could pick up files in the wrong order.

### Finding 6.3 [minor] -- `entry.ts` is wall-clock and `entry.mono_ns` is monotonic -- no cross-validation

The Entry struct carries both `ts` (wall-clock nanoseconds) and `mono_ns` (monotonic nanoseconds). The writer does not validate that `ts` and `mono_ns` are consistent (e.g., that `mono_ns` is non-decreasing across entries within a file). The writer simply records whatever the caller provides.

This is by design (the library is a dumb pipe), but it means a consumer cannot rely on either field for ordering without external knowledge of the source's clock discipline.

**Impact**: Not a bug; design documentation concern.

### Finding 6.4 [nit] -- Filename millisecond formatting is correct

`capture.rs:318`:
```rust
let millis = now.timestamp_subsec_millis();
```

`timestamp_subsec_millis()` returns 0-999. The format string `{millis:03}` zero-pads to 3 digits. Correct.

---

## 7. Drop/Shutdown Correctness

### Finding 7.1 [minor] -- Channel drain on shutdown is correct

When all `Capture` handles are dropped:
1. All `mpsc::Sender` clones are dropped.
2. `rx.recv()` returns `None` (line 218), breaking the `loop`.
3. The writer falls through to the shutdown block (lines 274-293).

Between step 1 and step 2, there may be entries in the channel buffer. The `tokio::select!` loop processes them one at a time until `recv()` returns `None`. No entries are lost in this transition because `mpsc::Receiver::recv()` returns `None` only after all senders are dropped *and* the buffer is empty.

This is correct. There is no race condition.

### Finding 7.2 [minor] -- Last `send` vs channel close is handled correctly

Consider: Thread A calls `cap.log(entry).await`, and Thread B drops the last `Capture` clone (including the one Thread A is using, if cloned). Since `mpsc::Sender::send` takes `&self`, Thread A can be mid-send while Thread B drops its clone. But Thread A holds its own clone of the `Sender` (via `self.tx`), so the channel remains open until Thread A's `send` completes and Thread A's `Capture` is dropped. There is no data loss.

If Thread A is the last sender and its `send` succeeds, the entry is in the channel buffer. When Thread A's `Capture` is subsequently dropped (after `send` returns), the sender count reaches zero, triggering channel closure. The writer's `recv()` will return the buffered entry before returning `None`. Correct.

### Finding 7.3 [major] -- Shutdown path does not drain remaining channel entries

Wait -- examining more carefully. The `loop` at `capture.rs:212-272` uses `tokio::select!`:

```rust
loop {
    let entry = tokio::select! {
        biased;
        entry = rx.recv() => {
            match entry {
                Some(e) => e,
                None => break,  // All senders dropped
            }
        }
        _ = fsync_interval.tick() => {
            // fsync and continue
            continue;
        }
    };
    // process entry
}
```

When `rx.recv()` returns `None`, the loop breaks. But there is a subtle issue with `biased` select: since `rx.recv()` is the first branch, it always takes priority over the fsync tick. When the channel closes, `rx.recv()` returns `None` immediately (once the buffer is drained). This is correct -- all buffered entries are processed before the break.

But is there a window where the fsync tick fires *and* there are entries in the channel? With `biased`, the `recv` branch always wins, so yes -- the fsync might be starved during high-throughput periods. This is acceptable because the shutdown block does an explicit fsync.

However, there is another concern: after the `loop` breaks, the shutdown block (lines 274-289) only fsyncs and finalizes the current file. It does **not** call `rx.recv()` again to check for any stragglers. Since `recv()` returning `None` guarantees the buffer is empty, this is actually fine -- the break only happens when the buffer is truly empty.

**Revised assessment**: The drain is correct. No entries are lost. Revising severity to [nit].

### Finding 7.4 [minor] -- `spawn_blocking` for compression during rotation is fire-and-forget

`capture.rs:249-251`:
```rust
tokio::task::spawn_blocking(move || {
    compress_file(&final_path);
});
```

During rotation (not shutdown), the `spawn_blocking` return value (`JoinHandle`) is discarded. If compression panics, the panic is silently swallowed. If the writer task shuts down while multiple compression tasks are in flight, those tasks continue to run in the background but their results are never checked.

On the shutdown path at `capture.rs:282-284`, the `JoinHandle` is `.await`ed:
```rust
let _ = tokio::task::spawn_blocking(move || {
    compress_file(&final_path);
}).await;
```

The `let _ =` discards the `Result<(), JoinError>`, so a panic in `compress_file` during shutdown is also silently swallowed. But at least the shutdown waits for it.

**Impact**: A panic in `compress_file` (e.g., from a zstd bug or OOM during compression) would be silently lost during rotation. The uncompressed `.wcap` file would be deleted by `compress_file` only on success, so the data is preserved, but no error is reported.

**Suggested fix**: Log the `JoinError` from the rotation `spawn_blocking`:
```rust
let handle = tokio::task::spawn_blocking(move || {
    compress_file(&final_path);
});
tokio::spawn(async move {
    if let Err(e) = handle.await {
        error!(error = %e, "compression task panicked");
    }
});
```

---

## 8. Spec Compliance

### Finding 8.1 [nit] -- SPEC.md and code agree on all field offsets and sizes

I verified every field offset in the v3 record format:

| Field | Spec offset | Spec size | Code (format.rs:189-199) | Match? |
|---|---|---|---|---|
| version | 0 | 1 | `ver_buf[0]` (read before `read_record_v3`) | Yes |
| ts | 1 | 8 | `rest[0..8]` (offset 0 in `rest`, which starts at byte 1) | Yes |
| mono_ns | 9 | 8 | `rest[8..16]` | Yes |
| recv_seq | 17 | 8 | `rest[16..24]` | Yes |
| meta_len | 25 | 2 | `rest[24..26]` | Yes |
| payload_len | 27 | 4 | `rest[26..30]` | Yes |
| src | 31 | 1 | `rest[30]` | Yes |
| dir | 32 | 1 | `rest[31]` | Yes |
| meta | 33 | meta_len | read after header | Yes |
| payload | 33+meta_len | payload_len | read after meta | Yes |

All correct. `RECORD_HEADER_SIZE` = 33 matches 1 (version) + 32 (rest of header).

### Finding 8.2 [nit] -- v2 record format: spec and code agree

Spec says v2 header is 17 bytes. Code reads `rest = [0u8; 16]` after the 1-byte version. 1 + 16 = 17. Field offsets:

| Field | Spec offset | Code index in `rest` |
|---|---|---|
| ts | 1 | `rest[0..8]` | Correct |
| meta_len | 9 | `rest[8..10]` | Correct |
| payload_len | 11 | `rest[10..14]` | Correct |
| src | 15 | `rest[14]` | Correct |
| dir | 16 | `rest[15]` | Correct |

All correct.

### Finding 8.3 [nit] -- v1 record format: spec and code agree

Spec says v1 header is 15 bytes. Code reads `rest = [0u8; 14]` after the 1-byte version. 1 + 14 = 15. Field offsets:

| Field | Spec offset | Code index in `rest` |
|---|---|---|
| ts | 1 | `rest[0..8]` | Correct |
| payload_len | 9 | `rest[8..12]` | Correct |
| src | 13 | `rest[12]` | Correct |
| dir | 14 | `rest[13]` | Correct |

All correct.

### Finding 8.4 [minor] -- SPEC.md says `Capture::start` but code uses `Capture::new`

SPEC.md line 99: "The run_id is a random 8-hex-char identifier generated at `Capture::start`."

The actual method is `Capture::new` (`capture.rs:101`). The spec references a method that does not exist.

**Impact**: Documentation inaccuracy. Minor confusion for consumers referencing the spec.

**Suggested fix**: Update SPEC.md to say `Capture::new`.

### Finding 8.5 [minor] -- Write order differs from spec field order

The v3 write order in `format.rs:88-94`:
```rust
w.write_all(&[RECORD_VERSION])?;      // offset 0
w.write_all(&entry.ts.to_le_bytes())?; // offset 1
w.write_all(&entry.mono_ns...)?;       // offset 9
w.write_all(&entry.recv_seq...)?;      // offset 17
w.write_all(&meta_len.to_le_bytes())?; // offset 25
w.write_all(&payload_len...)?;         // offset 27
w.write_all(&[entry.src, entry.dir as u8])?; // offset 31-32
```

This matches the spec's byte layout exactly. The read order in `read_record_v3` also matches. No issue -- noting for completeness.

---

## 9. Test Coverage Gaps

### Finding 9.1 [major] -- No test for meta field roundtrip

All `make_entry` calls in `tests/integration.rs` use `meta: Vec::new()`. There is no test that verifies a non-empty `meta` field survives write+read. The v3 format conditionally skips writing meta when it is empty (`format.rs:95-97`), and conditionally skips reading when `meta_len == 0` (`format.rs:201-206`). The non-empty path is untested.

**Suggested fix**: Add a test with `meta: b"test-meta".to_vec()` and verify it reads back correctly.

### Finding 9.2 [major] -- No test for the lossy `None` -> `Some(0)` roundtrip (Finding 4.6)

No test verifies the behavior of `mono_ns: None` or `recv_seq: None` through a write+read cycle. This is the lossy roundtrip identified in Finding 4.6.

**Suggested fix**: Add a test that writes an entry with `mono_ns: None` and verifies that `read_record` returns `mono_ns: Some(0)`, documenting the expected behavior.

### Finding 9.3 [minor] -- No test for crash recovery (`recover_active_files`)

There is no test that creates a `.wcap.active` file, starts a new writer, and verifies that the file is recovered. The recovery path is only exercised in production.

**Suggested fix**: Create a `.wcap.active` file in a temp dir, start a `Capture`, and verify the file is renamed to `.wcap.recovered.zst`.

### Finding 9.4 [minor] -- No test for `WcapReader` or `WcapTailer`

The tests use direct `format::read_file_header` and `format::read_record` calls. Neither `WcapReader` nor `WcapTailer` are exercised in any test. The `WcapReader` iterator's error-to-None conversion (Track 2 Finding 5.1) is completely untested.

### Finding 9.5 [minor] -- No test for v1 or v2 record reading

The tests only produce v3 records. `read_record_v1` and `read_record_v2` are dead code from the test suite's perspective. If a regression is introduced in v1/v2 parsing, it will not be caught.

### Finding 9.6 [nit] -- `recv_seq` is always hardcoded to `Some(0)` in tests

All test entries use `recv_seq: Some(0)`. There is no test verifying that distinct `recv_seq` values survive roundtrip.

---

## Summary of Findings by Severity

| Severity | Count | Key findings |
|---|---|---|
| **Critical** | 1 | 1.1 (use-after-move pattern -- maintenance hazard, not runtime bug) |
| **Major** | 7 | 1.2 (bytes_written excludes header), 1.3 (byte count masks truncation), 2.3 (entry lost on failed rotation), 4.6 (lossy None/Some(0) roundtrip), 5.2 (path traversal in instance_id), 3.2 (sync recovery blocks startup), 9.1 (no meta roundtrip test) |
| **Minor** | 16 | 1.4, 2.1, 2.2, 2.4, 3.1, 3.3, 3.4, 4.2, 5.4, 5.6, 6.1, 6.2, 6.3, 7.4, 8.4, 8.5 + 9.x |
| **Nit** | 7 | 1.5, 5.3, 5.5, 6.4, 8.1-8.3, 9.6 |

### Top 3 actionable items:

1. **Path traversal in `instance_id` (5.2)** -- validate `instance_id` contains no path separators. Simple fix, real security impact.
2. **Lossy `None`/`Some(0)` roundtrip (4.6)** -- decide on semantics and document. Affects downstream consumers who distinguish between "not set" and "zero."
3. **Entry dropped on failed rotation (2.3)** -- the `continue` after failed `open_file` silently loses the triggering entry. Buffer it or retry.
