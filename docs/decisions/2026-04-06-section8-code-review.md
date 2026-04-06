# Decision Log: Section 8 — Code Review Items
**Date**: 2026-04-06
**Intensity**: strict

## Decision 1: Which section to work on next

**Options considered:**
- Section 8: Code Review — stabilizes API before other sections build on it
- Section 3: Bug fix (parse_dir) — quick win, low risk
- Section 6: Cargo metadata — structural decisions (lib/bin split) best made early
- Section 5: Python reader — key user feature but depends on stable Rust API

**Choice:** Section 8: Code Review
**Rationale:** Stabilize the public API first, so docs, Python reader, and metadata work don't need rework.
**Rejected steelman:** Section 6 — the lib/bin crate split is a structural decision that gets harder to make later once more code depends on the current layout.
**Reversibility:** reversible
**Assumptions confirmed:** Project compiles cleanly; Section 1 fully done; Section 9 out of scope.
**Failure modes considered:** Section 8 is the largest section (9 items); risk of scope creep. Mitigated by tackling items individually with checkpoints.

## Decision 2: Which Section 8 item first

**Options considered:**
- compress_file → spawn_blocking — correctness fix for sync blocking in async runtime
- finalize_file → strip_suffix — mechanical robustness improvement
- WcapReader error handling — API change, Iterator swallows errors
- generate_run_id rand update — deprecated API fix

**Choice:** compress_file → spawn_blocking
**Rationale:** Most impactful correctness bug — sync blocking I/O in tokio::spawn risks starving the async runtime.
**Rejected steelman:** WcapReader error handling — changing Iterator to expose errors is the most user-facing API improvement; once callers depend on the current `Item = Entry` signature, changing it gets harder.
**Reversibility:** reversible
**Assumptions confirmed:** User requires empirical evidence (tests/experiments) before and after changes — no guessing.
**Failure modes considered:** spawn_blocking could introduce deadlocks if the blocking closure holds a resource needed by async tasks. The compress_file function operates on already-finalized files with no shared state, so this is low risk.

### Decision 2 — Implementation Results

**Empirical evidence (pre-fix):**
- Isolated experiment: `tokio::spawn` + blocking I/O caused 11,565 µs max latency spike (1ms expected interval)

**Empirical evidence (post-fix):**
- Isolated experiment: `spawn_blocking` reduced max spike to 2,581 µs (≈ baseline of 2,204 µs)
- Improvement: 4.5x reduction in latency spikes
- All 8 integration tests pass

**Note:** Full Capture experiment showed persistent ~13-17ms spikes even after fix. This is caused by the `writer_task` itself doing sync file I/O (write_record, fsync), which is a separate concern from compress_file. The isolated experiment confirms spawn_blocking specifically fixes the compression blocking.

**Changes made:**
- `capture.rs:274` — rotation: `tokio::spawn(async move { ... })` → `tokio::task::spawn_blocking(move || { ... })`
- `capture.rs:306` — shutdown: inline `compress_file(...)` → `spawn_blocking(...).await`
- `capture.rs:396` — recovery: `tokio::spawn(async move { ... })` → `tokio::task::spawn_blocking(move || { ... })`
