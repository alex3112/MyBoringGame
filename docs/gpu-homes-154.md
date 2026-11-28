# Build 0.154 — GPU homes, phase 1 (bounded readback reuse and census)

Inspired by Regaliant's local-memory homes, but not a port of its code.
VIctory runs the title's CPU instructions natively. Neither an observed
GPU-only traffic pattern nor D3D12 resource usage proves that the CPU will
not load the same guest address. Build 0.148's ownership index only covers
in-flight declarations. Therefore this phase does not make RAM stale
beyond the existing landing/fence boundary.

On the direct queue, the existing Had journal can prove that a completed
earlier fetch already covered the exact requested bytes of the same
Shadow after the requesting producer's fence and merged them. With
VICTORY_GPU_HOMES=1 (default), a local-memory Shadow may reuse that result
instead of requesting a duplicate fetch. No new GPU allocations, changes to
barriers, compare dispatches, snapshots, fence publication or game data.
The separate copy queue is deliberately excluded: it can race later
writes, and its journal does not prove this order. The pre-existing
VICTORY_LAND_SKIP setting is unchanged and independent. Setting
VICTORY_GPU_HOMES=0 disables the new reuse and candidate report.

The diagnostic traffic census now keys by native Shadow identity rather
than guest address/length, and logs local allocations of at least 1 MiB
with at least 1 MiB requested readback and no observed uploads during
the sample interval. These are candidates, not proven GPU-exclusive
allocations. Nominal allocation coverage is not physical VRAM usage.
Readback counts are logical requests, not measured PCIe bytes (whole
hash-block fetches and overlap may differ). Reuse counters exclude the
pre-existing LAND_SKIP path. On the dedicated copy queue the new reuse
count is expected to be zero. Do not disable that queue just to obtain
a nonzero count. No FPS gain claimed.

Future phase: intercept or otherwise prove every CPU read and write,
including native loads, mappings, aliases and kernel I/O, before making
persistent GPU ownership or deferring visibility past a fence possible.
Windows compilation and gameplay benchmark not performed.
