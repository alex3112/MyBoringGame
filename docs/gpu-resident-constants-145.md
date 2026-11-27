# Build 0.145 — GPU resident constants

The existing card-steps planner promotes fills that would otherwise wait for in-flight GPU writers. It used to snapshot each unique source range into a 4 MiB intermediate allocation and point the table slot there. This build retains the planner's holder and dependency checks and adds a direct path for stable source ranges in dedicated GPU memory.

Static table CBV reads with matching native declarations are translated to raw twin loads (scalar and vector). Root CBVs and ambiguous/dynamic sources retain CBV lowering. The translator records whether a stage uses raw table constants and has no remaining heap CBV handles. The shader cache format is VSH2 and its existing translation fingerprint includes the changed source. Cached tables distinguish raw-only stages from other stages.

A promoted slot may point directly into its source if the complete submission has no overlapping CPU copy, placed copy, declared GPU write, readback destination or bottom-of-pipe write, if its source is local card memory, and if no physical-page view covers the range. Raw-only slots can be 16-byte aligned; native CBVs require 256-byte alignment and size. Other cases retain the existing snapshot path. The source refresh and existing GPU ordering barrier remain. The unused intermediate buffer is not transitioned when all fills are direct.

The intermediate pool and recording-time scratch remain allocated for fallback. This does not eliminate readbacks or all descriptor reconstruction. Counters report promoted slot candidates, direct raw, direct CBV and bytes of omitted copies (logical per-slot bytes; shared snapshots can reduce unique physical bytes).

Switches: VICTORY_GPU_RESIDENT_CONSTANTS=0 keeps snapshots; VICTORY_RAW_TABLE_CONSTANTS=0 keeps old shader lowering and limits direct binding to aligned native CBVs. No command file changes required. Static review only; Windows compilation and graphics correctness not yet tested. The first run may regenerate cached shaders.
