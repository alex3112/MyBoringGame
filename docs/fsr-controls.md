# FSR controls — build 0.132

Build 0.131 confirmed successful Xbox FidelityFX upscale dispatches from
1280x720 to 1920x1080. This release adds live sharpening control to the
original Xbox FSR dispatch, in both Windows release and diagnostic builds.
Press ², toggle Sharpening, change Sharpness in 5% increments using +/-,
or reset to game defaults. The default is pass-through. The override
copies the confirmed 432-byte public descriptor only for the known upscale
type with no extension chain; the original descriptor is not written.
The loader is hooked once, not its nested provider. The menu reports the
last accepted dispatch or error, not proof of visually correct shaders.
No driver setting or second upscaler is used.

A synchronous diagnostic observer attempts to resolve the actual color,
depth, motion and output title objects to existing host D3D12 textures.
It does not allocate, transition, copy, retain or submit resources.
It runs on sampled successful calls and logs FFX bridge lines; this
validates identity, not GPU completion or frame ownership. THIN may
defer texture creation and writes. FSR off, quality/resolution overrides
and native frame generation remain unimplemented; changing the FSR
descriptor resolution alone would not change the RAGE render resolution.
The previous GPU input capture remains available through the existing
diagnostic environment flags; the menu focuses on live settings.

Test by opening the overlay in gameplay, switching Sharpening On,
changing it to 100%, closing and observing the image, switching it Off
and restoring the original setting. Send launch.log (diagnostics).
Look for FFX runtime and FFX bridge. A success return only establishes
that the API accepted the parameters.

Set VICTORY_FFX_CONTROLS=0 before launch to bypass all title FFX hooks.
VICTORY_FFX_AUDIT=0 disables diagnostic sampling but preserves controls.
