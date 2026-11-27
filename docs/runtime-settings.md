# Runtime graphics settings (build 0.128)

Press **²** (the key left of 1 on AZERTY, grave on US layout) in the game window. Click the values to cycle between options. Press ² again, Escape, or Close to return to the title. This is an owned native Windows settings window; it is not composited into the game swap chain.

The AMD Fluid Motion Frames control uses the official ADLX driver API. It does not implement FSR 3's motion-vector/depth-based interpolation pipeline. No extra SDK is needed at build time; ADLX is loaded only when the menu is opened, from Windows System32. Unsupported controls are disabled. The driver determines actual interpolation, including application-specific overrides, presentation conditions and availability; On reflects the driver setting, not proof of generated frames. Driver-level frame counts are not included in the existing native FPS metric.

**These are global AMD driver settings, not VIctory-only settings.** Opening the menu changes nothing. Clicking a setting changes the driver profile and can affect other applications; values may survive process exit. Restore initial settings explicitly to restore the values observed at the first successful connection. No automatic changes are made on startup or exit. The menu thread keeps the renderer independent of driver requests. Low native frame rates (such as 13 FPS) may produce visible artifacts and increased latency even when AFMF engages.

AMD references: https://gpuopen.com/manuals/adlx/adlx-sdk-references/adlx-interfaces/3d-graphics/iadlx3damdfluidmotionframes/ and https://github.com/GPUOpen-LibrariesAndSDKs/ADLX (public ABI declarations in SDK/Include/ISystem.h, I3DSettings1.h and I3DSettings3.h).
