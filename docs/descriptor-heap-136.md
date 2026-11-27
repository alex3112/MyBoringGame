# Build 0.136 — incremental descriptor heap ranges

The buffer sweep used to reduce every cached descriptor range into one min/max envelope per host shadow on every pass. It now maintains a reverse index by shadow ID with counted members (slot-specific ordered endpoints). A changed slot updates only the old and new shadow; an unchanged slot does not modify either endpoint set. A representative slot supplies the existing Arc, so the index does not keep resources alive. Removing a slot, including a vanished member of the periodic shard, removes its contribution. The resulting refresh envelope is intentionally unchanged, including any holes.

The heap sweep does not re-enqueue descriptors solely because it mirrored them itself. External changes still mark dirty slots, and unresolved buffer associations are explicitly retried. The existing per-heap lock, periodic 16-pass revalidation, resource lifetime checks, picked-write processing, and refresh order constraints remain. No periodic recheck is skipped and no GPU-visible address is changed.

A/B: VICTORY_HEAP_INCREMENTAL=0 restores the previous full reduction and normal mirror notification path. New log line: incremental buffer heap — inspected slots, index changes, retained views, resource spans, span reduction ms/frame. Compare with existing heap timing, submit waits, queue idle and native FPS, on the same stationary prologue. Unit tests cover endpoint removal, equal endpoints, movement between resources and comparison against a full reduction after slot rewrites.

No measured performance claim before a Windows benchmark. Resource-release/remapping invalidation continues to use the existing descriptor event paths and periodic validation; no new global remap callback or texture scheduling changes in this build.
