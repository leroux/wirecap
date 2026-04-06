# Wirecap TODO

## Done (v0.2.0 rewrite)

- [x] Unified `wirecap::Error` enum (Io, Format, Closed)
- [x] `WriteEntry` / `ReadEntry` type split
- [x] Writer on dedicated OS thread (not tokio::spawn)
- [x] BufWriter wrapping (7-9 syscalls/record → batched)
- [x] `WcapWriter<W>` public sync writer
- [x] `CaptureConfig` builder with validation
- [x] PathBuf throughout (was String)
- [x] Input validation: instance_id (no path separators), meta ≤ u16::MAX, payload ≤ configurable max
- [x] Read-side allocation bomb protection (MAX_READ_PAYLOAD = 256 MB)
- [x] `WcapReader` iterator returns `Result<ReadEntry, Error>` (was swallowing errors)
- [x] All modules private, flat `wirecap::*` re-exports
- [x] Removed `anyhow`, `chrono`, `rand` deps (~12 fewer transitive crates)
- [x] `try_log()` for non-blocking sends
- [x] `Capture::start()` returns `JoinHandle` (was unnameable `impl Future`)
- [x] Dir: added Hash, Display, TryFrom<u8>
- [x] Recovery compression runs in background thread (was blocking startup)
- [x] Writer exits after 100 consecutive I/O failures (was silent black hole)
- [x] bytes_written includes file header
- [x] Python reader: v3 record support, mono_ns/recv_seq fields, _read_exact for short reads
- [x] `finalize_file` uses strip_suffix(".wcap.active") directly
- [x] `recover_active_files` uses background thread for compression

## Testing

- [ ] Property-based tests for format roundtrip (WriteEntry → write_record → read_record → ReadEntry)
- [ ] v1 and v2 record reading tests (backward compat code is currently unverified)
- [ ] Crash recovery test: create .wcap.active, start writer, verify .wcap.recovered.zst
- [ ] WcapTailer tests: try_open, read_batch, partial record handling, rotation detection
- [ ] WcapReader test on .wcap.zst files (compressed read path)
- [ ] Backpressure test: fill channel, verify log() blocks
- [ ] CaptureClosed test: kill writer, verify log() returns Err(Closed)
- [ ] Truncated record test: write partial record, verify reader returns error (not None)
- [ ] Time-based rotation test (max_file_secs)
- [ ] Zero-length payload roundtrip test
- [ ] Large payload test (near 16 MB default max)
- [ ] Concurrent rotation stress test

## Code quality

- [ ] Replace `Box<dyn Read>` in WcapReader with generics (enables monomorphization/inlining)
- [ ] Atomic compression: write to .wcap.zst.tmp, rename on success, clean up .tmp on startup
- [ ] Add `is_healthy()` method to Capture (Arc<AtomicBool> set by writer thread)
- [ ] Consider returning `Result` from `CaptureConfig` builder methods instead of panicking on invalid values
- [ ] Add `#[must_use]` to appropriate types and methods
- [ ] stream_position() in WcapTailer: track position manually instead of lseek(2) per record

## Documentation

- [ ] Add rustdoc to all public API items
- [ ] Add `# Examples` sections to Capture::start, WcapWriter::new, WcapReader::open
- [ ] Add `# Errors` sections to fallible public methods
- [ ] Add crate-level doc comment to lib.rs
- [ ] Update README.md quick start examples for new API (Capture::start, WriteEntry, WcapReader)
- [ ] Update SPEC.md: `Capture::start` not `Capture::new`

## Packaging

- [ ] Add `description`, `license`, `repository`, `keywords`, `categories` to Cargo.toml
- [ ] Add LICENSE file
- [ ] Set `rust-version` (MSRV) — effective minimum is 1.82 (Option::is_none_or)
- [ ] CI: GitHub Actions for test + clippy + fmt
- [ ] Consider publishing to crates.io

## Future (post-initial-release)

- [ ] Benchmarks (throughput, latency, compression ratio)
- [ ] `no_std` support for format module
- [ ] Optional `serde` feature for Entry serialization
- [ ] Per-record CRC32 for corruption detection (format v4)
- [ ] Index/summary record at end of file for fast stats without full scan
- [ ] Configurable BufWriter buffer size
- [ ] Configurable fsync interval
- [ ] Configurable consecutive failure threshold
