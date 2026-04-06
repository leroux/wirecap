/// File magic bytes: "WCAP"
pub const MAGIC: &[u8; 4] = b"WCAP";

/// Current file format version.
pub const FILE_VERSION: u8 = 1;

/// Current record version.
pub const RECORD_VERSION: u8 = 3;

/// Record header size (v3): ver(1) + ts(8) + mono_ns(8) + recv_seq(8) + meta_len(2) + payload_len(4) + src(1) + dir(1) = 33 bytes.
pub const RECORD_HEADER_SIZE: usize = 33;


/// Direction of the wire communication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Dir {
    In = 0,
    Out = 1,
}

impl Dir {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::In),
            1 => Some(Self::Out),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
        }
    }
}

/// A single capture entry, ready to be written to disk.
pub struct Entry {
    /// Wall-clock timestamp in nanoseconds since epoch.
    pub ts: u64,
    /// Monotonic timestamp in nanoseconds since process start (for ordering).
    /// `None` when read from v1/v2 files that predate this field.
    pub mono_ns: Option<u64>,
    /// Process-global receive sequence number (for total ordering).
    /// `None` when read from v1/v2 files that predate this field.
    pub recv_seq: Option<u64>,
    /// Consumer-defined channel tag (opaque u8).
    pub src: u8,
    /// Direction: inbound or outbound.
    pub dir: Dir,
    /// Opaque metadata bytes (source-specific context). Empty = no meta.
    /// REST uses this for req_id, method, path, params, status, latency.
    /// WS/S3 leave this empty — all context is in the payload.
    // TODO: why does REST use meta for these things?
    pub meta: Vec<u8>,
    /// Raw wire payload bytes (untouched from the transport layer).
    pub payload: Vec<u8>,
}

/// Write the file header to a writer.
pub fn write_file_header(
    w: &mut impl std::io::Write,
    instance_id: &str,
    run_id: &str,
) -> std::io::Result<()> {
    w.write_all(MAGIC)?;
    w.write_all(&[FILE_VERSION])?;
    let id_bytes = instance_id.as_bytes();
    #[allow(clippy::cast_possible_truncation)]
    w.write_all(&[id_bytes.len() as u8])?;
    w.write_all(id_bytes)?;
    let run_bytes = run_id.as_bytes();
    #[allow(clippy::cast_possible_truncation)]
    w.write_all(&[run_bytes.len() as u8])?;
    w.write_all(run_bytes)?;
    Ok(())
}

/// Write a single v3 record (header + meta + payload) to a writer.
/// Returns the total bytes written.
pub fn write_record(w: &mut impl std::io::Write, entry: &Entry) -> std::io::Result<usize> {
    #[allow(clippy::cast_possible_truncation)]
    let meta_len = entry.meta.len() as u16;
    #[allow(clippy::cast_possible_truncation)]
    let payload_len = entry.payload.len() as u32;
    w.write_all(&[RECORD_VERSION])?;
    w.write_all(&entry.ts.to_le_bytes())?;
    w.write_all(&entry.mono_ns.unwrap_or(0).to_le_bytes())?;
    w.write_all(&entry.recv_seq.unwrap_or(0).to_le_bytes())?;
    w.write_all(&meta_len.to_le_bytes())?;
    w.write_all(&payload_len.to_le_bytes())?;
    w.write_all(&[entry.src, entry.dir as u8])?;
    if !entry.meta.is_empty() {
        w.write_all(&entry.meta)?;
    }
    w.write_all(&entry.payload)?;
    Ok(RECORD_HEADER_SIZE + entry.meta.len() + entry.payload.len())
}

/// Read and validate the file header. Returns (instance_id, run_id).
pub fn read_file_header(r: &mut dyn std::io::Read) -> std::io::Result<(String, String)> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("bad magic: expected WCAP, got {magic:?}"),
        ));
    }
    let mut ver = [0u8; 1];
    r.read_exact(&mut ver)?;
    if ver[0] != FILE_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsupported file version: {}", ver[0]),
        ));
    }
    let instance_id = read_length_prefixed_string(r)?;
    let run_id = read_length_prefixed_string(r)?;
    Ok((instance_id, run_id))
}

/// Read a single record. Returns None at EOF. Supports v1, v2, and v3 records.
pub fn read_record(r: &mut dyn std::io::Read) -> std::io::Result<Option<Entry>> {
    // Read the version byte first.
    let mut ver_buf = [0u8; 1];
    match r.read_exact(&mut ver_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }

    let ver = ver_buf[0];
    match ver {
        1 => read_record_v1(r),
        2 => read_record_v2(r),
        3 => read_record_v3(r),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsupported record version: {ver}"),
        )),
    }
}

/// Read a v1 record (no meta field). Remaining header after ver: ts(8) + payload_len(4) + src(1) + dir(1) = 14 bytes.
fn read_record_v1(r: &mut dyn std::io::Read) -> std::io::Result<Option<Entry>> {
    let mut rest = [0u8; 14];
    r.read_exact(&mut rest)?;

    let ts = u64::from_le_bytes(rest[0..8].try_into().expect("8 bytes"));
    let payload_len = u32::from_le_bytes(rest[8..12].try_into().expect("4 bytes"));
    let src = rest[12];
    let dir = parse_dir(rest[13])?;

    let mut payload = vec![0u8; payload_len as usize];
    r.read_exact(&mut payload)?;

    Ok(Some(Entry { ts, mono_ns: None, recv_seq: None, src, dir, meta: Vec::new(), payload }))
}

/// Read a v2 record (with meta). Remaining header after ver: ts(8) + meta_len(2) + payload_len(4) + src(1) + dir(1) = 16 bytes.
fn read_record_v2(r: &mut dyn std::io::Read) -> std::io::Result<Option<Entry>> {
    let mut rest = [0u8; 16];
    r.read_exact(&mut rest)?;

    let ts = u64::from_le_bytes(rest[0..8].try_into().expect("8 bytes"));
    let meta_len = u16::from_le_bytes(rest[8..10].try_into().expect("2 bytes"));
    let payload_len = u32::from_le_bytes(rest[10..14].try_into().expect("4 bytes"));
    let src = rest[14];
    let dir = parse_dir(rest[15])?;

    let meta = if meta_len > 0 {
        let mut buf = vec![0u8; meta_len as usize];
        r.read_exact(&mut buf)?;
        buf
    } else {
        Vec::new()
    };

    let mut payload = vec![0u8; payload_len as usize];
    r.read_exact(&mut payload)?;

    Ok(Some(Entry { ts, mono_ns: None, recv_seq: None, src, dir, meta, payload }))
}

/// Read a v3 record. Remaining header after ver: ts(8) + mono_ns(8) + recv_seq(8) + meta_len(2) + payload_len(4) + src(1) + dir(1) = 32 bytes.
fn read_record_v3(r: &mut dyn std::io::Read) -> std::io::Result<Option<Entry>> {
    let mut rest = [0u8; 32];
    r.read_exact(&mut rest)?;

    let ts = u64::from_le_bytes(rest[0..8].try_into().expect("8 bytes"));
    let mono_ns = u64::from_le_bytes(rest[8..16].try_into().expect("8 bytes"));
    let recv_seq = u64::from_le_bytes(rest[16..24].try_into().expect("8 bytes"));
    let meta_len = u16::from_le_bytes(rest[24..26].try_into().expect("2 bytes"));
    let payload_len = u32::from_le_bytes(rest[26..30].try_into().expect("4 bytes"));
    let src = rest[30];
    let dir = parse_dir(rest[31])?;

    let meta = if meta_len > 0 {
        let mut buf = vec![0u8; meta_len as usize];
        r.read_exact(&mut buf)?;
        buf
    } else {
        Vec::new()
    };

    let mut payload = vec![0u8; payload_len as usize];
    r.read_exact(&mut payload)?;

    Ok(Some(Entry {
        ts,
        mono_ns: Some(mono_ns),
        recv_seq: Some(recv_seq),
        src,
        dir,
        meta,
        payload,
    }))
}

fn parse_dir(b: u8) -> std::io::Result<Dir> {
    Dir::from_u8(b).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("unknown dir: {b}"))
    })
}

fn read_length_prefixed_string(r: &mut dyn std::io::Read) -> std::io::Result<String> {
    let mut len_buf = [0u8; 1];
    r.read_exact(&mut len_buf)?;
    let len = len_buf[0] as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
