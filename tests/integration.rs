use wirecap::{
    discover_files, find_active_file, Capture, CaptureConfig, Dir, Error, ReadEntry, WcapReader,
    WcapTailer, WriteEntry,
};

fn make_entry(ts: u64, payload: &str) -> WriteEntry {
    WriteEntry {
        ts,
        mono_ns: ts,
        recv_seq: 0,
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

/// Read all records from all wirecap files in a directory using WcapReader.
fn read_all_records(dir: &std::path::Path) -> Vec<ReadEntry> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .filter(|e| {
            let s = e.path().to_string_lossy().to_string();
            (s.ends_with(".wcap") || s.ends_with(".wcap.zst")
                || s.ends_with(".wcap.recovered") || s.ends_with(".wcap.recovered.zst"))
                && !s.ends_with(".wcap.active")
        })
        .collect();
    files.sort_by_key(|e| e.file_name());

    let mut all = Vec::new();
    for entry in &files {
        let reader = WcapReader::open(&entry.path()).expect("open wcap file");
        for record in reader {
            all.push(record.expect("read record"));
        }
    }
    all
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

// ---------------------------------------------------------------------------
// Happy-path tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_and_readback() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = CaptureConfig::new("test", tmp.path()).expect("config");
    let (cap, handle) = Capture::start(config).expect("start");

    let ts = now_ns();
    for i in 0..10 {
        cap.log(make_entry(ts, &format!(r#"{{"seq":{i}}}"#)))
            .await
            .expect("log");
    }

    drop(cap);
    handle.join().expect("writer");

    let records = read_all_records(tmp.path());
    assert_eq!(records.len(), 10);

    for (i, rec) in records.iter().enumerate() {
        assert_eq!(rec.ts, ts);
        assert_eq!(rec.src, 0);
        assert_eq!(rec.dir, Dir::In);
        let payload = String::from_utf8_lossy(&rec.payload);
        assert!(payload.contains(&format!(r#""seq":{i}"#)));
    }
}

#[tokio::test]
async fn file_header_roundtrip() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = CaptureConfig::new("hdr-test", tmp.path()).expect("config");
    let (cap, handle) = Capture::start(config).expect("start");
    let run_id = cap.run_id().to_string();

    cap.log(make_entry(now_ns(), r#"{"x":1}"#)).await.expect("log");
    drop(cap);
    handle.join().expect("writer");

    let files: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("readdir")
        .filter_map(Result::ok)
        .filter(|e| {
            e.path().to_str().is_some_and(|s| s.ends_with(".wcap") || s.ends_with(".wcap.zst"))
        })
        .collect();
    assert!(!files.is_empty());

    let reader = WcapReader::open(&files[0].path()).expect("open");
    assert_eq!(reader.instance_id(), "hdr-test");
    assert_eq!(reader.run_id(), run_id);
}

#[tokio::test]
async fn meta_field_roundtrip() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = CaptureConfig::new("meta-test", tmp.path()).expect("config");
    let (cap, handle) = Capture::start(config).expect("start");

    let ts = now_ns();
    let meta = b"GET /api/v1/orders 200 12ms".to_vec();
    cap.log(WriteEntry {
        ts,
        mono_ns: ts,
        recv_seq: 1,
        src: 2,
        dir: Dir::Out,
        meta: meta.clone(),
        payload: b"response body".to_vec(),
    })
    .await
    .expect("log");

    drop(cap);
    handle.join().expect("writer");

    let records = read_all_records(tmp.path());
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].meta, meta);
    assert_eq!(records[0].payload, b"response body");
    assert_eq!(records[0].src, 2);
    assert_eq!(records[0].dir, Dir::Out);
    assert_eq!(records[0].mono_ns, Some(ts));
    assert_eq!(records[0].recv_seq, Some(1));
}

#[tokio::test]
async fn all_channel_and_dir_variants() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = CaptureConfig::new("var-test", tmp.path()).expect("config");
    let (cap, handle) = Capture::start(config).expect("start");

    let ts = now_ns();
    let variants: Vec<(u8, Dir)> = vec![
        (0, Dir::In), (0, Dir::Out), (1, Dir::In), (1, Dir::Out),
        (2, Dir::In), (3, Dir::In), (4, Dir::In), (255, Dir::In),
    ];

    for &(src, dir) in &variants {
        cap.log(WriteEntry {
            ts, mono_ns: ts, recv_seq: 0, src, dir,
            meta: Vec::new(), payload: br#"{"v":1}"#.to_vec(),
        }).await.expect("log");
    }

    drop(cap);
    handle.join().expect("writer");

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
    let config = CaptureConfig::new("conc-test", tmp.path()).expect("config");
    let (cap, handle) = Capture::start(config).expect("start");

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
    handle.join().expect("writer");

    let records = read_all_records(tmp.path());
    assert_eq!(records.len(), 100);
}

#[tokio::test]
async fn size_based_rotation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = CaptureConfig::new("rot-test", tmp.path())
        .expect("config")
        .channel_capacity(1024)
        .max_file_bytes(1024)
        .max_file_secs(3600);
    let (cap, handle) = Capture::start(config).expect("start");

    let ts = now_ns();
    for i in 0..100 {
        cap.log(make_entry(ts, &format!(r#"{{"i":{i},"padding":"xxxxxxxxxxxxxxxx"}}"#)))
            .await
            .expect("log");
    }

    drop(cap);
    handle.join().expect("writer");

    let records = read_all_records(tmp.path());
    assert_eq!(records.len(), 100);
    assert!(count_zst_files(tmp.path()) > 3, "expected multiple rotated files");
}

#[tokio::test]
async fn shutdown_compresses_final_segment() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = CaptureConfig::new("shut-test", tmp.path()).expect("config");
    let (cap, handle) = Capture::start(config).expect("start");

    for i in 0..20 {
        cap.log(make_entry(now_ns(), &format!(r#"{{"i":{i}}}"#)))
            .await
            .expect("log");
    }

    // Before shutdown: active file should exist.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(count_active_files(tmp.path()) > 0);

    drop(cap);
    handle.join().expect("writer");

    // After shutdown: no active or raw files.
    assert_eq!(count_active_files(tmp.path()), 0);
    assert_eq!(count_raw_wcap_files(tmp.path()), 0);
    assert!(count_zst_files(tmp.path()) > 0);

    let records = read_all_records(tmp.path());
    assert_eq!(records.len(), 20);
}

#[tokio::test]
async fn filename_contains_instance_and_run() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = CaptureConfig::new("fn-test", tmp.path()).expect("config");
    let (cap, handle) = Capture::start(config).expect("start");
    let run_id = cap.run_id().to_string();

    cap.log(make_entry(now_ns(), r#"{"x":1}"#)).await.expect("log");
    drop(cap);
    handle.join().expect("writer");

    let files: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("readdir")
        .filter_map(Result::ok)
        .collect();
    assert!(!files.is_empty());

    let name = files[0].file_name().to_string_lossy().to_string();
    assert!(name.contains("fn-test"));
    assert!(name.contains(&run_id));
    assert!(name.contains(".wcap"));
}

// ---------------------------------------------------------------------------
// Error path tests
// ---------------------------------------------------------------------------

#[test]
fn bad_magic_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("bad.wcap");
    std::fs::write(&path, b"NOPE\x01\x04test\x04abcd").expect("write");
    let result = WcapReader::open(&path);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, Error::Format(_)));
}

#[test]
fn unsupported_version_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("badver.wcap");
    std::fs::write(&path, b"WCAP\x99\x04test\x04abcd").expect("write");
    let result = WcapReader::open(&path);
    assert!(result.is_err());
}

#[test]
fn empty_file_returns_zero_records() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("empty.wcap");
    // Valid header, no records.
    let mut data = Vec::new();
    data.extend_from_slice(b"WCAP\x01");
    data.push(4); // instance_id len
    data.extend_from_slice(b"test");
    data.push(4); // run_id len
    data.extend_from_slice(b"abcd");
    std::fs::write(&path, &data).expect("write");

    let reader = WcapReader::open(&path).expect("open");
    let records: Vec<_> = reader.collect::<Result<Vec<_>, _>>().expect("read");
    assert_eq!(records.len(), 0);
}

#[test]
fn wcap_writer_roundtrip() {
    use wirecap::WcapWriter;

    let mut buf = Vec::new();
    let mut writer = WcapWriter::new(&mut buf, "test", "abcd", 16 * 1024 * 1024).expect("new");

    let entry = WriteEntry {
        ts: 12345,
        mono_ns: 67890,
        recv_seq: 1,
        src: 3,
        dir: Dir::Out,
        meta: b"meta".to_vec(),
        payload: b"payload".to_vec(),
    };
    writer.write(&entry).expect("write");

    // Write to a temp file to verify via WcapReader.
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("test.wcap");
    {
        let file = std::fs::File::create(&path).expect("create");
        let mut writer = WcapWriter::new(file, "writer-test", "run1", 16 * 1024 * 1024)
            .expect("new");
        writer.write(&entry).expect("write");
        writer.flush().expect("flush");
    }

    let reader = WcapReader::open(&path).expect("open");
    assert_eq!(reader.instance_id(), "writer-test");
    assert_eq!(reader.run_id(), "run1");
    let records: Vec<_> = reader.collect::<Result<Vec<_>, _>>().expect("read");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].ts, 12345);
    assert_eq!(records[0].mono_ns, Some(67890));
    assert_eq!(records[0].recv_seq, Some(1));
    assert_eq!(records[0].src, 3);
    assert_eq!(records[0].dir, Dir::Out);
    assert_eq!(records[0].meta, b"meta");
    assert_eq!(records[0].payload, b"payload");
    drop(buf);
}

#[test]
fn config_rejects_bad_instance_id() {
    assert!(CaptureConfig::new("", "/tmp").is_err());
    assert!(CaptureConfig::new("../escape", "/tmp").is_err());
    assert!(CaptureConfig::new("/absolute", "/tmp").is_err());
    assert!(CaptureConfig::new("has\\backslash", "/tmp").is_err());
    assert!(CaptureConfig::new(".", "/tmp").is_err());
    assert!(CaptureConfig::new("..", "/tmp").is_err());
    assert!(CaptureConfig::new("good-name", "/tmp").is_ok());
    assert!(CaptureConfig::new("also_good.123", "/tmp").is_ok());
}

#[tokio::test]
async fn try_log_returns_false_when_full() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = CaptureConfig::new("try-test", tmp.path())
        .expect("config")
        .channel_capacity(2);
    let (cap, handle) = Capture::start(config).expect("start");

    // Fill the channel. The writer thread will be processing, but with a tiny
    // channel we might see backpressure.
    let mut sent = 0;
    for _ in 0..1000 {
        match cap.try_log(make_entry(now_ns(), "x")) {
            Ok(true) => sent += 1,
            Ok(false) => break, // Channel full — success
            Err(_) => panic!("writer died"),
        }
    }
    assert!(sent > 0, "should have sent at least one entry");

    drop(cap);
    handle.join().expect("writer");
}

#[test]
fn dir_display_and_traits() {
    assert_eq!(Dir::In.to_string(), "in");
    assert_eq!(Dir::Out.to_string(), "out");
    assert_eq!(Dir::In.as_str(), "in");

    // Hash works (can be used as HashMap key).
    let mut set = std::collections::HashSet::new();
    set.insert(Dir::In);
    set.insert(Dir::Out);
    assert_eq!(set.len(), 2);

    // TryFrom<u8>
    assert_eq!(Dir::try_from(0).unwrap(), Dir::In);
    assert_eq!(Dir::try_from(1).unwrap(), Dir::Out);
    assert!(Dir::try_from(2).is_err());
}

// ===========================================================================
// Layer 2: WcapWriter
// ===========================================================================

fn writer_sample() -> WriteEntry {
    WriteEntry {
        ts: 1,
        mono_ns: 2,
        recv_seq: 3,
        src: 4,
        dir: Dir::In,
        meta: b"m".to_vec(),
        payload: b"p".to_vec(),
    }
}

#[test]
fn writer_bytes_written_starts_at_header_size() {
    use wirecap::WcapWriter;
    let buf = Vec::new();
    let writer = WcapWriter::new(buf, "abc", "wxyz", 1024).unwrap();
    // 4 magic + 1 ver + 1 id_len + 3 id + 1 run_len + 4 run = 14
    assert_eq!(writer.bytes_written(), 14);
}

#[test]
fn writer_bytes_written_accumulates() {
    use wirecap::WcapWriter;
    let buf = Vec::new();
    let mut writer = WcapWriter::new(buf, "i", "r", 1024).unwrap();
    let header_bytes = writer.bytes_written();
    // record = 33 header + 1 meta + 1 payload = 35
    let n1 = writer.write(&writer_sample()).unwrap();
    assert_eq!(n1, 35);
    assert_eq!(writer.bytes_written(), header_bytes + 35);
    let n2 = writer.write(&writer_sample()).unwrap();
    assert_eq!(n2, 35);
    assert_eq!(writer.bytes_written(), header_bytes + 70);
    let n3 = writer.write(&writer_sample()).unwrap();
    assert_eq!(n3, 35);
    assert_eq!(writer.bytes_written(), header_bytes + 105);
}

#[test]
fn writer_into_inner_returns_underlying() {
    use wirecap::WcapWriter;
    let buf: Vec<u8> = Vec::new();
    let mut writer = WcapWriter::new(buf, "i", "r", 1024).unwrap();
    writer.write(&writer_sample()).unwrap();
    writer.flush().unwrap();
    let recovered = writer.into_inner();
    assert!(!recovered.is_empty());
    // First 4 bytes should be the magic.
    assert_eq!(&recovered[..4], b"WCAP");
}

#[test]
fn writer_flush_propagates_inner_flush() {
    use std::cell::RefCell;
    use std::io::{self, Write};
    use wirecap::WcapWriter;

    struct CountingWriter {
        flushes: RefCell<u32>,
        buf: Vec<u8>,
    }
    impl Write for CountingWriter {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.buf.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            *self.flushes.borrow_mut() += 1;
            Ok(())
        }
    }

    let cw = CountingWriter { flushes: RefCell::new(0), buf: Vec::new() };
    let mut writer = WcapWriter::new(cw, "i", "r", 1024).unwrap();
    writer.flush().unwrap();
    let inner = writer.into_inner();
    assert_eq!(*inner.flushes.borrow(), 1);
}

#[test]
fn writer_propagates_io_errors() {
    use std::io::{self, Write};
    use wirecap::WcapWriter;

    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("simulated"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // new() writes the file header — should fail.
    let result = WcapWriter::new(FailingWriter, "i", "r", 1024);
    match result {
        Err(Error::Io(_)) => {}
        Err(e) => panic!("expected Io, got {e:?}"),
        Ok(_) => panic!("expected error"),
    }
}

#[test]
fn writer_meta_overflow_rejected() {
    use wirecap::WcapWriter;
    let buf = Vec::new();
    let mut writer = WcapWriter::new(buf, "i", "r", 16 * 1024 * 1024).unwrap();
    let bytes_before = writer.bytes_written();

    let mut e = writer_sample();
    e.meta = vec![0; u16::MAX as usize + 1];
    let err = writer.write(&e).unwrap_err();
    assert!(matches!(err, Error::Format(_)));
    assert_eq!(writer.bytes_written(), bytes_before, "bytes_written should not advance on rejected write");
}

#[test]
fn writer_payload_overflow_rejected() {
    use wirecap::WcapWriter;
    let buf = Vec::new();
    let mut writer = WcapWriter::new(buf, "i", "r", 1024).unwrap();
    let bytes_before = writer.bytes_written();

    let mut e = writer_sample();
    e.payload = vec![0; 1025];
    let err = writer.write(&e).unwrap_err();
    assert!(matches!(err, Error::Format(_)));
    assert_eq!(writer.bytes_written(), bytes_before);
}

#[test]
fn writer_to_buffer_then_disk_then_read() {
    use wirecap::WcapWriter;
    // Write to a Vec, then dump to disk, then read with WcapReader.
    let mut buf = Vec::new();
    {
        let mut writer = WcapWriter::new(&mut buf, "buf-test", "r123", 1024).unwrap();
        writer.write(&writer_sample()).unwrap();
        writer.flush().unwrap();
    }

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("from_buf.wcap");
    std::fs::write(&path, &buf).unwrap();

    let reader = WcapReader::open(&path).unwrap();
    assert_eq!(reader.instance_id(), "buf-test");
    let records: Vec<_> = reader.collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].payload, b"p");
    assert_eq!(records[0].meta, b"m");
}

// ===========================================================================
// Layer 3a: WcapReader
// ===========================================================================

/// Build a valid raw .wcap file in memory with the given entries.
fn build_wcap_bytes(instance_id: &str, run_id: &str, entries: &[WriteEntry]) -> Vec<u8> {
    use wirecap::WcapWriter;
    let mut buf = Vec::new();
    {
        let mut writer = WcapWriter::new(&mut buf, instance_id, run_id, 16 * 1024 * 1024).unwrap();
        for e in entries {
            writer.write(e).unwrap();
        }
        writer.flush().unwrap();
    }
    buf
}

#[test]
fn reader_opens_zstd_file() {
    let bytes = build_wcap_bytes("zst-test", "r1", &[writer_sample(), writer_sample()]);

    let tmp = tempfile::tempdir().unwrap();
    let zst_path = tmp.path().join("compressed.wcap.zst");
    {
        let f = std::fs::File::create(&zst_path).unwrap();
        let mut encoder = zstd::Encoder::new(f, 3).unwrap();
        std::io::Write::write_all(&mut encoder, &bytes).unwrap();
        encoder.finish().unwrap();
    }

    let reader = WcapReader::open(&zst_path).unwrap();
    assert_eq!(reader.instance_id(), "zst-test");
    let records: Vec<_> = reader.collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(records.len(), 2);
}

#[test]
fn reader_opens_recovered_file() {
    let bytes = build_wcap_bytes("rec-test", "r1", &[writer_sample()]);
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("file.wcap.recovered");
    std::fs::write(&path, &bytes).unwrap();

    let reader = WcapReader::open(&path).unwrap();
    assert_eq!(reader.instance_id(), "rec-test");
    let records: Vec<_> = reader.collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(records.len(), 1);
}

#[test]
fn reader_opens_recovered_zst_file() {
    let bytes = build_wcap_bytes("rec-zst", "r1", &[writer_sample()]);
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("file.wcap.recovered.zst");
    {
        let f = std::fs::File::create(&path).unwrap();
        let mut encoder = zstd::Encoder::new(f, 3).unwrap();
        std::io::Write::write_all(&mut encoder, &bytes).unwrap();
        encoder.finish().unwrap();
    }

    let reader = WcapReader::open(&path).unwrap();
    assert_eq!(reader.instance_id(), "rec-zst");
    let records: Vec<_> = reader.collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(records.len(), 1);
}

#[test]
fn reader_iterator_exhausted_after_error() {
    // Build a valid header followed by garbage that will fail to parse.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"WCAP\x01");
    bytes.push(4);
    bytes.extend_from_slice(b"test");
    bytes.push(4);
    bytes.extend_from_slice(b"abcd");
    // Garbage record: version 99
    bytes.push(99);
    // ... and then "another" record after, which should never be reached
    bytes.push(3);
    bytes.extend_from_slice(&[0u8; 32]);

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("midstream-err.wcap");
    std::fs::write(&path, &bytes).unwrap();

    let mut reader = WcapReader::open(&path).unwrap();
    // First next() should be Some(Err(_))
    let first = reader.next();
    assert!(matches!(first, Some(Err(_))));
    // Subsequent next() should be None — iterator is exhausted.
    assert!(reader.next().is_none());
    assert!(reader.next().is_none());
}

#[test]
fn reader_iterator_clean_eof_immediately() {
    // Header only, no records.
    let bytes = build_wcap_bytes("empty", "r1", &[]);
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("empty.wcap");
    std::fs::write(&path, &bytes).unwrap();

    let mut reader = WcapReader::open(&path).unwrap();
    assert!(reader.next().is_none());
}

#[test]
fn reader_open_nonexistent_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("does-not-exist.wcap");
    match WcapReader::open(&path) {
        Err(Error::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
        other => panic!("expected Io NotFound, got {other:?}"),
    }
}

#[test]
fn reader_open_directory_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    // Pass the directory itself as the file path.
    let result = WcapReader::open(tmp.path());
    assert!(result.is_err());
}

#[test]
fn reader_debug_format_includes_instance_id() {
    let bytes = build_wcap_bytes("debug-test", "r1", &[]);
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("dbg.wcap");
    std::fs::write(&path, &bytes).unwrap();

    let reader = WcapReader::open(&path).unwrap();
    let debug_str = format!("{reader:?}");
    assert!(debug_str.contains("debug-test"));
    assert!(debug_str.contains("WcapReader"));
}

#[test]
fn reader_handles_v1_v2_v3_in_same_file() {
    // Hand-craft a file with mixed-version records following a v1 header.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"WCAP\x01");
    bytes.push(3);
    bytes.extend_from_slice(b"mix");
    bytes.push(2);
    bytes.extend_from_slice(b"r1");

    // v1 record
    bytes.push(1);
    bytes.extend_from_slice(&100u64.to_le_bytes());
    bytes.extend_from_slice(&2u32.to_le_bytes()); // payload_len
    bytes.push(0); // src
    bytes.push(0); // dir = In
    bytes.extend_from_slice(b"v1");

    // v2 record
    bytes.push(2);
    bytes.extend_from_slice(&200u64.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes()); // meta_len
    bytes.extend_from_slice(&2u32.to_le_bytes()); // payload_len
    bytes.push(1);
    bytes.push(1); // dir = Out
    bytes.extend_from_slice(b"m2");
    bytes.extend_from_slice(b"v2");

    // v3 record (assemble by hand)
    bytes.push(3);
    bytes.extend_from_slice(&300u64.to_le_bytes());
    bytes.extend_from_slice(&301u64.to_le_bytes()); // mono_ns
    bytes.extend_from_slice(&302u64.to_le_bytes()); // recv_seq
    bytes.extend_from_slice(&2u16.to_le_bytes()); // meta_len
    bytes.extend_from_slice(&2u32.to_le_bytes()); // payload_len
    bytes.push(2);
    bytes.push(0);
    bytes.extend_from_slice(b"m3");
    bytes.extend_from_slice(b"v3");

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("mixed.wcap");
    std::fs::write(&path, &bytes).unwrap();

    let reader = WcapReader::open(&path).unwrap();
    let records: Vec<_> = reader.collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(records.len(), 3);

    assert_eq!(records[0].ts, 100);
    assert_eq!(records[0].payload, b"v1");
    assert_eq!(records[0].mono_ns, None);
    assert_eq!(records[0].recv_seq, None);
    assert!(records[0].meta.is_empty());

    assert_eq!(records[1].ts, 200);
    assert_eq!(records[1].payload, b"v2");
    assert_eq!(records[1].meta, b"m2");
    assert_eq!(records[1].mono_ns, None);
    assert_eq!(records[1].recv_seq, None);

    assert_eq!(records[2].ts, 300);
    assert_eq!(records[2].mono_ns, Some(301));
    assert_eq!(records[2].recv_seq, Some(302));
    assert_eq!(records[2].meta, b"m3");
    assert_eq!(records[2].payload, b"v3");
}

// ===========================================================================
// Layer 3b: WcapTailer
// ===========================================================================

/// Write a complete .wcap.active file with the given entries.
fn write_active_file(dir: &std::path::Path, name: &str, entries: &[WriteEntry]) -> std::path::PathBuf {
    let path = dir.join(format!("{name}.wcap.active"));
    let bytes = build_wcap_bytes("tail-test", "r1", entries);
    std::fs::write(&path, &bytes).unwrap();
    path
}

#[test]
fn tailer_new_does_no_io() {
    // Pass a nonexistent directory — new() must not error or panic.
    let nonexistent = std::path::PathBuf::from("/this/does/not/exist/anywhere");
    let tailer = WcapTailer::new(nonexistent);
    assert!(!tailer.is_open());
    assert!(tailer.current_path().is_none());
}

#[test]
fn tailer_try_open_empty_dir_returns_false() {
    let tmp = tempfile::tempdir().unwrap();
    let mut tailer = WcapTailer::new(tmp.path().to_path_buf());
    assert!(!tailer.try_open());
    assert!(!tailer.is_open());
}

#[test]
fn tailer_try_open_finds_active_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_active_file(tmp.path(), "alpha", &[writer_sample()]);

    let mut tailer = WcapTailer::new(tmp.path().to_path_buf());
    assert!(tailer.try_open());
    assert!(tailer.is_open());
    assert_eq!(tailer.current_path(), Some(path.as_path()));
    assert_eq!(tailer.instance_id(), Some("tail-test"));
    assert_eq!(tailer.run_id(), Some("r1"));
}

#[test]
fn tailer_try_open_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    write_active_file(tmp.path(), "alpha", &[writer_sample()]);

    let mut tailer = WcapTailer::new(tmp.path().to_path_buf());
    assert!(tailer.try_open());
    let path1 = tailer.current_path().unwrap().to_path_buf();
    assert!(tailer.try_open()); // second call
    let path2 = tailer.current_path().unwrap().to_path_buf();
    assert_eq!(path1, path2);
}

#[test]
fn tailer_try_open_falls_back_to_wcap() {
    let tmp = tempfile::tempdir().unwrap();
    // Only a sealed .wcap file, no .active.
    let bytes = build_wcap_bytes("tail-test", "r1", &[writer_sample()]);
    let path = tmp.path().join("sealed.wcap");
    std::fs::write(&path, &bytes).unwrap();

    let mut tailer = WcapTailer::new(tmp.path().to_path_buf());
    assert!(tailer.try_open());
    assert_eq!(tailer.current_path(), Some(path.as_path()));
}

#[test]
fn tailer_read_batch_returns_records() {
    let tmp = tempfile::tempdir().unwrap();
    let entries = vec![writer_sample(), writer_sample(), writer_sample()];
    write_active_file(tmp.path(), "alpha", &entries);

    let mut tailer = WcapTailer::new(tmp.path().to_path_buf());
    assert!(tailer.try_open());
    let batch = tailer.read_batch(10);
    assert_eq!(batch.len(), 3);
}

#[test]
fn tailer_read_batch_partial_record_rewinds() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("partial.wcap.active");

    // Generate a single record's bytes by writing to a buffer with the same
    // instance/run id as the target file, then stripping its header.
    let header_only = build_wcap_bytes("tail-test", "r1", &[]);
    let header_len = header_only.len();
    let with_one = build_wcap_bytes("tail-test", "r1", &[writer_sample()]);
    let record_bytes = with_one[header_len..].to_vec();

    // Initial file: header + 1 full record + half of a second record.
    let mut bytes = build_wcap_bytes("tail-test", "r1", &[writer_sample()]);
    let half = record_bytes.len() / 2;
    bytes.extend_from_slice(&record_bytes[..half]);
    std::fs::write(&path, &bytes).unwrap();

    let mut tailer = WcapTailer::new(tmp.path().to_path_buf());
    assert!(tailer.try_open());

    // First read: gets the one complete record, then rewinds on the partial.
    let batch1 = tailer.read_batch(10);
    assert_eq!(batch1.len(), 1);

    // Now append the rest of the second record (full file rewrite, same path).
    let mut new_bytes = bytes.clone();
    new_bytes.extend_from_slice(&record_bytes[half..]);
    std::fs::write(&path, &new_bytes).unwrap();

    // Second read: should get the now-complete second record.
    let batch2 = tailer.read_batch(10);
    assert_eq!(batch2.len(), 1, "expected the rewound partial to now succeed");
}

#[test]
fn tailer_read_batch_respects_max_batch() {
    let tmp = tempfile::tempdir().unwrap();
    let entries: Vec<_> = (0..10).map(|_| writer_sample()).collect();
    write_active_file(tmp.path(), "alpha", &entries);

    let mut tailer = WcapTailer::new(tmp.path().to_path_buf());
    assert!(tailer.try_open());
    let batch = tailer.read_batch(3);
    assert_eq!(batch.len(), 3);
    let batch2 = tailer.read_batch(3);
    assert_eq!(batch2.len(), 3);
    let batch3 = tailer.read_batch(10);
    assert_eq!(batch3.len(), 4);
}

#[test]
fn tailer_read_batch_empty_at_eof() {
    let tmp = tempfile::tempdir().unwrap();
    write_active_file(tmp.path(), "alpha", &[writer_sample()]);

    let mut tailer = WcapTailer::new(tmp.path().to_path_buf());
    assert!(tailer.try_open());
    let _ = tailer.read_batch(10); // consume the one record
    let empty = tailer.read_batch(10);
    assert!(empty.is_empty());
}

#[test]
fn tailer_detects_rotation() {
    let tmp = tempfile::tempdir().unwrap();
    let _path_a = write_active_file(tmp.path(), "aaa", &[writer_sample()]);

    let mut tailer = WcapTailer::new(tmp.path().to_path_buf());
    assert!(tailer.try_open());
    let initial = tailer.current_path().unwrap().to_path_buf();

    // Drain the file.
    let _ = tailer.read_batch(10);

    // Create a newer .active file (must have strictly later mtime) and remove the old one.
    std::thread::sleep(std::time::Duration::from_millis(10));
    let path_b = write_active_file(tmp.path(), "bbb", &[writer_sample(), writer_sample()]);
    std::fs::remove_file(&initial).unwrap();

    // Poll until the rotation is detected (current_path changes). The check
    // runs every 100 EOFs; collect any records returned along the way.
    let mut collected = Vec::new();
    let mut rotated = false;
    for _ in 0..200 {
        let batch = tailer.read_batch(10);
        collected.extend(batch);
        if tailer.current_path() == Some(path_b.as_path()) {
            rotated = true;
            // Drain the new file in the same call sequence.
            collected.extend(tailer.read_batch(10));
            break;
        }
    }
    assert!(rotated, "tailer should have detected rotation within 200 polls");
    assert_eq!(collected.len(), 2, "expected to read both records from path_b");
}

#[test]
fn tailer_read_batch_empty_when_not_open() {
    let tmp = tempfile::tempdir().unwrap();
    let mut tailer = WcapTailer::new(tmp.path().to_path_buf());
    // Never called try_open.
    assert!(tailer.read_batch(10).is_empty());
}

#[test]
fn tailer_handles_invalid_header_gracefully() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("bad.wcap.active");
    std::fs::write(&path, b"NOPE not a wcap file").unwrap();

    let mut tailer = WcapTailer::new(tmp.path().to_path_buf());
    // try_open should return false (warning logged, no panic).
    assert!(!tailer.try_open());
    assert!(!tailer.is_open());
}

// ===========================================================================
// Layer 3c: discover_files / find_active_file
// ===========================================================================

fn touch(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, b"").unwrap();
    path
}

#[test]
fn discover_empty_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let files = discover_files(tmp.path()).unwrap();
    assert!(files.is_empty());
}

#[test]
fn discover_nonexistent_dir_errors() {
    let nonexistent = std::path::PathBuf::from("/this/does/not/exist/anywhere");
    assert!(discover_files(&nonexistent).is_err());
}

#[test]
fn discover_finds_all_extensions() {
    let tmp = tempfile::tempdir().unwrap();
    touch(tmp.path(), "a.wcap");
    touch(tmp.path(), "b.wcap.zst");
    touch(tmp.path(), "c.wcap.active");
    touch(tmp.path(), "d.wcap.recovered");

    let files = discover_files(tmp.path()).unwrap();
    assert_eq!(files.len(), 4);
}

#[test]
fn discover_excludes_non_wcap() {
    let tmp = tempfile::tempdir().unwrap();
    touch(tmp.path(), "a.wcap");
    touch(tmp.path(), "b.txt");
    touch(tmp.path(), "c.log");
    touch(tmp.path(), "README");

    let files = discover_files(tmp.path()).unwrap();
    assert_eq!(files.len(), 1);
    assert!(files[0].file_name().unwrap().to_str().unwrap().ends_with("a.wcap"));
}

#[test]
fn discover_sorted_by_mtime() {
    let tmp = tempfile::tempdir().unwrap();
    touch(tmp.path(), "first.wcap");
    std::thread::sleep(std::time::Duration::from_millis(10));
    touch(tmp.path(), "second.wcap");
    std::thread::sleep(std::time::Duration::from_millis(10));
    touch(tmp.path(), "third.wcap");

    let files = discover_files(tmp.path()).unwrap();
    assert_eq!(files.len(), 3);
    assert!(files[0].file_name().unwrap().to_str().unwrap().contains("first"));
    assert!(files[1].file_name().unwrap().to_str().unwrap().contains("second"));
    assert!(files[2].file_name().unwrap().to_str().unwrap().contains("third"));
}

#[test]
fn find_active_returns_none_when_empty() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(find_active_file(tmp.path()).is_none());
}

#[test]
fn find_active_returns_none_when_no_wcap() {
    let tmp = tempfile::tempdir().unwrap();
    touch(tmp.path(), "foo.txt");
    touch(tmp.path(), "bar.log");
    assert!(find_active_file(tmp.path()).is_none());
}

#[test]
fn find_active_prefers_active_over_wcap() {
    let tmp = tempfile::tempdir().unwrap();
    let _wcap = touch(tmp.path(), "old.wcap");
    let active = touch(tmp.path(), "new.wcap.active");
    let result = find_active_file(tmp.path()).unwrap();
    assert_eq!(result, active);
}

#[test]
fn find_active_returns_latest_active() {
    let tmp = tempfile::tempdir().unwrap();
    touch(tmp.path(), "a.wcap.active");
    std::thread::sleep(std::time::Duration::from_millis(10));
    let newest = touch(tmp.path(), "b.wcap.active");
    let result = find_active_file(tmp.path()).unwrap();
    assert_eq!(result, newest);
}

#[test]
fn find_active_falls_back_to_wcap_when_no_active() {
    let tmp = tempfile::tempdir().unwrap();
    let wcap = touch(tmp.path(), "sealed.wcap");
    let result = find_active_file(tmp.path()).unwrap();
    assert_eq!(result, wcap);
}

#[test]
fn find_active_returns_latest_wcap_when_multiple() {
    let tmp = tempfile::tempdir().unwrap();
    touch(tmp.path(), "old.wcap");
    std::thread::sleep(std::time::Duration::from_millis(10));
    let newest = touch(tmp.path(), "new.wcap");
    let result = find_active_file(tmp.path()).unwrap();
    assert_eq!(result, newest);
}

#[test]
fn find_active_nonexistent_dir_returns_none() {
    let nonexistent = std::path::PathBuf::from("/this/does/not/exist/anywhere");
    assert!(find_active_file(&nonexistent).is_none());
}

// ===========================================================================
// Layer 4: CaptureConfig
// ===========================================================================

#[test]
#[should_panic]
fn config_channel_capacity_panics_on_zero() {
    let _ = CaptureConfig::new("ok", "/tmp")
        .expect("config")
        .channel_capacity(0);
}

#[test]
#[should_panic]
fn config_max_file_bytes_panics_on_zero() {
    let _ = CaptureConfig::new("ok", "/tmp")
        .expect("config")
        .max_file_bytes(0);
}

#[test]
#[should_panic]
fn config_max_file_secs_panics_on_zero() {
    let _ = CaptureConfig::new("ok", "/tmp")
        .expect("config")
        .max_file_secs(0);
}

#[test]
#[should_panic]
fn config_max_payload_bytes_panics_on_zero() {
    let _ = CaptureConfig::new("ok", "/tmp")
        .expect("config")
        .max_payload_bytes(0);
}

#[test]
fn config_instance_id_at_max_length() {
    let id = "a".repeat(255);
    assert!(CaptureConfig::new(id, "/tmp").is_ok());
}

#[test]
fn config_instance_id_over_max_length() {
    let id = "a".repeat(256);
    assert!(CaptureConfig::new(id, "/tmp").is_err());
}

#[test]
fn config_instance_id_with_null_byte_rejected() {
    assert!(CaptureConfig::new("foo\0bar", "/tmp").is_err());
}

#[test]
fn config_builder_chains() {
    // Verify chaining returns Self correctly.
    let _config = CaptureConfig::new("test", "/tmp")
        .unwrap()
        .channel_capacity(100)
        .max_file_bytes(2048)
        .max_file_secs(60)
        .max_payload_bytes(1024);
}

// ===========================================================================
// Layer 4: Capture (extended)
// ===========================================================================

#[tokio::test]
async fn capture_creates_output_dir_if_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let nested = tmp.path().join("does/not/exist/yet");
    let config = CaptureConfig::new("dir-test", &nested).unwrap();
    let (cap, handle) = Capture::start(config).unwrap();
    cap.log(make_entry(now_ns(), "x")).await.unwrap();
    drop(cap);
    handle.join().unwrap();

    assert!(nested.exists(), "writer should have created the output directory");
    let records = read_all_records(&nested);
    assert_eq!(records.len(), 1);
}

#[tokio::test]
async fn capture_clone_keeps_writer_alive() {
    let tmp = tempfile::tempdir().unwrap();
    let config = CaptureConfig::new("clone-test", tmp.path()).unwrap();
    let (cap, handle) = Capture::start(config).unwrap();

    let cap2 = cap.clone();
    drop(cap); // drop the original; cap2 still alive

    cap2.log(make_entry(now_ns(), "via-clone")).await.unwrap();
    drop(cap2);
    handle.join().unwrap();

    let records = read_all_records(tmp.path());
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].payload, b"via-clone");
}

#[tokio::test]
async fn capture_drop_all_clones_drains_and_exits() {
    let tmp = tempfile::tempdir().unwrap();
    let config = CaptureConfig::new("drain-test", tmp.path()).unwrap();
    let (cap, handle) = Capture::start(config).unwrap();

    for i in 0..50 {
        cap.log(make_entry(now_ns(), &format!("e{i}"))).await.unwrap();
    }

    let cap2 = cap.clone();
    let cap3 = cap.clone();
    drop(cap);
    drop(cap2);
    drop(cap3);

    handle.join().unwrap();
    let records = read_all_records(tmp.path());
    assert_eq!(records.len(), 50);
}

#[test]
fn capture_run_id_unique_across_starts() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg1 = CaptureConfig::new("run1", tmp.path()).unwrap();
    let (cap1, h1) = Capture::start(cfg1).unwrap();
    let id1 = cap1.run_id().to_string();
    drop(cap1);
    h1.join().unwrap();

    let cfg2 = CaptureConfig::new("run2", tmp.path()).unwrap();
    let (cap2, h2) = Capture::start(cfg2).unwrap();
    let id2 = cap2.run_id().to_string();
    drop(cap2);
    h2.join().unwrap();

    assert_ne!(id1, id2, "two consecutive run_ids should differ");
}

#[test]
fn capture_run_id_format() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = CaptureConfig::new("fmt-test", tmp.path()).unwrap();
    let (cap, handle) = Capture::start(cfg).unwrap();
    let run_id = cap.run_id().to_string();
    drop(cap);
    handle.join().unwrap();

    // Expect 8 lowercase hex chars.
    assert_eq!(run_id.len(), 8);
    assert!(
        run_id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "run_id '{run_id}' should be 8 lowercase hex chars"
    );
}

#[tokio::test]
async fn capture_recovery_renames_active_to_recovered() {
    let tmp = tempfile::tempdir().unwrap();

    // Pre-create a leftover .wcap.active file with valid content.
    let leftover_bytes = build_wcap_bytes("recovery", "leftover", &[writer_sample()]);
    let leftover_path = tmp.path().join("leftover_2026-01-01T000000.000Z_xxxx.wcap.active");
    std::fs::write(&leftover_path, &leftover_bytes).unwrap();

    // Start capture in same dir — recovery should kick in.
    let config = CaptureConfig::new("recovery", tmp.path()).unwrap();
    let (cap, handle) = Capture::start(config).unwrap();
    cap.log(make_entry(now_ns(), "new")).await.unwrap();
    drop(cap);
    handle.join().unwrap();

    // The leftover .active should no longer exist as .active.
    assert!(!leftover_path.exists(), "leftover .active should have been renamed");

    // Give the background recovery compression a moment.
    std::thread::sleep(std::time::Duration::from_millis(200));

    // The dir should contain a .wcap.recovered (or .wcap.recovered.zst) file.
    let entries: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    let has_recovered = entries.iter().any(|n| n.contains(".wcap.recovered"));
    assert!(has_recovered, "expected .wcap.recovered file, got {entries:?}");
}

#[tokio::test]
async fn capture_recovery_handles_garbage_active_file() {
    let tmp = tempfile::tempdir().unwrap();

    // Pre-create a garbage .wcap.active file. Recovery should rename it but
    // compression may or may not succeed — neither should crash.
    let garbage_path = tmp.path().join("garbage.wcap.active");
    std::fs::write(&garbage_path, b"NOT VALID WCAP DATA").unwrap();

    let config = CaptureConfig::new("garbage-recover", tmp.path()).unwrap();
    let (cap, handle) = Capture::start(config).unwrap();
    cap.log(make_entry(now_ns(), "ok")).await.unwrap();
    drop(cap);
    handle.join().unwrap();

    // Garbage .active should be gone (renamed to .recovered).
    assert!(!garbage_path.exists());
}

#[tokio::test]
async fn capture_recovery_no_active_files_is_noop() {
    let tmp = tempfile::tempdir().unwrap();
    // No leftover files at all.
    let config = CaptureConfig::new("noop-recover", tmp.path()).unwrap();
    let (cap, handle) = Capture::start(config).unwrap();
    cap.log(make_entry(now_ns(), "x")).await.unwrap();
    drop(cap);
    handle.join().unwrap();

    // Should produce exactly one .wcap.zst, no .recovered files.
    let entries: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(entries.iter().any(|n| n.ends_with(".wcap.zst")));
    assert!(!entries.iter().any(|n| n.contains(".recovered")));
}

#[tokio::test]
async fn capture_concurrent_log_and_try_log() {
    let tmp = tempfile::tempdir().unwrap();
    let config = CaptureConfig::new("mixed", tmp.path()).unwrap();
    let (cap, handle) = Capture::start(config).unwrap();

    let cap_log = cap.clone();
    let log_task = tokio::spawn(async move {
        for i in 0..50 {
            cap_log.log(make_entry(now_ns(), &format!("log{i}"))).await.unwrap();
        }
    });

    let cap_try = cap.clone();
    let try_task = tokio::task::spawn_blocking(move || {
        let mut sent = 0;
        for i in 0..50 {
            // Loop until try_log accepts (channel may be momentarily full).
            loop {
                match cap_try.try_log(make_entry(0, &format!("try{i}"))) {
                    Ok(true) => {
                        sent += 1;
                        break;
                    }
                    Ok(false) => std::thread::yield_now(),
                    Err(_) => panic!("writer died"),
                }
            }
        }
        sent
    });

    log_task.await.unwrap();
    let try_sent = try_task.await.unwrap();
    assert_eq!(try_sent, 50);

    drop(cap);
    handle.join().unwrap();

    let records = read_all_records(tmp.path());
    assert_eq!(records.len(), 100);
}

/// After a clean shutdown that included rotations, *all* rotated segments
/// must be compressed — not just the final one. The previous implementation
/// spawned detached compression threads on rotation, so a fast shutdown could
/// race them and leave raw .wcap files behind.
#[tokio::test]
async fn shutdown_compresses_all_rotated_segments_no_race() {
    let tmp = tempfile::tempdir().unwrap();
    // Use a moderately-sized rotation threshold and large payloads so each
    // compression has nontrivial work — this widens the race window.
    let config = CaptureConfig::new("race-test", tmp.path())
        .unwrap()
        .max_file_bytes(64 * 1024); // 64KB per file
    let (cap, handle) = Capture::start(config).unwrap();

    // ~1KB per entry × 500 entries = 500KB total → ~7-8 rotations.
    let big_payload: String = "x".repeat(1024);
    for i in 0..500 {
        cap.log(make_entry(now_ns(), &format!("e{i}_{big_payload}")))
            .await
            .unwrap();
    }

    drop(cap);
    handle.join().unwrap();

    // After clean shutdown, NO raw .wcap files should remain.
    let raw = count_raw_wcap_files(tmp.path());
    let zst = count_zst_files(tmp.path());
    assert_eq!(
        raw, 0,
        "expected 0 raw .wcap files after shutdown (compression race), got {raw}"
    );
    assert!(zst > 1, "expected multiple .wcap.zst files from rotation, got {zst}");

    // And every record should be readable.
    let records = read_all_records(tmp.path());
    assert_eq!(records.len(), 500);
}

#[tokio::test]
async fn concurrent_writers_per_task_ordering() {
    let tmp = tempfile::tempdir().unwrap();
    let config = CaptureConfig::new("order-test", tmp.path()).unwrap();
    let (cap, handle) = Capture::start(config).unwrap();

    // Each task tags its records with its task ID via the meta field, sequence
    // via payload. Total ordering across tasks isn't guaranteed; per-task ordering is.
    let mut handles = Vec::new();
    for task_id in 0..5u8 {
        let cap = cap.clone();
        handles.push(tokio::spawn(async move {
            for seq in 0..20u32 {
                cap.log(WriteEntry {
                    ts: now_ns(),
                    mono_ns: 0,
                    recv_seq: 0,
                    src: task_id,
                    dir: Dir::In,
                    meta: vec![],
                    payload: seq.to_le_bytes().to_vec(),
                })
                .await
                .unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    drop(cap);
    handle.join().unwrap();

    let records = read_all_records(tmp.path());
    assert_eq!(records.len(), 100);

    // Group by src (task id) and verify each task's seq numbers are monotonically increasing.
    let mut per_task: std::collections::HashMap<u8, Vec<u32>> = std::collections::HashMap::new();
    for r in &records {
        let seq = u32::from_le_bytes(r.payload[..4].try_into().unwrap());
        per_task.entry(r.src).or_default().push(seq);
    }
    assert_eq!(per_task.len(), 5);
    for (task_id, seqs) in &per_task {
        assert_eq!(seqs.len(), 20, "task {task_id} should have 20 entries");
        for w in seqs.windows(2) {
            assert!(w[0] < w[1], "task {task_id}: seqs should be monotonic, got {seqs:?}");
        }
    }
}
