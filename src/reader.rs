//! Wirecap file reading: open, iterate, discover, and tail.

use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use crate::buggify::{buggify, buggify_io_err};
use crate::error::Error;
use crate::format::{self, ReadEntry};

// ---------------------------------------------------------------------------
// File discovery
// ---------------------------------------------------------------------------

/// Recognized wirecap file extensions.
fn is_wcap_file(name: &str) -> bool {
    name.ends_with(".wcap")
        || name.ends_with(".wcap.zst")
        || name.ends_with(".wcap.active")
        || name.ends_with(".wcap.recovered")
}

/// Find all wirecap files in a directory, sorted by modification time (oldest first).
pub fn discover_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if is_wcap_file(name) {
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    files.push((path, modified));
                }
            }
        }
    }

    files.sort_by_key(|(_, t)| *t);
    Ok(files.into_iter().map(|(p, _)| p).collect())
}

/// Find the currently active (.wcap.active) file, or fall back to the latest
/// finalized .wcap file. Returns `None` if no wcap files exist.
pub fn find_active_file(dir: &Path) -> Option<PathBuf> {
    let mut active: Option<(PathBuf, std::time::SystemTime)> = None;
    let mut fallback: Option<(PathBuf, std::time::SystemTime)> = None;

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "failed to read wcap directory");
            return None;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        let is_active = name.ends_with(".wcap.active");
        let is_wcap = !is_active && name.ends_with(".wcap");

        if !is_active && !is_wcap {
            continue;
        }

        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                let bucket = if is_active { &mut active } else { &mut fallback };
                if bucket.as_ref().is_none_or(|(_, t)| modified > *t) {
                    *bucket = Some((path, modified));
                }
            }
        }
    }

    active.or(fallback).map(|(p, _)| p)
}

// ---------------------------------------------------------------------------
// WcapReader — batch reading of closed files
// ---------------------------------------------------------------------------

/// Reads records from a wirecap file (raw or zstd-compressed).
///
/// Implements `Iterator<Item = Result<ReadEntry, Error>>` for ergonomic
/// consumption. Errors are propagated, not swallowed.
pub struct WcapReader {
    reader: Box<dyn Read>,
    instance_id: String,
    run_id: String,
    done: bool,
}

impl std::fmt::Debug for WcapReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WcapReader")
            .field("instance_id", &self.instance_id)
            .field("run_id", &self.run_id)
            .field("done", &self.done)
            .finish()
    }
}

impl WcapReader {
    /// Open a wirecap file. Handles `.wcap`, `.wcap.zst`, and `.wcap.recovered`.
    pub fn open(path: &Path) -> Result<Self, Error> {
        buggify_io_err!("reader file open"); // B14
        let file = File::open(path)?;
        let is_zst = path.to_str().is_some_and(|s| s.ends_with(".zst"));

        let mut reader: Box<dyn Read> = if is_zst {
            Box::new(BufReader::new(zstd::Decoder::new(BufReader::new(file))?))
        } else {
            Box::new(BufReader::new(file))
        };

        buggify_io_err!("reader header read"); // B15
        let (instance_id, run_id) = format::read_file_header(&mut reader)?;
        debug!(
            path = %path.display(),
            instance_id,
            run_id,
            "opened wcap file"
        );

        Ok(Self {
            reader,
            instance_id,
            run_id,
            done: false,
        })
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }
}

impl Iterator for WcapReader {
    type Item = Result<ReadEntry, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        // B16: simulate mid-stream record read failure.
        if buggify!() {
            self.done = true;
            return Some(Err(Error::Io(std::io::Error::other(
                "buggified record read",
            ))));
        }
        match format::read_record(&mut self.reader) {
            Ok(Some(entry)) => Some(Ok(entry)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// WcapTailer — follow a live .wcap.active file
// ---------------------------------------------------------------------------

/// Follows a live wirecap file being written by the writer.
///
/// Handles partial records at EOF (seeks back and retries later)
/// and file rotation detection.
pub struct WcapTailer {
    wcap_dir: PathBuf,
    reader: Option<BufReader<File>>,
    current_path: Option<PathBuf>,
    eof_count: u64,
    instance_id: Option<String>,
    run_id: Option<String>,
}

impl WcapTailer {
    pub fn new(wcap_dir: PathBuf) -> Self {
        Self {
            wcap_dir,
            reader: None,
            current_path: None,
            eof_count: 0,
            instance_id: None,
            run_id: None,
        }
    }

    /// Try to open the active wcap file. Returns true if a file was opened.
    pub fn try_open(&mut self) -> bool {
        if self.reader.is_some() {
            return true;
        }
        if let Some(path) = find_active_file(&self.wcap_dir) {
            match open_raw_wcap(&path) {
                Ok((reader, instance_id, run_id)) => {
                    info!(
                        path = %path.display(),
                        instance_id,
                        run_id,
                        "tailing wcap file"
                    );
                    self.reader = Some(reader);
                    self.current_path = Some(path);
                    self.instance_id = Some(instance_id);
                    self.run_id = Some(run_id);
                    true
                }
                Err(e) => {
                    warn!(error = %e, "failed to open wcap file for tailing");
                    false
                }
            }
        } else {
            debug!(dir = %self.wcap_dir.display(), "no wcap file found");
            false
        }
    }

    /// Read up to `max_batch` records.
    ///
    /// Returns an empty vec at EOF (caller should poll later).
    /// Handles partial records by seeking back to retry.
    pub fn read_batch(&mut self, max_batch: usize) -> Vec<ReadEntry> {
        let reader = match &mut self.reader {
            Some(r) => r,
            None => return Vec::new(),
        };

        let mut entries = Vec::new();

        for _ in 0..max_batch {
            let pos = match reader.stream_position() {
                Ok(p) => p,
                Err(_) => break,
            };

            // B17: simulate a tailer read failure (treated as a partial record).
            if buggify!() {
                debug!("buggified tailer read; treating as partial");
                if let Err(seek_err) = reader.seek(std::io::SeekFrom::Start(pos)) {
                    warn!(error = %seek_err, "failed to rewind after buggified read");
                }
                break;
            }

            match format::read_record(reader) {
                Ok(Some(entry)) => {
                    self.eof_count = 0;
                    entries.push(entry);
                }
                Ok(None) => {
                    self.eof_count += 1;
                    if self.eof_count.is_multiple_of(100) {
                        self.check_rotation();
                    }
                    break;
                }
                Err(_) => {
                    // Partial record — rewind to retry after more data arrives.
                    if let Err(seek_err) = reader.seek(std::io::SeekFrom::Start(pos)) {
                        warn!(error = %seek_err, "failed to rewind after partial read");
                    }
                    break;
                }
            }
        }

        entries
    }

    fn check_rotation(&mut self) {
        let current = match &self.current_path {
            Some(p) => p.clone(),
            None => return,
        };
        if let Some(new_path) = find_active_file(&self.wcap_dir) {
            if new_path != current {
                info!(
                    old = %current.display(),
                    new = %new_path.display(),
                    "wcap file rotation detected"
                );
                match open_raw_wcap(&new_path) {
                    Ok((new_reader, instance_id, run_id)) => {
                        self.reader = Some(new_reader);
                        self.current_path = Some(new_path);
                        self.instance_id = Some(instance_id);
                        self.run_id = Some(run_id);
                        self.eof_count = 0;
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to open rotated wcap file");
                    }
                }
            }
        }
    }

    pub fn is_open(&self) -> bool {
        self.reader.is_some()
    }

    pub fn current_path(&self) -> Option<&Path> {
        self.current_path.as_deref()
    }

    pub fn instance_id(&self) -> Option<&str> {
        self.instance_id.as_deref()
    }

    pub fn run_id(&self) -> Option<&str> {
        self.run_id.as_deref()
    }
}

fn open_raw_wcap(path: &Path) -> Result<(BufReader<File>, String, String), Error> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let (instance_id, run_id) = format::read_file_header(&mut reader)?;
    Ok((reader, instance_id, run_id))
}
