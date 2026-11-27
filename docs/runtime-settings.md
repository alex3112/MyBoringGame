# Native FSR 3.1 integration, stage 1 (build 0.129)

AFMF and ADLX have been removed. The port does not change any AMD driver setting.
The menu (key ²) is retained. The Frame Generation control is explicitly disabled:
no interpolation runs in this build. A provider probe uses the native AMD
FidelityFX API (SDK 1.1.4) against the actual host D3D12 device, if the
official amd_fidelityfx_dx12.dll is beside victory.exe. The SDK is not
redistributed by this repository. Checking it does not allocate a frame
generation context or change the swapchain.

In a diagnostics build, open the settings menu in controllable 3D gameplay
and click Capture FSR inputs. The menu closes and schedules target dumps
32 and 40 native presents later. Slowly pan the camera after clicking.
The capture folder is fsr-input-audit-<pid> in the working directory
(or VICTORY_CAPTURE if set). It includes target descriptions, existing PNG
previews and the frame's recorded operations. Dumps are expensive and are
not a benchmark. No captures run unless explicitly requested or previous
capture flags are used. The existing diagnostic target collector may not
observe all targets in deferred/behind submission paths; check logs and
the number of candidates before interpreting the result. Resource addresses
are session-local, not stable identifiers.

The next integration stage requires identifying the depth and motion-vector
producer passes and encodings, render resolution, jitter, camera projection
and reset conditions. The SDK's prepare call cannot use guessed textures.
Then create a frame-generation context and swapchain, configure/prepare each
native frame, register HUD composition as appropriate and measure native and
generated presents separately. No placeholder frames or duplicate presents
are implemented.

References: https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK/tree/v1.1.4
and https://gpuopen.com/manuals/fidelityfx_sdk/techniques/super-resolution-interpolation/
