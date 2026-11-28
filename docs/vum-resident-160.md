# Build 0.160 — resident native GPU homes, submission-side admission

Build 0.159 with physical demand enabled: about 11.82 vs 14.39 native
FPS; ~13.17 missed global boundaries/frame, ~1.26 KiB/frame deferred
and later republished. The new path does not use a relay-time snapshot
or require a globally completed producer. It uses the same D3D12 local
allocation already bound by the translated shaders.

## Activation

Before launching:
```cmd
set "VICTORY_INPLACE=0"
set "VICTORY_VUM_PHYSICAL=0"
set "VICTORY_VUM_PHYSICAL_DEMAND=0"
set "VICTORY_VUM_DEMAND=0"
set "VICTORY_VUM_RESIDENT=1"
```
Resident mode is opt-in and mutually exclusive with both old demand
pilots. The physical mapping/CPU access mediation remains active.
Set VICTORY_VUM_RESIDENT=0 to restore the previous eager path.
No change to the frame-generation configuration.

## New data path

- Learn whole 64 KiB local pages from actual changed readback requests.
  This is a bounded working set, NOT proof they are CPU inaccessible.
- After upload preparation, but before native submission, arm pages
  whose full range is declared, whose physical section/alias identity
  is known, whose prior eager writers have all landed, whose reference
  matches the guarded CPU view, and which have no competing current
  producer or CPU access. Initial admission rejects SEC_COMMIT,
  window/placed buffers and incompatible/unguarded pages. The previous
  native allocation is the GPU home; no extra VRAM snapshot.
- All aliases are protected together. A CPU read before actual send can
  revoke an admission without a GPU copy, since no new producer has run. The enqueue gate serializes recording the
  producer certificate and native ExecuteCommandLists/Signal with
  demand materialization. A fault never waits for a submission that
  has not been enqueued. The direct-queue demand copy runs behind
  already enqueued work and the fault waits its private readback fence.
  No global completed-producer check on this route.
- Subsequent same-home GPU writes retain the resident page and extend
  its producer coverage, even across pending flights. CPU reads, CPU
  writes, foreign native copies/aliases, mapping changes and opaque
  callbacks still publish first. Placed copies resolve their specific
  source/destination ranges, not unrelated homes. Same-home refreshes
  may omit already owned intervals. Exact per-CardPlan coverage removes
  eager readback requests even if a CPU fault materializes before relay.
  Hash compare dispatches are retained so fallback comparison baselines
  remain valid; this build does not eliminate their GPU cost.
- Fast striped virtual CPU-access leases replace global VIEWS/STATE
  lookups for the new mode. Arm stripes are advertised before testing
  active counters; either admission refuses or a racing reader waits
  for STATE and resolves. Collisions only conservatively refuse or
  cause a lookup. Physical DirectStorage leases still use section IDs.
  Same-home staging accesses are leased across the actual refresh.
- Budget: 1024 protected 64 KiB pages (64 MiB existing native homes),
  at most 64 new admissions per submission, 8192 learned page keys,
  one reusable 64 KiB demand readback allocation. A real CPU access
  cools down admission of that physical page for 120 frames.
  No additional immutable GPU snapshot for the new mode.

## Validation and limits

New log "VUM resident": candidates, new pages, eager readback requests
omitted, actual demand bytes, CPU publications, successive producers,
earlier-writer and other refusals, pending pages. Omitted logical
requests and actual transferred bytes differ; use memory_audit actual
relay bytes and native FPS, plus resident demand bytes, for A/B.
The design still restricts admission and can miss the main buffers.
Direct-queue materialization may itself be expensive when a CPU
consumer exists. Opaque callbacks still drain for correctness.
The native code has unmediated kernel access/protection operations;
no general guarantee for arbitrary buffers. Opt-in only. This is a
new resident ownership route, not a production-wide replacement of
all legacy memory. Windows compilation and gameplay not validated.

Diagnostics also count pre-send revocations and opaque drains.
