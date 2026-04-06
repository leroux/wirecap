# wirecap

Append-only binary capture for wire-level data. Record every byte in and out
across multiple channels, then read it back — fast.

Wirecap writes a compact binary format (`.wcap`) designed for one thing:
getting high-throughput wire data to disk with minimal overhead and no data
loss. It handles file rotation, zstd compression, crash recovery, and async
backpressure so your application code doesn't have to.

## Features

- **Application-level capture** — records structured entries with direction
  (`In`/`Out`), channel tags, and metadata, not raw packets. Natural fit for
  WebSocket frames, REST calls, or any bidirectional byte stream.
- **Write-path simplicity** — 33-byte fixed header, no inline compression.
  Designed to stay off the hot path.
- **Operational batteries included** — file rotation (by size and age), zstd
  compression (background, post-rotation), crash recovery (`.active` →
  `.recovered`), periodic fsync, and Prometheus metrics are all built in.
- **Async backpressure** — configurable channel depth (default 64K entries)
  lets the writer apply backpressure instead of silently dropping data.
- **Live tailing** — `WcapTailer` follows active files like `tail -f`,
  handling partial records and file rotation automatically.

## Limitations

- **No ecosystem** — custom binary format means no Wireshark, no tcpdump, no
  third-party tooling. You use the wirecap reader (Rust or Python).
- **No random access** — strictly sequential. No index, no summary section,
  no seeking to a timestamp without scanning from the start.
- **Channel tags are a bare u8** — 256 channels max, no built-in label
  registry. Mapping channel IDs to names is the caller's responsibility.

## Install

```toml
[dependencies]
wirecap = { git = "https://github.com/leroux/wirecap" }
```

## Quick start

### Writing

`Capture::start` validates the config, spawns a background writer thread,
and returns a cheap-to-clone handle plus the thread's `JoinHandle`. Drop all
clones of the handle to signal shutdown; join the handle to wait for the
writer (and all background compression threads) to finish.

```rust
use wirecap::{Capture, CaptureConfig, Dir, WriteEntry};

let config = CaptureConfig::new("my-service", "/var/data/capture")?;
let (cap, handle) = Capture::start(config)?;

cap.log(WriteEntry {
    ts: now_ns,
    mono_ns,
    recv_seq,
    src: 0,           // your channel tag
    dir: Dir::In,
    meta: Vec::new(),
    payload: raw_bytes,
}).await?;

drop(cap);              // signal shutdown
handle.join().unwrap(); // wait for drain + compression
```

`CaptureConfig::new` validates `instance_id` (no path separators, nulls,
`.`/`..`, length ≤255 bytes) and returns `Result`. Builder methods let you
tune the defaults:

```rust
let config = CaptureConfig::new("my-service", "/var/data/capture")?
    .channel_capacity(1024)         // default 65,536
    .max_file_bytes(64 * 1024 * 1024)  // default 100 MB
    .max_file_secs(600)             // default 1800 (30 min)
    .max_payload_bytes(8 * 1024 * 1024) // default 16 MB
    .max_consecutive_failures(50);  // default 100
```

For callers that want to write `.wcap` files without the async `Capture`
machinery, `WcapWriter` is a synchronous writer that wraps any `impl Write`:

```rust
use wirecap::WcapWriter;

let file = std::fs::File::create("out.wcap")?;
let mut writer = WcapWriter::new(file, "my-service", "run-id", 16 * 1024 * 1024)?;
writer.write(&entry)?;
writer.flush()?;
```

### Reading

`WcapReader::open` opens any `.wcap`, `.wcap.zst`, `.wcap.recovered`, or
`.wcap.recovered.zst` file. The iterator yields `Result<ReadEntry, Error>`,
so errors are propagated, not swallowed.

```rust
use wirecap::WcapReader;

let reader = WcapReader::open(path)?;
for entry in reader {
    let entry = entry?;
    println!(
        "ch={} dir={} len={}",
        entry.src,
        entry.dir.as_str(),
        entry.payload.len()
    );
}
```

`ReadEntry` has `Option<u64>` for `mono_ns` and `recv_seq` because v1 and v2
records (legacy formats) didn't have those fields. v3 records always populate
them as `Some(_)`.

### Tailing a live file

`WcapTailer` follows the current `.wcap.active` file, handling partial
records (seeks back and retries) and rotation (switches to the new active
file automatically). Polling is the caller's responsibility.

```rust
use std::time::Duration;
use wirecap::WcapTailer;

let mut tailer = WcapTailer::new("/var/data/capture".into());
loop {
    if !tailer.try_open() {
        std::thread::sleep(Duration::from_millis(100));
        continue;
    }
    let batch = tailer.read_batch(64);
    if batch.is_empty() {
        std::thread::sleep(Duration::from_millis(100));
        continue;
    }
    for entry in batch {
        // process entry
    }
}
```

## Format

Wirecap files go through a lifecycle:

| Extension | State |
|---|---|
| `.wcap.active` | Being written by the capture task |
| `.wcap` | Sealed after rotation (size or age trigger) |
| `.wcap.zst` | Compressed with zstd (background, post-rotation) |
| `.wcap.recovered` | Renamed from `.active` after an unclean shutdown |

Each file starts with a header (`WCAP` magic + instance/run IDs), followed by
a flat sequence of records. Each v3 record is a 33-byte header followed by
optional metadata and payload bytes.

See [SPEC.md](SPEC.md) for the full byte-level format.

## Testing

```bash
# Run the standard test suite (unit + integration + property tests).
cargo test
```

Wirecap also has an optional `buggify` feature that enables `ferro-buggify`
probabilistic fault injection at 17 sites in the writer and reader. Enable it
to run swarm-style chaos tests that exercise error paths normal tests can't
reach:

```bash
cargo test --features buggify --test buggify
```

Each fault site has a deterministic per-seed activation, so failures are
reproducible: `FERRO_BUGGIFY_SEED=42 cargo test --features buggify` always
explores the same fault combinations.
