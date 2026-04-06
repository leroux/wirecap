use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use metrics::{counter, describe_counter, describe_gauge, gauge, Counter, Gauge};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::buggify::{buggify, buggify_io_err};
use crate::error::Error;
use crate::format::{self, WriteEntry};

/// Default channel capacity.
const DEFAULT_CHANNEL_CAPACITY: usize = 65_536;

/// Default max file size in bytes before rotation (100 MB).
const DEFAULT_MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;

/// Default max file age in seconds before rotation (30 minutes).
const DEFAULT_MAX_FILE_SECS: u64 = 1800;

/// Default max payload size per entry (16 MB).
const DEFAULT_MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

/// Default writer-thread exit threshold for consecutive I/O failures.
const DEFAULT_MAX_CONSECUTIVE_FAILURES: u32 = 100;

/// Fsync interval in seconds.
const FSYNC_INTERVAL_SECS: u64 = 1;

// ---------------------------------------------------------------------------
// WcapWriter — synchronous file writer
// ---------------------------------------------------------------------------

/// Writes wcap records to any [`Write`] destination.
///
/// Handles the file header and record serialization. Does not handle
/// rotation, compression, or async — those are [`Capture`]'s job.
pub struct WcapWriter<W: Write> {
    writer: W,
    bytes_written: u64,
    max_payload: usize,
}

impl<W: Write> WcapWriter<W> {
    /// Create a new writer, writing the file header immediately.
    pub fn new(
        mut writer: W,
        instance_id: &str,
        run_id: &str,
        max_payload: usize,
    ) -> Result<Self, Error> {
        buggify_io_err!("header write"); // B1
        let header_bytes = format::write_file_header(&mut writer, instance_id, run_id)?;
        Ok(Self {
            writer,
            bytes_written: header_bytes as u64,
            max_payload,
        })
    }

    /// Write a single entry. Returns total bytes written for this record.
    pub fn write(&mut self, entry: &WriteEntry) -> Result<usize, Error> {
        buggify_io_err!("record write"); // B2
        let n = format::write_record(&mut self.writer, entry, self.max_payload)?;
        self.bytes_written += n as u64;
        Ok(n)
    }

    /// Flush buffered data to the underlying writer.
    pub fn flush(&mut self) -> Result<(), Error> {
        self.writer.flush()?;
        Ok(())
    }

    /// Total bytes written (including file header).
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Consume the writer, returning the inner writer.
    pub fn into_inner(self) -> W {
        self.writer
    }
}

// ---------------------------------------------------------------------------
// CaptureConfig
// ---------------------------------------------------------------------------

/// Configuration for creating a [`Capture`].
///
/// Use [`CaptureConfig::new`] for defaults, then chain builder methods.
pub struct CaptureConfig {
    instance_id: String,
    output_dir: PathBuf,
    channel_capacity: usize,
    max_file_bytes: u64,
    max_file_secs: u64,
    max_payload_bytes: usize,
    max_consecutive_failures: u32,
}

impl CaptureConfig {
    /// Create a new config with defaults.
    ///
    /// `instance_id` must be a simple name (no path separators).
    pub fn new(
        instance_id: impl Into<String>,
        output_dir: impl Into<PathBuf>,
    ) -> Result<Self, Error> {
        let instance_id = instance_id.into();
        validate_instance_id(&instance_id)?;
        Ok(Self {
            instance_id,
            output_dir: output_dir.into(),
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_file_secs: DEFAULT_MAX_FILE_SECS,
            max_payload_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
            max_consecutive_failures: DEFAULT_MAX_CONSECUTIVE_FAILURES,
        })
    }

    pub fn channel_capacity(mut self, n: usize) -> Self {
        assert!(n > 0, "channel_capacity must be > 0");
        self.channel_capacity = n;
        self
    }

    pub fn max_file_bytes(mut self, n: u64) -> Self {
        assert!(n > 0, "max_file_bytes must be > 0");
        self.max_file_bytes = n;
        self
    }

    pub fn max_file_secs(mut self, n: u64) -> Self {
        assert!(n > 0, "max_file_secs must be > 0");
        self.max_file_secs = n;
        self
    }

    pub fn max_payload_bytes(mut self, n: usize) -> Self {
        assert!(n > 0, "max_payload_bytes must be > 0");
        self.max_payload_bytes = n;
        self
    }

    /// Number of consecutive write failures the writer thread tolerates
    /// before exiting. Default 100. Must be > 0.
    pub fn max_consecutive_failures(mut self, n: u32) -> Self {
        assert!(n > 0, "max_consecutive_failures must be > 0");
        self.max_consecutive_failures = n;
        self
    }
}

fn validate_instance_id(id: &str) -> Result<(), Error> {
    if id.is_empty() {
        return Err(Error::Format("instance_id must not be empty".into()));
    }
    if id.contains('/') || id.contains('\\') || id.contains('\0') {
        return Err(Error::Format(
            "instance_id must not contain path separators or null bytes".into(),
        ));
    }
    if id == "." || id == ".." {
        return Err(Error::Format(
            "instance_id must not be '.' or '..'".into(),
        ));
    }
    if id.len() > 255 {
        return Err(Error::Format(format!(
            "instance_id too long: {} bytes (max 255)",
            id.len()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Capture — async handle
// ---------------------------------------------------------------------------

/// Cached Prometheus metric handles.
#[derive(Clone)]
struct MetricHandles {
    entries_total: Counter,
    bytes_total: Counter,
    channel_depth: Gauge,
    write_errors: Counter,
    writer_healthy: Gauge,
}

impl MetricHandles {
    fn new() -> Self {
        describe_counter!("wirecap_entries_total", "Total wire entries captured");
        describe_counter!("wirecap_bytes_total", "Total payload bytes captured");
        describe_gauge!("wirecap_channel_depth", "Entries queued or pending send");
        describe_counter!("wirecap_write_errors_total", "Total write_record failures");
        describe_gauge!(
            "wirecap_writer_healthy",
            "Writer thread alive (1) or dead (0)"
        );

        Self {
            entries_total: counter!("wirecap_entries_total"),
            bytes_total: counter!("wirecap_bytes_total"),
            channel_depth: gauge!("wirecap_channel_depth"),
            write_errors: counter!("wirecap_write_errors_total"),
            writer_healthy: gauge!("wirecap_writer_healthy"),
        }
    }
}

/// Handle for capturing wire entries. Cheap to clone.
///
/// Call [`Capture::log`] to write entries asynchronously. Applies
/// backpressure when the writer can't keep up.
///
/// Drop all clones to signal the writer to drain and exit.
#[derive(Clone)]
pub struct Capture {
    tx: mpsc::Sender<WriteEntry>,
    instance_id: Arc<str>,
    run_id: Arc<str>,
    metrics: MetricHandles,
}

impl Capture {
    /// Create a capture handle and start the background writer thread.
    ///
    /// Returns the handle and a [`JoinHandle`] for the writer thread.
    /// The writer thread runs until all `Capture` clones are dropped,
    /// then drains remaining entries, compresses the final file, and exits.
    pub fn start(config: CaptureConfig) -> Result<(Self, std::thread::JoinHandle<()>), Error> {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let run_id = generate_run_id();

        let instance_id: Arc<str> = Arc::from(config.instance_id.as_str());
        let run_id: Arc<str> = Arc::from(run_id.as_str());
        let metrics = MetricHandles::new();

        info!(
            instance_id = %instance_id,
            run_id = %run_id,
            output_dir = %config.output_dir.display(),
            channel_capacity = config.channel_capacity,
            max_file_mb = config.max_file_bytes / (1024 * 1024),
            max_file_secs = config.max_file_secs,
            max_payload_bytes = config.max_payload_bytes,
            "wirecap starting"
        );

        metrics.writer_healthy.set(1.0);

        let writer_ctx = WriterContext {
            rx,
            instance_id: Arc::clone(&instance_id),
            run_id: Arc::clone(&run_id),
            output_dir: config.output_dir,
            metrics: metrics.clone(),
            max_file_bytes: config.max_file_bytes,
            max_file_secs: config.max_file_secs,
            max_payload_bytes: config.max_payload_bytes,
            max_consecutive_failures: config.max_consecutive_failures,
        };

        let handle = std::thread::Builder::new()
            .name("wirecap-writer".into())
            .spawn(move || writer_thread(writer_ctx))
            .map_err(Error::Io)?;

        Ok((
            Self {
                tx,
                instance_id,
                run_id,
                metrics,
            },
            handle,
        ))
    }

    /// Log a wire entry. Applies backpressure if the writer is behind.
    /// Returns `Err(Error::Closed)` if the writer thread has exited.
    pub async fn log(&self, entry: WriteEntry) -> Result<(), Error> {
        self.metrics.channel_depth.increment(1.0);
        self.tx.send(entry).await.map_err(|_| {
            self.metrics.channel_depth.decrement(1.0);
            Error::Closed
        })
    }

    /// Try to log without blocking. Returns `Err(Error::Closed)` if the
    /// writer is dead, or `Ok(false)` if the channel is full.
    pub fn try_log(&self, entry: WriteEntry) -> Result<bool, Error> {
        self.metrics.channel_depth.increment(1.0);
        match self.tx.try_send(entry) {
            Ok(()) => Ok(true),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.metrics.channel_depth.decrement(1.0);
                Ok(false)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.metrics.channel_depth.decrement(1.0);
                Err(Error::Closed)
            }
        }
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }
    pub fn run_id(&self) -> &str {
        &self.run_id
    }
}

// ---------------------------------------------------------------------------
// Writer thread
// ---------------------------------------------------------------------------

struct WriterContext {
    rx: mpsc::Receiver<WriteEntry>,
    instance_id: Arc<str>,
    run_id: Arc<str>,
    output_dir: PathBuf,
    metrics: MetricHandles,
    max_file_bytes: u64,
    max_file_secs: u64,
    max_payload_bytes: usize,
    max_consecutive_failures: u32,
}

struct OpenFile {
    writer: WcapWriter<BufWriter<File>>,
    path: PathBuf,
    opened_at: Instant,
}

fn writer_thread(mut ctx: WriterContext) {
    // Track every background compression thread we spawn (rotations + recovery).
    // We join them all before this thread exits, so a clean shutdown guarantees
    // all files are compressed (no detached-thread race on process exit).
    let mut compress_handles: Vec<std::thread::JoinHandle<()>> =
        recover_active_files(&ctx.output_dir);

    let mut current: Option<OpenFile> = None;
    let mut consecutive_failures: u32 = 0;

    // Pre-open file so we're ready immediately.
    match open_file(&ctx.output_dir, &ctx.instance_id, &ctx.run_id, ctx.max_payload_bytes) {
        Ok(of) => {
            info!(path = %of.path.display(), "wirecap file opened");
            current = Some(of);
        }
        Err(e) => {
            error!(error = %e, "failed to open initial wirecap file");
        }
    }

    let mut last_fsync = Instant::now();

    while let Some(entry) = ctx.rx.blocking_recv() {

        ctx.metrics.channel_depth.decrement(1.0);

        // Periodic fsync.
        if last_fsync.elapsed().as_secs() >= FSYNC_INTERVAL_SECS {
            if let Some(ref mut of) = current {
                if buggify!() {
                    error!("buggified periodic flush"); // B3
                } else if let Err(e) = of.writer.flush() {
                    error!(error = %e, "flush failed");
                }
                if buggify!() {
                    error!("buggified periodic fsync"); // B4
                } else if let Err(e) = of.writer.inner().get_ref().sync_data() {
                    error!(error = %e, "fsync failed");
                }
            }
            ctx.metrics.writer_healthy.set(1.0);
            last_fsync = Instant::now();
        }

        // Check if we need to rotate.
        let needs_rotate = current.as_ref().is_some_and(|of| {
            of.writer.bytes_written() >= ctx.max_file_bytes
                || of.opened_at.elapsed().as_secs() >= ctx.max_file_secs
        });

        if needs_rotate || current.is_none() {
            if let Some(of) = current.take() {
                if let Some(h) = seal_and_spawn_compress(of) {
                    compress_handles.push(h);
                }
            }
            match open_file(&ctx.output_dir, &ctx.instance_id, &ctx.run_id, ctx.max_payload_bytes) {
                Ok(of) => {
                    info!(path = %of.path.display(), "wirecap file opened");
                    current = Some(of);
                }
                Err(e) => {
                    error!(error = %e, "failed to open wirecap file");
                    // Don't continue — fall through to write_entry which handles None.
                }
            }
        }

        // Write the entry.
        if let Some(ref mut of) = current {
            match of.writer.write(&entry) {
                Ok(n) => {
                    ctx.metrics.entries_total.increment(1);
                    ctx.metrics.bytes_total.increment(n as u64);
                    consecutive_failures = 0;
                }
                Err(e) => {
                    ctx.metrics.write_errors.increment(1);
                    error!(error = %e, "failed to write wirecap record");
                    consecutive_failures += 1;
                    if consecutive_failures >= ctx.max_consecutive_failures {
                        error!(
                            consecutive_failures,
                            "too many consecutive write failures, exiting"
                        );
                        break;
                    }
                }
            }
        } else {
            ctx.metrics.write_errors.increment(1);
            warn!("no open file, entry dropped");
            consecutive_failures += 1;
            if consecutive_failures >= ctx.max_consecutive_failures {
                error!(
                    consecutive_failures,
                    "too many consecutive failures, exiting"
                );
                break;
            }
        }
    }

    // Shutdown: seal the final segment and spawn its compression like any other.
    if let Some(of) = current.take() {
        if let Some(h) = seal_and_spawn_compress(of) {
            compress_handles.push(h);
        }
    }

    // Wait for all background compressions (rotation + recovery + final) to
    // finish before declaring shutdown complete. This guarantees the post-
    // shutdown contract: no .wcap or .wcap.recovered files remain on disk —
    // every sealed file is compressed.
    for h in compress_handles {
        if let Err(panic) = h.join() {
            error!(?panic, "compression thread panicked");
        }
    }

    ctx.metrics.writer_healthy.set(0.0);
    info!("wirecap writer thread exiting");
}

/// Flush, seal (.active → .wcap), and spawn a background thread to compress
/// (.wcap → .wcap.zst). Returns the compression thread's `JoinHandle` so the
/// caller can wait for it. Returns `None` if finalize or thread spawn failed.
fn seal_and_spawn_compress(mut of: OpenFile) -> Option<std::thread::JoinHandle<()>> {
    if buggify!() {
        error!("buggified flush on seal"); // B5
    } else if let Err(e) = of.writer.flush() {
        error!(error = %e, "flush on seal failed");
    }
    let buf_writer = of.writer.into_inner();
    if buggify!() {
        error!("buggified fsync on seal"); // B6
    } else if let Err(e) = buf_writer.get_ref().sync_data() {
        error!(error = %e, "fsync on seal failed");
    }
    drop(buf_writer);

    match finalize_file(&of.path) {
        Ok(final_path) => std::thread::Builder::new()
            .name("wirecap-compress".into())
            .spawn(move || compress_file(&final_path))
            .map_err(|e| {
                error!(error = %e, "failed to spawn compression thread");
            })
            .ok(),
        Err(e) => {
            error!(error = %e, path = %of.path.display(), "failed to finalize file");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// File operations
// ---------------------------------------------------------------------------

fn open_file(
    output_dir: &Path,
    instance_id: &str,
    run_id: &str,
    max_payload: usize,
) -> Result<OpenFile, Error> {
    buggify_io_err!("create_dir_all"); // B11
    fs::create_dir_all(output_dir)?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before epoch");
    let secs = now.as_secs();
    let millis = now.subsec_millis();

    // Format as UTC: YYYY-MM-DDTHHMMSS.mmmZ
    // Manual formatting to avoid chrono dependency.
    let (y, mo, d, h, mi, s) = secs_to_utc(secs);
    let timestamp = format!("{y:04}-{mo:02}-{d:02}T{h:02}{mi:02}{s:02}");
    let filename = format!("{instance_id}_{timestamp}.{millis:03}Z_{run_id}.wcap.active");
    let path = output_dir.join(&filename);

    buggify_io_err!("open file"); // B12
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)?;

    let buf_writer = BufWriter::new(file);
    let wcap_writer = WcapWriter::new(buf_writer, instance_id, run_id, max_payload)?;

    Ok(OpenFile {
        writer: wcap_writer,
        path,
        opened_at: Instant::now(),
    })
}

/// Rename .wcap.active → .wcap
fn finalize_file(active_path: &Path) -> std::io::Result<PathBuf> {
    let name = active_path.to_string_lossy();
    let final_path = match name.strip_suffix(".wcap.active") {
        Some(base) => PathBuf::from(format!("{base}.wcap")),
        None => {
            debug_assert!(false, "finalize_file called on non-.wcap.active path");
            active_path.to_path_buf()
        }
    };
    if buggify!() {
        // B7: simulate rotation rename failure.
        return Err(std::io::Error::other("buggified finalize rename"));
    }
    fs::rename(active_path, &final_path)?;
    Ok(final_path)
}

/// Recover .wcap.active files from a previous unclean shutdown. Returns handles
/// to the spawned compression threads so the writer can join them on shutdown.
fn recover_active_files(output_dir: &Path) -> Vec<std::thread::JoinHandle<()>> {
    let mut handles = Vec::new();
    let entries = match fs::read_dir(output_dir) {
        Ok(e) => e,
        Err(_) => return handles,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) if n.ends_with(".wcap.active") => n.to_owned(),
            _ => continue,
        };

        let base = &name[..name.len() - ".wcap.active".len()];
        let recovered_path = output_dir.join(format!("{base}.wcap.recovered"));

        warn!(
            active = %path.display(),
            recovered = %recovered_path.display(),
            "recovering file from unclean shutdown"
        );

        if buggify!() {
            // B13: simulate recovery rename failure.
            error!(path = %path.display(), "buggified recovery rename");
            continue;
        }
        if let Err(e) = fs::rename(&path, &recovered_path) {
            error!(error = %e, path = %path.display(), "failed to rename for recovery");
            continue;
        }

        // Compress in background to avoid blocking startup. The handle is
        // tracked so shutdown can wait for it.
        match std::thread::Builder::new()
            .name("wirecap-recover".into())
            .spawn(move || compress_file(&recovered_path))
        {
            Ok(h) => handles.push(h),
            Err(e) => error!(error = %e, "failed to spawn recovery compression thread"),
        }
    }
    handles
}

/// Compress a .wcap or .wcap.recovered file to .zst, then delete the original.
fn compress_file(path: &Path) {
    let zst_path = PathBuf::from(format!("{}.zst", path.display()));
    info!(src = %path.display(), dst = %zst_path.display(), "compressing");

    let result = (|| -> std::io::Result<()> {
        let input = File::open(path)?;
        if buggify!() {
            // B8: simulate compression target file creation failure.
            return Err(std::io::Error::other("buggified compress create"));
        }
        let output = File::create(&zst_path)?;
        let mut encoder = zstd::Encoder::new(output, 3)?;
        std::io::copy(&mut std::io::BufReader::new(input), &mut encoder)?;
        if buggify!() {
            // B9: simulate compression encoder finish failure.
            return Err(std::io::Error::other("buggified encoder finish"));
        }
        encoder.finish()?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            if buggify!() {
                // B10: simulate failure to delete raw file after compression.
                warn!(path = %path.display(), "buggified post-compression delete");
            } else if let Err(e) = fs::remove_file(path) {
                warn!(error = %e, path = %path.display(), "failed to delete after compression");
            } else {
                info!(path = %zst_path.display(), "compression complete");
            }
        }
        Err(e) => {
            error!(error = %e, path = %path.display(), "compression failed");
            let _ = fs::remove_file(&zst_path);
        }
    }
}

fn generate_run_id() -> String {
    let mut buf = [0u8; 4];
    getrandom::getrandom(&mut buf).expect("getrandom failed");
    format!("{:02x}{:02x}{:02x}{:02x}", buf[0], buf[1], buf[2], buf[3])
}

/// Convert seconds since Unix epoch to (year, month, day, hour, minute, second) in UTC.
fn secs_to_utc(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let h = time_of_day / 3600;
    let mi = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;

    // Civil date from day count (algorithm from Howard Hinnant).
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };

    (y, mo, d, h, mi, s)
}

impl<W: Write> WcapWriter<W> {
    /// Borrow the inner writer.
    pub(crate) fn inner(&self) -> &W {
        &self.writer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // secs_to_utc — hand-rolled date algorithm, easy to break
    // -----------------------------------------------------------------------

    #[test]
    fn secs_to_utc_unix_epoch() {
        // 1970-01-01 00:00:00 UTC
        assert_eq!(secs_to_utc(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn secs_to_utc_known_dates() {
        // 2000-01-01 00:00:00 UTC = 946684800
        assert_eq!(secs_to_utc(946684800), (2000, 1, 1, 0, 0, 0));
        // 2024-02-29 00:00:00 UTC = 1709164800 (leap day)
        assert_eq!(secs_to_utc(1709164800), (2024, 2, 29, 0, 0, 0));
        // 2024-02-29 23:59:59 UTC = 1709251199
        assert_eq!(secs_to_utc(1709251199), (2024, 2, 29, 23, 59, 59));
        // 2024-03-01 00:00:00 UTC = 1709251200 (post-leap-day boundary)
        assert_eq!(secs_to_utc(1709251200), (2024, 3, 1, 0, 0, 0));
        // 2026-04-06 00:00:00 UTC = 1775433600
        assert_eq!(secs_to_utc(1775433600), (2026, 4, 6, 0, 0, 0));
        // Year boundary: 2025-12-31 23:59:59 = 1767225599
        assert_eq!(secs_to_utc(1767225599), (2025, 12, 31, 23, 59, 59));
        // 2026-01-01 00:00:00 = 1767225600
        assert_eq!(secs_to_utc(1767225600), (2026, 1, 1, 0, 0, 0));
    }

    #[test]
    fn secs_to_utc_century_leap_rules() {
        // 2000 is a leap year (divisible by 400)
        // Feb 29, 2000 00:00:00 = 951782400
        assert_eq!(secs_to_utc(951782400), (2000, 2, 29, 0, 0, 0));
        // 2100 is NOT a leap year (divisible by 100, not 400)
        // 2100-02-28 23:59:59 = 4107542399
        assert_eq!(secs_to_utc(4107542399), (2100, 2, 28, 23, 59, 59));
        // 2100-03-01 00:00:00 = 4107542400 (no Feb 29 in 2100)
        assert_eq!(secs_to_utc(4107542400), (2100, 3, 1, 0, 0, 0));
        // 2400 is a leap year (divisible by 400)
        // 2400-02-29 12:00:00 = 13574606400
        assert_eq!(secs_to_utc(13574606400), (2400, 2, 29, 12, 0, 0));
    }

    #[test]
    fn secs_to_utc_far_future() {
        // 9999-12-31 23:59:59 = 253402300799
        assert_eq!(secs_to_utc(253402300799), (9999, 12, 31, 23, 59, 59));
    }
}
