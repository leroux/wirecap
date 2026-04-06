# Wirecap — Open Source Split TODO

## 1. Split Mechanics

- [ ] Copy `crates/wirecap/` to `~/wirecap` (or wherever), `git init`
- [ ] De-workspace `Cargo.toml`: replace all `workspace = true` deps with explicit versions
  - tokio, tokio-util, tracing, metrics, chrono, anyhow, serde, serde_json
- [ ] Remove `[lints] workspace = true` (inline or drop)
- [ ] Remove `edition.workspace = true`, set `edition = "2021"` explicitly
- [ ] Add `.gitignore` (target/, *.pyc, __pycache__/, etc.)
- [ ] Remove `py/__pycache__/` from tracked files

## 2. ~~Generalize Source Enum~~ ✅ DONE

Chose option **(a)**: plain `u8` channel ID, no label registry.
Decision log: `docs/decisions/2026-04-06-wirecap-generalize-source.md`

- [x] Replaced `Source` enum with `u8` channel tag in `Entry`, `format.rs`, `read_record`, `write_record`
- [x] Removed `Source::from_u8()` / `Source::as_str()` / `parse_src()`
- [x] Simplified `MetricHandles` to aggregate-only (no per-channel breakdown)
- [x] Dir enum kept as-is (In/Out is universal)
- [x] Updated `dump.rs`, `tail.rs` CLI tools for `u8` channels
- [x] Updated Python reader: removed `Source` IntEnum, `src` is plain `int`

## 3. ~~Bugs to Fix~~ ✅ DONE (fixed by Section 2 generalization)

- [x] SOURCE_COUNT / SOURCE_LABELS OOB — eliminated entirely (aggregate metrics only)
- [x] `by_source` arrays — `dump.rs` now uses `[u64; 256]`, `convert.rs` removed
- [ ] `parse_dir()` returns `std::io::Error` — should be a dedicated error type

## 4. Remove Internal-Only Code

- [x] Removed `wirecap-convert` binary (`src/bin/convert.rs`)
- [x] Removed `serde` and `serde_json` dependencies (only used by convert)

## 5. Python Reader

- [ ] Add v3 record support (currently only handles v1 and v2)
- [ ] Update `Source` enum or document as user-defined `int`
- [ ] Add `mono_ns` and `recv_seq` fields to `Record` dataclass
- [ ] Consider: ship Python reader in the repo or separate package?

## 6. Cargo.toml — Package Metadata

- [ ] Add `description`
- [ ] Add `license` (MIT? Apache-2.0? MIT OR Apache-2.0?)
- [ ] Add `repository` URL
- [ ] Add `keywords` (e.g., capture, replay, binary-log, append-only, wire-protocol)
- [ ] Add `categories` (e.g., data-structures, encoding, network-programming)
- [ ] Set `rust-version` (MSRV)
- [ ] Consider: split lib and bins into separate crates? (`wirecap` lib + `wirecap-cli` bin)
      Bins pull in `clap` which is heavy for library consumers.

## 7. Documentation

- [ ] Add README.md — what it is, why not MCAP, quick start, format spec sketch
- [ ] Add LICENSE file
- [ ] Add rustdoc to public API items (Capture, CaptureConfig, Entry, WcapReader, WcapTailer, format fns)
- [ ] Document the wire format in a SPEC.md or in the README
- [ ] Add examples/ directory (basic write + read, tail, custom channel labels)

## 8. Code Review Items

- [ ] `Capture::log()` takes owned `Entry` — consider `&Entry` or builder pattern
- [ ] `Entry.meta` and `Entry.payload` are `Vec<u8>` — consider `Bytes` or `&[u8]` for zero-copy
- [x] `compress_file()` is sync blocking inside a tokio::spawn — should use `spawn_blocking`
- [ ] `finalize_file()` uses string slicing (`&name[..name.len() - ".active".len()]`) — use
      `strip_suffix` or `Path` methods for robustness
- [ ] `generate_run_id()` uses deprecated `rand::Rng::gen` pattern — update for rand 0.9+
- [ ] `WcapReader` swallows errors as `None` via `warn!` — consider returning `Result<Option<Entry>>`
- [ ] Duplicate JSONL formatting logic in `dump.rs` and `tail.rs` — extract if both ship
- [ ] `open_file` truncates on create — safe because filename includes timestamp, but document why
- [ ] The `drop(of.file)` before `finalize_file` is after a moved `of` via `current.take()` —
      verify this doesn't drop the file handle before fsync completes (it doesn't, but add comment)

## 9. Monorepo Consumer Updates (not part of OSS repo)

After the split, these crates in the monorepo need updating to depend on the published
crate (or git dependency) instead of `path = "../wirecap"`:

- `crates/kalshi` — uses wirecap for WS capture
- `crates/recorder` — primary producer, uses Capture + format
- `crates/cfbenchmarks` — uses wirecap for CFB WS capture
- `crates/kraken` — uses wirecap for Kraken WS capture
- `crates/health-monitor` — uses wirecap (tailer?)
- `crates/sim` — reads wcap files for simulation
- `crates/backtest` — reads wcap files for backtesting
- `crates/kalshi-tools` — uses wirecap
- `experiments/fill-model-calibration` — uses wirecap
- `experiments/sub-perf` — uses wirecap

These consumers all use the domain-specific `Source` enum and will need updating
when it becomes a generic `u8` channel tag. This is a breaking change for the monorepo.

## 10. Nice-to-Have (post-initial-release)

- [ ] CI: GitHub Actions for test + clippy + fmt
- [ ] Publish to crates.io
- [ ] Benchmarks (throughput, latency, compression ratio)
- [ ] `no_std` support for format module (read/write without alloc?)
- [ ] Optional `serde` feature for Entry serialization
- [ ] Index/summary record at end of file (like MCAP) for fast stats without full scan
- [ ] Property-based tests (proptest/quickcheck) for format roundtrip
