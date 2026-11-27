# Build 0.144 — Unified buffer backing, phase 1

The 0.143 gameplay memory audit showed 409 MiB of overlapping loose windows and 603 descriptors pointing at a different native buffer than the one refreshed. The previous GPU ownership path had no eligible copies.

- A weak, sharded native resource -> Shadow identity registry is populated before a backing is published. Descriptor-driven refreshes and UAV declarations resolve the backing actually kept by their slot, for heap sweep, direct tables and fresh/listed/template dynamic tables. The ordinary lookup remains the fallback if the native backing is unavailable or does not cover the range. No descriptor is rewritten to migrate it; existing reference and fence lifetime rules remain in force.
- Registered buffer objects share an existing allocation when their ranges have the same end and the older allocation contains the new buffer. This preserves the upper bound of unsized root accesses. In-place resources are not shared by this path.
- Sized loose descriptor requests may reuse an allocation containing their actual span rather than the rounded allocation window. The stable alias key identifies the exact requested span, not merely its grid cell. Unsized views retain their upper bound. Existing windows are not migrated or resized.
- Physical views at different guest virtual addresses still use the existing alias-aware write-watch/remap machinery. This phase does NOT merge their native allocations: sharing them needs address/offset translation and ownership of writes across remaps. Overlapping ranges with no containing compatible allocation still need separate backing.

Switch: VICTORY_UNIFIED_BACKING=0 disables sharing and native identity publication. New log line: unified backing per frame. The reported shared window bytes count requests, not unique bytes saved. Existing memory/window coverage and descriptor mismatch lines remain comparable. No Windows compilation or gameplay test was performed.
