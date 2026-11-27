# Build 0.135 — relay memory path

This update preserves the existing relay/copy-queue coherence rules. It reduces redundant GPU work and fence round trips rather than assuming any title address is GPU-only. No live buffer is relocated and no title-visible writeback is suppressed.

- `VICTORY_COMPARE_JOIN=1` (default): share compare-result slots for overlapping/adjacent 64 KiB hash spans of the same shadow in asynchronous relay submissions. Exact declared landing ranges remain unchanged; synchronous, picked-only and capacity-exhaustion paths retain the previous behavior. Set 0 to disable.
- `VICTORY_RELAY_FETCH_MB=32` (default, 1–64): bounded reusable relay readback buffer. Allocation falls back to 16, 4 and 1 MiB (never above the requested size). Set 16 for the old capacity. In hash-check mode charge each physical block only once per batch.
- Build the flagged-piece membership set directly and avoid recording unusable past fetches on the dedicated copy queue.

Benchmark the unchanged first controllable prologue scene for 30 seconds with Frame Generation disabled. Compare native FPS, relay fetch wait, fetch batch count/bytes, total landing time, compare passes, and the new `memory compare join per frame` line. Verify rendering, fences and stability. A/B the two switches independently if needed. No speedup is assumed before measurement.

Not included: arbitrary GPU-only classification or on-demand readback without reliable interception of CPU reads; changing live GPU addresses; removing global alias barriers. These need stronger correctness guarantees.
