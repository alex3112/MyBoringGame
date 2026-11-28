# Build 0.155 — descriptor preparation pipeline

Build 0.154's stationary prologue trace recorded about 17–19 ms/frame
processing aged waiting descriptors and 19–20 ms waiting for heap sync.
Scoped heap sync excluded no heaps in the late samples. The per-heap
aged-slot mirror and raw-page scan formerly ran as separate worker
batches, even though the scan's read/compare phase does not depend on
host descriptor state.

This phase combines aged-slot mirroring and raw-record read/compare
into a single interleaved work batch on the existing descriptor-record
pool. Classification of changed records (table use, null-slot state,
deferral) is delayed until all aged mirrors finish. New descriptor
mirrors still execute afterward; all required work completes before
submission, with no queue, fence, lifetime or shader changes. CPU
writes that happen after collecting dirty pages remain in the existing
watch journal for the next round. As before, submitted descriptors must
already have been published by the title. No parallel physical remap
experiment; that path uses the original ordering. The old-slot table
count uses one atomic per work chunk rather than one per slot.

VICTORY_HEAP_PIPELINE=0 restores separate batches and inline
classification. No new workers or persistent descriptor caches.
The new log reports mixed wall time, worker sums and classification
time separately. In mixed rounds the legacy old-slot and scan timers
overlap and must not be added or compared as isolated phases.
The scheduler test checks that every task is visited once, including
unequal group sizes and empty groups. Windows compilation and the
gameplay benchmark are not performed.

The previous VICTORY_HEAP_INCREMENTAL setting is unchanged (off in the
benchmark); no assumption that turning it on is beneficial on this PC.
This is intra-submission descriptor preparation, not arbitrary parallel
submission or early GPU execution of partially prepared lists.
