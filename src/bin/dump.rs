use std::fs::File;
use std::io::BufReader;

use clap::Parser;

#[derive(Parser)]
#[command(name = "wirecap-dump", about = "Read and dump wirecap capture files")]
struct Cli {
    /// Path to .wcap or .wcap.zst file
    file: std::path::PathBuf,

    /// Show summary statistics instead of records
    #[arg(long)]
    stats: bool,

    /// Filter by channel ID (u8, e.g., 0, 1, 2)
    #[arg(long)]
    filter: Option<u8>,
}

fn main() {
    let cli = Cli::parse();

    let file = File::open(&cli.file).unwrap_or_else(|e| {
        eprintln!("failed to open {}: {e}", cli.file.display());
        std::process::exit(1);
    });

    let is_zst = cli.file.extension().is_some_and(|ext| ext == "zst");

    let mut reader: Box<dyn std::io::Read> = if is_zst {
        Box::new(zstd::Decoder::new(file).unwrap_or_else(|e| {
            eprintln!("failed to open zstd stream: {e}");
            std::process::exit(1);
        }))
    } else {
        Box::new(BufReader::new(file))
    };

    let (instance_id, run_id) = wirecap::format::read_file_header(&mut reader).unwrap_or_else(|e| {
        eprintln!("bad file header: {e}");
        std::process::exit(1);
    });

    if cli.stats {
        print_stats(&mut reader, &instance_id, &run_id);
    } else {
        dump_records(&mut reader, cli.filter);
    }
}

fn dump_records(r: &mut dyn std::io::Read, filter: Option<u8>) {
    loop {
        match wirecap::format::read_record(r) {
            Ok(Some(entry)) => {
                if let Some(f) = filter {
                    if entry.src != f {
                        continue;
                    }
                }
                let payload = if entry.payload.is_empty() { "null" } else { std::str::from_utf8(&entry.payload).unwrap_or("null") };
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
                println!(
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
            Ok(None) => break,
            Err(e) => {
                eprintln!("read error: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn print_stats(r: &mut dyn std::io::Read, instance_id: &str, run_id: &str) {
    let mut count: u64 = 0;
    let mut bytes: u64 = 0;
    let mut min_ts: u64 = u64::MAX;
    let mut max_ts: u64 = 0;
    let mut by_channel = [0u64; 256];

    loop {
        match wirecap::format::read_record(r) {
            Ok(Some(entry)) => {
                count += 1;
                bytes += entry.payload.len() as u64;
                if entry.ts < min_ts { min_ts = entry.ts; }
                if entry.ts > max_ts { max_ts = entry.ts; }
                by_channel[entry.src as usize] += 1;
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("read error at record {count}: {e}");
                break;
            }
        }
    }

    let duration_secs = if max_ts > min_ts {
        (max_ts - min_ts) / 1_000_000_000
    } else {
        0
    };

    println!("instance_id: {instance_id}");
    println!("run_id:      {run_id}");
    println!("entries:     {count}");
    println!("payload_bytes: {bytes}");
    println!("duration:    {duration_secs}s");
    println!("channels:");
    for (ch, &n) in by_channel.iter().enumerate() {
        if n > 0 {
            println!("  {ch}: {n}");
        }
    }
}
