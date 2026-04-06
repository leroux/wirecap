# Wirecap format specification

All multi-byte integers are **little-endian**. Strings are length-prefixed
(1-byte length + UTF-8 bytes).

## File header

Every `.wcap` file begins with:

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | magic | `WCAP` (0x57 0x43 0x41 0x50) |
| 4 | 1 | file_version | Currently `1` |
| 5 | 1 | instance_id_len | Length of instance_id string |
| 6 | N | instance_id | UTF-8 instance identifier |
| 6+N | 1 | run_id_len | Length of run_id string |
| 7+N | M | run_id | UTF-8 run identifier |

The header is followed immediately by a sequence of records.

## Record format (v3)

Current version. Header is 33 bytes fixed, followed by variable-length meta
and payload.

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 1 | version | `3` |
| 1 | 8 | ts | Wall-clock timestamp, nanoseconds since Unix epoch |
| 9 | 8 | mono_ns | Monotonic timestamp, nanoseconds since process start |
| 17 | 8 | recv_seq | Process-global receive sequence number |
| 25 | 2 | meta_len | Length of metadata bytes |
| 27 | 4 | payload_len | Length of payload bytes |
| 31 | 1 | src | Channel tag (opaque u8, caller-defined) |
| 32 | 1 | dir | Direction: `0` = In, `1` = Out |
| 33 | meta_len | meta | Opaque metadata bytes (absent if meta_len = 0) |
| 33+meta_len | payload_len | payload | Raw wire payload bytes |

Total record size: `33 + meta_len + payload_len` bytes.

## Record format (v2)

Legacy format with metadata but no monotonic/sequence fields. Header is 17
bytes.

| Offset | Size | Field |
|--------|------|-------|
| 0 | 1 | version (`2`) |
| 1 | 8 | ts |
| 9 | 2 | meta_len |
| 11 | 4 | payload_len |
| 15 | 1 | src |
| 16 | 1 | dir |
| 17 | meta_len | meta |
| 17+meta_len | payload_len | payload |

When read, `mono_ns` and `recv_seq` are set to `None`.

## Record format (v1)

Oldest format. No metadata field. Header is 15 bytes.

| Offset | Size | Field |
|--------|------|-------|
| 0 | 1 | version (`1`) |
| 1 | 8 | ts |
| 9 | 4 | payload_len |
| 13 | 1 | src |
| 14 | 1 | dir |
| 15 | payload_len | payload |

When read, `mono_ns`, `recv_seq`, and `meta` are set to `None`/empty.

## File lifecycle

| Extension | State |
|---|---|
| `.wcap.active` | Being written by the capture task |
| `.wcap` | Sealed after rotation |
| `.wcap.zst` | Compressed with zstd |
| `.wcap.recovered` | Recovered from unclean shutdown |

Rotation triggers when a file exceeds the configured max size (default 100 MB)
or max age (default 30 min). After rotation the sealed `.wcap` file is
compressed to `.wcap.zst` in a background thread, and the original is deleted.

On startup, any leftover `.wcap.active` files from a previous crash are renamed
to `.wcap.recovered` and compressed.

## Filename convention

```
{instance_id}_{timestamp}.{millis}Z_{run_id}.wcap.active
```

Example: `my-service_2026-04-06T143022.517Z_a1b2c3d4.wcap.active`

The timestamp is UTC. The run_id is a random 8-hex-char identifier generated
at `Capture::start`.
