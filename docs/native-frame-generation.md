# Native AMD FSR Frame Generation — experimental build 0.133

Uses the original Xbox FSR upscaler unchanged (including the existing
sharpening controls) and creates AMD's native DX12 pacing swapchain if
the installed SDK exposes FSR Frame Generation 3.1.6 and a 3.1.x swapchain
provider. The default is off. Press ² and click Frame Generation, then
close the overlay. It is intentionally paused while the menu is open.
Set VICTORY_FG=0 before launch to bypass the AMD swapchain entirely.

The title's original FSR dispatch supplies actual depth, motion vectors,
jitter, render/output size, reset, camera near/far and FOV. A frame-indexed
bridge holds existing native texture owners, rejects missing/ambiguous
inputs, and runs preparation on the presentation list, after prior title
submissions, before AMD's callback generates the interpolated image.
The first implementation uses the same direct queue, no async compute.
The HUD-less base plane is copied separately where the title supplies
two display planes. SDR RGBA8 output is supported; other formats bypass
FG. A zero view-space conversion from the title is treated as 1 meter/unit.

The SDK's wait-for-presents is used before the next configure/prepare to
avoid callback races and reuse the HUD-less texture and GPU timestamp
queries. This is intentionally conservative and may reduce native FPS.
The per-frame association relies on the title dispatch occurring before
its present, and depth/MVs still representing that frame after all its
passes; these assumptions need validation in game. No guessed motion
vectors or duplicate Present calls. Actual DXGI present count is a
submitted-present measure, not a guarantee of physical scanout FPS.
Generated dispatch FPS excludes explicit history resets and measures
accepted interpolation dispatches, not physical scanout. Timings use
timestamps around prepare and interpolation, on the same direct queue.

Test with the existing SDK DLLs and diagnostics: run the benchmark with
FG off, toggle it on and close menu, keep the same scene, then test camera
movement and turn it off. Send launch.log, especially FFX FG lines and any
crash logs. Native resolution, FSR quality and FSR off remain unchanged.

On SDK wait/disable failure, presentation is stopped rather than reusing
an active callback with stale inputs. Restart with VICTORY_FG=0 to bypass
it. The frame-rate line also reports captured/rejected/ambiguous/missing
input counts, so the exact association can be checked in real gameplay.
