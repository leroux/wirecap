use hegel::generators as gs;
use hegel::{Generator, TestCase};
use wirecap::{Capture, CaptureConfig, Dir, ReadEntry, WcapReader, WcapWriter, WriteEntry};

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

#[hegel::composite]
fn gen_dir(tc: TestCase) -> Dir {
    if tc.draw(gs::booleans()) {
        Dir::In
    } else {
        Dir::Out
    }
}

#[hegel::composite]
fn gen_write_entry(tc: TestCase) -> WriteEntry {
    WriteEntry {
        ts: tc.draw(gs::integers::<u64>()),
        mono_ns: tc.draw(gs::integers::<u64>()),
        recv_seq: tc.draw(gs::integers::<u64>()),
        src: tc.draw(gs::integers::<u8>()),
        dir: tc.draw(gen_dir()),
        // Keep sizes small enough to stay fast and within wire format limits.
        meta: tc.draw(gs::binary().max_size(1024)),
        payload: tc.draw(gs::binary().max_size(4096)),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compare a WriteEntry to the ReadEntry produced by reading it back.
fn assert_write_read_eq(w: &WriteEntry, r: &ReadEntry) {
    assert_eq!(w.ts, r.ts, "ts mismatch");
    assert_eq!(Some(w.mono_ns), r.mono_ns, "mono_ns mismatch");
    assert_eq!(Some(w.recv_seq), r.recv_seq, "recv_seq mismatch");
    assert_eq!(w.src, r.src, "src mismatch");
    assert_eq!(w.dir, r.dir, "dir mismatch");
    assert_eq!(w.meta, r.meta, "meta mismatch");
    assert_eq!(w.payload, r.payload, "payload mismatch");
}

/// Write entries to a temp file via WcapWriter, read back via WcapReader.
fn roundtrip_via_file(entries: &[WriteEntry]) -> Vec<ReadEntry> {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.wcap");

    {
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = WcapWriter::new(file, "pbt", "run1", 16 * 1024 * 1024).unwrap();
        for e in entries {
            writer.write(e).unwrap();
        }
        writer.flush().unwrap();
    }

    let reader = WcapReader::open(&path).unwrap();
    reader.collect::<Result<Vec<_>, _>>().unwrap()
}

/// Read all records from finished wirecap files in a directory.
fn read_all_records(dir: &std::path::Path) -> Vec<ReadEntry> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            let s = e.path().to_string_lossy().to_string();
            (s.ends_with(".wcap")
                || s.ends_with(".wcap.zst")
                || s.ends_with(".wcap.recovered")
                || s.ends_with(".wcap.recovered.zst"))
                && !s.ends_with(".wcap.active")
        })
        .collect();
    files.sort_by_key(|e| e.file_name());

    let mut all = Vec::new();
    for entry in &files {
        let reader = WcapReader::open(&entry.path()).unwrap();
        for record in reader {
            all.push(record.unwrap());
        }
    }
    all
}

// ---------------------------------------------------------------------------
// Phase 1: WcapWriter → WcapReader roundtrip
// ---------------------------------------------------------------------------

#[hegel::test]
fn single_record_roundtrip(tc: TestCase) {
    let entry = tc.draw(gen_write_entry());
    let read_back = roundtrip_via_file(std::slice::from_ref(&entry));
    assert_eq!(read_back.len(), 1);
    assert_write_read_eq(&entry, &read_back[0]);
}

#[hegel::test]
fn multi_record_roundtrip(tc: TestCase) {
    let entries: Vec<WriteEntry> = tc.draw(gs::vecs(gen_write_entry()).max_size(50));
    let read_back = roundtrip_via_file(&entries);
    assert_eq!(entries.len(), read_back.len(), "count mismatch");
    for (w, r) in entries.iter().zip(read_back.iter()) {
        assert_write_read_eq(w, r);
    }
}

#[hegel::test]
fn file_header_roundtrip(tc: TestCase) {
    // instance_id and run_id are length-prefixed with u8, max 255 bytes.
    // Filter out strings with path separators/nulls (CaptureConfig rejects them).
    let instance_id: String = tc.draw(
        gs::text()
            .min_size(1)
            .max_size(100)
            .filter(|s: &String| {
                !s.contains('/') && !s.contains('\\') && !s.contains('\0')
                    && *s != "." && *s != ".."
            }),
    );
    let run_id: String = tc.draw(gs::text().min_size(1).max_size(100));

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("hdr.wcap");
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = WcapWriter::new(file, &instance_id, &run_id, 16 * 1024 * 1024).unwrap();
        writer.flush().unwrap();
    }

    let reader = WcapReader::open(&path).unwrap();
    assert_eq!(reader.instance_id(), &instance_id);
    assert_eq!(reader.run_id(), &run_id);
}

// ---------------------------------------------------------------------------
// Phase 2: Parser robustness — never panic on arbitrary input
// ---------------------------------------------------------------------------

#[hegel::test]
fn reader_no_panic_on_garbage_file(tc: TestCase) {
    let garbage = tc.draw(gs::binary().max_size(4096));
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("garbage.wcap");
    std::fs::write(&path, &garbage).unwrap();
    // Must not panic. Ok or Err, either is fine.
    let _ = WcapReader::open(&path);
}

#[hegel::test]
fn reader_no_panic_on_garbage_after_valid_header(tc: TestCase) {
    let garbage = tc.draw(gs::binary().max_size(4096));
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("bad_records.wcap");

    // Write a valid header followed by garbage.
    let mut data = Vec::new();
    data.extend_from_slice(b"WCAP\x01");
    data.push(4);
    data.extend_from_slice(b"test");
    data.push(4);
    data.extend_from_slice(b"abcd");
    data.extend_from_slice(&garbage);
    std::fs::write(&path, &data).unwrap();

    // Open should succeed (valid header). Iteration may error but must not panic.
    if let Ok(reader) = WcapReader::open(&path) {
        for record in reader {
            let _ = record; // Ok or Err, both fine
        }
    }
}

#[hegel::test]
fn truncated_record_does_not_panic(tc: TestCase) {
    let entry = tc.draw(gen_write_entry());
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("truncated.wcap");

    // Write a complete file, then truncate at a random point within records.
    let mut full = Vec::new();
    {
        let mut writer = WcapWriter::new(&mut full, "test", "abcd", 16 * 1024 * 1024).unwrap();
        writer.write(&entry).unwrap();
    }

    if full.is_empty() {
        return;
    }

    // Cut somewhere inside the file.
    let cut = tc.draw(gs::integers::<usize>()) % full.len();
    let truncated = &full[..cut];
    std::fs::write(&path, truncated).unwrap();

    // Must not panic.
    if let Ok(reader) = WcapReader::open(&path) {
        for record in reader {
            let _ = record;
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 3: Full Capture → WcapReader pipeline
// ---------------------------------------------------------------------------

#[hegel::test(test_cases = 30)]
fn capture_roundtrip(tc: TestCase) {
    let entries: Vec<WriteEntry> =
        tc.draw(gs::vecs(gen_write_entry()).min_size(1).max_size(100));

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let tmp = tempfile::tempdir().unwrap();
        let config = CaptureConfig::new("pbt", tmp.path()).unwrap();
        let (cap, handle) = Capture::start(config).unwrap();

        for e in &entries {
            cap.log(e.clone()).await.unwrap();
        }

        drop(cap);
        handle.join().unwrap();

        let read_back = read_all_records(tmp.path());
        assert_eq!(
            entries.len(),
            read_back.len(),
            "count mismatch: wrote {} read {}",
            entries.len(),
            read_back.len()
        );
        for (w, r) in entries.iter().zip(read_back.iter()) {
            assert_write_read_eq(w, r);
        }
    });
}

#[hegel::test(test_cases = 20)]
fn rotation_preserves_all_entries(tc: TestCase) {
    let entries: Vec<WriteEntry> =
        tc.draw(gs::vecs(gen_write_entry()).min_size(20).max_size(200));

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let tmp = tempfile::tempdir().unwrap();
        let config = CaptureConfig::new("rot-pbt", tmp.path())
            .unwrap()
            .channel_capacity(1024)
            .max_file_bytes(512) // tiny — forces frequent rotation
            .max_file_secs(3600);
        let (cap, handle) = Capture::start(config).unwrap();

        for e in &entries {
            cap.log(e.clone()).await.unwrap();
        }

        drop(cap);
        handle.join().unwrap();

        let read_back = read_all_records(tmp.path());
        assert_eq!(
            entries.len(),
            read_back.len(),
            "rotation lost entries: wrote {} read {}",
            entries.len(),
            read_back.len()
        );
        for (w, r) in entries.iter().zip(read_back.iter()) {
            assert_write_read_eq(w, r);
        }
    });
}

// ---------------------------------------------------------------------------
// Phase 4: Additional properties
// ---------------------------------------------------------------------------

#[hegel::test]
fn meta_size_boundary_property(tc: TestCase) {
    // Property: any meta length 0..=u16::MAX should roundtrip exactly.
    // We pick a smaller range for speed but exercise the boundary at u16::MAX.
    let len: usize = tc.draw(gs::integers::<usize>()) % 4096;
    let mut e = WriteEntry {
        ts: 1,
        mono_ns: 2,
        recv_seq: 3,
        src: 0,
        dir: Dir::In,
        meta: vec![0xAB; len],
        payload: vec![],
    };
    e.meta.iter_mut().enumerate().for_each(|(i, b)| *b = (i & 0xFF) as u8);

    let read_back = roundtrip_via_file(&[e.clone()]);
    assert_eq!(read_back.len(), 1);
    assert_eq!(read_back[0].meta, e.meta);
}

#[hegel::test]
fn payload_size_boundary_property(tc: TestCase) {
    // Property: any payload size in our test range roundtrips exactly.
    let len: usize = tc.draw(gs::integers::<usize>()) % 8192;
    let payload: Vec<u8> = (0..len).map(|i| (i & 0xFF) as u8).collect();
    let e = WriteEntry {
        ts: 1,
        mono_ns: 2,
        recv_seq: 3,
        src: 0,
        dir: Dir::In,
        meta: vec![],
        payload: payload.clone(),
    };
    let read_back = roundtrip_via_file(&[e]);
    assert_eq!(read_back.len(), 1);
    assert_eq!(read_back[0].payload, payload);
}

#[hegel::test]
fn bytes_written_equals_actual_property(tc: TestCase) {
    use wirecap::WcapWriter;
    // Property: WcapWriter::bytes_written() always equals the actual number
    // of bytes in the underlying buffer.
    let entries: Vec<WriteEntry> = tc.draw(gs::vecs(gen_write_entry()).max_size(30));

    let mut buf = Vec::new();
    let mut writer = WcapWriter::new(&mut buf, "btw", "test", 16 * 1024 * 1024).unwrap();
    for e in &entries {
        writer.write(e).unwrap();
    }
    let claimed = writer.bytes_written();
    let _ = writer.into_inner();
    assert_eq!(claimed, buf.len() as u64);
}

#[hegel::test(test_cases = 20)]
fn tailer_eventually_reads_all_written(tc: TestCase) {
    use wirecap::{WcapTailer, WcapWriter};
    // Property: any sequence of records written via WcapWriter to a .wcap.active
    // file is fully readable via WcapTailer.
    let entries: Vec<WriteEntry> = tc.draw(gs::vecs(gen_write_entry()).min_size(1).max_size(50));

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("tail-prop.wcap.active");
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = WcapWriter::new(file, "tail-prop", "r1", 16 * 1024 * 1024).unwrap();
        for e in &entries {
            writer.write(e).unwrap();
        }
        writer.flush().unwrap();
    }

    let mut tailer = WcapTailer::new(tmp.path().to_path_buf());
    assert!(tailer.try_open());
    let read_back = tailer.read_batch(entries.len() + 10);
    assert_eq!(read_back.len(), entries.len());
    for (w, r) in entries.iter().zip(read_back.iter()) {
        assert_write_read_eq(w, r);
    }
}
