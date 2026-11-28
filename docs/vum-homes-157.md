# Build 0.157 — VUM persistent homes and ordered writeback

This resumes the architectural migration inspired by Regaliant homes.rs.
The existing device-local backing is retained, not duplicated. Its CPU
mirror, dirty ranges, upload versions, common producer ownership and
exact publication certificates are now managed by VUM homes.rs instead
of by the legacy Shadow structures. The existing ownership and producer
counts are moved, not duplicated, and use the same lock order. Existing Shadow identities, native descriptors, addresses and
allocation limits are retained, so recorded GPU commands do not change
their resource behind their backs. Every old two-sided buffer now has a
Home object. Nonlocal two-sided buffers retain their previous path.

The new relay route creates one reusable device-local snapshot (at most
the existing 32 MiB default readback capacity, 64 MiB if overridden).
When a later producer of the same home is already complete and overlaps
a piece, take a queue-ordered snapshot and signal a native fence on the
graphics queue. Copy queue waits on that fence, then reads the immutable
snapshot instead of racing later writes of the original allocation.
Ordinary direct/copy-queue routes remain the fallback; no snapshot is
taken when the known graphics queue is behind or the batch is nonlocal,
or when torn-fetch / upload diagnostics are active. Allocation failure
also retains the old path. New local copies do not cross PCIe.

Only after the existing byte merge and journal publication, and only
for whole requested intervals without missing bytes or newer CPU writes,
record the producer fence and mapping generation on the home. A later
landing may omit the same bytes if that certificate covers its producer.
Mapping/free events invalidate certificates conservatively, and resource
destruction discards them. CPU write history still excludes new bytes
from old fetches. The copy-queue upload-history retirement watermark stays
at the original landing's producer order even after a newer snapshot.
All guest-visible fences and bottom-of-pipe writes
remain after the required merges. This is bounded coalescing across
native submissions, not deferred readbacks beyond CPU visibility.

VICTORY_VUM_HOME_WRITEBACK=0 disables ordered snapshots and their reuse;
the VUM Home refactoring remains. The legacy direct-queue proof has its
old switches. New log: VUM home publication. Existing local allocation
budgets and descriptor optimizations are unchanged.

Regaliant's demand-only writeback for GPU-only buffers assumes it sees
the relevant CPU readers. VIctory executes guest x64 code which can read
raw addresses without going through a mediated API. The benchmark has
no proof that candidate ranges are CPU-inaccessible. No arbitrary
PAGE_NOACCESS trapping or permanently stale RAM is enabled. Full
GPU-owned data surviving a guest-visible fence and demand readback
requires a separately controlled allocation/access model; this build
does not yet implement it. Windows compilation and gameplay not run.
