# Track 4: Async Correctness & Performance Review

**Crate**: `wirecap` (v0.1.0, ~960 lines)
**Date**: 2026-04-06
**Reviewer**: Claude Opus 4.6
**Scope**: Sync-in-async hazards, buffering, allocation patterns, channel design, backpressure semantics, compression performance, reader performance, tailer efficiency, metrics overhead, tokio feature flags

**Prior findings referenced but not duplicated**: Track 2 flagged writer_task as a silent black hole for I/O errors (T2-2.1). Track 3 flagged recovery compression blocks startup synchronously (T3-3.2), entry dropped on failed rotation (T3-2.3), bytes_written excludes header (T3-1.2), compression is fire-and-forget on rotation (T3-7.4). The `compress_file` to `spawn_blocking` fix was applied in commit f7aca8f. This track focuses exclusively on async correctness and performance dimensions.

---

## 1. Sync I/O in Async Context

### Finding 1.1 [critical] -- Entire writer_task performs blocking I/O on the tokio async thread pool

`capture.rs:184-294` -- `writer_task` is an `async fn` intended to be driven by `tokio::spawn` (see `capture.rs:113`, `tests/integration.rs:113`). Inside the `select!` loop, every I/O operation uses `std::fs` and `std::io` synchronous APIs:

| Operation | Call site | Blocking call | Typical latency |
|-----------|-----------|---------------|-----------------|
| Write record | `capture.rs:299` via `format::write_record` | `File::write_all` x 6-8 per record | 1-10 us (page cache), 1-50 ms (flush/eviction) |
| Open file | `capture.rs:312-326` | `File::create`, `write_all` | 50-500 us (metadata + journal) |
| fsync | `capture.rs:223` | `File::sync_data` | 1-50 ms (NVMe), 10-200 ms (spinning disk) |
| Rotation fsync | `capture.rs:243` | `File::sync_data` | same |
| Rename | `capture.rs:345` via `finalize_file` | `fs::rename` | 50-500 us |
| mkdir | `capture.rs:314` | `fs::create_dir_all` | 10-100 us |
| Rotation sync_data + finalize + open | `capture.rs:242-269` | all of the above combined | 2-250 ms |
| Recovery | `capture.rs:194` | `read_dir`, `rename`, `compress_file` | seconds (!) |

**Why this is critical**: `tokio::spawn` places the future on the multi-threaded runtime's cooperative task queue. A blocking call holds a runtime worker thread hostage. With tokio's default of `num_cpus` worker threads, a single 50 ms `sync_data` call blocks 1/N of the runtime's capacity. During rotation (fsync + rename + open), the block can exceed 100 ms. During recovery at startup (`capture.rs:194`, `recover_active_files` calls `compress_file` synchronously -- flagged in T3-3.2), the block can be seconds.

Meanwhile, other tasks on the same worker -- including the `mpsc::Receiver::recv` that drives this very loop -- are starved. The `tokio::select!` loop cannot make progress on the fsync timer branch while a write blocks the recv branch.

The 1-second fsync interval (`capture.rs:209`) is particularly dangerous: `sync_data()` is called on a biased select where `recv` has priority. But between two `recv` completions, a single `sync_data` call blocks the entire worker thread for the duration of the disk flush. Under high entry throughput, the fsync branch rarely fires (good for throughput, but means data durability is poor). Under low throughput, the fsync fires every second and each one blocks the worker.

**Quantifying the impact**: On a system with 4 worker threads and 10 ms average fsync latency, 10 fsyncs/sec would consume 100 ms/sec of worker time = 2.5% of total runtime capacity. Under rotation (once per 30 min at default), the ~100-250 ms combined block is a single spike. The real danger is tail latency: any task co-scheduled with the writer sees up to 50 ms of added latency from fsync alone.

**Suggested fix**: Move the entire writer_task body into `spawn_blocking`. The writer_task is a fundamentally I/O-bound loop that does not benefit from async. The only truly async operation it uses is `mpsc::Receiver::recv()` and `tokio::time::interval`, both of which have blocking equivalents:

```rust
// Option A: Use spawn_blocking for the entire writer
let writer = tokio::task::spawn_blocking(move || {
    // Use std::sync::mpsc or crossbeam channel instead
    // Use std::thread::sleep or a condvar for fsync interval
    writer_task_blocking(rx, ...);
});

// Option B: Wrap individual operations (less clean but minimal refactor)
// Replace std::fs::File with tokio::fs::File
// Use tokio::task::spawn_blocking for sync_data and open
```

Option A is strongly preferred. The writer_task has no reason to be async -- it is a single sequential loop doing blocking I/O. Making it a blocking task frees the async runtime entirely.

### Finding 1.2 [major] -- `recover_active_files` blocks the async runtime at startup

`capture.rs:194` calls `recover_active_files(output_dir)` at the top of `writer_task`. This function (`capture.rs:350-375`) does `read_dir`, multiple `rename` calls, and -- critically -- calls `compress_file` synchronously for each `.wcap.active` file found. The `compress_file` function reads an entire file and zstd-compresses it. For a 100 MB file at zstd level 3, this takes 200-800 ms. Multiple files multiply this.

This was flagged in T3-3.2 but bears repeating in the async context: this blocks a tokio worker thread for potentially seconds before the writer loop even starts. Any task sharing that worker is starved.

**Note**: The `compress_file` call during rotation (line 249-251) was correctly moved to `spawn_blocking` in commit f7aca8f, but the recovery path at line 372 was not updated to match.

**Suggested fix**: Either move `recover_active_files` into a `spawn_blocking` call, or (better) apply Finding 1.1 and make the entire writer a blocking task.

---

## 2. Buffering

### Finding 2.1 [major] -- write_record issues 6-8 unbuffered write(2) syscalls per record

`format.rs:83-99` -- `write_record` takes `w: &mut impl Write`. The caller passes `&mut of.file` directly (`capture.rs:299`), which is a raw `std::fs::File` with no `BufWriter` wrapping. Each `write_all` call becomes a `write(2)` syscall:

```
Call 1: write_all(&[RECORD_VERSION])             -- 1 byte
Call 2: write_all(&entry.ts.to_le_bytes())       -- 8 bytes
Call 3: write_all(&mono_ns.to_le_bytes())        -- 8 bytes
Call 4: write_all(&recv_seq.to_le_bytes())       -- 8 bytes
Call 5: write_all(&meta_len.to_le_bytes())       -- 2 bytes
Call 6: write_all(&payload_len.to_le_bytes())    -- 4 bytes
Call 7: write_all(&[entry.src, entry.dir as u8]) -- 2 bytes
Call 8: write_all(&entry.meta)                   -- 0-64KB (conditional, line 95-97)
Call 9: write_all(&entry.payload)                -- 0-4GB
```

That is 7-9 `write(2)` syscalls per record. For 10,000 records/sec, that is 70,000-90,000 syscalls/sec. Each syscall costs ~200-500 ns (user-kernel transition), totaling 14-45 ms/sec of pure syscall overhead. The kernel may coalesce small writes in the page cache, but the syscall entry/exit overhead remains.

Similarly, `write_file_header` (`format.rs:63-79`) issues 7 `write_all` calls for the header.

**Suggested fix**: Wrap the `File` in a `BufWriter` when creating the `OpenFile`:

```rust
struct OpenFile {
    file: BufWriter<File>,
    // ...
}
```

A `BufWriter` with the default 8 KB buffer will coalesce all header fields into a single `write(2)` syscall for any record smaller than 8 KB. For the common case (33-byte header + small payload), a single record becomes 1 syscall instead of 7-9.

**Caveat**: When using `BufWriter`, the `sync_data()` call must be preceded by a `flush()` to ensure the buffer contents reach the OS. The current code calls `of.file.sync_data()` directly -- with a `BufWriter` wrapping, this must become `of.file.flush()?; of.file.get_ref().sync_data()?;` or the `BufWriter` must be unwrapped before fsync. Alternatively, implement a wrapper that flushes before syncing.

### Finding 2.2 [minor] -- write_record could build the header into a stack buffer

Even with `BufWriter`, the 7 individual `write_all` calls for the 33-byte fixed header generate 7 method calls with 7 bounds checks (the `Write` trait checks). A single stack-allocated `[u8; 33]` buffer written via one `write_all` call would be both cleaner and faster:

```rust
let mut hdr = [0u8; RECORD_HEADER_SIZE];
hdr[0] = RECORD_VERSION;
hdr[1..9].copy_from_slice(&entry.ts.to_le_bytes());
hdr[9..17].copy_from_slice(&entry.mono_ns.unwrap_or(0).to_le_bytes());
// ... etc
w.write_all(&hdr)?;
```

This reduces the fixed-header write from 7 calls to 1, eliminating 6 function calls and bounds checks even with BufWriter. Combined with BufWriter from Finding 2.1, total syscalls per record drops from 7-9 to 1-3 (header + optional meta + payload, typically coalesced to 1).

---

## 3. Allocation Patterns

### Finding 3.1 [minor] -- `format!` allocation on every file open

`capture.rs:319`:
```rust
let filename = format!("{instance_id}_{timestamp}.{millis:03}Z_{run_id}.wcap.active");
```

This allocates a `String` on every file open. File opens happen at most once per 30 minutes at default settings, so this is negligible. No action needed.

### Finding 3.2 [minor] -- `PathBuf::from(format!(...))` in compress_file

`capture.rs:379`:
```rust
let zst_path = PathBuf::from(format!("{}.zst", path.display()));
```

One `String` allocation + one `PathBuf` allocation. Called once per rotation. Negligible.

### Finding 3.3 [minor] -- `read_record` allocates meta and payload Vec on every record

`format.rs:157-158` (v1), `format.rs:174-175, 182` (v2), `format.rs:201-202, 209` (v3):
```rust
let mut buf = vec![0u8; meta_len as usize];  // heap alloc
// ...
let mut payload = vec![0u8; payload_len as usize]; // heap alloc
```

Every `read_record` call allocates 1-2 `Vec<u8>` on the heap (meta + payload). For the reader hot path, this is one allocation per record. With a typical payload of 1 KB and thousands of records, this generates significant allocator pressure.

This is fundamentally hard to avoid because `Entry` owns its `meta` and `payload` buffers, and the sizes are not known until the header is parsed. Possible mitigations:

- **Reusable buffer pool**: Pass a `&mut Vec<u8>` scratch buffer that is cleared and reused across reads. Requires changing the `Entry` ownership model or using a borrow-based design.
- **Arena allocator**: Use `bumpalo` or a similar arena for batch reads.
- **Accept it**: For a file I/O bound reader, the allocator overhead is likely dwarfed by I/O time. The `BufReader` wrapping (Finding 7.1) ensures the actual `read(2)` syscalls are batched.

**Verdict**: This is a real cost but likely not the bottleneck. Flag as minor and revisit if profiling shows allocator contention.

### Finding 3.4 [nit] -- `Entry` in the channel carries owned Vec allocations

Each `Entry` sent through the channel (`capture.rs:89, 151`) contains `meta: Vec<u8>` and `payload: Vec<u8>`. These are heap-allocated by the caller and moved into the channel. The channel itself (`mpsc` with 65K capacity) stores the `Entry` values inline in its buffer. Since `Entry` is 80 bytes on the stack (3x u64, 2x Option<u64>, u8, Dir, 2x Vec<u8> = 2x24 bytes + padding), the channel buffer is 65K x 80 = ~5 MB of inline storage plus the heap allocations for meta/payload data.

This is a reasonable design. The alternative (using `Bytes` or `Arc<[u8]>` for zero-copy) would add complexity for marginal gain, since the data must be serialized into the file format anyway.

---

## 4. Channel Design

### Finding 4.1 [minor] -- 65K capacity mpsc channel is the right primitive but the capacity may be oversized

`capture.rs:14`:
```rust
const DEFAULT_CHANNEL_CAPACITY: usize = 65_536;
```

`capture.rs:102`:
```rust
let (tx, rx) = mpsc::channel(config.channel_capacity);
```

**Channel type analysis**: `tokio::sync::mpsc` is correct for multi-producer (many callers of `cap.log()`) to single-consumer (the writer task). Alternatives:

| Channel | Pros | Cons |
|---------|------|------|
| `tokio::sync::mpsc` (current) | async-native, integrates with `select!`, bounded backpressure | Requires tokio runtime |
| `tokio::sync::mpsc::unbounded` | Never blocks sender | No backpressure; OOM risk under sustained overload |
| `flume` | Faster for high contention, supports both sync/async endpoints | Extra dependency |
| `crossbeam::channel` | Fastest for sync-to-sync | No async recv; would require `spawn_blocking` for the writer (which Finding 1.1 recommends anyway) |
| `kanal` | Very fast, supports async | Newer/less battle-tested |

If Finding 1.1 is adopted (moving writer to `spawn_blocking`), `crossbeam::channel::bounded` becomes the natural choice. It avoids the tokio async machinery entirely, has lower overhead per send/recv, and the bounded variant provides backpressure.

**Capacity analysis**: 65,536 entries x 80 bytes (Entry stack size) = 5.2 MB of channel buffer, plus heap allocations for each entry's meta/payload. If the average payload is 1 KB, the channel at full capacity holds ~65 MB of payload data. This is a significant memory commitment.

For most use cases (wire capture at 1-10K entries/sec), 4,096 or 8,192 entries would provide sufficient buffering for write latency spikes without the memory overhead. The 65K capacity means the system can absorb ~6.5 seconds of write stall at 10K entries/sec, which is generous.

**Suggested fix**: Consider reducing the default to 8,192 (still 800 ms of buffer at 10K/sec). Document the capacity tradeoff. If using `crossbeam` per Finding 1.1, profile to find the sweet spot.

### Finding 4.2 [nit] -- Channel could use `try_send` for non-blocking callers

There is no `try_log` method on `Capture`. Some callers may prefer to drop entries rather than block when the channel is full (e.g., a latency-sensitive request handler). This is a design choice rather than a bug. See Finding 5.1.

---

## 5. Backpressure Behavior

### Finding 5.1 [major] -- `log().await` blocks the caller when the channel is full, with no alternative

`capture.rs:148-155`:
```rust
pub async fn log(&self, entry: Entry) -> Result<(), CaptureClosed> {
    self.metrics.channel_depth.increment(1.0);
    self.tx.send(entry).await.map_err(|_| {
        self.metrics.channel_depth.decrement(1.0);
        CaptureClosed
    })
}
```

When the channel is full, `send().await` suspends the calling task until space is available. This is the correct default for ensuring no data loss, but it has severe implications:

1. **Latency coupling**: If the writer stalls (disk I/O spike, rotation, fsync), all callers of `log()` are suspended. In a request-handling context, this means request latency directly tracks worst-case disk I/O latency.

2. **Cascading backpressure**: If multiple async tasks call `log()` simultaneously and the channel is full, they all suspend. When the writer drains one slot, exactly one wakes up; the rest remain suspended. Under sustained overload, this creates a convoy effect.

3. **Deadlock risk with `select!`**: If a caller uses `tokio::select!` with `cap.log(entry).await` and a timeout, the `entry` is consumed when `log()` is called (moved into the future). If the timeout fires first, the entry is dropped. This is correct behavior but may surprise callers who expect to retry with the same entry.

**Suggested fix**: Add a `try_log` method for latency-sensitive callers:

```rust
pub fn try_log(&self, entry: Entry) -> Result<(), Entry> {
    match self.tx.try_send(entry) {
        Ok(()) => {
            self.metrics.channel_depth.increment(1.0);
            Ok(())
        }
        Err(mpsc::error::TrySendError::Full(entry)) => Err(entry),
        Err(mpsc::error::TrySendError::Closed(entry)) => Err(entry),
    }
}
```

This lets callers choose: `log().await` for guaranteed delivery with backpressure, or `try_log()` for best-effort with bounded latency.

### Finding 5.2 [minor] -- Metrics increment before send, decrement only on error

`capture.rs:149`:
```rust
self.metrics.channel_depth.increment(1.0);
```

The `channel_depth` gauge is incremented before the `send().await` call. If the send blocks (channel full), the gauge is already incremented even though the entry is not yet in the channel. This means `wirecap_channel_depth` shows a value higher than the actual channel occupancy during backpressure. The decrement happens on the writer side (`capture.rs:233`).

**Impact**: The gauge is transiently inaccurate during backpressure. Under sustained backpressure, the overcounting equals the number of blocked senders. In practice this is unlikely to cause operational confusion, but it is semantically incorrect.

**Suggested fix**: Increment after successful send:
```rust
pub async fn log(&self, entry: Entry) -> Result<(), CaptureClosed> {
    self.tx.send(entry).await.map_err(|_| CaptureClosed)?;
    self.metrics.channel_depth.increment(1.0);
    Ok(())
}
```

---

## 6. Compression Performance

### Finding 6.1 [minor] -- zstd level 3 is a reasonable default

`capture.rs:385`:
```rust
let mut encoder = zstd::Encoder::new(output, 3)?;
```

zstd level 3 (the library default) provides a good balance:

| Level | Compression ratio (typical) | Speed (MB/s encode) |
|-------|---------------------------|---------------------|
| 1 | ~2.8x | ~500 |
| 3 | ~3.2x | ~350 |
| 6 | ~3.5x | ~150 |
| 9 | ~3.8x | ~50 |

For 100 MB files, level 3 takes ~285 ms. Level 1 would save ~85 ms per rotation at the cost of ~12% larger files. Since compression runs in `spawn_blocking` (post-f7aca8f), the async runtime is not affected. Level 3 is the right choice.

### Finding 6.2 [minor] -- Compression uses BufReader on input but no BufWriter on output

`capture.rs:382-389`:
```rust
let input = File::open(path)?;
let output = File::create(&zst_path)?;
let mut encoder = zstd::Encoder::new(output, 3)?;
std::io::copy(&mut std::io::BufReader::new(input), &mut encoder)?;
```

The input is wrapped in `BufReader` (good -- `io::copy` will read in 8 KB chunks). The zstd `Encoder` has its own internal buffer, so it batches writes to the underlying `File`. However, the encoder's `finish()` call flushes the final block, which may issue small writes. This is a negligible concern since compression happens once per rotation in a blocking thread.

### Finding 6.3 [nit] -- Compression error handling is asymmetric

`capture.rs:391-403` -- On compression failure, the partial `.zst` file is cleaned up (`remove_file(&zst_path)`), but the original file is preserved. On success, the original is deleted. This is correct behavior. However, the error from `remove_file` of the partial `.zst` on failure is silently discarded (`let _ = fs::remove_file(&zst_path)`). If the partial file cannot be deleted, it will be left on disk as a corrupt `.zst` file that could confuse readers.

**Impact**: Very minor. The reader's `WcapReader::open` would fail to decompress the corrupt file and return an error. But the corrupt file would be discovered in `discover_files` and returned to callers.

---

## 7. Reader Performance

### Finding 7.1 [major] -- `Box<dyn Read>` prevents monomorphization and inlining

`reader.rs:91-95`:
```rust
pub struct WcapReader {
    reader: Box<dyn Read>,
    // ...
}
```

`reader.rs:105-109`:
```rust
let mut reader: Box<dyn Read> = if is_zst {
    Box::new(BufReader::new(zstd::Decoder::new(BufReader::new(file))?))
} else {
    Box::new(BufReader::new(file))
};
```

Using `Box<dyn Read>` means every `read` call on `self.reader` goes through a vtable dispatch. Since `read_record` (`format.rs:126`) takes `r: &mut dyn Read`, there is a double dynamic dispatch: `WcapReader.next()` calls `read_record(&mut self.reader)` which calls `r.read_exact()` on the `dyn Read`.

More importantly, `Box<dyn Read>` prevents the compiler from inlining `BufReader`'s buffering logic. `BufReader::read_exact` is normally inlined to a memcpy from the internal buffer + occasional `read(2)` syscall. Behind `dyn Read`, the compiler cannot inline any of this; every read goes through two vtable lookups.

**Quantifying the impact**: For small reads (the 1-33 byte header fields), vtable dispatch adds ~2-5 ns per call. With 4-6 `read_exact` calls per record in `read_record`, this is 8-30 ns per record of vtable overhead. At 100K records/sec read throughput, that is 0.8-3 ms/sec -- minor but measurable.

The larger cost is the lost inlining opportunity. Without inlining, the compiler cannot see that `BufReader::read_exact` for small sizes is a hot memcpy from a buffer, and cannot apply SIMD or loop unrolling optimizations.

**Suggested fix**: Use an enum instead of trait object:

```rust
enum WcapInput {
    Raw(BufReader<File>),
    Zstd(BufReader<zstd::Decoder<'static, BufReader<File>>>),
}

impl Read for WcapInput {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Raw(r) => r.read(buf),
            Self::Zstd(r) => r.read(buf),
        }
    }
}
```

This eliminates vtable dispatch and allows the compiler to monomorphize the read path. The branch prediction for the `match` is effectively free since the variant never changes after construction.

Also change `read_record` and `read_file_header` in `format.rs` from `&mut dyn Read` to generic `&mut impl Read` to propagate the monomorphization. Lines `format.rs:103` and `format.rs:126` both use `&mut dyn std::io::Read` which forces dynamic dispatch even when the concrete type is known.

### Finding 7.2 [minor] -- Double BufReader wrapping for zstd path

`reader.rs:106`:
```rust
Box::new(BufReader::new(zstd::Decoder::new(BufReader::new(file))?))
```

This creates `BufReader<Decoder<BufReader<File>>>` -- two layers of `BufReader`. The inner `BufReader` buffers reads from the file (good -- prevents many small syscalls for zstd's internal reads). The outer `BufReader` buffers reads from the decoder.

The outer `BufReader` is potentially useful because `zstd::Decoder` may return decompressed data in variable-sized chunks that do not align with `read_exact` calls. However, `zstd::Decoder` already has an internal output buffer (default 128 KB). The outer `BufReader` (default 8 KB) is smaller than the decoder's buffer, so it adds a redundant copy: decoder's 128 KB buffer -> outer BufReader's 8 KB buffer -> caller's buffer.

**Impact**: One extra memcpy per 8 KB of decompressed data. For a 100 MB file, that is ~12,800 extra memcpys. At ~1 ns/byte for memcpy, the 8 KB copies add ~100 us total -- negligible.

**Suggested fix**: Remove the outer `BufReader` for the zstd path since the decoder already buffers. Keep it for the raw path:

```rust
let reader: WcapInput = if is_zst {
    WcapInput::Zstd(zstd::Decoder::new(BufReader::new(file))?)
} else {
    WcapInput::Raw(BufReader::new(file))
};
```

### Finding 7.3 [nit] -- `read_record` uses `dyn Read` parameters throughout format.rs

`format.rs:103`:
```rust
pub fn read_file_header(r: &mut dyn std::io::Read) -> ...
```

`format.rs:126`:
```rust
pub fn read_record(r: &mut dyn std::io::Read) -> ...
```

And the internal helpers `read_record_v1` (line 148), `read_record_v2` (line 164), `read_record_v3` (line 189), `read_length_prefixed_string` (line 229) all take `&mut dyn std::io::Read`.

This forces dynamic dispatch for all read operations regardless of whether the caller knows the concrete type. Changing these to `impl Read` (or a generic `R: Read`) would allow monomorphization when the concrete type is known at compile time, while still supporting `dyn Read` callers through implicit coercion.

Note that this interacts with Finding 7.1: even if `WcapReader` uses an enum, the `dyn Read` parameters in `format.rs` re-erase the type. Both must be changed together for the optimization to take effect.

---

## 8. Tailer Efficiency

### Finding 8.1 [major] -- `stream_position()` syscall before every record read

`reader.rs:228-229`:
```rust
let pos = match reader.stream_position() {
    Ok(p) => p,
    Err(_) => break,
};
```

Inside the `read_batch` loop (which runs up to `max_batch` times per call), `stream_position()` is called before every `read_record` attempt. `BufReader::stream_position()` calls `self.inner.seek(SeekFrom::Current(0))` minus the buffer offset -- this is an `lseek(2)` syscall.

For a batch of 100 records, this is 100 `lseek(2)` syscalls, each costing ~200-500 ns. At 1000 batches/sec (polling at 1 ms intervals with 100 records per batch), that is 100K lseek syscalls/sec = 20-50 ms/sec of syscall overhead.

**Why it exists**: The position is saved so the tailer can rewind on a partial record at EOF (line 247). This is correct behavior -- the tailer must handle partial writes from the recorder. But the save/restore is only needed for the *last* record read (the one that hits EOF or gets a partial read error).

**Suggested fix**: Track position incrementally instead of calling `stream_position()`:

```rust
pub fn read_batch(&mut self, max_batch: usize) -> Vec<Entry> {
    let reader = match &mut self.reader {
        Some(r) => r,
        None => return Vec::new(),
    };

    let mut entries = Vec::new();
    // Get position once at the start
    let mut pos = match reader.stream_position() {
        Ok(p) => p,
        Err(_) => return entries,
    };

    for _ in 0..max_batch {
        match format::read_record(reader) {
            Ok(Some(entry)) => {
                // Update position by the record's serialized size
                pos += (format::RECORD_HEADER_SIZE + entry.meta.len() + entry.payload.len()) as u64;
                entries.push(entry);
                self.eof_count = 0;
            }
            Ok(None) => {
                self.eof_count += 1;
                if self.eof_count % 100 == 0 {
                    self.check_rotation();
                }
                break;
            }
            Err(_) => {
                // Rewind to last known good position
                let _ = reader.seek(std::io::SeekFrom::Start(pos));
                break;
            }
        }
    }
    entries
}
```

This reduces `lseek` syscalls from N per batch to 1 per batch. The position tracking is slightly fragile (must exactly match serialized size), so an alternative is to call `stream_position()` only once before the loop and update it by adding `RECORD_HEADER_SIZE + meta.len() + payload.len()` after each successful read.

### Finding 8.2 [minor] -- Rotation detection uses filesystem polling every 100 EOFs

`reader.rs:240-243`:
```rust
Ok(None) => {
    self.eof_count += 1;
    if self.eof_count % 100 == 0 {
        self.check_rotation();
    }
    break;
}
```

`check_rotation` (`reader.rs:262-288`) calls `find_active_file`, which calls `read_dir` and iterates the entire directory. This happens every 100 EOF encounters.

If the tailer is polling at 100 ms intervals and hitting EOF each time, the rotation check fires every 10 seconds. The `read_dir` call costs ~10-100 us depending on directory size. This is acceptable.

However, the 100-EOF threshold is a magic number with no configuration. If the polling interval is 10 ms (aggressive polling), the rotation check fires every second. If it is 1 second (lazy polling), the check fires every ~100 seconds, meaning rotation detection could be delayed by up to 100 seconds.

**Suggested fix**: Make the rotation check interval time-based rather than count-based:

```rust
last_rotation_check: Instant,
// ...
if self.last_rotation_check.elapsed() > Duration::from_secs(5) {
    self.check_rotation();
    self.last_rotation_check = Instant::now();
}
```

### Finding 8.3 [nit] -- `read_batch` allocates a new Vec on every call

`reader.rs:224`:
```rust
let mut entries = Vec::new();
```

Every call to `read_batch` allocates a new `Vec`. If called at high frequency (e.g., 1000 times/sec for polling), that is 1000 allocations/sec. Since `Vec::new()` does not allocate heap memory until the first push, this is only a stack allocation until records are found, so the cost is negligible in the EOF case.

When records are found, the Vec grows via reallocation. Using `Vec::with_capacity(max_batch)` would avoid reallocation but wastes memory when few records are available.

**Verdict**: Acceptable. A more efficient API would let the caller pass a `&mut Vec<Entry>` to reuse, but that changes the API ergonomics.

---

## 9. Metrics Overhead

### Finding 9.1 [minor] -- Per-entry metrics overhead is ~20-50 ns, acceptable

Every `log()` call executes:
```
capture.rs:149: self.metrics.channel_depth.increment(1.0)  // atomic f64 add
capture.rs:151: self.tx.send(entry).await                   // channel send
```

Every `write_entry` call executes:
```
capture.rs:233: metrics.channel_depth.decrement(1.0)        // atomic f64 add
capture.rs:301: metrics.record_write(n as u64)              // 2x atomic u64 add
```

And on error:
```
capture.rs:305: metrics.write_errors.increment(1)           // atomic u64 add
```

The `metrics` crate (v0.24) uses atomic operations for Counter (AtomicU64) and Gauge (AtomicU64 storing f64 bits). Each atomic operation costs ~5-15 ns (uncontended) or ~20-50 ns (contended, due to cache-line bouncing between cores).

Per entry, the hot path executes 3 atomic operations: 1 increment (sender side), 1 decrement + 1 increment pair (writer side). Total overhead: ~15-45 ns uncontended, ~60-150 ns under contention.

At 100K entries/sec, this is 1.5-4.5 ms/sec uncontended -- well under 1% overhead. At 1M entries/sec, it rises to 15-45 ms/sec, still manageable.

**The `describe_*!` macros** (`capture.rs:59-63`) are called once during `MetricHandles::new()`. These register metric descriptions in the global recorder. No per-entry cost.

**Verdict**: The metrics overhead is acceptable. No changes needed.

### Finding 9.2 [nit] -- `MetricHandles` stores pre-resolved handles, which is the correct pattern

The crate pre-resolves metric handles in `MetricHandles::new()` (`capture.rs:58-72`) and stores them in the struct. This avoids the overhead of looking up metrics by name on every operation. The `counter!` and `gauge!` macros return resolved handles that are essentially pointers to the underlying atomic. This is the correct high-performance pattern.

---

## 10. Tokio Feature Flags

### Finding 10.1 [minor] -- `rt-multi-thread` is missing from dependencies

`Cargo.toml:7`:
```toml
tokio = { version = "1", features = ["rt", "sync", "time", "macros"] }
```

The `rt` feature provides only the single-threaded (`current_thread`) runtime. For `tokio::spawn` to work on a multi-threaded runtime (which is what most applications use), the `rt-multi-thread` feature must be enabled by the *application* crate, not the library.

However, `tokio::task::spawn_blocking` (`capture.rs:249, 282`) requires the `rt` feature, which is present. And `tokio::spawn` is called by the downstream application (e.g., `tests/integration.rs:113`), not by wirecap itself.

**The issue**: wirecap is a library. It should not enable `rt-multi-thread` -- that is the application's responsibility. The current feature set is actually correct for a library: `rt` (for `spawn_blocking`), `sync` (for `mpsc`), `time` (for `interval`), `macros` (for `select!`).

**But**: The `macros` feature enables `tokio::select!` which is used in `writer_task` (`capture.rs:213`). The `macros` feature also pulls in `tokio-macros` as a proc-macro dependency, which increases compile time. Since `select!` is the only macro used, and it is essential to the implementation, this is justified.

**Verdict**: The feature flags are correct for a library crate. No changes needed.

### Finding 10.2 [nit] -- Dev-dependencies duplicate tokio features

`Cargo.toml:15`:
```toml
[dev-dependencies]
tokio = { version = "1", features = ["rt", "macros"] }
```

The dev-dependencies enable `rt` and `macros`, which are already enabled by the main dependencies. This is harmless (Cargo unifies features), but the dev-dependencies should enable `rt-multi-thread` since the tests use `#[tokio::test]` which defaults to the `current_thread` runtime. The tests work because `current_thread` is sufficient for the test workload, but if tests ever need `spawn_blocking` to complete (which they do -- `capture.rs:282` uses `spawn_blocking` and the tests await the writer), they need a runtime that supports it.

Actually, `spawn_blocking` works on `current_thread` runtime -- it spawns an OS thread. So this is fine. But `#[tokio::test]` with `current_thread` means all `spawn` calls and timer ticks happen on a single thread, which could mask concurrency bugs. Adding `rt-multi-thread` to dev-dependencies and using `#[tokio::test(flavor = "multi_thread")]` for the concurrent test would be more realistic.

---

## Summary

| ID | Severity | Finding | File:Line |
|----|----------|---------|-----------|
| 1.1 | critical | All sync I/O in writer_task blocks tokio worker threads | capture.rs:184-294 |
| 1.2 | major | recover_active_files blocks async runtime at startup | capture.rs:194, 350-375 |
| 2.1 | major | 7-9 unbuffered write(2) syscalls per record (no BufWriter) | format.rs:83-99, capture.rs:299 |
| 2.2 | minor | Record header could be written as single 33-byte buffer | format.rs:88-94 |
| 3.1 | minor | format! allocation on file open (negligible frequency) | capture.rs:319 |
| 3.2 | minor | PathBuf::from(format!(...)) in compress_file (negligible) | capture.rs:379 |
| 3.3 | minor | read_record allocates Vec per record for meta and payload | format.rs:157-158, 174-175, 201-209 |
| 3.4 | nit | Entry in channel carries owned Vec allocations (acceptable) | format.rs:40-60 |
| 4.1 | minor | 65K channel capacity may be oversized for memory | capture.rs:14, 102 |
| 4.2 | nit | No try_send / try_log API for non-blocking callers | capture.rs:148-155 |
| 5.1 | major | log().await blocks caller with no non-blocking alternative | capture.rs:148-155 |
| 5.2 | minor | channel_depth gauge incremented before send completes | capture.rs:149 |
| 6.1 | minor | zstd level 3 is appropriate (informational) | capture.rs:385 |
| 6.2 | minor | No BufWriter on compression output (negligible) | capture.rs:384-388 |
| 6.3 | nit | Partial .zst cleanup error silently discarded | capture.rs:401 |
| 7.1 | major | Box<dyn Read> prevents monomorphization in WcapReader | reader.rs:91, 105-109 |
| 7.2 | minor | Double BufReader wrapping for zstd (redundant copy) | reader.rs:106 |
| 7.3 | nit | format.rs uses dyn Read everywhere, preventing inlining | format.rs:103, 126, 148, 164, 189, 229 |
| 8.1 | major | stream_position() lseek syscall per record in read_batch | reader.rs:228-229 |
| 8.2 | minor | Rotation check interval is count-based, not time-based | reader.rs:240-243 |
| 8.3 | nit | read_batch allocates new Vec per call (acceptable) | reader.rs:224 |
| 9.1 | minor | Per-entry metrics overhead ~20-50 ns (acceptable) | capture.rs:149, 233, 301 |
| 9.2 | nit | MetricHandles pre-resolves handles (correct pattern) | capture.rs:58-72 |
| 10.1 | minor | Feature flags are correct for library (informational) | Cargo.toml:7 |
| 10.2 | nit | Dev-deps could enable rt-multi-thread for realism | Cargo.toml:15 |

**Critical (1)**: Sync I/O in async writer_task -- the single most impactful issue. The entire writer should be moved to a blocking thread (or use tokio::fs).

**Major (5)**: Unbuffered writes (2.1), no BufWriter wrapping, Box<dyn Read> preventing optimization (7.1), per-record lseek in tailer (8.1), no non-blocking log API (5.1), recovery blocking startup (1.2).

**Minor (10)**: Various allocation and configuration concerns, none individually critical but collectively worth addressing.

**Nit (6)**: Style and minor correctness points.

### Recommended Priority Order

1. **Finding 1.1** -- Move writer_task to spawn_blocking (or a dedicated std::thread). This resolves the critical async correctness issue and simplifies the channel choice (can use crossbeam instead of tokio mpsc).
2. **Finding 2.1** -- Wrap File in BufWriter. Simple change, 7-9x reduction in write syscalls.
3. **Finding 5.1** -- Add try_log() method. Small API addition, large usability win.
4. **Finding 7.1 + 7.3** -- Replace Box<dyn Read> with enum + change dyn Read to impl Read. Enables compiler optimizations for the read path.
5. **Finding 8.1** -- Fix stream_position per-record overhead in tailer. Reduces lseek syscalls by ~100x.
6. **Finding 1.2** -- Move recovery compression to spawn_blocking (or apply as part of Finding 1.1).
