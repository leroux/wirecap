use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use metrics::{counter, describe_counter, describe_gauge, gauge, Counter, Gauge};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::format::{self, Entry};

/// Default channel capacity (Decision 8).
const DEFAULT_CHANNEL_CAPACITY: usize = 65_536;

/// Default max file size in bytes before rotation (Decision 7).
const DEFAULT_MAX_FILE_BYTES: u64 = 100 * 1024 * 1024; // 100MB

/// Default max file age in seconds before rotation (Decision 7).
const DEFAULT_MAX_FILE_SECS: u64 = 1800; // 30 minutes

/// Fsync interval in seconds (Decision 4).
const FSYNC_INTERVAL_SECS: u64 = 1;

/// Configuration for creating a [`Capture`].
pub struct CaptureConfig {
    pub instance_id: String,
    pub output_dir: String,
    pub channel_capacity: usize,
    pub max_file_bytes: u64,
    pub max_file_secs: u64,
}

impl CaptureConfig {
    pub fn new(instance_id: impl Into<String>, output_dir: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
            output_dir: output_dir.into(),
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_file_secs: DEFAULT_MAX_FILE_SECS,
        }
    }
}

/// Cached Prometheus metric handles. Noop if no recorder is installed
/// (i.e. when wirecap is used outside the recorder binary).
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
        describe_counter!("wirecap_bytes_total", "Total bytes captured");
        describe_gauge!("wirecap_channel_depth", "Current capture channel queue depth");
        describe_counter!("wirecap_write_errors_total", "Total write_record failures");
        describe_gauge!("wirecap_writer_healthy", "Writer task alive (1) or dead/stale (0)");

        Self {
            entries_total: counter!("wirecap_entries_total"),
            bytes_total: counter!("wirecap_bytes_total"),
            channel_depth: gauge!("wirecap_channel_depth"),
            write_errors: counter!("wirecap_write_errors_total"),
            writer_healthy: gauge!("wirecap_writer_healthy"),
        }
    }

    /// Record a successful write.
    fn record_write(&self, bytes: u64) {
        self.entries_total.increment(1);
        self.bytes_total.increment(bytes);
    }
}

/// Handle for capturing wire entries. Cheap to clone (Arc internally).
///
/// Call [`Capture::log`] to write entries. This is async — it applies
/// backpressure when the writer can't keep up (Decision 1).
#[derive(Clone)]
pub struct Capture {
    tx: mpsc::Sender<Entry>,
    instance_id: Arc<str>,
    run_id: Arc<str>,
    metrics: MetricHandles,
    writer_handle: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    shutdown_token: tokio_util::sync::CancellationToken,
}

impl Capture {
    /// Start the capture writer. Spawns a background writer task.
    pub fn start(config: CaptureConfig) -> Self {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let run_id = generate_run_id();

        let instance_id: Arc<str> = Arc::from(config.instance_id.as_str());
        let run_id: Arc<str> = Arc::from(run_id.as_str());
        let output_dir = config.output_dir;
        let max_file_bytes = config.max_file_bytes;
        let max_file_secs = config.max_file_secs;

        info!(
            instance_id = %instance_id,
            run_id = %run_id,
            output_dir = %output_dir,
            channel_capacity = config.channel_capacity,
            max_file_mb = max_file_bytes / (1024 * 1024),
            max_file_secs,
            "wirecap started"
        );

        // Recover any .wcap.active files from a previous unclean shutdown.
        recover_active_files(&output_dir);

        let stop_token = tokio_util::sync::CancellationToken::new();

        // Cached Prometheus metric handles — noop if no recorder installed.
        let metrics = MetricHandles::new();

        let writer_instance_id = Arc::clone(&instance_id);
        let writer_run_id = Arc::clone(&run_id);
        let writer_metrics = metrics.clone();
        let writer_stop = stop_token.child_token();
        let handle = tokio::spawn(async move {
            writer_task(
                rx, writer_stop,
                &writer_instance_id, &writer_run_id, &output_dir,
                &writer_metrics,
                max_file_bytes, max_file_secs,
            )
            .await;
        });

        Self {
            tx,
            instance_id,
            run_id,
            metrics,
            writer_handle: Arc::new(tokio::sync::Mutex::new(Some(handle))),
            shutdown_token: stop_token,
        }
    }

    /// Log a wire entry. Applies backpressure if the writer is behind (Decision 1).
    /// Returns Err only if the writer task is dead (channel closed).
    pub async fn log(&self, entry: Entry) -> Result<(), CaptureClosed> {
        self.metrics.channel_depth.increment(1.0);

        self.tx.send(entry).await.map_err(|_| {
            self.metrics.channel_depth.decrement(1.0);
            CaptureClosed
        })
    }

    /// Gracefully shut down the writer task. Signals the writer to stop,
    /// waits for it to flush and compress the final file.
    ///
    /// Safe to call from any clone. Only the first call actually waits —
    /// subsequent calls return immediately.
    pub async fn shutdown(&self) {
        // Signal the writer to stop.
        self.shutdown_token.cancel();
        // Wait for the writer task to finish (flush + compress).
        if let Some(h) = self.writer_handle.lock().await.take() {
            match h.await {
                Ok(()) => info!("wirecap writer task shut down cleanly"),
                Err(e) => error!(error = %e, "wirecap writer task panicked"),
            }
        }
    }

    pub fn instance_id(&self) -> &str { &self.instance_id }
    pub fn run_id(&self) -> &str { &self.run_id }
}

/// Error returned when the writer task is dead.
#[derive(Debug)]
pub struct CaptureClosed;

impl std::fmt::Display for CaptureClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "wirecap writer task is dead")
    }
}

impl std::error::Error for CaptureClosed {}

// ---------------------------------------------------------------------------
// Writer task
// ---------------------------------------------------------------------------

struct OpenFile {
    file: File,
    path: PathBuf,
    bytes_written: u64,
    opened_at: Instant,
}

async fn writer_task(
    mut rx: mpsc::Receiver<Entry>,
    stop: tokio_util::sync::CancellationToken,
    instance_id: &str,
    run_id: &str,
    output_dir: &str,
    metrics: &MetricHandles,
    max_file_bytes: u64,
    max_file_secs: u64,
) {
    let mut current: Option<OpenFile> = None;

    // Pre-open the file so we're ready to drain immediately (Decision 3 note).
    match open_file(output_dir, instance_id, run_id) {
        Ok(of) => {
            info!(path = %of.path.display(), "wirecap file opened");
            current = Some(of);
        }
        Err(e) => {
            error!(error = %e, "failed to open initial wirecap file");
        }
    }

    let mut fsync_interval = tokio::time::interval(std::time::Duration::from_secs(FSYNC_INTERVAL_SECS));
    fsync_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let entry = tokio::select! {
            biased;
            entry = rx.recv() => {
                match entry {
                    Some(e) => e,
                    None => break,
                }
            }
            () = stop.cancelled() => {
                // Drain remaining entries in the channel before exiting.
                while let Ok(e) = rx.try_recv() {
                    write_entry(&mut current, &e, metrics);
                }
                break;
            }
            _ = fsync_interval.tick() => {
                if let Some(ref of) = current {
                    if let Err(e) = of.file.sync_data() {
                        error!(error = %e, "fsync failed");
                    }
                }
                // Heartbeat: signal that the writer is alive.
                metrics.writer_healthy.set(1.0);
                continue;
            }
        };

        metrics.channel_depth.decrement(1.0);

        // Check if we need to rotate.
        let needs_rotate = current.as_ref().is_some_and(|of| {
            of.bytes_written >= max_file_bytes
                || of.opened_at.elapsed().as_secs() >= max_file_secs
        });

        if needs_rotate || current.is_none() {
            if let Some(of) = current.take() {
                if let Err(e) = of.file.sync_data() {
                    error!(error = %e, "fsync on rotation failed");
                }
                drop(of.file);
                match finalize_file(&of.path) {
                    Ok(final_path) => {
                        tokio::spawn(async move {
                            compress_file(&final_path);
                        });
                    }
                    Err(e) => {
                        error!(error = %e, path = %of.path.display(), "failed to finalize file");
                    }
                }
            }

            match open_file(output_dir, instance_id, run_id) {
                Ok(of) => {
                    info!(path = %of.path.display(), "wirecap file opened");
                    current = Some(of);
                }
                Err(e) => {
                    error!(error = %e, "failed to open wirecap file");
                    continue;
                }
            }
        }

        write_entry(&mut current, &entry, metrics);
    }

    // Shutdown: fsync, finalize (.active → .wcap), compress.
    if let Some(of) = current.take() {
        if let Err(e) = of.file.sync_data() {
            error!(error = %e, "fsync on shutdown failed");
        }
        drop(of.file);
        match finalize_file(&of.path) {
            Ok(final_path) => compress_file(&final_path),
            Err(e) => {
                error!(error = %e, path = %of.path.display(), "failed to finalize file on shutdown");
            }
        }
    }

    metrics.writer_healthy.set(0.0);
    info!("wirecap writer task exiting");
}

/// Write a single entry to the current file and record metrics.
fn write_entry(current: &mut Option<OpenFile>, entry: &Entry, metrics: &MetricHandles) {
    if let Some(of) = current {
        match format::write_record(&mut of.file, entry) {
            Ok(n) => {
                of.bytes_written += n as u64;
                metrics.record_write(n as u64);
            }
            Err(e) => {
                metrics.write_errors.increment(1);
                error!(error = %e, "failed to write wirecap record");
            }
        }
    }
}

fn open_file(output_dir: &str, instance_id: &str, run_id: &str) -> std::io::Result<OpenFile> {
    let dir = Path::new(output_dir);
    fs::create_dir_all(dir)?;

    let now = Utc::now();
    let timestamp = now.format("%Y-%m-%dT%H%M%S");
    let millis = now.timestamp_subsec_millis();
    let filename = format!("{instance_id}_{timestamp}.{millis:03}Z_{run_id}.wcap.active");
    let path = dir.join(&filename);

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)?;

    format::write_file_header(&mut file, instance_id, run_id)?;

    Ok(OpenFile {
        file,
        path,
        bytes_written: 0,
        opened_at: Instant::now(),
    })
}

/// Finalize a closed file: rename .wcap.active → .wcap.
fn finalize_file(active_path: &Path) -> std::io::Result<PathBuf> {
    let name = active_path.to_str().unwrap_or_default();
    let final_path = if name.ends_with(".wcap.active") {
        PathBuf::from(&name[..name.len() - ".active".len()])
    } else {
        active_path.to_path_buf()
    };
    fs::rename(active_path, &final_path)?;
    Ok(final_path)
}

/// Recover .wcap.active files from a previous unclean shutdown.
fn recover_active_files(output_dir: &str) {
    let dir = Path::new(output_dir);
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.to_str().unwrap_or_default();
        if name.ends_with(".wcap.active") {
            let recovered_path = PathBuf::from(&name[..name.len() - ".active".len()])
                .with_extension("wcap.recovered");
            warn!(
                active = %path.display(),
                recovered = %recovered_path.display(),
                "recovering file from unclean shutdown"
            );
            if let Err(e) = fs::rename(&path, &recovered_path) {
                error!(error = %e, path = %path.display(), "failed to rename active file for recovery");
                continue;
            }
            let rp = recovered_path.clone();
            tokio::spawn(async move {
                compress_file(&rp);
            });
        }
    }
}

/// Compress a wirecap file (.wcap or .wcap.recovered) to .zst, then delete the original.
fn compress_file(path: &Path) {
    let zst_path = PathBuf::from(format!("{}.zst", path.display()));
    info!(src = %path.display(), dst = %zst_path.display(), "compressing rotated file");

    let result = (|| -> std::io::Result<()> {
        let input = File::open(path)?;
        let output = File::create(&zst_path)?;
        let mut encoder = zstd::Encoder::new(output, 3)?;
        std::io::copy(&mut std::io::BufReader::new(input), &mut encoder)?;
        encoder.finish()?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            if let Err(e) = fs::remove_file(path) {
                warn!(error = %e, path = %path.display(), "failed to delete raw file after compression");
            } else {
                info!(path = %zst_path.display(), "compression complete");
            }
        }
        Err(e) => {
            error!(error = %e, path = %path.display(), "compression failed, raw file preserved");
            let _ = fs::remove_file(&zst_path);
        }
    }
}

fn generate_run_id() -> String {
    let n: u32 = rand::Rng::r#gen(&mut rand::thread_rng());
    format!("{n:08x}")
}
