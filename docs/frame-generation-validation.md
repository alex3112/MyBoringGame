# Build 0.134: validate native frame generation

Build 0.133 log at 13:15:08–24 UTC confirms about 13 native + 13 accepted
generated dispatches/s and 26 DXGI submitted presents/s. These are not
physical scanout measurements. The old F3 overlay and title caption counted
only native presents, explaining their unchanged number.

F3 now shows DXGI submitted FPS, native FPS, accepted generated dispatch
FPS, FG state and DXGI completed FPS if available. CPU/GPU timings remain
per native frame. The caption reports the same submitted FPS. No estimate
of generated frames is substituted for DXGI measurements.

The AMD proxy's GetFrameStatistics is sampled at native presents. It gives
the latest refresh-synchronized sample, so multiple presents may be
aggregated; mean/min/max are per-sample average intervals, not a full
per-image pacing trace. Unsupported/disjoint stats are unavailable.
External PresentMon/ETW remains necessary for exact per-image display timing.

Diagnostics builds probe 16 patches of 8x8 RGB pixels on actual native and
generated textures after successful interpolation, two consecutive frames
every 16 title frames. Copies restore the SDK states. Readback is after
the existing wait-for-presents. Logs give hashes, changed pixel count and
mean absolute RGB error versus current and (when adjacent) previous native
frame. Identical samples are not proof of duplicate full frames, especially
in a stationary scene. Different samples demonstrate different sampled
pixel content, not correct optical flow. Probe only known RGBA8 and SDK
PRESENT/UAV resources; VICTORY_FG_PIXEL_AUDIT=0 disables it. It adds small
sampled GPU copies, not a full frame readback.

No change to the actual interpolation algorithm or render resolution.
Test with FG off/on, close menu, move camera steadily for ~15 seconds,
then send launch.log. Look for FFX FG: DXGI refresh timing and
FFX FG: actual interpolation pixels. Exact pacing and visual correctness
still require runtime validation on Windows.
