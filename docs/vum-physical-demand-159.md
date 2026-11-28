# Build 0.159 — physical-pool VUM demand paging

This extends the post-fence GPU-owned state of 0.158 to the existing
Xbox physical sections. No new physical allocator or separate GPU
copy of a whole pool is introduced. VUM uses the section generation
and 64 KiB physical offset as page identity and indexes every registered
guest alias back to one deferred page. The source remains an immutable
device-local snapshot captured at the existing completed-producer boundary.

## Running the experiment

Add this one line to LaunchGame.cmd before the executable:
```cmd
set "VICTORY_VUM_PHYSICAL_DEMAND=1"
```
No dedicated XMem attribute selector is needed. The 0.158 dedicated
pilot is independent and can stay disabled. The older imported-memory
experiments (VICTORY_INPLACE nonzero, VICTORY_VUM_PHYSICAL=1) are
incompatible. Only legacy SEC_RESERVE physical sections are candidates;
SEC_COMMIT sections (including VUM1 shared heaps) are excluded. Default is off because raw guest kernel access is not
fully mediated. Set the same variable to 0 to return to eager writebacks.

## Ownership and access

- All known guest aliases must be protected by the existing write
  guards, and the original page must be clean. The full page is
  compared with the reference mirror. Partial, window, conflicting
  producer, unknown or excessively aliased pages keep eager writeback.
- A mapping read lease pins the section and alias list during
  capture/admission; each guest alias becomes PAGE_NOACCESS only
  while the logical GPU version is held in an immutable snapshot.
  One failure restores all protections. Publication writes the
  shared physical bytes through the private section view and the
  original Shadow mirror before restoring each guest alias.
- New physical mappings resolve the old version before MapViewOfFile3;
  unmaps and section release publish before removing the mapping.
  A different native Shadow, even for an alias, resolves first.
- DirectStorage section writes and decode windows have physical
  access leases keyed by section generation/byte range. Known
  internal CPU reads, merges beside guards and writes through hidden
  views also resolve and lease their ranges. New admission cannot
  race an overlapping tracked access.
- Native CPU faults and leased CPU accesses that materialize a page
  mark its physical identity hot for 120 frames (bounded 4096 entries).
  Hot pages use eager writeback. Texture refreshes now resolve only
  their known source ranges, rather than draining all deferred pages;
  opaque callbacks and placed copies still force publication.
- Same-home reads can keep the GPU version; different GPU writers
  currently publish it before their existing comparison/merge.
  This is not yet a sparse VRAM pool or fully GPU-only multiwriter
  path. Snapshot budget remains 16 MiB, at most 4 MiB per batch.

The VUM physical demand log records candidates, admissions, hot
refusals, materializations, pre-map publications and missed producer
boundaries and guard refusals. The VUM demand line still gives total deferred and
materialized bytes. The field VICTORY_VUM_DEMAND refers specifically
to the dedicated pilot, not the physical switch. Expect zero admissions
if the completed-producer boundary is never available or buffers are
ineligible. No throughput gain has been measured.

Limitations: unmediated OS accesses and guest-side protection calls
are not proven covered; physical aliases outside the known section
registry are not safe to admit. The pilot excludes imported GPU pages.
This remains experimental and requires native Windows validation
before default enablement. No Windows compile or test run here.
