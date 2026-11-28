# Build 0.158 — controlled VUM demand paging pilot

This is a new post-fence GPU-owned state, not another cache optimization.
The existing eager relay is still the default. The native CPU title code
runs directly, and arbitrary OS kernel reads and VirtualProtect calls
do not dispatch the user-mode VEH, so it is not safe to activate this
for an arbitrary allocation selected by traffic heuristics.

Experimental opt-in:
```cmd
set "VICTORY_VUM_DEMAND=1"
set "VICTORY_VUM_DEMAND_ATTRIBUTES=<exact hexadecimal XMem attribute>"
```
Only select a known GPU scratch allocation class after inspecting the
candidate logs. Do not select I/O, audio, descriptor, executable or other
CPU/OS-managed memory. With no exact attribute selection, no allocation
changes; the log lists candidate attributes instead. In-place import
or physical VUM experiments exclude this route. Disable with
VICTORY_VUM_DEMAND=0.

Dedicated XMemAllocDefault allocation (1–64 MiB, total cap 64 MiB) uses
one pagefile section with two CPU views: the exposed guest pointer and a
private publication alias. The first page (allocator header) is excluded.
No GPU import and no ordinary physical pool remapping. The original
allocator and shared VUM paths remain fallbacks.

At a completed producer boundary where the relay can try-lock submission
preparation, take a queue-ordered immutable VRAM snapshot of at most
64 full pages (4 MiB) of registered local non-window buffers. Total
snapshot budget 16 MiB. Only previously clean read-only guarded pages
whose entire CPU alias equals their Shadow mirror and which have no
newer CPU uploads or another writer in this landing may be admitted.
Protect guest view PAGE_NOACCESS, record page/producer/owner, mark the
logical write in the existing journal. No PCIe readback at this point.
Guest fences are still published through the normal relay.

Read/write faults resolve the snapshot through an independent copy queue
and reusable 64 KiB readback buffer. The private alias and Shadow mirror
are updated before restoring the ordinary read-only guard, so CPU reads
cannot observe a partially published page. Writes then use the existing
write guard. Known internal CPU reads/copies and texture commitment checks lease
their ranges through the call; kernel writes, conflicting GPU writers,
other Shadow refreshes and mappings resolve first. Admission checks
the allocation generation and rejects active CPU access leases. Same-home
read-only refreshes can leave pages GPU-owned, including gaps in mixed
requests. Unknown deferred callbacks, textures and placed copies force
publication; this intentionally limits the pilot. Free resolves pending
pages before releasing the two views. The fault path does not acquire
the submission or relay lock. A failed native publication is fatal
rather than continuing with stale CPU data.

No universal alias tracking, no protection of existing imported pages,
no proof of GPU-only behavior from missing CPU uploads. This is not a
production-wide replacement of the legacy readback path. The 0.157 traffic census does not establish that its largest GPU-only
candidates use this dedicated XMem allocation path. It may have few/no
eligible pages in gameplay; the extra local snapshots and
conservative forced publications may offset any benefit. Native
Windows compilation, tests and gameplay have not been run. In
particular, unmediated kernel reads or direct protection changes remain
outside the pilot contract. Tests cover range partition and allocation
bounds; they do not prove the native fault/copy path correct.
