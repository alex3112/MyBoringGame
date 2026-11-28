# Build 0.156 — persistent descriptor and refresh cache

The 0.155 prologue remained about 14.74 native FPS. The last log samples
show about 200 persistent table template hits versus 475 builds per frame,
and about 38,000 inspected slots versus 148,000 retained heap ranges.
The 0.140 build disabled the old incremental index by default because
the measured gain was insufficient; this is a revised data structure,
not just a switch change.

- Persistent mixed-table cache: four ways per set, 256 entries rather
  than 128 direct-mapped entries, exact keys, empty-first/oldest eviction.
  Same whole-table validation and same per-list readiness.
- A separate 2048-entry, four-way, CPU-only descriptor cache for eligible
  ordinary SRV buffers, reused on ListViews misses across recordings.
  Every use resolves the current record and registered Shadow; a miss
  verifies the native descriptor resource identity. No texture, CBV,
  UAV or physical-remap experiment enters the new single-view cache.
  The cache owns no native resources, and normal list ownership, per-list
  readiness, and fence lifetime remain. No cached shader data.
  VICTORY_PERSISTENT_VIEW_CACHE=0 restores the previous template size,
  direct mapping and list descriptor creation. At 32-byte native
  descriptor stride, additional CPU heap capacity is about 640 KiB per
  translator (actual driver allocation can differ).
- Short refresh proof: only after the existing published/asked coverage
  and copied range bounds check, a complete-range stamp can prove clean
  if every intersected journal block's newest write is no newer. Otherwise
  the original per-page comparison runs. No CPU/GPU visibility changes,
  no deferred readbacks, no use of untracked memory. Large wide_each
  batches claim 8 tasks per scheduler atomic, preserving every task.
  VICTORY_REFRESH_VERSION_FAST=0 restores previous behavior.
- Revised incremental heap index: exact slot members and cached min/max
  envelope per Shadow, rather than two ordered sets per resource. Removal
  of an endpoint/representative invalidates the envelope; next reduction
  recomputes from remaining members. Updates and periodic revalidation
  still use the existing paths. No extra retained Shadow references.
  Now enabled by default; VICTORY_HEAP_INCREMENTAL=0 selects full reduction.

New stats: persistent single buffer views, short refresh version proof;
existing template hit/build and index stats remain. New unit tests for
cache replacement and short-range version proof, existing exact span
comparison test retained. No Windows compile or gameplay benchmark run.
No measured FPS gain claimed. No changes to presentation or shaders.
