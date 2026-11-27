# Runtime graphics menu (build 0.130)

Press ² in the game to open/close the blue settings overlay, or Escape / Close
to close it. It is rendered in the game's own D3D12 backbuffer before Present,
not a separate Windows window. Controls are mouse hit-tested in backbuffer
coordinates with client-size scaling. GameInput keyboard and gamepad readings
are neutral while it is open. The title continues rendering (not paused).
The menu's immutable upload buffer is reused until contents change; nothing
is drawn when closed. No AMD driver settings are touched.

FidelityFX SDK 2.x modular DX12 packages: place amd_fidelityfx_loader_dx12.dll
and amd_fidelityfx_framegeneration_dx12.dll beside victory.exe. Other official
SDK dependencies may be kept there. Older SDK 1.1.x monolithic
amd_fidelityfx_dx12.dll is also accepted. Check SDK loads the API and queries
providers on the actual D3D12 device; failures report Windows loader error
codes or missing exports and are logged. This is not a frame generation
dispatch. Frame Generation remains explicitly off until actual game inputs
are identified, no fabricated frames are presented.

In a diagnostics build, open the settings menu in controllable 3D gameplay,
click Capture FSR inputs, then move the camera slowly. The overlay closes
and captures targets 32 and 40 native presents later. The capture folder is
fsr-input-audit-<pid> in the working directory (or VICTORY_CAPTURE if set).
Return the folder and launch log. Readbacks are costly, not a benchmark.
The existing target collector may miss deferred-path writes. Resource
addresses are session-local. Native interpolation still requires validated
depth, motion vectors, projection and jitter data plus a generation context,
presentation, frame pacing, HUD composition and native/generated FPS metrics.

SDK references: https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK
and https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK/tree/v1.1.4
