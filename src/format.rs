//! Wirecap binary wire format.
//!
//! All multi-byte integers are serialized as little-endian.
//! See SPEC.md for the full byte-level format.

use crate::error::Error;

/// File magic bytes: "WCAP"
pub(crate) const MAGIC: &[u8; 4] = b"WCAP";

/// Current file format version.
pub(crate) const FILE_VERSION: u8 = 1;

/// Current record version.
pub(crate) const RECORD_VERSION: u8 = 3;

/// Record header size (v3): ver(1) + ts(8) + mono_ns(8) + recv_seq(8) +
/// meta_len(2) + payload_len(4) + src(1) + dir(1) = 33 bytes.
pub(crate) const RECORD_HEADER_SIZE: usize = 33;

/// Maximum payload size accepted by `read_record` (256 MB).
/// Safety limit to prevent allocation bombs from corrupt files.
const MAX_READ_PAYLOAD: usize = 256 * 1024 * 1024;


// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Direction of the wire communication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Dir {
    In = 0,
    Out = 1,
}

impl Dir {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
        }
    }
}

impl std::fmt::Display for Dir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<u8> for Dir {
    type Error = Error;

    fn try_from(v: u8) -> Result<Self, Error> {
        match v {
            0 => Ok(Self::In),
            1 => Ok(Self::Out),
            _ => Err(Error::Format(format!("invalid dir byte: {v}"))),
        }
    }
}

/// An entry to be written to a wcap file. All fields are required.
#[derive(Debug, Clone)]
pub struct WriteEntry {
    /// Wall-clock timestamp in nanoseconds since epoch.
    pub ts: u64,
    /// Monotonic timestamp in nanoseconds since process start.
    pub mono_ns: u64,
    /// Process-global receive sequence number.
    pub recv_seq: u64,
    /// Consumer-defined channel tag (opaque u8).
    pub src: u8,
    /// Direction: inbound or outbound.
    pub dir: Dir,
    /// Opaque metadata bytes. Empty = no meta.
    pub meta: Vec<u8>,
    /// Raw wire payload bytes.
    pub payload: Vec<u8>,
}

/// An entry read from a wcap file. Legacy fields are optional.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadEntry {
    /// Wall-clock timestamp in nanoseconds since epoch.
    pub ts: u64,
    /// Monotonic timestamp (v3 only; `None` for v1/v2 records).
    pub mono_ns: Option<u64>,
    /// Receive sequence number (v3 only; `None` for v1/v2 records).
    pub recv_seq: Option<u64>,
    /// Consumer-defined channel tag (opaque u8).
    pub src: u8,
    /// Direction: inbound or outbound.
    pub dir: Dir,
    /// Opaque metadata bytes (v2+ only; empty for v1 records).
    pub meta: Vec<u8>,
    /// Raw wire payload bytes.
    pub payload: Vec<u8>,
}

// ---------------------------------------------------------------------------
// File header
// ---------------------------------------------------------------------------

/// Write the file header. Returns the number of bytes written.
pub(crate) fn write_file_header(
    w: &mut impl std::io::Write,
    instance_id: &str,
    run_id: &str,
) -> Result<usize, Error> {
    let id_bytes = instance_id.as_bytes();
    if id_bytes.len() > 255 {
        return Err(Error::Format(format!(
            "instance_id too long: {} bytes (max 255)",
            id_bytes.len()
        )));
    }
    let run_bytes = run_id.as_bytes();
    if run_bytes.len() > 255 {
        return Err(Error::Format(format!(
            "run_id too long: {} bytes (max 255)",
            run_bytes.len()
        )));
    }

    w.write_all(MAGIC)?;
    w.write_all(&[FILE_VERSION])?;
    w.write_all(&[id_bytes.len() as u8])?;
    w.write_all(id_bytes)?;
    w.write_all(&[run_bytes.len() as u8])?;
    w.write_all(run_bytes)?;

    // 4 (magic) + 1 (ver) + 1 (id_len) + id + 1 (run_len) + run
    Ok(4 + 1 + 1 + id_bytes.len() + 1 + run_bytes.len())
}

/// Read and validate the file header. Returns (instance_id, run_id).
pub(crate) fn read_file_header(r: &mut impl std::io::Read) -> Result<(String, String), Error> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(Error::Format(format!(
            "bad magic: expected WCAP, got {magic:?}"
        )));
    }
    let mut ver = [0u8; 1];
    r.read_exact(&mut ver)?;
    if ver[0] != FILE_VERSION {
        return Err(Error::Format(format!(
            "unsupported file version: {}",
            ver[0]
        )));
    }
    let instance_id = read_length_prefixed_string(r)?;
    let run_id = read_length_prefixed_string(r)?;
    Ok((instance_id, run_id))
}

// ---------------------------------------------------------------------------
// Record write
// ---------------------------------------------------------------------------

/// Write a single v3 record. Returns the total bytes written.
///
/// Validates that meta and payload sizes fit in the wire format fields.
pub(crate) fn write_record(
    w: &mut impl std::io::Write,
    entry: &WriteEntry,
    max_payload: usize,
) -> Result<usize, Error> {
    if entry.meta.len() > u16::MAX as usize {
        return Err(Error::Format(format!(
            "meta too large: {} bytes (max {})",
            entry.meta.len(),
            u16::MAX
        )));
    }
    if entry.payload.len() > max_payload {
        return Err(Error::Format(format!(
            "payload too large: {} bytes (max {max_payload})",
            entry.payload.len()
        )));
    }

    let meta_len = entry.meta.len() as u16;
    let payload_len = entry.payload.len() as u32;

    w.write_all(&[RECORD_VERSION])?;
    w.write_all(&entry.ts.to_le_bytes())?;
    w.write_all(&entry.mono_ns.to_le_bytes())?;
    w.write_all(&entry.recv_seq.to_le_bytes())?;
    w.write_all(&meta_len.to_le_bytes())?;
    w.write_all(&payload_len.to_le_bytes())?;
    w.write_all(&[entry.src, entry.dir as u8])?;
    if !entry.meta.is_empty() {
        w.write_all(&entry.meta)?;
    }
    w.write_all(&entry.payload)?;

    Ok(RECORD_HEADER_SIZE + entry.meta.len() + entry.payload.len())
}

// ---------------------------------------------------------------------------
// Record read
// ---------------------------------------------------------------------------

/// Read a single record. Returns `Ok(None)` at clean EOF.
/// Supports v1, v2, and v3 records.
pub(crate) fn read_record(r: &mut impl std::io::Read) -> Result<Option<ReadEntry>, Error> {
    let mut ver_buf = [0u8; 1];
    match r.read_exact(&mut ver_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }

    match ver_buf[0] {
        1 => read_record_v1(r),
        2 => read_record_v2(r),
        3 => read_record_v3(r),
        v => Err(Error::Format(format!("unsupported record version: {v}"))),
    }
}

fn read_record_v1(r: &mut impl std::io::Read) -> Result<Option<ReadEntry>, Error> {
    let mut hdr = [0u8; 14];
    r.read_exact(&mut hdr)?;

    let ts = u64::from_le_bytes(hdr[0..8].try_into().expect("8 bytes"));
    let payload_len = u32::from_le_bytes(hdr[8..12].try_into().expect("4 bytes")) as usize;
    let src = hdr[12];
    let dir = Dir::try_from(hdr[13])?;

    validate_payload_len(payload_len)?;
    let mut payload = vec![0u8; payload_len];
    r.read_exact(&mut payload)?;

    Ok(Some(ReadEntry {
        ts,
        mono_ns: None,
        recv_seq: None,
        src,
        dir,
        meta: Vec::new(),
        payload,
    }))
}

fn read_record_v2(r: &mut impl std::io::Read) -> Result<Option<ReadEntry>, Error> {
    let mut hdr = [0u8; 16];
    r.read_exact(&mut hdr)?;

    let ts = u64::from_le_bytes(hdr[0..8].try_into().expect("8 bytes"));
    let meta_len = u16::from_le_bytes(hdr[8..10].try_into().expect("2 bytes")) as usize;
    let payload_len = u32::from_le_bytes(hdr[10..14].try_into().expect("4 bytes")) as usize;
    let src = hdr[14];
    let dir = Dir::try_from(hdr[15])?;

    validate_payload_len(payload_len)?;

    let meta = if meta_len > 0 {
        let mut buf = vec![0u8; meta_len];
        r.read_exact(&mut buf)?;
        buf
    } else {
        Vec::new()
    };

    let mut payload = vec![0u8; payload_len];
    r.read_exact(&mut payload)?;

    Ok(Some(ReadEntry {
        ts,
        mono_ns: None,
        recv_seq: None,
        src,
        dir,
        meta,
        payload,
    }))
}

fn read_record_v3(r: &mut impl std::io::Read) -> Result<Option<ReadEntry>, Error> {
    let mut hdr = [0u8; 32];
    r.read_exact(&mut hdr)?;

    let ts = u64::from_le_bytes(hdr[0..8].try_into().expect("8 bytes"));
    let mono_ns = u64::from_le_bytes(hdr[8..16].try_into().expect("8 bytes"));
    let recv_seq = u64::from_le_bytes(hdr[16..24].try_into().expect("8 bytes"));
    let meta_len = u16::from_le_bytes(hdr[24..26].try_into().expect("2 bytes")) as usize;
    let payload_len = u32::from_le_bytes(hdr[26..30].try_into().expect("4 bytes")) as usize;
    let src = hdr[30];
    let dir = Dir::try_from(hdr[31])?;

    validate_payload_len(payload_len)?;

    let meta = if meta_len > 0 {
        let mut buf = vec![0u8; meta_len];
        r.read_exact(&mut buf)?;
        buf
    } else {
        Vec::new()
    };

    let mut payload = vec![0u8; payload_len];
    r.read_exact(&mut payload)?;

    Ok(Some(ReadEntry {
        ts,
        mono_ns: Some(mono_ns),
        recv_seq: Some(recv_seq),
        src,
        dir,
        meta,
        payload,
    }))
}

fn validate_payload_len(len: usize) -> Result<(), Error> {
    if len > MAX_READ_PAYLOAD {
        return Err(Error::Format(format!(
            "payload too large: {len} bytes (max {MAX_READ_PAYLOAD})"
        )));
    }
    Ok(())
}

fn read_length_prefixed_string(r: &mut impl std::io::Read) -> Result<String, Error> {
    let mut len_buf = [0u8; 1];
    r.read_exact(&mut len_buf)?;
    let len = len_buf[0] as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| Error::Format(format!("invalid UTF-8: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // -----------------------------------------------------------------------
    // Helpers for hand-building legacy v1/v2 records
    // -----------------------------------------------------------------------

    /// Build a v1 record byte sequence.
    /// v1 layout: ver(1) + ts(8) + payload_len(4) + src(1) + dir(1) + payload
    fn build_v1_record(ts: u64, src: u8, dir: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(1); // version
        out.extend_from_slice(&ts.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.push(src);
        out.push(dir);
        out.extend_from_slice(payload);
        out
    }

    /// Build a v2 record byte sequence.
    /// v2 layout: ver(1) + ts(8) + meta_len(2) + payload_len(4) + src(1) + dir(1) + meta + payload
    fn build_v2_record(ts: u64, src: u8, dir: u8, meta: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(2);
        out.extend_from_slice(&ts.to_le_bytes());
        out.extend_from_slice(&(meta.len() as u16).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.push(src);
        out.push(dir);
        out.extend_from_slice(meta);
        out.extend_from_slice(payload);
        out
    }

    fn sample_entry() -> WriteEntry {
        WriteEntry {
            ts: 0x1122_3344_5566_7788,
            mono_ns: 0x99aa_bbcc_ddee_ff00,
            recv_seq: 42,
            src: 7,
            dir: Dir::Out,
            meta: b"meta".to_vec(),
            payload: b"hello".to_vec(),
        }
    }

    // =======================================================================
    // File header tests
    // =======================================================================

    #[test]
    fn header_roundtrip_basic() {
        let mut buf = Vec::new();
        let n = write_file_header(&mut buf, "inst", "run").unwrap();
        assert_eq!(n, buf.len());
        let (inst, run) = read_file_header(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(inst, "inst");
        assert_eq!(run, "run");
    }

    #[test]
    fn header_roundtrip_empty_strings() {
        // Format layer permits empty strings; CaptureConfig is the validation gate.
        let mut buf = Vec::new();
        write_file_header(&mut buf, "", "").unwrap();
        let (inst, run) = read_file_header(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(inst, "");
        assert_eq!(run, "");
    }

    #[test]
    fn header_roundtrip_max_length() {
        let big = "a".repeat(255);
        let mut buf = Vec::new();
        write_file_header(&mut buf, &big, &big).unwrap();
        let (inst, run) = read_file_header(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(inst, big);
        assert_eq!(run, big);
    }

    #[test]
    fn header_roundtrip_unicode() {
        let inst = "日本語";
        let run = "🎉";
        let mut buf = Vec::new();
        write_file_header(&mut buf, inst, run).unwrap();
        let (inst_r, run_r) = read_file_header(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(inst_r, inst);
        assert_eq!(run_r, run);
    }

    #[test]
    fn header_write_rejects_oversized_instance_id() {
        let mut buf = Vec::new();
        let huge = "x".repeat(256);
        let err = write_file_header(&mut buf, &huge, "ok").unwrap_err();
        assert!(matches!(err, Error::Format(_)));
    }

    #[test]
    fn header_write_rejects_oversized_run_id() {
        let mut buf = Vec::new();
        let huge = "x".repeat(256);
        let err = write_file_header(&mut buf, "ok", &huge).unwrap_err();
        assert!(matches!(err, Error::Format(_)));
    }

    #[test]
    fn header_write_returns_correct_byte_count() {
        let mut buf = Vec::new();
        let n = write_file_header(&mut buf, "abcd", "wxyz").unwrap();
        // 4 magic + 1 ver + 1 id_len + 4 id + 1 run_len + 4 run = 15
        assert_eq!(n, 15);
        assert_eq!(buf.len(), 15);
    }

    #[test]
    fn header_byte_layout() {
        let mut buf = Vec::new();
        write_file_header(&mut buf, "abc", "wxyz").unwrap();
        // 4 (WCAP) + 1 (version=1) + 1 (id_len=3) + 3 (id) + 1 (run_len=4) + 4 (run) = 14
        let expected: &[u8] = b"WCAP\x01\x03abc\x04wxyz";
        assert_eq!(buf, expected);
    }

    #[test]
    fn header_read_rejects_bad_magic() {
        let bytes = b"NOPE\x01\x04test\x04abcd";
        let err = read_file_header(&mut Cursor::new(bytes.as_slice())).unwrap_err();
        assert!(matches!(err, Error::Format(_)));
    }

    #[test]
    fn header_read_rejects_unknown_version() {
        let bytes = b"WCAP\x99\x04test\x04abcd";
        let err = read_file_header(&mut Cursor::new(bytes.as_slice())).unwrap_err();
        assert!(matches!(err, Error::Format(_)));
    }

    #[test]
    fn header_read_rejects_truncated_magic() {
        let bytes = b"WCA";
        let err = read_file_header(&mut Cursor::new(bytes.as_slice())).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn header_read_rejects_truncated_version() {
        let bytes = b"WCAP";
        let err = read_file_header(&mut Cursor::new(bytes.as_slice())).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn header_read_rejects_truncated_id_length() {
        let bytes = b"WCAP\x01";
        let err = read_file_header(&mut Cursor::new(bytes.as_slice())).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn header_read_rejects_truncated_id_body() {
        // claims id_len=10 but only 3 bytes follow
        let bytes = b"WCAP\x01\x0Aabc";
        let err = read_file_header(&mut Cursor::new(bytes.as_slice())).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn header_read_rejects_invalid_utf8() {
        let bytes = b"WCAP\x01\x02\xff\xfe\x04abcd";
        let err = read_file_header(&mut Cursor::new(bytes.as_slice())).unwrap_err();
        assert!(matches!(err, Error::Format(_)));
    }

    // =======================================================================
    // v3 record tests
    // =======================================================================

    fn assert_v3_roundtrip(entry: &WriteEntry) {
        let mut buf = Vec::new();
        let n = write_record(&mut buf, entry, 16 * 1024 * 1024).unwrap();
        assert_eq!(n, buf.len(), "write_record return value should equal bytes written");
        assert_eq!(n, RECORD_HEADER_SIZE + entry.meta.len() + entry.payload.len());

        let r = read_record(&mut Cursor::new(&buf)).unwrap().unwrap();
        assert_eq!(r.ts, entry.ts);
        assert_eq!(r.mono_ns, Some(entry.mono_ns));
        assert_eq!(r.recv_seq, Some(entry.recv_seq));
        assert_eq!(r.src, entry.src);
        assert_eq!(r.dir, entry.dir);
        assert_eq!(r.meta, entry.meta);
        assert_eq!(r.payload, entry.payload);
    }

    #[test]
    fn record_v3_roundtrip_full() {
        assert_v3_roundtrip(&sample_entry());
    }

    #[test]
    fn record_v3_roundtrip_empty_meta() {
        let mut e = sample_entry();
        e.meta = Vec::new();
        assert_v3_roundtrip(&e);
    }

    #[test]
    fn record_v3_roundtrip_empty_payload() {
        let mut e = sample_entry();
        e.payload = Vec::new();
        assert_v3_roundtrip(&e);
    }

    #[test]
    fn record_v3_roundtrip_both_empty() {
        let mut e = sample_entry();
        e.meta = Vec::new();
        e.payload = Vec::new();
        assert_v3_roundtrip(&e);
    }

    #[test]
    fn record_v3_roundtrip_max_meta() {
        let mut e = sample_entry();
        e.meta = vec![0xab; u16::MAX as usize];
        assert_v3_roundtrip(&e);
    }

    #[test]
    fn record_v3_roundtrip_dir_in() {
        let mut e = sample_entry();
        e.dir = Dir::In;
        let mut buf = Vec::new();
        write_record(&mut buf, &e, 1024).unwrap();
        // Dir is the byte at offset 32 in the v3 header (after version byte at 0).
        assert_eq!(buf[32], 0, "Dir::In should serialize as 0");
    }

    #[test]
    fn record_v3_roundtrip_dir_out() {
        let mut e = sample_entry();
        e.dir = Dir::Out;
        let mut buf = Vec::new();
        write_record(&mut buf, &e, 1024).unwrap();
        assert_eq!(buf[32], 1, "Dir::Out should serialize as 1");
    }

    #[test]
    fn record_write_returns_byte_count() {
        let e = sample_entry();
        let mut buf = Vec::new();
        let n = write_record(&mut buf, &e, 1024).unwrap();
        assert_eq!(n, buf.len());
        assert_eq!(n, RECORD_HEADER_SIZE + e.meta.len() + e.payload.len());
    }

    #[test]
    fn record_byte_layout_v3() {
        let e = WriteEntry {
            ts: 0x0807_0605_0403_0201,
            mono_ns: 0x1817_1615_1413_1211,
            recv_seq: 0x2827_2625_2423_2221,
            src: 0xAB,
            dir: Dir::Out,
            meta: vec![0xDE, 0xAD],
            payload: vec![0xBE, 0xEF, 0xCA],
        };
        let mut buf = Vec::new();
        write_record(&mut buf, &e, 1024).unwrap();

        // Header layout:
        // [0]    version = 3
        // [1..9] ts (LE)
        // [9..17] mono_ns (LE)
        // [17..25] recv_seq (LE)
        // [25..27] meta_len (LE u16) = 2
        // [27..31] payload_len (LE u32) = 3
        // [31] src
        // [32] dir
        // [33..35] meta
        // [35..38] payload
        assert_eq!(buf[0], 3);
        assert_eq!(&buf[1..9], &0x0807_0605_0403_0201u64.to_le_bytes());
        assert_eq!(&buf[9..17], &0x1817_1615_1413_1211u64.to_le_bytes());
        assert_eq!(&buf[17..25], &0x2827_2625_2423_2221u64.to_le_bytes());
        assert_eq!(&buf[25..27], &2u16.to_le_bytes());
        assert_eq!(&buf[27..31], &3u32.to_le_bytes());
        assert_eq!(buf[31], 0xAB);
        assert_eq!(buf[32], 1);
        assert_eq!(&buf[33..35], &[0xDE, 0xAD]);
        assert_eq!(&buf[35..38], &[0xBE, 0xEF, 0xCA]);
        assert_eq!(buf.len(), 38);
    }

    #[test]
    fn record_write_rejects_oversized_meta() {
        let mut e = sample_entry();
        e.meta = vec![0; u16::MAX as usize + 1];
        let mut buf = Vec::new();
        let err = write_record(&mut buf, &e, 16 * 1024 * 1024).unwrap_err();
        assert!(matches!(err, Error::Format(_)));
    }

    #[test]
    fn record_write_rejects_oversized_payload() {
        let mut e = sample_entry();
        e.payload = vec![0; 1025];
        let mut buf = Vec::new();
        let err = write_record(&mut buf, &e, 1024).unwrap_err();
        assert!(matches!(err, Error::Format(_)));
    }

    #[test]
    fn record_write_payload_at_limit_succeeds() {
        let mut e = sample_entry();
        e.payload = vec![0xCC; 1024];
        let mut buf = Vec::new();
        write_record(&mut buf, &e, 1024).unwrap();
    }

    #[test]
    fn record_write_meta_at_limit_succeeds() {
        let mut e = sample_entry();
        e.meta = vec![0xDD; u16::MAX as usize];
        let mut buf = Vec::new();
        write_record(&mut buf, &e, 16 * 1024 * 1024).unwrap();
    }

    #[test]
    fn record_read_clean_eof() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let result = read_record(&mut cursor).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn record_read_rejects_unknown_version() {
        let bytes = vec![99u8];
        let err = read_record(&mut Cursor::new(bytes)).unwrap_err();
        assert!(matches!(err, Error::Format(_)));
    }

    #[test]
    fn record_read_rejects_invalid_dir() {
        // Build a v3 record by hand with dir=2 (invalid).
        let mut bytes = vec![3u8]; // version
        bytes.extend_from_slice(&0u64.to_le_bytes()); // ts
        bytes.extend_from_slice(&0u64.to_le_bytes()); // mono_ns
        bytes.extend_from_slice(&0u64.to_le_bytes()); // recv_seq
        bytes.extend_from_slice(&0u16.to_le_bytes()); // meta_len
        bytes.extend_from_slice(&0u32.to_le_bytes()); // payload_len
        bytes.push(0); // src
        bytes.push(2); // dir = invalid
        let err = read_record(&mut Cursor::new(bytes)).unwrap_err();
        assert!(matches!(err, Error::Format(_)));
    }

    #[test]
    fn record_read_rejects_payload_over_256mb() {
        let mut bytes = vec![3u8];
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        // payload_len = MAX_READ_PAYLOAD + 1
        bytes.extend_from_slice(&((MAX_READ_PAYLOAD as u32) + 1).to_le_bytes());
        bytes.push(0);
        bytes.push(0);
        let err = read_record(&mut Cursor::new(bytes)).unwrap_err();
        assert!(matches!(err, Error::Format(_)));
    }

    #[test]
    fn record_read_rejects_truncated_header() {
        // version + only 5 bytes of ts (should be 8)
        let bytes = vec![3u8, 0, 0, 0, 0, 0];
        let err = read_record(&mut Cursor::new(bytes)).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn record_read_rejects_truncated_meta() {
        let mut bytes = vec![3u8];
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&10u16.to_le_bytes()); // meta_len=10
        bytes.extend_from_slice(&0u32.to_le_bytes()); // payload_len=0
        bytes.push(0);
        bytes.push(0);
        // No meta bytes follow.
        let err = read_record(&mut Cursor::new(bytes)).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn record_read_rejects_truncated_payload() {
        let mut bytes = vec![3u8];
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&10u32.to_le_bytes()); // payload_len=10
        bytes.push(0);
        bytes.push(0);
        // No payload bytes follow.
        let err = read_record(&mut Cursor::new(bytes)).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    // =======================================================================
    // v1 backward compatibility tests
    // =======================================================================

    #[test]
    fn record_read_v1_basic() {
        let bytes = build_v1_record(123456789, 5, 0, b"v1 payload");
        let r = read_record(&mut Cursor::new(&bytes)).unwrap().unwrap();
        assert_eq!(r.ts, 123456789);
        assert_eq!(r.mono_ns, None);
        assert_eq!(r.recv_seq, None);
        assert_eq!(r.src, 5);
        assert_eq!(r.dir, Dir::In);
        assert!(r.meta.is_empty());
        assert_eq!(r.payload, b"v1 payload");
    }

    #[test]
    fn record_read_v1_dir_out() {
        let bytes = build_v1_record(0, 0, 1, b"x");
        let r = read_record(&mut Cursor::new(&bytes)).unwrap().unwrap();
        assert_eq!(r.dir, Dir::Out);
    }

    #[test]
    fn record_read_v1_rejects_invalid_dir() {
        let bytes = build_v1_record(0, 0, 5, b"x");
        let err = read_record(&mut Cursor::new(&bytes)).unwrap_err();
        assert!(matches!(err, Error::Format(_)));
    }

    #[test]
    fn record_read_v1_rejects_truncated_header() {
        let bytes = vec![1u8, 0, 0, 0]; // version + 3 bytes (need 14)
        let err = read_record(&mut Cursor::new(bytes)).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn record_read_v1_rejects_truncated_payload() {
        // v1 header claims payload_len=10, no payload bytes follow
        let mut bytes = vec![1u8];
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&10u32.to_le_bytes());
        bytes.push(0);
        bytes.push(0);
        let err = read_record(&mut Cursor::new(bytes)).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    // =======================================================================
    // v2 backward compatibility tests
    // =======================================================================

    #[test]
    fn record_read_v2_basic() {
        let bytes = build_v2_record(987654321, 9, 1, b"meta-v2", b"payload-v2");
        let r = read_record(&mut Cursor::new(&bytes)).unwrap().unwrap();
        assert_eq!(r.ts, 987654321);
        assert_eq!(r.mono_ns, None);
        assert_eq!(r.recv_seq, None);
        assert_eq!(r.src, 9);
        assert_eq!(r.dir, Dir::Out);
        assert_eq!(r.meta, b"meta-v2");
        assert_eq!(r.payload, b"payload-v2");
    }

    #[test]
    fn record_read_v2_empty_meta() {
        let bytes = build_v2_record(1, 0, 0, b"", b"only payload");
        let r = read_record(&mut Cursor::new(&bytes)).unwrap().unwrap();
        assert!(r.meta.is_empty());
        assert_eq!(r.payload, b"only payload");
    }

    #[test]
    fn record_read_v2_rejects_invalid_dir() {
        let bytes = build_v2_record(0, 0, 99, b"", b"x");
        let err = read_record(&mut Cursor::new(&bytes)).unwrap_err();
        assert!(matches!(err, Error::Format(_)));
    }

    #[test]
    fn record_read_v2_rejects_truncated_meta() {
        // Claim meta_len=10, no meta bytes follow
        let mut bytes = vec![2u8];
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&10u16.to_le_bytes()); // meta_len=10
        bytes.extend_from_slice(&0u32.to_le_bytes()); // payload_len=0
        bytes.push(0);
        bytes.push(0);
        let err = read_record(&mut Cursor::new(bytes)).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    // =======================================================================
    // Mixed-version stream test
    // =======================================================================

    #[test]
    fn record_read_mixed_v1_v2_v3() {
        let mut buf = Vec::new();
        // v1 record
        buf.extend(build_v1_record(100, 1, 0, b"v1"));
        // v2 record
        buf.extend(build_v2_record(200, 2, 1, b"m2", b"v2"));
        // v3 record (write via real write_record)
        let v3_entry = WriteEntry {
            ts: 300,
            mono_ns: 301,
            recv_seq: 302,
            src: 3,
            dir: Dir::In,
            meta: b"m3".to_vec(),
            payload: b"v3".to_vec(),
        };
        write_record(&mut buf, &v3_entry, 1024).unwrap();

        let mut cursor = Cursor::new(&buf);

        let r1 = read_record(&mut cursor).unwrap().unwrap();
        assert_eq!(r1.ts, 100);
        assert_eq!(r1.src, 1);
        assert_eq!(r1.dir, Dir::In);
        assert_eq!(r1.mono_ns, None);
        assert_eq!(r1.recv_seq, None);
        assert!(r1.meta.is_empty());
        assert_eq!(r1.payload, b"v1");

        let r2 = read_record(&mut cursor).unwrap().unwrap();
        assert_eq!(r2.ts, 200);
        assert_eq!(r2.src, 2);
        assert_eq!(r2.dir, Dir::Out);
        assert_eq!(r2.mono_ns, None);
        assert_eq!(r2.recv_seq, None);
        assert_eq!(r2.meta, b"m2");
        assert_eq!(r2.payload, b"v2");

        let r3 = read_record(&mut cursor).unwrap().unwrap();
        assert_eq!(r3.ts, 300);
        assert_eq!(r3.mono_ns, Some(301));
        assert_eq!(r3.recv_seq, Some(302));
        assert_eq!(r3.src, 3);
        assert_eq!(r3.dir, Dir::In);
        assert_eq!(r3.meta, b"m3");
        assert_eq!(r3.payload, b"v3");

        // Clean EOF
        assert!(read_record(&mut cursor).unwrap().is_none());
    }
}
