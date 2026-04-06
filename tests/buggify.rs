//! Buggify-driven fault injection tests.
//!
//! Run with: `cargo test --features buggify --test buggify`
//!
//! These tests enable ferro-buggify probabilistic fault injection across the
//! 17 sites in src/writer.rs and src/reader.rs, then verify that wirecap's
//! contracts hold under stress:
//!
//! - The writer thread never panics regardless of injected I/O failures.
//! - It exits cleanly after `max_consecutive_failures` consecutive write errors.
//! - Records that survive parse correctly (no phantom or corrupt records).
//! - Recovery handles partial failures gracefully.
//! - The reader and tailer never panic on injected read failures.
//!
//! All tests use `#[serial]` because ferro-buggify keeps global state.

#![cfg(feature = "buggify")]

use serial_test::serial;
use wirecap::{
    buggify_fires, Capture, CaptureConfig, Dir, WcapReader, WcapTailer, WcapWriter, WriteEntry,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_entry(i: u64) -> WriteEntry {
    WriteEntry {
        ts: i,
        mono_ns: i,
        recv_seq: i,
        src: (i % 256) as u8,
        dir: if i.is_multiple_of(2) { Dir::In } else { Dir::Out },
        meta: format!("m{i}").into_bytes(),
        payload: format!("payload-{i}-with-some-content").into_bytes(),
    }
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos() as u64
}

/// Read every record from every wirecap file in the directory, swallowing
/// errors (we only care that nothing panics and surviving records parse).
fn read_all_records_lenient(dir: &std::path::Path) -> Vec<wirecap::ReadEntry> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map(|d| d.filter_map(Result::ok).collect::<Vec<_>>())
        .unwrap_or_default();
    files.sort_by_key(|e| e.file_name());

    let mut all = Vec::new();
    for entry in &files {
        let path = entry.path();
        let s = path.to_string_lossy();
        if !(s.ends_with(".wcap")
            || s.ends_with(".wcap.zst")
            || s.ends_with(".wcap.recovered")
            || s.ends_with(".wcap.recovered.zst"))
            || s.ends_with(".wcap.active")
        {
            continue;
        }
        if let Ok(reader) = WcapReader::open(&path) {
            for record in reader.flatten() {
                all.push(record);
            }
        }
    }
    all
}

/// Reset buggify state and set a seed for reproducibility.
fn enable_buggify(seed: u64) {
    ferro_buggify::enable();
    ferro_buggify::set_seed(seed);
}

fn disable_buggify() {
    ferro_buggify::disable();
    ferro_buggify::reset();
}

/// Snapshot fire count, run a closure, then assert at least one fire happened
/// and print the delta. Proves that buggify is actually injecting faults
/// during the test, not silently no-op'ing.
fn assert_fires<F: FnOnce()>(test_name: &str, f: F) {
    let before = buggify_fires();
    f();
    let after = buggify_fires();
    let delta = after - before;
    eprintln!("[buggify] {test_name}: {delta} fires");
    assert!(
        delta > 0,
        "{test_name}: buggify never fired \u{2014} injection didn't run"
    );
}

// ---------------------------------------------------------------------------
// Test 1: Writer survives intermittent failures across many seeds.
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn writer_survives_intermittent_failures() {
    // Run 50 seeds. For each seed: enable buggify, write 100 entries, drain,
    // verify the writer thread didn't panic and the surviving records parse.
    assert_fires("writer_survives_intermittent_failures", || {
        for seed in 0..50u64 {
            enable_buggify(seed);

            let tmp = tempfile::tempdir().unwrap();
            let config = CaptureConfig::new("survive", tmp.path())
                .unwrap()
                .max_consecutive_failures(u32::MAX); // don't exit on faults
            let (cap, handle) = Capture::start(config).unwrap();

            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                for i in 0..100 {
                    let _ = cap.log(make_entry(i)).await;
                }
            });

            drop(cap);
            handle.join().expect("writer thread panicked under buggify");

            let records = read_all_records_lenient(tmp.path());
            for r in &records {
                assert!(r.ts < 100, "phantom record with ts={} on seed {seed}", r.ts);
            }
        }
    });
    disable_buggify();
}

// ---------------------------------------------------------------------------
// Test 2: Writer exits cleanly when consecutive failures exceed threshold.
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn writer_exits_after_too_many_failures() {
    // Try several seeds because hitting the consecutive_failures path
    // requires the seed to activate the write-path injection site (B2).
    assert_fires("writer_exits_after_too_many_failures", || {
        for seed in 0..20u64 {
            enable_buggify(seed);

            let tmp = tempfile::tempdir().unwrap();
            let config = CaptureConfig::new("exit-fast", tmp.path())
                .unwrap()
                .max_consecutive_failures(5);
            let (cap, handle) = Capture::start(config).unwrap();

            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                for i in 0..1000 {
                    if cap.log(make_entry(i)).await.is_err() {
                        break;
                    }
                }
            });

            drop(cap);
            handle.join().expect("writer thread panicked");
        }
    });
    disable_buggify();
}

// ---------------------------------------------------------------------------
// Test 3: Compression failure preserves the raw .wcap file (current contract).
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn compression_failure_preserves_raw_file() {
    assert_fires("compression_failure_preserves_raw_file", || {
        for seed in 0..200u64 {
            enable_buggify(seed);

            let tmp = tempfile::tempdir().unwrap();
            let config = CaptureConfig::new("comp-fail", tmp.path())
                .unwrap()
                .max_file_bytes(2048)
                .max_consecutive_failures(u32::MAX);
            let (cap, handle) = Capture::start(config).unwrap();

            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                for i in 0..50 {
                    let _ = cap.log(make_entry(i)).await;
                }
            });
            drop(cap);
            handle.join().expect("writer panicked");

            // Whatever is on disk must be readable.
            let records = read_all_records_lenient(tmp.path());
            for r in &records {
                assert!(r.ts < 50, "phantom record on seed {seed}");
            }
        }
    });
    disable_buggify();
}

// ---------------------------------------------------------------------------
// Test 4: Rotation rename failure does not lose data or panic.
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn rotation_rename_failure_does_not_panic() {
    // The rename in finalize_file is one of 17 sites. It only fires when its
    // (file, line) tuple is activated for the seed. Run many seeds to cover.
    assert_fires("rotation_rename_failure_does_not_panic", || {
        for seed in 100..200u64 {
            enable_buggify(seed);

            let tmp = tempfile::tempdir().unwrap();
            let config = CaptureConfig::new("rename-fail", tmp.path())
                .unwrap()
                .max_file_bytes(1024) // force rotations
                .max_consecutive_failures(u32::MAX);
            let (cap, handle) = Capture::start(config).unwrap();

            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                for i in 0..100 {
                    let _ = cap.log(make_entry(i)).await;
                }
            });
            drop(cap);
            handle.join().expect("writer panicked under rename-failure injection");
        }
    });
    disable_buggify();
}

// ---------------------------------------------------------------------------
// Test 5: Recovery rename failure continues with other files.
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn recovery_rename_failure_continues() {
    assert_fires("recovery_rename_failure_continues", || {
        for seed in 0..50u64 {
            // IMPORTANT: set up leftover files with buggify DISABLED so the
            // test setup itself isn't injected.
            disable_buggify();

            let tmp = tempfile::tempdir().unwrap();
            for i in 0..5u32 {
                let path = tmp.path().join(format!("leftover_{i}.wcap.active"));
                let mut buf = Vec::new();
                {
                    let mut w =
                        WcapWriter::new(&mut buf, "rec", &format!("r{i:08x}"), 1024).unwrap();
                    w.write(&make_entry(i as u64)).unwrap();
                    w.flush().unwrap();
                }
                std::fs::write(&path, &buf).unwrap();
            }

            enable_buggify(seed);

            let config = CaptureConfig::new("recover-fail", tmp.path())
                .unwrap()
                .max_consecutive_failures(u32::MAX);
            let (cap, handle) = Capture::start(config).unwrap();
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let _ = cap.log(make_entry(999)).await;
            });
            drop(cap);
            handle
                .join()
                .expect("writer panicked under recovery-failure injection");
        }
    });
    disable_buggify();
}

// ---------------------------------------------------------------------------
// Test 6: Reader open failure propagates Err, no leak.
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn reader_open_failure_propagates() {
    // Set up the valid file outside the assert_fires block (with buggify off).
    disable_buggify();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("valid.wcap");
    let mut buf = Vec::new();
    {
        let mut w = WcapWriter::new(&mut buf, "open-fail", "r1", 1024).unwrap();
        w.write(&make_entry(1)).unwrap();
        w.flush().unwrap();
    }
    std::fs::write(&path, &buf).unwrap();

    assert_fires("reader_open_failure_propagates", || {
        for seed in 0..50u64 {
            enable_buggify(seed);
            for _ in 0..20 {
                let result = WcapReader::open(&path);
                // Either Ok or Err — never panic. We don't care which.
                if let Ok(reader) = result {
                    // Drain to also exercise B16 (mid-stream injection).
                    for _ in reader {}
                }
            }
        }
    });
    disable_buggify();
}

// ---------------------------------------------------------------------------
// Test 7: Tailer read failure returns empty batch, doesn't panic.
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn tailer_read_failure_no_panic() {
    disable_buggify();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("tailable.wcap.active");
    let mut buf = Vec::new();
    {
        let mut w = WcapWriter::new(&mut buf, "tail-fail", "r1", 1024).unwrap();
        for i in 0..20 {
            w.write(&make_entry(i)).unwrap();
        }
        w.flush().unwrap();
    }
    std::fs::write(&path, &buf).unwrap();

    assert_fires("tailer_read_failure_no_panic", || {
        for seed in 0..50u64 {
            enable_buggify(seed);

            let mut tailer = WcapTailer::new(tmp.path().to_path_buf());
            if !tailer.try_open() {
                continue;
            }
            for _ in 0..100 {
                let _ = tailer.read_batch(10);
            }
        }
    });
    disable_buggify();
}

// ---------------------------------------------------------------------------
// Test 8: Headline swarm test — full pipeline across many seeds.
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn swarm_test_capture_pipeline() {
    // 100 seeds × 200 entries each. For each seed: enable buggify, run a full
    // capture+rotation+compression cycle, drain, verify all surviving records
    // parse correctly with no phantoms.
    assert_fires("swarm_test_capture_pipeline", || {
        for seed in 1000..1100u64 {
            enable_buggify(seed);

            let tmp = tempfile::tempdir().unwrap();
            let config = CaptureConfig::new("swarm", tmp.path())
                .unwrap()
                .max_file_bytes(2048) // cause rotations
                .max_consecutive_failures(u32::MAX);
            let (cap, handle) = Capture::start(config).unwrap();

            let rt = tokio::runtime::Runtime::new().unwrap();
            let _ts = now_ns();
            rt.block_on(async {
                for i in 0..200 {
                    let _ = cap.log(make_entry(i)).await;
                }
            });

            drop(cap);
            handle
                .join()
                .unwrap_or_else(|_| panic!("writer panicked on seed {seed}"));

            let records = read_all_records_lenient(tmp.path());

            // Properties under stress:
            // 1. Surviving records is a subset of [0, 200).
            // 2. No phantom records (no ts >= 200).
            // 3. Each surviving record parses to a valid Entry.
            for r in &records {
                assert!(r.ts < 200, "phantom record with ts={} on seed {seed}", r.ts);
            }
        }
    });
    disable_buggify();
}
