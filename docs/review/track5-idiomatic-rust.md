# Track 5: Idiomatic Rust & Code Quality

## Summary

This review examines the `wirecap` crate for adherence to Rust conventions, idiomatic patterns, appropriate trait implementations, visibility correctness, and general code quality. Findings are new and do not duplicate issues already catalogued in Tracks 1-4.

---

## 1. Naming Conventions

### F5.1 — `src` field on `Entry` is ambiguous [minor]
**File:** `src/format.rs:50`

The field `pub src: u8` is documented as "Consumer-defined channel tag (opaque u8)" but `src` universally reads as "source" in Rust code (cf. `src/`, `Iterator::map(|src| ...)`). In a binary format crate the name suggests "source address." The doc comment helps, but callers writing `entry.src = 2` get no semantic hint. A name like `channel` or `channel_tag` would be self-documenting and eliminate the need to re-read the doc every time.

### F5.2 — `OpenFile` name collides conceptually with `std::fs::OpenOptions` [nit]
**File:** `src/capture.rs:177`

The internal struct `OpenFile` is fine for an unexported type, but `ActiveSegment` or `WritableSegment` would better express what it models -- a segment being actively written to, with rotation metadata. `OpenFile` sounds like a thin wrapper around `File`.

### F5.3 — `generate_run_id` is more of a `random_hex_id` [nit]
**File:** `src/capture.rs:406`

The function name couples it to a concept ("run id") rather than describing what it does (generate 8-hex-char random string). If the function were named `random_hex_id()` the caller site `let run_id = random_hex_id()` would be equally clear and the helper would be reusable.

### F5.4 — `MetricHandles::new()` has a side-effect that belies `new` [minor]
**File:** `src/capture.rs:58-72`

`new()` conventionally constructs a value. This `new()` also calls `describe_counter!` / `describe_gauge!` which globally registers metric descriptions with any installed recorder. Calling it twice registers descriptions twice. A name like `register()` or `init()` would better signal the side effect. Alternatively, separate construction from registration.

### F5.5 — Module name `capture` is the same as the crate name `wirecap` minus a prefix [nit]
**File:** `src/lib.rs:1`

`mod capture` is not `pub` -- it is re-exported via `pub use`. This is fine structurally but the module name is slightly redundant with the crate name. Not actionable, just noted.

---

## 2. Visibility

### F5.6 — `CaptureConfig` fields are all `pub` with no validation [major]
**File:** `src/capture.rs:26-32`

All five fields of `CaptureConfig` are `pub`, allowing callers to set `max_file_bytes: 0` or `channel_capacity: 0`, both of which would cause pathological behavior (infinite rotation loop, or a zero-capacity channel which Tokio rejects with a panic). Public fields bypass any chance of validation. Either:
- Make fields private and provide builder methods that validate, or
- Add a `validate()` method and call it in `Capture::new()`, or
- At minimum, document the constraints (non-zero values).

### F5.7 — `WcapReader.instance_id` and `run_id` are `pub` fields, not accessor methods [minor]
**File:** `src/reader.rs:93-94`

`Capture` exposes `instance_id()` and `run_id()` as methods returning `&str` (line 157-158). `WcapReader` exposes the same data as `pub` fields of type `String`. This is inconsistent. Public `String` fields let callers mutate them (`reader.instance_id.clear()`), which is almost certainly unintended. Prefer `pub(crate)` or private fields with `&str`-returning accessors.

### F5.8 — `WcapTailer.instance_id` and `run_id` are `pub` mutable fields [minor]
**File:** `src/reader.rs:166-167`

Same issue as F5.7 but worse: these are `pub Option<String>` fields. A caller can set `tailer.instance_id = Some("evil".into())` and corrupt the tailer's notion of what file it is reading. These should be private with read-only accessors.

### F5.9 — `format` module is fully `pub` but most of its functions are implementation details [minor]
**File:** `src/lib.rs:2`

`pub mod format` exposes every `pub` item in `format.rs` to downstream crates: `write_file_header`, `write_record`, `read_file_header`, `read_record`, `MAGIC`, `FILE_VERSION`, `RECORD_VERSION`, `RECORD_HEADER_SIZE`. Downstream consumers likely only need `Entry`, `Dir`, and possibly the read functions. The write functions and constants are internal plumbing. Consider `pub(crate) mod format` with selective `pub use` re-exports of the items intended for external consumption.

### F5.10 — `discover_files` and `find_active_file` are `pub` but not re-exported from `lib.rs` [minor]
**File:** `src/reader.rs:27,49`

These functions are `pub` inside `pub mod reader`, so they are accessible as `wirecap::reader::discover_files`. But they are not re-exported from the crate root, unlike `WcapTailer`. This creates an inconsistent public surface: some reader items are at the root, others require module-pathing. Either re-export them or narrow their visibility to `pub(crate)`.

---

## 3. Struct and Enum Design

### F5.11 — `Entry` contains two `Vec<u8>` making it 6 words + 2 heap allocs per entry [minor]
**File:** `src/format.rs:39-60`

Each `Entry` is ~112 bytes on the stack (u64 + Option<u64> + Option<u64> + u8 + Dir(u8) + Vec(24) + Vec(24) + padding). For a high-throughput capture path sending 65,536 entries through a channel, this is a lot of per-entry heap allocation. Consider using `bytes::Bytes` which is refcounted and cheaply cloneable, or a single `Vec<u8>` with a split point. This is a design-level observation more than a pure style issue, but the idiomatic Rust approach for wire data is `Bytes`.

### F5.12 — `CaptureClosed` is a unit struct but could be a zero-variant enum for future extensibility [nit]
**File:** `src/capture.rs:163`

`pub struct CaptureClosed;` works, but a common pattern for error types that might grow is to use an enum from the start. Not urgent for a single-variant error, but noted for future-proofing.

### F5.13 — `RECORD_HEADER_SIZE` is a manually computed constant [minor]
**File:** `src/format.rs:11`

The comment documents the arithmetic: `ver(1) + ts(8) + mono_ns(8) + recv_seq(8) + meta_len(2) + payload_len(4) + src(1) + dir(1) = 33`. But the constant is just `33`. If any field is added or changed, the comment and the value must be updated separately. Consider:
```rust
const RECORD_HEADER_SIZE: usize = 1 + 8 + 8 + 8 + 2 + 4 + 1 + 1;
```
This way the arithmetic is self-documenting and verified by the compiler.

---

## 4. Trait Implementations

### F5.14 — `Entry` does not implement `Default` [minor]
**File:** `src/format.rs:39-60`

`Entry` has reasonable defaults (0 timestamps, `None` for optional fields, empty vecs, `Dir::In`). Implementing `Default` would simplify test code and partial construction patterns. Currently every test must construct all 7 fields manually.

### F5.15 — `CaptureConfig` does not implement `Default` [minor]
**File:** `src/capture.rs:26-32`

`CaptureConfig::new()` provides defaults for 3 of 5 fields. Implementing `Default` (with placeholder strings) would enable the `..Default::default()` pattern. Alternatively, since `instance_id` and `output_dir` are required, a builder pattern (flagged in T1) would be more appropriate.

### F5.16 — `WcapReader` does not implement `Debug` [minor]
**File:** `src/reader.rs:91-96`

`WcapReader` is a public type but does not derive or implement `Debug`. While the `Box<dyn Read>` field prevents `#[derive(Debug)]`, a manual implementation could print instance_id, run_id, and done status. This matters for diagnostics when a reader is stuck or failing.

### F5.17 — `WcapTailer` does not implement `Debug` [minor]
**File:** `src/reader.rs:160-168`

Same issue. `WcapTailer` is a public type without `Debug`. A manual impl could print `wcap_dir`, `current_path`, `eof_count`, and whether a reader is open.

### F5.18 — `MetricHandles` should not need `Clone` [nit]
**File:** `src/capture.rs:48`

`MetricHandles` derives `Clone`, but `Counter` and `Gauge` from the `metrics` crate are already internally `Arc`-wrapped. Cloning them is cheap but conceptually these handles are shared references. The struct is only cloned once (line 125). Wrapping in `Arc<MetricHandles>` instead of cloning would be more precise about intent, though functionally equivalent.

---

## 5. Pattern Matching

### F5.19 — `if let Some(of) = current.take()` followed by use-after-move of `of.path` [critical]
**File:** `src/capture.rs:242-256`

```rust
if let Some(of) = current.take() {
    // ...
    drop(of.file);
    match finalize_file(&of.path) {  // <-- of.path used after of.file dropped
```

This works because `drop(of.file)` only drops the `File` field, not the entire `of`. But the explicit `drop(of.file)` is not idiomatic. The `File` inside `of` will be dropped when `of` goes out of scope anyway. The explicit `drop` suggests the author wants the file closed before `finalize_file` renames it, which is a correctness concern. The idiomatic approach is to destructure:
```rust
if let Some(OpenFile { file, path, .. }) = current.take() {
    drop(file);
    match finalize_file(&path) { ... }
}
```
This makes the intent (close file handle before rename) explicit and avoids the confusing partial-move pattern. The same issue appears in the shutdown block at lines 275-289.

### F5.20 — Nested `if let` / `match` in `check_rotation` could be flattened [nit]
**File:** `src/reader.rs:262-288`

```rust
fn check_rotation(&mut self) {
    let current = match &self.current_path {
        Some(p) => p.clone(),
        None => return,
    };
    if let Some(new_path) = find_active_file(&self.wcap_dir) {
        if new_path != current {
```

The clone on line 264 is only needed because `self` is later borrowed mutably. This could use an early-return pattern:
```rust
let Some(current) = &self.current_path else { return };
let Some(new_path) = find_active_file(&self.wcap_dir) else { return };
if new_path == *current { return; }
```
This avoids the clone and the nesting.

Wait -- `find_active_file` takes `&self.wcap_dir` which borrows `self` immutably, and then `self.reader = ...` borrows mutably. The clone is needed to release the borrow. Still, `let-else` chains would flatten the nesting.

---

## 6. Lifetime and Borrowing Idioms

### F5.21 — `output_dir` is `String` but used as `&str` everywhere [minor]
**File:** `src/capture.rs:28,107,189`

`CaptureConfig.output_dir` is `String`, and `writer_task` receives it as `&str`. But `open_file` at line 312 takes `output_dir: &str` and immediately does `Path::new(output_dir)`. Using `PathBuf` / `&Path` throughout would be more type-safe and avoid the stringly-typed path handling. Similarly, `instance_id` is `String` but could be `impl Into<String>` on the config (which `new()` already handles, but direct field access bypasses).

### F5.22 — Unnecessary `.clone()` in `check_rotation` [minor]
**File:** `src/reader.rs:264`

```rust
let current = match &self.current_path {
    Some(p) => p.clone(),
    None => return,
};
```

The `PathBuf` is cloned to avoid holding a borrow on `self`. But the comparison `new_path != current` only needs `&Path`. This could be restructured to avoid the clone by comparing before opening the new file:
```rust
let current = self.current_path.as_deref().unwrap_or(return);
let new_path = find_active_file(&self.wcap_dir)?;
if new_path == current { return; }
// Now we can borrow self mutably
```

### F5.23 — `path.to_str().unwrap_or_default()` is lossy path handling [minor]
**File:** `src/capture.rs:340,360`; `src/reader.rs:33,63`

Multiple places convert `Path` to `&str` using `.to_str().unwrap_or_default()`. On non-UTF-8 file systems, this silently produces an empty string, which would cause subtle failures (empty path, no match on suffix check). The idiomatic approach is to use `Path::extension()` or `Path::file_name()` methods which work with `OsStr`, or to use `.to_string_lossy()` when a string is truly needed for display.

In `finalize_file` (line 340), `active_path.to_str().unwrap_or_default()` followed by `name.strip_suffix(".active")` means non-UTF-8 paths silently skip finalization (the `_` arm returns the path unchanged). This is a correctness concern disguised as a naming issue.

---

## 7. Use of Standard Library

### F5.24 — `finalize_file` hand-rolls suffix stripping instead of using `Path` methods [minor]
**File:** `src/capture.rs:339-347`

```rust
let name = active_path.to_str().unwrap_or_default();
let final_path = match name.strip_suffix(".active") {
    Some(base) if name.ends_with(".wcap.active") => PathBuf::from(base),
    _ => active_path.to_path_buf(),
};
```

This converts to `&str`, strips a suffix, then converts back to `PathBuf`. The `Path` API provides `with_extension()` and `file_stem()`. Since `.wcap.active` is a double extension, `Path::with_extension` is awkward, but the OsStr approach is safer:
```rust
let name = active_path.as_os_str().to_string_lossy();
// or use active_path.file_name() + manipulation
```

### F5.25 — `recover_active_files` converts `Path` to `&str` for suffix checks [minor]
**File:** `src/capture.rs:359-361`

Same pattern: `path.to_str().unwrap_or_default()` then string suffix matching. `path.extension()` returns `Some("active")` and `path.file_name()` could be checked with `OsStr::to_str()` more precisely.

### F5.26 — Manual iteration in `read_all_records` test helper instead of using `WcapReader` [minor]
**File:** `tests/integration.rs:32-66`

The test helper `read_all_records` manually opens files, creates `Box<dyn Read>`, reads headers, and loops with `read_record`. This duplicates exactly what `WcapReader::open` + its `Iterator` impl does. Using `WcapReader` would be shorter and also test the reader itself:
```rust
for path in sorted_paths {
    let reader = WcapReader::open(&path).expect("open");
    all.extend(reader);
}
```

### F5.27 — `generate_run_id` uses verbose trait-method syntax [nit]
**File:** `src/capture.rs:407`

```rust
let n: u32 = rand::Rng::r#gen(&mut rand::thread_rng());
```

The `r#gen` syntax is needed because `gen` is a reserved keyword in Rust 2024 edition, but the crate uses `edition = "2021"`. In edition 2021, `rng.gen::<u32>()` works. However, the `r#gen` syntax is forward-compatible. Still, `rand::random::<u32>()` would be simpler:
```rust
let n: u32 = rand::random();
format!("{n:08x}")
```

---

## 8. Documentation

### F5.28 — No `//!` crate-level doc comment in `lib.rs` [major]
**File:** `src/lib.rs`

`lib.rs` has zero documentation. No `//!` crate doc describing what wirecap is, how to use it, or linking to the key types. This is the first thing users (and `cargo doc`) see. Even a two-line summary would help:
```rust
//! Append-only binary wire capture library.
//!
//! Use [`Capture`] to log wire entries and [`WcapReader`] to read them back.
```

### F5.29 — No `//!` module doc in `capture.rs` [minor]
**File:** `src/capture.rs`

`reader.rs` has a proper `//!` module doc (lines 1-4). `format.rs` has none but is simpler. `capture.rs` has no module doc despite being the largest and most complex module. It should describe the capture pipeline: channel, writer task, rotation, compression.

### F5.30 — `Capture::new` lacks `# Examples` section [minor]
**File:** `src/capture.rs:96-101`

`Capture::new` is the primary entry point for the entire crate. Its doc comment describes what it returns but gives no usage example. An `# Examples` section showing the create-spawn-log-drop lifecycle would be valuable for `cargo doc`.

### F5.31 — Public constants in `format.rs` lack doc comments [minor]
**File:** `src/format.rs:7-11`

`RECORD_VERSION` and `RECORD_HEADER_SIZE` are `pub` constants without `///` doc comments. `MAGIC` and `FILE_VERSION` have minimal one-line docs. For a binary format, these constants deserve explanation of what they mean and when they change.

### F5.32 — `TODO` comment left in published source [nit]
**File:** `src/format.rs:56`

```rust
// TODO: why does REST use meta for these things?
```

This is a design question for the author, not documentation for consumers. It should be resolved or moved to an issue tracker before publishing.

---

## 9. Code Organization

### F5.33 — `writer_task` is 110 lines with mixed concerns [minor]
**File:** `src/capture.rs:184-293`

`writer_task` handles: recovery, file opening, the select loop, rotation logic, entry writing, shutdown draining, metric updates, and compression dispatch. This is the longest function in the crate. Consider extracting:
- The rotation block (lines 236-269) into a `rotate_file()` method
- The shutdown block (lines 274-289) into a `shutdown()` helper

Both blocks share nearly identical fsync-drop-finalize-compress sequences that differ only in whether compression is awaited.

### F5.34 — Duplicate fsync-drop-finalize-compress pattern [minor]
**File:** `src/capture.rs:242-256` vs `275-289`

The rotation path and shutdown path both do:
```
sync_data → drop file → finalize_file → compress_file
```
This is repeated almost verbatim. Extracting a `close_segment(of: OpenFile, await_compress: bool)` function would eliminate the duplication.

### F5.35 — Test file helper functions duplicate reader module logic [minor]
**File:** `tests/integration.rs:24-103`

The test file defines `is_wirecap_file`, `read_all_records`, `count_wcap_files`, `count_raw_wcap_files`, `count_zst_files`, `count_active_files` -- six helper functions that largely replicate logic from `reader.rs` (`is_wcap_file`, `discover_files`). The test helpers also have subtly different matching logic (tests include `.recovered.zst`, the library `is_wcap_file` does not). This divergence is a maintenance risk.

### F5.36 — Integration test file should use `#[cfg(test)] mod tests` structure [nit]
**File:** `tests/integration.rs`

The integration test file defines helper functions at module scope, then individual `#[tokio::test]` functions. While valid for integration tests (they are always test-only), grouping related tests into sub-modules (e.g., `mod rotation_tests`, `mod header_tests`) would improve organization as the test suite grows.

---

## 10. Clippy Pedantic Lint Audit

### F5.37 — `clippy::cast_possible_truncation` is suppressed rather than handled [minor]
**File:** `src/format.rs:71-72,75-76,84-87`

Four `#[allow(clippy::cast_possible_truncation)]` annotations suppress warnings for `len() as u8`, `len() as u16`, `len() as u32`. These casts can silently truncate. The write path should either:
- Use `u8::try_from(len).map_err(...)` to fail gracefully, or
- Assert the length fits (with a clear error message), or
- At minimum, document the maximum lengths accepted.

For `instance_id` (cast to `u8`), strings longer than 255 bytes silently corrupt the file.

### F5.38 — `clippy::module_name_repetitions` would flag `WcapReader` and `WcapTailer` [nit]
**File:** `src/reader.rs:91,160`

With `clippy::pedantic`, the types `WcapReader` in module `reader` and `WcapTailer` in module `reader` would trigger `module_name_repetitions`. Since they are re-exported at the crate root, the full path becomes `wirecap::WcapTailer` which is fine, but within the module they are `reader::WcapReader` which is redundant. This is a stylistic preference -- suppressing this lint is common.

### F5.39 — `clippy::must_use_candidate` for multiple public functions [minor]
**Files:** `src/format.rs:23,30`; `src/reader.rs:27,49,290,294`

Pedantic clippy would flag these functions as missing `#[must_use]`:
- `Dir::from_u8()` -- caller could silently discard the `Option`
- `Dir::as_str()` -- pure function, result should be used
- `discover_files()` -- returns a `Result<Vec<PathBuf>>` the caller should use
- `find_active_file()` -- returns `Option<PathBuf>`
- `WcapTailer::is_open()` -- pure query
- `WcapTailer::current_path()` -- pure query

### F5.40 — `clippy::needless_pass_by_value` on `write_entry` [nit]
**File:** `src/capture.rs:297`

```rust
fn write_entry(current: &mut Option<OpenFile>, entry: &Entry, metrics: &MetricHandles)
```

This is actually fine (takes references), but `metrics` could be passed as part of a struct context rather than a separate argument. Not a clippy issue per se, but the three-argument internal function suggests the writer state should be a struct with methods.

### F5.41 — `clippy::similar_names` would flag `meta_len` / `payload_len` [nit]
**File:** `src/format.rs:84-87`

Pedantic clippy flags variables with similar names. `meta_len` and `payload_len` are clear enough that this would be an allowed suppression, but it is worth noting.

### F5.42 — `clippy::missing_errors_doc` on all public fallible functions [minor]
**Files:** Multiple

None of the public functions that return `Result` or `Option` document their error conditions with `# Errors` sections:
- `Capture::log()` -- when does it return `Err(CaptureClosed)`?
- `WcapReader::open()` -- what errors can occur?
- `discover_files()` -- what IO errors?
- `write_file_header()`, `write_record()`, `read_file_header()`, `read_record()` -- what errors?

### F5.43 — `clippy::missing_panics_doc` on functions containing `.expect()` [minor]
**File:** `src/format.rs:152,153,169,170,195,196`

The `read_record_v*` functions call `.try_into().expect("N bytes")` which can theoretically panic (though the slice lengths are compile-time correct). Pedantic clippy would flag the lack of `# Panics` documentation. Since these cannot actually panic (the slice sizes are statically correct), consider using `unwrap()` or adding a brief doc note.

---

## Additional Observations

### F5.44 — `use` imports are not grouped by convention [nit]
**File:** `src/capture.rs:1-11`

Rust convention (enforced by `rustfmt` with `group_imports = "StdExternalCrate"`) groups imports as: (1) std, (2) external crates, (3) crate-local. The current code mixes them loosely. While `rustfmt` with default settings does not enforce grouping, enabling `group_imports` in `rustfmt.toml` would keep them organized.

### F5.45 — `compress_file` uses IIFE pattern for error handling [nit]
**File:** `src/capture.rs:382-389`

```rust
let result = (|| -> std::io::Result<()> {
    // ...
    Ok(())
})();
```

This immediately-invoked closure is used to get `?` ergonomics inside a function that does not return `Result`. This is a recognized Rust pattern but is less common than extracting a named helper function (e.g., `fn compress_file_inner(path: &Path) -> io::Result<()>`). The named helper would be more readable and debuggable (it would appear in stack traces with a meaningful name).

### F5.46 — `is_wcap_file` is private but could be a method on a `WcapExtension` enum [nit]
**File:** `src/reader.rs:19-24`

The function checks for four different extensions. If the set of recognized extensions grows, this becomes a maintenance burden. An enum with a `from_path(path: &Path) -> Option<WcapExtension>` method would be more extensible, but this is over-engineering for four variants.

### F5.47 — Inconsistent error handling style: `anyhow` vs `std::io::Error` [minor]
**Files:** `src/reader.rs:101` vs `src/format.rs:103`

`WcapReader::open()` returns `anyhow::Result<Self>` while `read_file_header()` returns `std::io::Result<(String, String)>`. The `anyhow` dependency is used only for these reader open paths. This mixes two error philosophies in the same crate. Either commit to `anyhow` for all fallible public APIs, or use typed errors / `std::io::Error` throughout. For a library crate, typed errors are generally preferred over `anyhow` (which is designed for applications).

---

## Severity Summary

| Severity | Count | Finding IDs |
|----------|-------|-------------|
| Critical | 1     | F5.19 |
| Major    | 2     | F5.6, F5.28 |
| Minor    | 22    | F5.1, F5.4, F5.7, F5.8, F5.9, F5.10, F5.11, F5.13, F5.14, F5.15, F5.16, F5.17, F5.21, F5.22, F5.23, F5.24, F5.25, F5.26, F5.29, F5.30, F5.31, F5.33, F5.34, F5.35, F5.37, F5.39, F5.42, F5.43, F5.47 |
| Nit      | 12    | F5.2, F5.3, F5.5, F5.12, F5.18, F5.20, F5.27, F5.32, F5.36, F5.38, F5.40, F5.41, F5.44, F5.45, F5.46 |

**Top 5 most impactful items to address:**
1. **F5.19** [critical] -- Restructure the fsync-drop-finalize pattern using destructuring to make the intent clear and avoid partial-move confusion.
2. **F5.6** [major] -- Add validation for `CaptureConfig` fields to prevent zero-capacity panics and infinite rotation loops.
3. **F5.28** [major] -- Add crate-level documentation in `lib.rs`.
4. **F5.37** [minor] -- Replace `#[allow(clippy::cast_possible_truncation)]` with actual bounds checking to prevent silent data corruption.
5. **F5.47** [minor] -- Settle on a single error strategy; prefer typed errors over `anyhow` for a library crate.
