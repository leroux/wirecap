# Track 6: Test Coverage & Gaps

**Reviewer**: Claude (automated code review)
**Date**: 2026-04-06
**Scope**: `tests/integration.rs` (383 lines) vs. source in `src/capture.rs`, `src/reader.rs`, `src/format.rs`
**Severity levels**: [critical] = likely to mask a real bug or already does; [major] = significant gap that should be filled before v1; [minor] = nice-to-have; [nit] = cosmetic or style

---

## 1. Coverage Matrix

### 1.1 Public API Functions

| Function | Scenario | Tested? | Notes |
|---|---|---|---|
| **`Capture::new`** | Happy path construction | Yes | Every test exercises this |
| `Capture::new` | Output dir does not exist (auto-create) | No | `open_file` calls `create_dir_all` but no test verifies |
| `Capture::new` | Output dir is read-only / unwritable | No | Error path untested |
| **`Capture::log`** | Single entry | Yes | `write_and_readback` |
| `Capture::log` | Multiple entries | Yes | Several tests |
| `Capture::log` | After writer is dead (returns `CaptureClosed`) | No | **[critical]** |
| `Capture::log` | Backpressure (channel full) | No | **[major]** |
| `Capture::log` | Entry with meta field | No | Always `Vec::new()` |
| `Capture::log` | Entry with empty payload | No | |
| `Capture::log` | Entry with large payload | No | |
| `Capture::log` | Entry with `mono_ns: None` | No | Always `Some(ts)` |
| `Capture::log` | Entry with `recv_seq: None` | No | Always `Some(0)` |
| `Capture::instance_id` | Returns correct value | Indirectly | `filename_contains_instance_and_run` checks filename |
| `Capture::run_id` | Returns correct value | Yes | `file_header_roundtrip` line 145 |
| `Capture::clone` | Concurrent senders | Yes | `concurrent_writers` |
| **`CaptureConfig::new`** | Defaults applied | Indirectly | Used in many tests but defaults never asserted |
| `CaptureConfig` | Custom fields | Yes | `size_based_rotation` constructs manually |

### 1.2 Writer Task Internals

| Function | Scenario | Tested? | Notes |
|---|---|---|---|
| `writer_task` | Normal run + drain on shutdown | Yes | All tests exercise this |
| `writer_task` | `open_file` failure at startup | No | **[critical]** |
| `writer_task` | `write_record` failure mid-stream | No | **[critical]** |
| `writer_task` | fsync failure | No | |
| `writer_task` | fsync interval fires | Not verified | Tests sleep 200ms which is < 1s interval |
| `open_file` | Creates directory if missing | No | |
| `open_file` | File naming format correct | Yes | `filename_contains_instance_and_run` |
| `finalize_file` | `.active` -> `.wcap` rename | Yes | `shutdown_compresses_final_segment` |
| `finalize_file` | Path without `.active` suffix | No | Edge case at line 343 |
| `recover_active_files` | Renames leftover `.active` files | No | **[critical]** |
| `recover_active_files` | No leftover files (noop) | No | |
| `recover_active_files` | Directory does not exist | No | |
| `compress_file` | Successful compression | Yes | `shutdown_compresses_final_segment` |
| `compress_file` | Compression failure | No | |
| `compress_file` | Delete-after-compress failure | No | |
| `generate_run_id` | Produces 8-hex-char string | No | |

### 1.3 Reader Module (`src/reader.rs`)

| Function | Scenario | Tested? | Notes |
|---|---|---|---|
| **`WcapReader::open`** | Raw `.wcap` file | No | **[critical]** |
| `WcapReader::open` | Compressed `.wcap.zst` file | No | **[critical]** |
| `WcapReader::open` | `.wcap.recovered` file | No | |
| `WcapReader::open` | Non-existent path | No | |
| `WcapReader::open` | Truncated/corrupt file | No | |
| `WcapReader` | Iterator yields all records | No | **[critical]** |
| `WcapReader` | Iterator on empty file (header only) | No | |
| `WcapReader` | Iterator after read error (sets `done`) | No | |
| `WcapReader` | `instance_id` / `run_id` fields correct | No | |
| **`WcapTailer::new`** | Construction | No | **[critical]** |
| `WcapTailer::try_open` | Active file exists | No | |
| `WcapTailer::try_open` | No file exists | No | |
| `WcapTailer::try_open` | Already open (returns `true`) | No | |
| `WcapTailer::read_batch` | Reads available records | No | **[critical]** |
| `WcapTailer::read_batch` | Handles partial record at EOF | No | |
| `WcapTailer::read_batch` | `max_batch` limit respected | No | |
| `WcapTailer::check_rotation` | Detects rotated file | No | |
| `WcapTailer::is_open` | Returns correct state | No | |
| `WcapTailer::current_path` | Returns correct path | No | |
| **`discover_files`** | Finds and sorts files | No | **[major]** |
| `discover_files` | Empty directory | No | |
| `discover_files` | Mixed file types | No | |
| `discover_files` | Non-wcap files ignored | No | |
| **`find_active_file`** | Finds `.active` file | No | **[major]** |
| `find_active_file` | Falls back to `.wcap` | No | |
| `find_active_file` | No files -> `None` | No | |
| `find_active_file` | Multiple `.active` files | No | |

### 1.4 Format Module (`src/format.rs`)

| Function | Scenario | Tested? | Notes |
|---|---|---|---|
| `write_file_header` | Happy path | Yes | Indirectly via `Capture` |
| `write_file_header` | Long instance_id (>255 bytes) | No | **[major]** Truncates silently (line 72) |
| `read_file_header` | Happy path | Yes | `file_header_roundtrip` |
| `read_file_header` | Bad magic | No | **[major]** |
| `read_file_header` | Wrong version | No | |
| `read_file_header` | Truncated header | No | |
| `read_file_header` | Non-UTF-8 instance_id | No | |
| `write_record` (v3) | Happy path | Yes | Indirectly via `Capture` |
| `write_record` | With meta field | No | **[major]** |
| `write_record` | Empty payload | No | |
| `write_record` | Meta > 65535 bytes (u16 overflow) | No | **[major]** Truncates silently (line 85) |
| `write_record` | Payload > 4GB (u32 overflow) | No | Truncates silently (line 87) |
| `read_record` | v3 record | Yes | Indirectly via `read_all_records` |
| `read_record` | v2 record | No | **[major]** |
| `read_record` | v1 record | No | **[major]** |
| `read_record` | Unknown version byte | No | |
| `read_record` | EOF (returns None) | Yes | Implicitly in `read_all_records` loop |
| `read_record` | Truncated record | No | |
| `read_record_v3` | With non-empty meta | No | |
| `Dir::from_u8` | Valid values (0, 1) | Indirectly | Via `all_channel_and_dir_variants` |
| `Dir::from_u8` | Invalid value (>1) | No | |
| `Dir::as_str` | Both variants | No | |
| `parse_dir` | Invalid dir byte | No | |

### 1.5 Summary Counts

| Category | Total scenarios | Tested | Untested | Coverage % |
|---|---|---|---|---|
| Capture API | 14 | 7 | 7 | 50% |
| Writer internals | 14 | 3 | 11 | 21% |
| Reader module | 21 | 0 | 21 | 0% |
| Format module | 20 | 5 | 15 | 25% |
| **Total** | **69** | **15** | **54** | **22%** |

---

## 2. Edge Case Analysis

### 2.1 Payload & Meta Boundaries

**[major] T6-2.1: No zero-length payload test.** Every test uses a JSON string payload. An empty `payload: Vec::new()` is a valid entry and exercises the boundary where `payload_len == 0` in `write_record` (format.rs line 87). This is especially relevant because `write_record` unconditionally writes `w.write_all(&entry.payload)?` even when empty (line 98) -- the behavior is correct but unverified.

**[major] T6-2.2: No large payload test.** No test approaches the `u32::MAX` boundary for `payload_len`. The `as u32` truncation at format.rs line 87 will silently corrupt data if `payload.len() > u32::MAX`. While 4GB payloads are impractical in tests, a test with a payload length of e.g. `u16::MAX + 1` (65537 bytes) would verify the u32 encoding path handles lengths beyond u16 range.

**[major] T6-2.3: No meta field test at all.** Every single `make_entry` call and every manual `Entry` construction in the test suite uses `meta: Vec::new()`. The meta read/write path in v3 (`if meta_len > 0` branches at format.rs lines 95-97 and 201-205) is never exercised with actual data. This is a known gap (T3-9.1) but deserves emphasis: meta is used in production for REST request context.

**[minor] T6-2.4: `meta_len` u16 overflow.** If `entry.meta.len() > 65535`, the `as u16` cast at format.rs line 85 silently truncates. The written length will not match the actual meta bytes, producing corrupt records. No test catches this.

### 2.2 Source & Direction Boundaries

**[minor] T6-2.5: `src` coverage is sparse.** `all_channel_and_dir_variants` (line 179) tests `src` values 0, 1, 2, 3, 4, 255. Since `src` is a raw u8 written/read as a single byte, the format handling is trivial. But the test only covers 6 of 256 values. More importantly, `make_entry` hardcodes `src: 0`, meaning 7 of 8 tests only ever exercise `src == 0`.

**[nit] T6-2.6: `Dir` only has two variants.** The `all_channel_and_dir_variants` test covers both `Dir::In` and `Dir::Out`, which is complete. However, no test verifies that an invalid dir byte (e.g., 2, 255) in a record produces an error via `parse_dir` (format.rs line 223).

### 2.3 Rotation Boundaries

**[major] T6-2.7: No "exactly at threshold" rotation test.** `size_based_rotation` uses `max_file_bytes: 1024` and writes entries that are ~83 bytes, so rotation happens well past the threshold. There is no test that writes entries summing to exactly `max_file_bytes` and verifies whether rotation does or does not occur. The boundary condition is `>=` at capture.rs line 237, but this is never tested precisely.

**[major] T6-2.8: No time-based rotation test.** `max_file_secs` is always set to 3600 in tests. No test verifies that a file rotates after the time limit. This would require either a real sleep (slow) or a way to inject a clock (the code uses `Instant::now()` at capture.rs line 335, which is not mockable).

**[minor] T6-2.9: Single-entry file rotation.** No test verifies that a `max_file_bytes: 1` config (or similarly tiny) produces correct output when every single entry triggers rotation. This would stress the open-write-finalize-compress cycle.

### 2.4 Channel & Concurrency Boundaries

**[major] T6-2.10: No backpressure test.** `channel_capacity` is always 1024 in tests. No test fills the channel to capacity and verifies that `log()` blocks (applies backpressure) rather than dropping entries. This is a core design guarantee (Decision 1 per the comments).

**[minor] T6-2.11: `channel_capacity: 1` not tested.** A capacity of 1 would exercise the tightest possible backpressure path, where every `log()` blocks until the writer processes the previous entry.

### 2.5 Timestamp Edge Cases

**[nit] T6-2.12: `ts == 0` not tested.** While `now_ns()` always produces a large timestamp, a zero timestamp is valid and exercises the minimum value for the u64 field.

**[nit] T6-2.13: `ts == u64::MAX` not tested.** Maximum timestamp boundary.

---

## 3. Error Path Coverage

**[critical] T6-3.1: Zero error paths are tested.** This is the most significant gap in the test suite. Every test follows the happy path exclusively. The following error conditions have no coverage:

### 3.1 `Capture::log` errors

- **`CaptureClosed` error**: No test drops the writer (or lets it panic) and then calls `log()` on the returned `Capture` handle. The `CaptureClosed` type (capture.rs line 162) is defined, implements `Display` and `Error`, but is never constructed in any test.

### 3.2 File I/O errors

- **`open_file` failure**: No test uses an invalid/unwritable `output_dir`. When `open_file` fails at startup (capture.rs line 203), the writer continues with `current: None`. When it fails during rotation (line 265), it `continue`s the loop, silently dropping entries until the next attempt. Neither path is tested.
- **`write_record` failure**: When `write_entry` encounters a write error (capture.rs line 304), it increments `write_errors` metric and logs. No test verifies this path or that the writer survives a transient write error.
- **`finalize_file` failure**: When rename fails (capture.rs line 253), the error is logged. No test verifies the file is not left in a corrupt state.
- **`compress_file` failure**: When compression fails (capture.rs line 399), the raw file is preserved and the partial `.zst` is deleted. No test verifies this recovery behavior.

### 3.3 Format-level errors

- **Bad magic in `read_file_header`**: format.rs line 107-110 returns `InvalidData`. Untested.
- **Wrong file version**: format.rs line 115-118. Untested.
- **Unknown record version**: format.rs line 141-143. Untested.
- **Invalid dir byte**: format.rs line 223-226. Untested.
- **Non-UTF-8 string in header**: format.rs line 235 `String::from_utf8` error. Untested.
- **Truncated record (mid-read EOF)**: Various `read_exact` calls will return `UnexpectedEof`. Untested. This is especially important for `WcapTailer` which must handle partial records.

### 3.4 Recovery errors

- **`recover_active_files`**: capture.rs line 350. No test verifies that leftover `.wcap.active` files from a prior crash are renamed to `.wcap.recovered` and compressed. The rename failure path (line 368) is also untested.

---

## 4. Integration vs. Unit Test Balance

**[major] T6-4.1: No unit tests exist anywhere.** There are no `#[cfg(test)]` modules in any source file. Every test is an integration test in `tests/integration.rs`. This has several consequences:

### 4.1 Functions that need unit tests

**Format roundtrip functions** (`write_record` / `read_record`): These are pure functions operating on byte buffers. They are ideal candidates for unit tests with a `Cursor<Vec<u8>>`. Currently they are only tested indirectly through the full `Capture` -> file -> `read_all_records` pipeline, which means:
- A format bug could be masked by complementary bugs in write/read
- Testing v1 and v2 record reading requires crafting binary test data, which is most naturally done in a unit test alongside the format code

**`finalize_file`**: Pure path manipulation. A unit test could verify behavior for paths with/without `.active` suffix, paths with no suffix, etc., without touching the filesystem.

**`recover_active_files`**: Requires filesystem fixtures but not async runtime. A sync unit test with `tempdir` would be simpler and faster than going through `Capture::new`.

**`Dir::from_u8` and `parse_dir`**: Trivial but with boundary conditions. One-liner unit tests would document the contract.

**`is_wcap_file` (in reader.rs)**: Private helper but drives file discovery logic. Unit tests would clarify what filenames are recognized.

### 4.2 Current integration tests are too coarse-grained

The existing tests always exercise the full write path: `Capture::new` -> `log` -> drop -> await writer -> read files. This means every test:
1. Starts an async runtime
2. Opens real files
3. Spawns background compression tasks
4. Waits for all async work to complete

A single failing assertion does not isolate which layer is broken (format? file I/O? channel? compression?). Unit tests for format and file operations would provide faster, more precise failure diagnostics.

---

## 5. Test Quality Analysis

### 5.1 Assertions that are too weak

**[major] T6-5.1: `write_and_readback` does not verify `mono_ns` or `recv_seq`.** Lines 128-137 assert `ts`, `src`, `dir`, and payload content, but never check `mono_ns` or `recv_seq`. Since these are v3-only fields, a bug in their serialization would go undetected. The test constructs entries with `mono_ns: Some(ts)` and `recv_seq: Some(0)` (line 8-9) but never verifies these values survive the roundtrip.

**[major] T6-5.2: `concurrent_writers` only asserts count.** Line 249 checks `records.len() == 100` but does not verify that all 100 distinct `(writer_id, seq)` pairs are present. If the writer silently duplicated or dropped entries (but still wrote 100), this test would pass. A proper assertion would deserialize each payload and verify the full set of `{w, s}` pairs.

**[major] T6-5.3: `size_based_rotation` only asserts `file_count > 3`.** Line 282 uses a loose lower bound. It does not verify that each individual file is under `max_file_bytes` (the actual invariant). A rotation bug that writes 99 records to one file and 1 to another would pass this test.

**[minor] T6-5.4: `all_channel_and_dir_variants` does not check payload.** Line 218 asserts `src` and `dir` but never verifies that the payload `{"v":1}` survived the roundtrip. While this is tested elsewhere, the omission means a payload corruption that only affects certain `src`/`dir` combinations would be missed.

**[minor] T6-5.5: `file_header_roundtrip` checks only one file.** Line 162 asserts `!files.is_empty()` then reads `files[0]`. If multiple files exist (unlikely in this test, but possible), it only checks the first.

### 5.2 Assertions that would pass even if the code were broken

**[major] T6-5.6: `shutdown_compresses_final_segment` could pass without compression.** The test at line 330 sleeps 200ms then asserts `count_active_files > 0`. But if the writer processes entries instantly and compresses before the sleep check, the active file could already be gone by the time we check. The assertion would fail spuriously. More critically, the post-shutdown assertion at line 344 (`raw == 0`) would also pass if the writer simply deleted raw files without compressing them -- the test does not verify that the `.zst` files contain valid data.

**[nit] T6-5.7: `filename_contains_instance_and_run` checks only substring.** Lines 310-312 use `name.contains(...)`. A filename like `fn-testXXX.wcap.zst` would pass even if the delimiter format were wrong. This is acceptable for an integration test but not rigorous.

### 5.3 `make_entry` helper analysis

**[minor] T6-5.8: `make_entry` hardcodes too many fields.** The helper at line 5 always produces `src: 0`, `dir: Dir::In`, `meta: Vec::new()`, `mono_ns: Some(ts)`, `recv_seq: Some(0)`. This means most tests never exercise other values. A builder pattern or additional parameters would allow variation without code duplication. At minimum, the meta and mono_ns/recv_seq fields should be configurable.

### 5.4 `read_all_records` helper analysis

**[minor] T6-5.9: `read_all_records` panics on error.** Lines 47, 55, 62 use `.expect()` and `panic!()`. This is standard for test helpers but means that a corrupt file will produce an unhelpful panic rather than a descriptive test failure. Using `assert!` with messages or returning `Result` would be more diagnostic.

**[minor] T6-5.10: `read_all_records` silently skips unreadable directory entries.** Line 34 uses `filter_map(Result::ok)`, which means if a directory entry fails to read (permissions, etc.), it is silently ignored. This could hide test failures.

**[nit] T6-5.11: `is_wirecap_file` in tests differs from `is_wcap_file` in reader.rs.** The test helper at line 24 explicitly excludes `.wcap.active` files and includes `.wcap.recovered` and `.wcap.recovered.zst`. The production function `is_wcap_file` in reader.rs line 19 includes `.wcap.active` but does not include `.wcap.recovered.zst`. These inconsistencies could cause tests to read different file sets than production code would discover.

---

## 6. Property-Based Testing Opportunities

**[major] T6-6.1: Format roundtrip is the highest-value target for proptest.** The `write_record` / `read_record` pair should satisfy:
```
forall entry: Entry . read_record(write_record(entry)) == entry
```
A proptest strategy could generate arbitrary `Entry` values with:
- `ts`: any u64
- `mono_ns`: `Option<u64>` (but note: write always stores `unwrap_or(0)`, read always returns `Some(v)` -- see T6-6.3)
- `recv_seq`: same issue
- `src`: 0..=255
- `dir`: In | Out
- `meta`: `Vec<u8>` with length 0..1000
- `payload`: `Vec<u8>` with length 0..10000

**[major] T6-6.2: File header roundtrip.** `write_file_header` / `read_file_header` should satisfy a similar property for arbitrary `instance_id` and `run_id` strings (constrained to length <= 255 since they use a u8 length prefix).

**[major] T6-6.3: Property testing would immediately catch the lossy `None`/`Some(0)` roundtrip bug.** `write_record` converts `mono_ns: None` to `0` (format.rs line 90) and `read_record_v3` reads it back as `Some(0)` (line 195). A property test asserting `read(write(entry)) == entry` would fail for any entry with `mono_ns: None` or `recv_seq: None`. This is a known issue (T3-9.2) but property testing is the right tool to catch it systematically.

**[minor] T6-6.4: Discover/sort ordering.** `discover_files` sorts by modification time. A property test could create N files with controlled modification times and verify the output order matches.

**[minor] T6-6.5: Compression roundtrip.** For any valid wcap file, `compress_file` followed by `WcapReader::open` on the `.zst` file should yield the same records as reading the original raw file.

---

## 7. Negative Testing

**[critical] T6-7.1: No test verifies behavior when given corrupt input.** The reader must handle:
- A file with valid magic but garbage after the header
- A file with a valid header but truncated in the middle of a record
- A file where `payload_len` claims 1GB but only 10 bytes remain
- A zero-byte file
- A file containing only the magic bytes

None of these are tested. `WcapReader` silently sets `done = true` on error (reader.rs line 142-145), which means corrupt data is indistinguishable from a normal EOF to the caller. This design decision should at least be verified by a test.

**[major] T6-7.2: No test for graceful shutdown ordering.** The tests always follow `drop(cap); handle.await;`. No test verifies behavior when:
- The writer task is aborted (e.g., `handle.abort()`) before the channel is drained
- Multiple `Capture` clones are dropped in different orders
- `log()` is called concurrently with the last clone being dropped

**[major] T6-7.3: No test verifies no file leaks.** After shutdown, no test checks that exactly the expected set of files exists. The `shutdown_compresses_final_segment` test checks `active == 0` and `raw == 0` and `zst > 0`, but does not assert the total file count. Leftover temp files, partial `.zst` files, or unexpected artifacts would go undetected.

**[minor] T6-7.4: No test for idempotent shutdown.** If `Capture::new` is called but `log()` is never called, then the writer is dropped, the system should not leave any files behind (or should leave exactly one empty compressed file). This boundary is untested.

**[minor] T6-7.5: No test for non-existent output directory.** `open_file` calls `create_dir_all`. No test verifies that deeply nested paths like `/tmp/a/b/c/d/` are created correctly.

**[nit] T6-7.6: No test that `Dir::from_u8(2)` returns `None`.** The negative case for the enum conversion is never tested.

---

## 8. Test Infrastructure Assessment

### 8.1 Helper Design Issues

**[minor] T6-8.1: `now_ns()` is non-deterministic.** The helper at line 17 uses `SystemTime::now()`, making tests time-dependent. Most tests pass `now_ns()` as the timestamp, so records will have different timestamps across test runs. This is fine for most tests, but any test that needs to reason about timestamp ordering (none currently do) would be flaky. A fixed timestamp constant would be more appropriate as the default.

**[minor] T6-8.2: Missing helper for creating entries with meta.** There is no equivalent of `make_entry` that populates the `meta` field. Every test that needs meta must construct `Entry` manually, which discourages testing meta paths.

**[nit] T6-8.3: `count_*` helpers could be a single parameterized function.** `count_wcap_files`, `count_raw_wcap_files`, `count_zst_files`, and `count_active_files` (lines 68-103) are four near-identical functions differing only in the filter predicate. A single `count_files_matching(dir, predicate)` would reduce duplication.

### 8.2 Potential Bugs in Test Code

**[minor] T6-8.4: `read_all_records` does not handle `.wcap.recovered.zst` decompression correctly.** Line 45 checks `path.extension() == Some("zst")`, which correctly identifies `.wcap.zst` and `.wcap.recovered.zst`. However, `is_wirecap_file` at line 26 includes `.wcap.recovered` (uncompressed) and `.wcap.recovered.zst`, while the reader treats `.wcap.recovered` as a raw file. This is correct behavior, but the combination of `is_wirecap_file` including `.wcap.recovered` and the reader at line 48-52 handling both compressed and uncompressed paths means this works by coincidence rather than by design.

**[minor] T6-8.5: `is_wirecap_file` in tests does not match production `is_wcap_file`.** As noted in T6-5.11, the test version excludes `.wcap.active` (line 28) while the production version in reader.rs line 22 includes it. The test version includes `.wcap.recovered.zst` which the production version does not. This divergence means the test helper may read a different set of files than `discover_files` or `WcapReader` would find in production.

### 8.3 Test Runtime Configuration

**[minor] T6-8.6: `#[tokio::test]` uses `current_thread` runtime.** The default `#[tokio::test]` uses a single-threaded runtime. The `concurrent_writers` test (line 222) exercises concurrent `tokio::spawn` tasks, but on a single-threaded runtime these are cooperatively scheduled, not truly parallel. The `spawn_blocking` call in `compress_file` does spawn OS threads, but the overall concurrency profile differs from production. The `Cargo.toml` dev-dependencies lack `rt-multi-thread`:
```toml
[dev-dependencies]
tokio = { version = "1", features = ["rt", "macros"] }
```
At minimum, `concurrent_writers` should use `#[tokio::test(flavor = "multi_thread")]` and the dev-dependency should include `rt-multi-thread`.

---

## 9. Previously Identified Gaps -- Deeper Analysis

### 9.1 T3-9.1: No meta roundtrip test

Beyond the known gap, the meta path has a subtle interaction: `write_record` skips `w.write_all(&entry.meta)` when `entry.meta.is_empty()` (format.rs line 95-97), but `meta_len` is still written as 0. On the read side, `read_record_v3` checks `if meta_len > 0` (line 201) before reading meta bytes. If the write side wrote `meta_len: 0` but the read side did not check, it would try to read 0 bytes (a no-op). So the `if` guard on the read side is unnecessary but harmless. A test with non-empty meta would verify the data path; a property test would catch any mismatch.

### 9.2 T3-9.2: Lossy `None`/`Some(0)` roundtrip

The write path (format.rs line 90-91) uses `unwrap_or(0)`. The read path (line 195-196) wraps in `Some(...)`. This means:
- `mono_ns: None` -> written as `0` -> read as `Some(0)` (LOSSY)
- `mono_ns: Some(0)` -> written as `0` -> read as `Some(0)` (OK)
- `mono_ns: Some(42)` -> written as `42` -> read as `Some(42)` (OK)

A roundtrip test would immediately surface this. Additionally, v1/v2 records set `mono_ns: None` on read (format.rs line 160, 185), so a mixed-version roundtrip test (write v3, read v3, compare; write v1 fixture, read, verify None) would document the intended semantics.

### 9.3 T3-9.3: No crash recovery test

`recover_active_files` (capture.rs line 350) runs at the start of every `writer_task`. A test would need to:
1. Create a `.wcap.active` file with valid header and records
2. Call `Capture::new` pointing at that directory
3. Verify the `.active` file was renamed to `.wcap.recovered` and compressed

This is testable today without mocking -- just create the fixture file before calling `Capture::new`. No test does this.

### 9.4 T3-9.4: No test for `WcapReader` or `WcapTailer`

The entire `reader.rs` module (310 lines) has zero test coverage. `WcapReader` is a straightforward iterator wrapper, but `WcapTailer` has complex stateful behavior:
- Partial record rewinding (reader.rs line 247-249)
- EOF counting and rotation checking (line 241-243)
- File switching on rotation (line 262-287)

These are the most bug-prone code paths and the hardest to test (they require a live writer producing data concurrently). A test harness that writes entries to a file while a `WcapTailer` reads them would be the minimum viable test.

### 9.5 T3-9.5: No v1/v2 record reading test

The codebase includes `read_record_v1` (format.rs line 148) and `read_record_v2` (format.rs line 164) for backward compatibility. These are never tested. Since the writer only produces v3 records, v1/v2 code paths can only be tested by:
1. Crafting binary fixtures manually
2. Using a hypothetical v1/v2 writer (does not exist in this crate)

Option 1 is the pragmatic approach. A unit test in `format.rs` could write raw v1/v2 bytes to a `Cursor` and verify `read_record` dispatches correctly and produces the expected `Entry` with `mono_ns: None` and `recv_seq: None`.

---

## 10. Recommended Test Plan (Priority Order)

### P0 -- Must have before any production use

1. **Format unit test module** in `src/format.rs`: roundtrip tests for `write_record`/`read_record` with v3 (including meta), plus fixture-based tests for v1/v2 reading. Covers T6-2.1, T6-2.3, T6-5.1, T3-9.1, T3-9.5.
2. **Error path test for `CaptureClosed`**: Drop writer, then call `log()`, assert `Err(CaptureClosed)`. Covers T6-3.1.
3. **`WcapReader` basic test**: Write a file with `Capture`, then open with `WcapReader` and iterate. Verify `instance_id`, `run_id`, and all records match. Covers T3-9.4 (partial).
4. **Corrupt input tests**: Feed bad magic, wrong version, truncated records to `read_file_header` and `read_record`. Covers T6-7.1.
5. **Crash recovery test**: Plant a `.wcap.active` fixture, start a new `Capture`, verify recovery. Covers T3-9.3.

### P1 -- Should have for confidence

6. **Meta roundtrip integration test**: Log entries with non-empty `meta`, read back, verify byte-exact match.
7. **`concurrent_writers` with payload verification**: Deserialize all payloads, verify the full `{w, s}` matrix.
8. **Rotation size invariant test**: After rotation, verify each file's size is <= `max_file_bytes` + one record.
9. **`WcapTailer` test**: Start a writer, concurrently tail with `WcapTailer`, verify all records arrive.
10. **Property-based format roundtrip**: Use proptest for `Entry` generation.

### P2 -- Nice to have

11. **`discover_files` and `find_active_file` unit tests**
12. **Time-based rotation** (requires real sleep or clock injection)
13. **Backpressure test** (channel full, verify `log()` blocks)
14. **Multi-thread runtime for `concurrent_writers`**
15. **`None`/`Some(0)` roundtrip documentation test** (make the lossy behavior explicit)

---

## 11. Summary

The test suite covers the happy path of the write pipeline thoroughly: construction, single/multi entry logging, file rotation, shutdown compression, and basic header/filename verification. However, it has fundamental gaps:

- **The entire reader module (310 lines, 3 public types) has zero coverage.** This is the most glaring omission.
- **Zero error paths are tested.** Every test assumes success. No `Err` variant is ever asserted.
- **No unit tests exist.** Everything goes through the full async pipeline, making failures hard to isolate and slowing iteration.
- **The format layer is tested only indirectly** and only for v3 records with empty meta. v1/v2 backward compatibility code is dead weight from a testing perspective.
- **Test assertions are sometimes too weak** to catch real bugs (e.g., `concurrent_writers` only checks count, not content).

The existing 8 integration tests are a solid foundation but cover roughly 22% of the meaningful scenario space. The recommended P0 additions (5 test groups) would raise coverage to approximately 50-60% and address all critical gaps.
