use std::io::Read;
use std::time::Duration;

use wirecap::{Capture, CaptureConfig, Dir, Entry};

fn make_entry(ts: u64, payload: &str) -> Entry {
    Entry {
        ts,
        mono_ns: Some(ts),
        recv_seq: Some(0),
        src: 0,
        dir: Dir::In,
        meta: Vec::new(),
        payload: payload.as_bytes().to_vec(),
    }
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos() as u64
}

fn is_wirecap_file(s: &str) -> bool {
    // Match any finished wirecap file. Exclude .wcap.active (still being written).
    (s.ends_with(".wcap") || s.ends_with(".wcap.zst")
        || s.ends_with(".wcap.recovered") || s.ends_with(".wcap.recovered.zst"))
        && !s.ends_with(".wcap.active")
}

/// Read all records from all wirecap files in a directory.
fn read_all_records(dir: &std::path::Path) -> Vec<Entry> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .filter(|e| {
            e.path().to_str().is_some_and(|s| is_wirecap_file(s))
        })
        .collect();
    files.sort_by_key(|e| e.file_name());

    let mut all = Vec::new();
    for entry in &files {
        let path = entry.path();
        let is_zst = path.extension().is_some_and(|ext| ext == "zst");

        let file = std::fs::File::open(&path).expect("open");
        let mut reader: Box<dyn Read> = if is_zst {
            Box::new(zstd::Decoder::new(file).expect("zstd decoder"))
        } else {
            Box::new(std::io::BufReader::new(file))
        };

        let (_instance_id, _run_id) =
            wirecap::format::read_file_header(&mut reader).expect("file header");

        loop {
            match wirecap::format::read_record(&mut reader) {
                Ok(Some(e)) => all.push(e),
                Ok(None) => break,
                Err(e) => panic!("read error: {e}"),
            }
        }
    }
    all
}

fn count_wcap_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .filter(|e| e.path().to_str().is_some_and(|s| is_wirecap_file(s)))
        .count()
}

fn count_raw_wcap_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .filter(|e| {
            let s = e.path().to_str().unwrap_or("").to_owned();
            (s.ends_with(".wcap") || s.ends_with(".wcap.recovered"))
                && !s.ends_with(".zst")
                && !s.ends_with(".active")
        })
        .count()
}

fn count_zst_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .filter(|e| e.path().to_str().is_some_and(|s| s.ends_with(".zst")))
        .count()
}

fn count_active_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .filter(|e| e.path().to_str().is_some_and(|s| s.ends_with(".wcap.active")))
        .count()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_and_readback() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cap = Capture::start(CaptureConfig::new("test", tmp.path().to_str().expect("path")));

    let ts = now_ns();
    for i in 0..10 {
        cap.log(make_entry(ts, &format!(r#"{{"seq":{i}}}"#)))
            .await
            .expect("log");
    }

    drop(cap);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let records = read_all_records(tmp.path());
    assert_eq!(records.len(), 10, "expected 10 records, got {}", records.len());

    for (i, rec) in records.iter().enumerate() {
        assert_eq!(rec.ts, ts);
        assert_eq!(rec.src, 0);
        assert_eq!(rec.dir, Dir::In);
        let payload = String::from_utf8_lossy(&rec.payload);
        assert!(
            payload.contains(&format!(r#""seq":{i}"#)),
            "record {i}: {payload}"
        );
    }
}

// stats_counters, channel_depth_and_high_water, and heartbeat tests were removed:
// these tested the old CaptureStats/.stats() API, now replaced by Prometheus metrics.
// Equivalent coverage is in crates/recorder/tests/metrics_smoke.rs.

#[tokio::test]
async fn file_header_roundtrip() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cap = Capture::start(CaptureConfig::new("hdr-test", tmp.path().to_str().expect("path")));
    let run_id = cap.run_id().to_string();

    cap.log(make_entry(now_ns(), r#"{"x":1}"#)).await.expect("log");

    drop(cap);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Read the file header directly.
    let files: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("readdir")
        .filter_map(Result::ok)
        .filter(|e| {
            e.path()
                .to_str()
                .is_some_and(|s| s.ends_with(".wcap") || s.ends_with(".wcap.zst"))
        })
        .collect();
    assert!(!files.is_empty());

    let path = files[0].path();
    let is_zst = path.extension().is_some_and(|ext| ext == "zst");
    let file = std::fs::File::open(&path).expect("open");
    let mut reader: Box<dyn Read> = if is_zst {
        Box::new(zstd::Decoder::new(file).expect("zstd"))
    } else {
        Box::new(std::io::BufReader::new(file))
    };

    let (inst, run) = wirecap::format::read_file_header(&mut reader).expect("header");
    assert_eq!(inst, "hdr-test");
    assert_eq!(run, run_id);
}

#[tokio::test]
async fn all_channel_and_dir_variants() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cap = Capture::start(CaptureConfig::new("var-test", tmp.path().to_str().expect("path")));

    let ts = now_ns();
    let variants: Vec<(u8, Dir)> = vec![
        (0, Dir::In),
        (0, Dir::Out),
        (1, Dir::In),
        (1, Dir::Out),
        (2, Dir::In),
        (3, Dir::In),
        (4, Dir::In),
        (255, Dir::In),
    ];

    for &(src, dir) in &variants {
        cap.log(Entry {
            ts,
            mono_ns: Some(ts),
            recv_seq: Some(0),
            src,
            dir,
            meta: Vec::new(),
            payload: br#"{"v":1}"#.to_vec(),
        })
        .await
        .expect("log");
    }

    drop(cap);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let records = read_all_records(tmp.path());
    assert_eq!(records.len(), variants.len());

    for (rec, &(exp_src, exp_dir)) in records.iter().zip(variants.iter()) {
        assert_eq!(rec.src, exp_src);
        assert_eq!(rec.dir, exp_dir);
    }
}

#[tokio::test]
async fn concurrent_writers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cap = Capture::start(CaptureConfig::new("conc-test", tmp.path().to_str().expect("path")));

    let ts = now_ns();
    let mut handles = Vec::new();
    for writer_id in 0..5 {
        let cap = cap.clone();
        handles.push(tokio::spawn(async move {
            for seq in 0..20 {
                cap.log(make_entry(ts, &format!(r#"{{"w":{writer_id},"s":{seq}}}"#)))
                    .await
                    .expect("log");
            }
        }));
    }

    for h in handles {
        h.await.expect("join");
    }

    drop(cap);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let records = read_all_records(tmp.path());
    assert_eq!(records.len(), 100);
}

#[tokio::test]
async fn size_based_rotation() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // Tiny max file size to force rotation.
    let config = CaptureConfig {
        instance_id: "rot-test".into(),
        output_dir: tmp.path().to_str().expect("path").into(),
        channel_capacity: 1024,
        max_file_bytes: 1024, // 1KB — will rotate frequently
        max_file_secs: 3600,  // don't trigger time rotation
    };
    let cap = Capture::start(config);

    let ts = now_ns();
    // Each entry is ~50 bytes payload + 15 header = ~65 bytes.
    // 1KB / 65 bytes ≈ 15 entries per file.
    // 100 entries should produce ~6-7 files.
    for i in 0..100 {
        cap.log(make_entry(ts, &format!(r#"{{"i":{i},"padding":"xxxxxxxxxxxxxxxx"}}"#)))
            .await
            .expect("log");
    }

    drop(cap);
    // Wait for compression of rotated files.
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let file_count = count_wcap_files(tmp.path());
    assert!(
        file_count > 3,
        "expected multiple files from rotation, got {file_count}"
    );

    // All 100 records should be readable across all files.
    let records = read_all_records(tmp.path());
    assert_eq!(records.len(), 100, "expected 100 total records, got {}", records.len());
}

#[tokio::test]
async fn filename_contains_instance_and_run() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cap = Capture::start(CaptureConfig::new("fn-test", tmp.path().to_str().expect("path")));
    let run_id = cap.run_id().to_string();

    cap.log(make_entry(now_ns(), r#"{"x":1}"#)).await.expect("log");

    drop(cap);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let files: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("readdir")
        .filter_map(Result::ok)
        .collect();
    assert!(!files.is_empty());

    let name = files[0].file_name().to_string_lossy().to_string();
    assert!(name.contains("fn-test"), "filename '{name}' should contain instance_id");
    assert!(name.contains(&run_id), "filename '{name}' should contain run_id");
    assert!(name.contains(".wcap"), "filename '{name}' should contain .wcap");
}

/// After shutdown(), the final segment must be compressed to .wcap.zst.
/// No raw .wcap files should remain on disk.
#[tokio::test]
async fn shutdown_compresses_final_segment() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cap = Capture::start(CaptureConfig::new("shut-test", tmp.path().to_str().expect("path")));

    let ts = now_ns();
    for i in 0..20 {
        cap.log(make_entry(ts, &format!(r#"{{"i":{i}}}"#)))
            .await
            .expect("log");
    }

    // Before shutdown: there should be 1 .wcap.active file (the active segment).
    // Give the writer a moment to process entries.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        count_active_files(tmp.path()) > 0,
        "expected at least 1 .wcap.active file before shutdown"
    );

    // Shutdown: writer flushes, compresses the final segment, exits.
    cap.shutdown().await;

    // After shutdown: no active or raw files should remain — all compressed.
    let active = count_active_files(tmp.path());
    let raw = count_raw_wcap_files(tmp.path());
    let zst = count_zst_files(tmp.path());
    assert_eq!(active, 0, "expected 0 .wcap.active files after shutdown, got {active}");
    assert_eq!(raw, 0, "expected 0 raw .wcap files after shutdown, got {raw}");
    assert!(zst > 0, "expected at least 1 .wcap.zst file after shutdown");

    // All records should be readable from the compressed file.
    let records = read_all_records(tmp.path());
    assert_eq!(records.len(), 20, "expected 20 records, got {}", records.len());
}

/// Shutdown with rotation: multiple segments should all be compressed.
#[tokio::test]
async fn shutdown_compresses_all_segments_after_rotation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = CaptureConfig {
        instance_id: "rot-shut-test".into(),
        output_dir: tmp.path().to_str().expect("path").into(),
        channel_capacity: 1024,
        max_file_bytes: 512, // tiny — forces frequent rotation
        max_file_secs: 3600,
    };
    let cap = Capture::start(config);

    let ts = now_ns();
    for i in 0..50 {
        cap.log(make_entry(ts, &format!(r#"{{"i":{i},"pad":"xxxxxxxxxxxx"}}"#)))
            .await
            .expect("log");
    }

    cap.shutdown().await;
    // Give background compression tasks a moment to finish.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let raw = count_raw_wcap_files(tmp.path());
    let zst = count_zst_files(tmp.path());
    assert_eq!(raw, 0, "expected 0 raw files after shutdown, got {raw}");
    assert!(zst > 1, "expected multiple .wcap.zst files from rotation, got {zst}");

    let records = read_all_records(tmp.path());
    assert_eq!(records.len(), 50, "expected 50 total records, got {}", records.len());
}
