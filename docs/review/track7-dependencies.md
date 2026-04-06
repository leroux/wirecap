# Track 7: Dependencies Review

**Crate**: `wirecap` v0.1.0
**Date**: 2026-04-06
**Reviewer**: Claude (automated)
**Scope**: Dependency necessity, currency, feature flags, MSRV, transitive weight

---

## Summary

The crate has 7 direct runtime dependencies resolving to ~29 unique transitive
crates (plus ~5 additional dev-only crates). The dependency tree is moderate but
has clear reduction opportunities. Two dependencies (`anyhow`, `chrono`) should
be removed entirely, one (`rand`) is outdated and overkill, and one (`metrics`)
should be feature-gated. The crate lacks an MSRV declaration.

---

## 1. Necessity Audit

### 1.1 `anyhow` = "1" -- REMOVE

**Severity**: [major]

Used in exactly two function signatures in `reader.rs`:

- `WcapReader::open()` (line 101): `pub fn open(path: &Path) -> anyhow::Result<Self>`
- `open_raw_wcap()` (line 305): `fn open_raw_wcap(path: &Path) -> anyhow::Result<...>`

Both functions only propagate `std::io::Error` via `?`. There is no `anyhow::Context`,
no `anyhow::bail!`, no `.context()` calls -- just plain `?` on `io::Result` values.
These are trivially replaceable with `std::io::Result`.

**Why it matters**: `anyhow::Result` in a library's public API (`WcapReader::open`) is
an anti-pattern. It erases the concrete error type, preventing callers from matching on
specific error kinds. Libraries should expose typed errors; `anyhow` is for applications.

**Recommendation**: Replace both signatures with `std::io::Result<T>` and remove the
`anyhow` dependency entirely. This was already flagged in Track 2 (T2-3.1/3.2).


### 1.2 `chrono` = "0.4" -- REPLACE or REMOVE

**Severity**: [major]

Used in a single location -- `capture.rs` line 316-318:

```rust
let now = Utc::now();
let timestamp = now.format("%Y-%m-%dT%H%M%S");
let millis = now.timestamp_subsec_millis();
```

This generates a timestamp string for filenames. The `chrono` crate pulls in default
features including `iana-time-zone` (which brings `core-foundation-sys` on macOS),
`num-traits`, and `autocfg` -- 4 transitive crates for a single `strftime` call.

**Alternatives** (in order of preference):

1. **`time` crate** (`time = { version = "0.3", features = ["formatting"] }`): Lighter,
   actively maintained, no C dependencies. The `OffsetDateTime::now_utc()` plus
   `format_description!` macro covers this use case.

2. **Pure `std`**: Use `std::time::SystemTime::now()` to get seconds-since-epoch, then
   manually decompose into Y-M-D-HMS. Eliminates the dependency entirely at the cost of
   ~15 lines of date arithmetic (or use a small helper).

3. **Minimize features**: At minimum, use `chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }` to drop the `iana-time-zone`, `winapi`, `wasm-bindgen`, `oldtime`, and `js-sys` feature chains. This eliminates `core-foundation-sys`
   and `iana-time-zone` from the tree.

**Recommendation**: Replace with the `time` crate or use `std` directly. For a filename
timestamp, pulling in `chrono` with all default features is disproportionate.


### 1.3 `rand` = "0.8" -- REPLACE with lighter alternative

**Severity**: [minor]

Used in a single location -- `capture.rs` line 407:

```rust
let n: u32 = rand::Rng::r#gen(&mut rand::thread_rng());
```

This generates 4 random bytes for an 8-hex-char run ID. The full `rand` 0.8 with default
features pulls in `rand_chacha`, `rand_core`, `getrandom`, `ppv-lite86`, `zerocopy`,
and `libc` -- 7 transitive crates for 4 bytes of randomness.

**Alternatives**:

1. **`getrandom` directly**: `getrandom::fill(&mut buf)` on a `[u8; 4]` buffer. One
   dependency instead of seven. This is what `rand` ultimately calls anyway.

2. **`rand` with `small_rng`**: If `rand` must stay, use
   `rand = { version = "0.8", default-features = false, features = ["small_rng", "std"] }`
   to avoid `rand_chacha` and its subtree.

**Recommendation**: Replace with `getrandom` (a single 4-byte fill). This removes
6 transitive crates.


### 1.4 `metrics` = "0.24" -- Feature-gate it

**Severity**: [minor]

Used throughout `capture.rs` for Prometheus-style counters and gauges (lines 7, 49-79,
149-153, 233, 292, 305). The `MetricHandles` struct uses `Counter` and `Gauge` types.
The `metrics` crate is lightweight (only `ahash` as a transitive dep), but it is not
universally needed -- many consumers of a wire capture library do not want Prometheus
metrics.

The code already notes: "Noop if no recorder is installed" (line 46). This is good
design, but consumers still pay the compile cost and the unconditional metric
registration calls.

**Recommendation**: Make `metrics` an optional dependency behind a `metrics` feature
flag, with the feature enabled by default:

```toml
[features]
default = ["metrics"]
metrics = ["dep:metrics"]
```

Guard the `MetricHandles` code with `#[cfg(feature = "metrics")]` and provide a no-op
stub when disabled. This lets lightweight consumers opt out.


### 1.5 `tokio` = { version = "1", features = ["rt", "sync", "time", "macros"] }

**Severity**: [nit]

Feature usage analysis:

| Feature | Used for | Location |
|---------|----------|----------|
| `rt` | `spawn_blocking` | capture.rs:249, 282 |
| `sync` | `mpsc::channel`, `mpsc::Sender`, `mpsc::Receiver` | capture.rs:8, 102, 185 |
| `time` | `interval`, `MissedTickBehavior` | capture.rs:209-210 |
| `macros` | `select!` | capture.rs:213 |

All four features are actively used. The selection is correct and minimal.

**Dev-dependency note**: The `[dev-dependencies]` section lists
`tokio = { version = "1", features = ["rt", "macros"] }`. This is fine -- Cargo merges
features when both `[dependencies]` and `[dev-dependencies]` reference the same crate.
However, the dev-dependency entry is redundant because the main `[dependencies]` already
enables a superset of features (`rt`, `sync`, `time`, `macros`). The dev-dependency line
can be removed without effect.

The tests use `#[tokio::test]` which defaults to `current_thread` flavor, plus
`tokio::spawn` and `tokio::time::sleep`. The `current_thread` runtime (provided by `rt`)
is sufficient; `rt-multi-thread` is not needed and is correctly absent.


### 1.6 `tracing` = "0.1"

**Severity**: none (appropriate)

Used in `capture.rs` (lines 9, 111, 205, 246, 254, 287, 293, 380, 394, 396, 402) and
`reader.rs` (lines 10, 112, 143, 195, 203, 208, 249, 269, 282). Macros used: `info!`,
`warn!`, `error!`, `debug!`.

`tracing` is the Rust ecosystem standard for structured logging. Its default features
(`std`, `attributes`) are appropriate. No changes needed.


### 1.7 `zstd` = "0.13"

**Severity**: [nit]

Used for compression in `capture.rs` (line 385: `zstd::Encoder::new`) and decompression
in `reader.rs` (line 106: `zstd::Decoder::new`) and `integration.rs` (line 49). This is
core functionality.

The default features include `legacy` (old zstd format support) and `zdict_builder`
(dictionary training), neither of which wirecap uses. These could be disabled:

```toml
zstd = { version = "0.13", default-features = false, features = ["arrays"] }
```

However, the zstd C library is compiled once regardless of features, so the practical
compile-time savings are negligible. The binary size reduction would also be minimal.

**Recommendation**: No action required. The default features are harmless.


### 1.8 `tempfile` = "3" (dev-dependency)

**Severity**: none (appropriate)

Used in all integration tests for temporary directory creation. Standard choice.

---

## 2. Version Currency

| Crate | Pinned | Resolved | Latest (as of 2026-04) | Status |
|-------|--------|----------|------------------------|--------|
| `tokio` | `"1"` | 1.51.0 | 1.51.x | Current |
| `tracing` | `"0.1"` | 0.1.44 | 0.1.x | Current |
| `metrics` | `"0.24"` | 0.24.3 | 0.24.x | Current |
| `chrono` | `"0.4"` | 0.4.44 | 0.4.x | Current (but see 1.2 above) |
| `anyhow` | `"1"` | 1.0.102 | 1.0.x | Current (but should be removed) |
| `zstd` | `"0.13"` | 0.13.3 | 0.13.x | Current |
| `rand` | `"0.8"` | 0.8.5 | **0.9.x** | **Outdated** |
| `tempfile` | `"3"` | 3.27.0 | 3.x | Current |

### 2.1 `rand` 0.8 vs 0.9

**Severity**: [minor]

`rand` 0.9 was released with breaking changes. The code at line 407 uses:

```rust
rand::Rng::r#gen(&mut rand::thread_rng())
```

In `rand` 0.9, `Rng::gen` was renamed to `Rng::random` (the `gen` name conflicted with
the `gen` keyword reserved in Rust edition 2024). The current code uses `r#gen` to
escape the keyword, which is a workaround that works on 0.8 but signals the API is
already deprecated-in-spirit.

This is moot if `rand` is replaced with `getrandom` as recommended in 1.3.

---

## 3. Feature Flags

### 3.1 Crate does not define any features

**Severity**: [major]

The `[features]` table is entirely absent from `Cargo.toml`. For a library crate,
this is a missed opportunity. Recommended feature structure:

```toml
[features]
default = ["metrics", "compression"]
metrics = ["dep:metrics"]
compression = ["dep:zstd"]
```

At minimum, `metrics` should be optional (see 1.4). Making `zstd` optional is more
debatable since compression is a core capability, but some consumers may want the raw
write path only.


### 3.2 `chrono` default features are over-broad

**Severity**: [minor]

As noted in 1.2, `chrono = "0.4"` enables all default features:
`alloc, clock, default, iana-time-zone, js-sys, now, oldtime, std, wasm-bindgen, wasmbind, winapi, windows-link`.

The only functionality used is `Utc::now()` and `DateTime::format()`, which require
only `clock` and `std`. If chrono is retained (not recommended), features should be
restricted.

---

## 4. MSRV

### 4.1 No `rust-version` field declared

**Severity**: [minor]

The `[package]` section does not specify `rust-version`. The crate uses `edition = "2021"`
which requires Rust 1.56+, but the actual MSRV is higher due to dependencies and
language features used:

- `is_some_and` (stable in 1.70)
- `is_none_or` (stable in 1.82)
- `tokio` 1.51 requires Rust 1.70+
- `chrono` 0.4.44 requires Rust 1.61+
- `metrics` 0.24 requires Rust 1.70+
- `zstd` 0.13 requires Rust 1.73+

The effective MSRV is **Rust 1.82** (due to `Option::is_none_or` in reader.rs line 75).

**Recommendation**: Add `rust-version = "1.82"` to `[package]`. This helps downstream
consumers and CI catch incompatibilities early. Also consider whether the `is_none_or`
usage could be replaced with `.map_or(true, |x| ...)` to lower the MSRV to 1.73 if
broader compatibility is desired.

---

## 5. Transitive Dependency Weight

### Full dependency tree (runtime only)

```
wirecap v0.1.0
  anyhow              (1 crate)     -- REMOVE
  chrono              (4 crates)    -- REMOVE/REPLACE
    iana-time-zone
      core-foundation-sys
    num-traits
      [build] autocfg
  metrics             (4 crates)
    ahash
      cfg-if
      once_cell
      zerocopy
      [build] version_check
  rand                (7 crates)    -- REPLACE with getrandom (1 crate)
    libc
    rand_chacha
      ppv-lite86
        zerocopy (shared)
      rand_core
        getrandom
          cfg-if (shared)
          libc (shared)
  tokio               (5 crates, proc-macro)
    pin-project-lite
    tokio-macros
      proc-macro2, quote, syn, unicode-ident
  tracing             (5 crates, proc-macro)
    pin-project-lite (shared)
    tracing-attributes
      proc-macro2, quote, syn (shared)
    tracing-core
      once_cell (shared)
  zstd                (3 crates + C build deps)
    zstd-safe
      zstd-sys
        [build] cc, pkg-config, jobserver, shlex, find-msvc-tools
```

**Total unique runtime crates**: ~29 (deduplicated)
**After recommended removals**: ~17 (removing anyhow, chrono subtree, rand subtree;
adding getrandom which is already in the tree)

### Shared crates (positive)

Several crates are shared across subtrees, reducing actual weight:
- `proc-macro2`, `quote`, `syn`, `unicode-ident` -- shared by `tokio-macros` and `tracing-attributes`
- `pin-project-lite` -- shared by `tokio` and `tracing`
- `once_cell` -- shared by `ahash` and `tracing-core`
- `cfg-if`, `libc` -- shared by `getrandom`, `rand`, and `zstd` build

### Duplicate crate versions

`cargo tree -d` shows one duplicate: `getrandom` appears at both v0.2.17 (via `rand`)
and v0.4.2 (via `tempfile`, dev-only). This is a dev-dependency-only duplication and
does not affect consumers. If `rand` is replaced with direct `getrandom` v0.4, this
duplication resolves.

---

## 6. Dev-Dependency Concerns

### 6.1 Redundant `tokio` in `[dev-dependencies]`

**Severity**: [nit]

```toml
[dependencies]
tokio = { version = "1", features = ["rt", "sync", "time", "macros"] }

[dev-dependencies]
tokio = { version = "1", features = ["rt", "macros"] }
```

The dev-dependency entry requests a strict subset of features already enabled by the
main dependency. Cargo unifies features, so the dev-dependency line has no effect. It
should be removed to avoid confusion about which features tests actually need.

---

## 7. Action Items (Prioritized)

| # | Severity | Item | Crates Removed | Effort |
|---|----------|------|----------------|--------|
| 1 | [major] | Remove `anyhow`, use `std::io::Result` | 1 | Low |
| 2 | [major] | Remove `chrono`, replace with `time` or `std` | 4 | Low |
| 3 | [major] | Add `[features]` table; feature-gate `metrics` | 0 | Medium |
| 4 | [minor] | Replace `rand` with `getrandom` | 6 | Low |
| 5 | [minor] | Add `rust-version = "1.82"` to `[package]` | 0 | Trivial |
| 6 | [nit] | Remove redundant `tokio` dev-dependency | 0 | Trivial |
| 7 | [nit] | Restrict `chrono` default features (if kept) | 2 | Trivial |

**Net effect of items 1+2+4**: Remove 11 transitive crates from the dependency tree,
reducing the runtime footprint from ~29 to ~17 unique crates (after adding `getrandom`
v0.4 or `time`).
