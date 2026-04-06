# Track 1: API Design & Ergonomics Review

**Crate**: `wirecap` (v0.1.0)
**Scope**: Public API surface, ownership semantics, type design, module structure
**Date**: 2026-04-06
**Reviewer**: Claude (automated)

---

## 1. Public API Surface Audit

### 1.1 [major] `format` module is fully public, exposing wire-level internals

`lib.rs:2` declares `pub mod format`, making every `pub` item inside it part of
the crate's stable API:

- `MAGIC`, `FILE_VERSION`, `RECORD_VERSION`, `RECORD_HEADER_SIZE` (constants)
- `write_file_header`, `write_record`, `read_file_header`, `read_record` (functions)
- `Dir`, `Entry` (types)

The constants `RECORD_VERSION`, `RECORD_HEADER_SIZE`, and `FILE_VERSION` are
implementation details of the binary format. Exposing them invites downstream
code to depend on values that must change when the format evolves (e.g., a
hypothetical v4 record). Similarly, `write_file_header` and `write_record` are
meant to be called only by the internal `writer_task`; making them public lets
callers produce malformed files (e.g., writing records without a header, or
writing a header with a mismatched `instance_id`).

The integration tests at `tests/integration.rs:55-58` call
`wirecap::format::read_file_header` and `wirecap::format::read_record` directly,
which confirms users currently depend on this leakage -- but only because
`WcapReader` is not re-exported (see finding 1.2).

**Recommendation**: Make `format` a `pub(crate)` module. Re-export only the
types consumers actually need (`Dir`, `Entry`) from `lib.rs`, which is already
done at `lib.rs:6`. For users who need low-level record access (e.g., custom
tooling), consider a `format::raw` sub-module gated behind a `raw-format` cargo
feature, or expose the needed subset through `WcapReader`.

### 1.2 [major] `WcapReader`, `discover_files`, `find_active_file` are not re-exported

`lib.rs:7` re-exports only `WcapTailer` from the `reader` module.
`WcapReader` -- the primary way to read closed `.wcap` / `.wcap.zst` files --
is missing from the root, as are the two discovery functions. Users must write
`wirecap::reader::WcapReader` instead of `wirecap::WcapReader`.

The integration tests work around this by manually re-implementing file reading
with raw `format::*` calls (`tests/integration.rs:32-66`), which is both a sign
of the missing export and a source of duplicated logic.

**Recommendation**: Add to `lib.rs`:
```rust
pub use reader::{WcapReader, discover_files, find_active_file};
```
This brings the read path to parity with the write path (`Capture`,
`CaptureConfig`, `CaptureClosed` are all re-exported).

### 1.3 [minor] `WcapReader` swallows read errors in its `Iterator` impl

`reader.rs:141-143` -- when `read_record` returns `Err`, the iterator emits a
`warn!` log and then returns `None`, indistinguishable from a clean EOF:

```rust
Err(e) => {
    warn!(error = %e, "wcap read error");
    self.done = true;
    None
}
```

This is a silent data-loss vector: a truncated file or disk error will be logged
but the caller has no way to detect it programmatically. The standard Rust idiom
for fallible iteration is `Iterator<Item = Result<T, E>>`.

**Recommendation**: Change the associated type to
`type Item = Result<Entry, std::io::Error>`. Callers who want the
fire-and-forget behaviour can use `.filter_map(Result::ok)`.

### 1.4 [minor] `CaptureClosed` does not implement common error traits fully

`capture.rs:162-171` -- `CaptureClosed` implements `Display` and `Error` but not
`From` for `Box<dyn Error>`, `Send`, or `Sync`. While `Send + Sync` are
auto-derived for this unit-like struct, it also lacks `Clone`, `Copy`,
`PartialEq`, and `Eq`, which are trivially derivable and useful for pattern
matching in tests.

**Recommendation**:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureClosed;
```

---

## 2. Ownership & Borrowing

### 2.1 [major] `Capture::log` takes `Entry` by value, forcing allocation on every call

`capture.rs:148` -- `pub async fn log(&self, entry: Entry)`. The `Entry` struct
(`format.rs:40-60`) owns two `Vec<u8>` fields (`meta` and `payload`). Every
call to `log` therefore requires the caller to allocate and fill two `Vec`s,
even though the data often already exists as a borrowed slice (e.g., from a
network buffer, a `bytes::Bytes` handle, or a `serde_json::to_vec` output that
the caller would otherwise keep).

Because `Entry` is sent across a `tokio::mpsc` channel, it does need to be
`'static` and owned. However, the allocation burden should be pushed as close to
the channel boundary as possible, not imposed on every call site.

**Recommendation (tiered)**:

1. *Minimal change*: Accept `&[u8]` for meta and payload in a builder or
   constructor on `Entry`, and perform the `to_vec()` inside. This makes the
   allocation explicit and local.
2. *Zero-copy path*: Swap `Vec<u8>` to `bytes::Bytes` in `Entry`. Callers with
   a `Bytes` handle (common in Tokio network stacks) pay zero allocation.
   `write_record` already writes via `&[u8]` slice, and `Bytes` derefs to
   `&[u8]`. The `bytes` crate is already a transitive dependency of `tokio`.
3. *Borrow-first API*: Introduce a `log_raw(&self, ts: u64, ..., meta: &[u8],
   payload: &[u8])` method that builds the `Entry` internally. This avoids
   exposing `Entry` on the write path entirely.

### 2.2 [minor] `write_record` borrows `Entry` but `log` consumes it

`format.rs:83` takes `entry: &Entry`, but `capture.rs:148` takes `entry: Entry`
by value, which is then sent over the channel and eventually passed to
`write_entry` as `&entry` (`capture.rs:271`). This is semantically correct (the
channel needs ownership), but the asymmetry means the caller cannot reuse an
`Entry` template without cloning. `Entry` does not derive `Clone`, making reuse
impossible.

**Recommendation**: Derive `Clone` on `Entry` (it is just scalars + `Vec`s, so
`Clone` is both correct and expected). Alternatively, if `Bytes` is adopted per
2.1, `Clone` becomes cheap (reference-counted).

---

## 3. Type Design

### 3.1 [major] Single `Entry` type conflates read and write concerns

`format.rs:40-60` -- `Entry` is used for both writing (v3 records) and reading
(v1/v2/v3 records). This leads to `Option<u64>` for `mono_ns` and `recv_seq`
(`format.rs:44-48`), which are only `None` when reading legacy files. On the
write path, these fields are always `Some`, but nothing in the type system
enforces this. `write_record` silently coerces `None` to `0`
(`format.rs:90-91`), masking a bug where a caller forgets to set them.

**Recommendation**: Introduce separate types:

```rust
/// Used by Capture::log -- all fields required.
pub struct WriteEntry {
    pub ts: u64,
    pub mono_ns: u64,
    pub recv_seq: u64,
    pub src: u8,
    pub dir: Dir,
    pub meta: Vec<u8>,   // or Bytes
    pub payload: Vec<u8>, // or Bytes
}

/// Returned by WcapReader -- legacy fields are optional.
pub struct ReadEntry {
    pub ts: u64,
    pub mono_ns: Option<u64>,
    pub recv_seq: Option<u64>,
    pub src: u8,
    pub dir: Dir,
    pub meta: Vec<u8>,
    pub payload: Vec<u8>,
}
```

If the type split is too disruptive, a lighter alternative is to have `log()`
accept a type where `mono_ns` and `recv_seq` are `u64` (not `Option`), and
convert internally.

### 3.2 [minor] `Dir` is missing standard trait impls

`format.rs:15-16` derives `Debug, Clone, Copy, PartialEq, Eq` but not `Hash`,
`PartialOrd`, `Ord`, or `serde::Serialize`/`Deserialize`. For a two-variant
enum that is `Copy` and `Eq`, `Hash` is essentially free and commonly expected
(enables use as a `HashMap` key). `Display` is also missing -- `as_str` exists
but `format!("{dir}")` does not work.

**Recommendation**:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dir { ... }

impl std::fmt::Display for Dir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
```

### 3.3 [nit] `Dir::from_u8` should be `TryFrom<u8>`

`format.rs:23-29` -- `Dir::from_u8` is an ad-hoc conversion. The idiomatic Rust
approach is `impl TryFrom<u8> for Dir`, which integrates with the `?` operator
and the broader `From`/`Into` ecosystem. The existing `from_u8` can be kept as a
convenience alias if desired.

### 3.4 [nit] `Entry` is missing `Debug`

`format.rs:40` -- `Entry` has no `Debug` impl. This makes it impossible to use
in `assert_eq!`, `dbg!()`, or tracing `?field` format. Given that `payload` can
be large, a manual `Debug` impl that truncates payload display would be ideal,
but even a derived `Debug` is better than none.

---

## 4. Builder Pattern

### 4.1 [major] `CaptureConfig` has all-pub fields, no validation, no forward-compatible path

`capture.rs:26-32`:
```rust
pub struct CaptureConfig {
    pub instance_id: String,
    pub output_dir: String,
    pub channel_capacity: usize,
    pub max_file_bytes: u64,
    pub max_file_secs: u64,
}
```

All fields are `pub`, so downstream code can construct the struct directly,
bypassing `CaptureConfig::new` and its defaults. This has three consequences:

1. **No validation**: `channel_capacity: 0` or `max_file_bytes: 0` will cause
   pathological behaviour (an unbounded loop of empty rotated files, or a
   channel that can never send). There is no check anywhere.
2. **Forward-incompatibility**: Adding a new field is a breaking change because
   callers constructing the struct literally (as in `tests/integration.rs:257-264`)
   will fail to compile. This is the textbook motivator for the builder pattern
   or `#[non_exhaustive]`.
3. **Test coupling**: The integration tests already construct `CaptureConfig`
   with literal fields to override defaults (`tests/integration.rs:257`),
   showing that the builder pattern would be actively used.

**Recommendation**: Either use `#[non_exhaustive]` + builder methods:

```rust
#[non_exhaustive]
pub struct CaptureConfig { /* private fields */ }

impl CaptureConfig {
    pub fn new(instance_id: impl Into<String>, output_dir: impl Into<PathBuf>) -> Self { ... }
    pub fn channel_capacity(mut self, n: usize) -> Self { self.channel_capacity = n; self }
    pub fn max_file_bytes(mut self, n: u64) -> Self { self.max_file_bytes = n; self }
    pub fn max_file_secs(mut self, n: u64) -> Self { self.max_file_secs = n; self }
    pub fn build(self) -> Result<Self, ConfigError> { /* validate */ }
}
```

Or use the `typed-builder` / `derive_builder` crate for less boilerplate. At
minimum, `#[non_exhaustive]` on the struct prevents direct construction outside
the crate and preserves semver compatibility.

### 4.2 [minor] `Entry` construction is verbose and error-prone

Every call site (`tests/integration.rs:6-15`, `tests/integration.rs:197-205`)
must spell out all seven fields, including the ceremony of `Some(ts)` for
`mono_ns` and `Some(0)` for `recv_seq`. A constructor or builder would reduce
boilerplate:

```rust
Entry::new(ts, src, dir, payload)
    .with_meta(meta)
    .with_mono_ns(mono_ns)
    .with_recv_seq(recv_seq)
```

This becomes even more valuable if `WriteEntry` / `ReadEntry` are introduced
(finding 3.1).

---

## 5. Module Structure

### 5.1 [minor] `mod capture` is private but fully re-exported -- inconsistent with `pub mod reader`

`lib.rs:1` declares `mod capture` (private), then re-exports its three public
types. `lib.rs:3` declares `pub mod reader`, making the entire module public.
The result: `wirecap::capture::Capture` does not exist, but
`wirecap::reader::WcapReader` does.

This is not wrong per se, but it is inconsistent. The private-module-with-
re-export pattern is generally preferred for a flat public API, while `pub mod`
is preferred when the module itself is part of the API namespace. Currently it
is a mix of both.

**Recommendation**: Pick one strategy. The cleanest option for a library this
size is to make both modules private and re-export all public types from
`lib.rs`:

```rust
mod capture;
mod format;
mod reader;

pub use capture::{Capture, CaptureClosed, CaptureConfig};
pub use format::{Dir, Entry};
pub use reader::{WcapReader, WcapTailer, discover_files, find_active_file};
```

This gives a flat `wirecap::*` namespace and hides module boundaries from
consumers.

### 5.2 [minor] `write_record` and `write_file_header` are public but only used internally

`format.rs:63` and `format.rs:83` -- both functions are `pub` and, because
`format` is a `pub` module, fully reachable by external code. However, the only
caller of `write_file_header` is `capture.rs:328` (`open_file`) and the only
caller of `write_record` is `capture.rs:299` (`write_entry`). No external
consumer should call these directly -- doing so risks producing corrupt files.

**Recommendation**: If `format` remains `pub`, change these to `pub(crate)`.
If `format` is made private (finding 1.1), `pub` is fine since it is
crate-scoped anyway.

---

## 6. Missing APIs

### 6.1 [major] No synchronous / non-async write path

Writing is only possible through `Capture::log`, which is `async` and requires a
running Tokio runtime plus the background `writer_task`. There is no
`WcapWriter` for synchronous use cases:

- CLI tools that convert or merge `.wcap` files
- Test harnesses that need to produce fixture files without Tokio
- Batch import of historical data

The building blocks exist (`format::write_file_header`, `format::write_record`),
but they are not composed into a safe public API.

**Recommendation**: Introduce a `WcapWriter` struct:

```rust
pub struct WcapWriter<W: Write> {
    writer: W,
    bytes_written: u64,
}

impl<W: Write> WcapWriter<W> {
    pub fn new(writer: W, instance_id: &str, run_id: &str) -> io::Result<Self> { ... }
    pub fn write(&mut self, entry: &Entry) -> io::Result<usize> { ... }
    pub fn flush(&mut self) -> io::Result<()> { ... }
    pub fn into_inner(self) -> W { ... }
}
```

This pairs symmetrically with `WcapReader` and gives `Capture` a clear role as
the "async + rotation + compression" layer on top.

### 6.2 [minor] `WcapReader` is missing metadata accessors and `into_inner`

`reader.rs:91-96` -- `instance_id` and `run_id` are `pub` fields on
`WcapReader`. This is fine for access but prevents `WcapReader` from ever
adding validation or lazy loading of header data. More critically, there is no
`into_inner(self) -> Box<dyn Read>` to reclaim the underlying reader, and no
method to retrieve the file path that was opened.

**Recommendation**: Make `instance_id` and `run_id` private with accessor
methods (`fn instance_id(&self) -> &str`). Add `into_inner` for composability.

### 6.3 [minor] `WcapTailer` has `pub` fields for `instance_id` and `run_id`

`reader.rs:166-167` -- same issue as 6.2. `instance_id` and `run_id` are
`pub Option<String>`. Callers can mutate them, which would desynchronize the
tailer's internal state from reality.

**Recommendation**: Make these private with `fn instance_id(&self) -> Option<&str>`
accessors.

---

## 7. Ergonomic Friction

### 7.1 [major] Inconsistent path types across the API

| API | Path type | Location |
|---|---|---|
| `CaptureConfig::output_dir` | `String` | `capture.rs:28` |
| `CaptureConfig::new` | `impl Into<String>` | `capture.rs:35` |
| `WcapReader::open` | `&Path` | `reader.rs:101` |
| `WcapTailer::new` | `PathBuf` | `reader.rs:171` |
| `discover_files` | `&Path` | `reader.rs:27` |
| `find_active_file` | `&Path` | `reader.rs:49` |
| Internal `open_file` | `&str` | `capture.rs:312` |
| Internal `recover_active_files` | `&str` | `capture.rs:350` |

Using `String` for filesystem paths is an anti-pattern in Rust because it cannot
represent all valid OS paths (non-UTF-8 paths on Unix). The idiomatic type is
`PathBuf` (owned) or `&Path` (borrowed). The inconsistency forces callers to
convert: `tmp.path().to_str().expect("path")` appears 8 times in the
integration tests.

**Recommendation**: Change `CaptureConfig::output_dir` to `PathBuf` and accept
`impl Into<PathBuf>` in the constructor. Update all internal functions to pass
`&Path`. This eliminates every `.to_str().expect("path")` in the test suite.

### 7.2 [minor] `WcapReader::open` returns `anyhow::Result` instead of a typed error

`reader.rs:101` -- `pub fn open(path: &Path) -> anyhow::Result<Self>`. Using
`anyhow` in a library's public API is generally discouraged because it erases
the concrete error type, preventing callers from matching on specific error
variants (e.g., file-not-found vs. bad-magic vs. unsupported-version). It also
forces every consumer to depend on `anyhow`.

The internal errors are all `std::io::Error` (from `File::open`,
`zstd::Decoder::new`, and `read_file_header`). There is no information loss in
returning `io::Result<Self>` directly.

**Recommendation**: Change to `io::Result<Self>` or introduce a
`wirecap::Error` enum that wraps `io::Error` with richer variants:

```rust
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    BadMagic([u8; 4]),
    UnsupportedVersion(u8),
}
```

### 7.3 [minor] `Capture::new` returns an unnamed `impl Future` -- hard to store

`capture.rs:101`:
```rust
pub fn new(config: CaptureConfig) -> (Self, impl std::future::Future<Output = ()>)
```

The writer future's type is unnameable, which means callers cannot store the
`(Capture, writer)` tuple in a struct without boxing or type-erasing:

```rust
// Does not compile:
struct App {
    capture: Capture,
    writer: ???,
}
```

Callers must immediately `tokio::spawn` the writer, which is the intended
pattern. But this rules out embedding `Capture` in a struct that manages its
own lifetime.

**Recommendation**: Return `Pin<Box<dyn Future<Output = ()> + Send>>` (or a
named future type), or document prominently that the writer must be spawned
immediately. A `Capture::spawn(config: CaptureConfig) -> (Self, JoinHandle<()>)`
convenience that calls `tokio::spawn` internally would cover the common case.

### 7.4 [nit] `WcapTailer::read_batch` returns `Vec<Entry>`, allocating on every poll

`reader.rs:218` -- even when there are zero new records, `read_batch` allocates
a fresh `Vec`. In a polling loop (the intended use for a tailer), this creates
allocation pressure on every tick.

**Recommendation**: Accept a `&mut Vec<Entry>` to allow the caller to reuse a
buffer:

```rust
pub fn read_batch_into(&mut self, max_batch: usize, buf: &mut Vec<Entry>) { ... }
```

Or return a `SmallVec` / `ArrayVec` if the batch size is small.

---

## 8. Re-export Completeness

### 8.1 [major] Asymmetric re-exports create a lopsided API

As noted in 1.2, `lib.rs:5-7`:
```rust
pub use capture::{Capture, CaptureClosed, CaptureConfig};
pub use format::{Dir, Entry};
pub use reader::WcapTailer;
```

The write side is fully re-exported. The read side is not: `WcapReader`,
`discover_files`, and `find_active_file` are reachable only via
`wirecap::reader::*`. This creates an uneven API surface:

- `wirecap::Capture` -- exists
- `wirecap::WcapReader` -- does not exist (must use `wirecap::reader::WcapReader`)
- `wirecap::WcapTailer` -- exists
- `wirecap::discover_files` -- does not exist

The likely explanation is that `WcapTailer` was needed by a top-level consumer
and was added ad hoc, while `WcapReader` was used only through the `format`
module's raw functions in tests.

**Recommendation**: Re-export the full reader API from `lib.rs`:
```rust
pub use reader::{WcapReader, WcapTailer, discover_files, find_active_file};
```

### 8.2 [nit] `reader` module's doc comment references types without qualifying them

`reader.rs:1-4`:
```rust
//! Provides [`WcapReader`] for batch reading closed files and
//! [`WcapTailer`] for following a live `.wcap.active` file.
```

These intra-doc links resolve correctly within the module, but if a user
browsing `wirecap` crate docs clicks through to the `reader` module, the links
work. However, because `WcapReader` is not re-exported at the crate root, it
does not appear in the top-level docs at all, making it harder to discover.

---

## Summary Table

| # | Severity | Finding | Location |
|---|----------|---------|----------|
| 1.1 | major | `format` module fully public, leaking wire-level internals | `lib.rs:2` |
| 1.2 | major | `WcapReader`, `discover_files`, `find_active_file` not re-exported | `lib.rs:5-7` |
| 1.3 | minor | `WcapReader` iterator swallows errors | `reader.rs:141-143` |
| 1.4 | minor | `CaptureClosed` missing trivial derive traits | `capture.rs:162` |
| 2.1 | major | `log()` takes owned `Entry` with `Vec<u8>`, forcing allocation per call | `capture.rs:148` |
| 2.2 | minor | `Entry` is not `Clone`, preventing template reuse | `format.rs:40` |
| 3.1 | major | Single `Entry` type conflates read/write; `Option` fields unforced on write | `format.rs:40-60` |
| 3.2 | minor | `Dir` missing `Hash`, `Display` | `format.rs:15-16` |
| 3.3 | nit | `Dir::from_u8` should be `TryFrom<u8>` | `format.rs:23-29` |
| 3.4 | nit | `Entry` missing `Debug` | `format.rs:40` |
| 4.1 | major | `CaptureConfig` all-pub fields, no validation, not forward-compatible | `capture.rs:26-32` |
| 4.2 | minor | `Entry` construction verbose and error-prone | `format.rs:40-60` |
| 5.1 | minor | Inconsistent module visibility strategy (private + re-export vs. pub mod) | `lib.rs:1-3` |
| 5.2 | minor | `write_record` / `write_file_header` pub but only used internally | `format.rs:63, 83` |
| 6.1 | major | No synchronous write path (`WcapWriter`) | -- |
| 6.2 | minor | `WcapReader` missing accessors, `into_inner` | `reader.rs:91-96` |
| 6.3 | minor | `WcapTailer` has mutable pub fields for header data | `reader.rs:166-167` |
| 7.1 | major | Inconsistent path types (`String` vs `&Path` vs `PathBuf`) | `capture.rs:28,35`, `reader.rs:101,171` |
| 7.2 | minor | `WcapReader::open` returns `anyhow::Result` in a library | `reader.rs:101` |
| 7.3 | minor | `Capture::new` returns unnameable `impl Future` | `capture.rs:101` |
| 7.4 | nit | `read_batch` allocates a `Vec` on every poll | `reader.rs:218` |
| 8.1 | major | Asymmetric re-exports: write side complete, read side partial | `lib.rs:5-7` |
| 8.2 | nit | Module docs reference un-re-exported types | `reader.rs:1-4` |

**Critical findings**: 0
**Major findings**: 8 (1.1, 1.2, 2.1, 3.1, 4.1, 6.1, 7.1, 8.1)
**Minor findings**: 11
**Nits**: 4

The most impactful changes in priority order:
1. Fix path types (`String` -> `PathBuf`) -- affects every consumer (7.1)
2. Complete re-exports -- half the API is hidden (1.2, 8.1)
3. Make `format` module `pub(crate)` -- reduces API surface by ~10 items (1.1)
4. Builder / `#[non_exhaustive]` on `CaptureConfig` -- semver hazard (4.1)
5. Split `Entry` into read/write types -- type-safety on the write path (3.1)
6. Add `WcapWriter` for sync use cases -- unblocks CLI tooling (6.1)
7. Accept `&[u8]` or `Bytes` in the hot path -- allocation reduction (2.1)
