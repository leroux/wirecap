//! Stream wirecap records to stdout as JSONL, following active files
//! like `tail -f`. Handles file rotation seamlessly.
//!
//! Usage: wirecap-tail [--from-start] <dir>

use std::io::{self, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

use wirecap::WcapTailer;

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const READ_BATCH: usize = 4096;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut from_start = false;
    let mut dir_arg = None;

    for arg in &args[1..] {
        match arg.as_str() {
            "--from-start" => from_start = true,
            s if s.starts_with('-') => {
                eprintln!("unknown flag: {s}");
                eprintln!("usage: wirecap-tail [--from-start] <dir>");
                std::process::exit(1);
            }
            _ => dir_arg = Some(arg.as_str()),
        }
    }

    let dir = dir_arg.unwrap_or_else(|| {
        eprintln!("usage: wirecap-tail [--from-start] <dir>");
        std::process::exit(1);
    });
    let dir = Path::new(dir);

    if !dir.is_dir() {
        eprintln!("not a directory: {}", dir.display());
        std::process::exit(1);
    }

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    let mut tailer = WcapTailer::new(dir.to_path_buf());

    // If not --from-start, we want to skip to end of the current file.
    // The WcapTailer always reads from the start, so for tail -f behavior
    // we drain all existing records silently before printing.
    if !from_start {
        if tailer.try_open() {
            eprintln!(
                "# seeking to end of {} (use --from-start to read existing)",
                tailer.current_path().unwrap_or(Path::new("?")).display()
            );
            // Drain all existing records
            loop {
                let batch = tailer.read_batch(READ_BATCH);
                if batch.is_empty() {
                    break;
                }
            }
        }
    }

    loop {
        if !tailer.is_open() {
            tailer.try_open();
        }

        let entries = tailer.read_batch(READ_BATCH);
        for entry in &entries {
            print_record(entry, &mut out);
        }
        let _ = out.flush();

        if entries.is_empty() {
            thread::sleep(POLL_INTERVAL);
        }
    }
}

fn print_record(entry: &wirecap::format::Entry, out: &mut impl Write) {
    let payload = if entry.payload.is_empty() {
        "null"
    } else {
        std::str::from_utf8(&entry.payload).unwrap_or("null")
    };
    let meta_str = if entry.meta.is_empty() {
        String::new()
    } else {
        let m = String::from_utf8_lossy(&entry.meta);
        format!(r#","meta":{m}"#)
    };
    let mono_str = match entry.mono_ns {
        Some(v) => format!(r#","mono_ns":{v}"#),
        None => String::new(),
    };
    let seq_str = match entry.recv_seq {
        Some(v) => format!(r#","recv_seq":{v}"#),
        None => String::new(),
    };
    let _ = writeln!(
        out,
        r#"{{"ts":{ts}{mono}{seq},"src":{src},"dir":"{dir}"{meta},"d":{payload}}}"#,
        ts = entry.ts,
        mono = mono_str,
        seq = seq_str,
        src = entry.src,
        dir = entry.dir.as_str(),
        meta = meta_str,
        payload = payload,
    );
}
