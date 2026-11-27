# FidelityFX title-side audit — build 0.131

The build 0.130 log shows the original Xbox loader and upscaler libraries
mapped with no unresolved imports. Loading is not evidence that an upscale
dispatch succeeds. The existing resolver passed title exports through and
did not observe their use.

The two captured target inventories (9405, 9413) include 1280x720 depth
(DXGI format 19) and a 1280x720 two-channel floating-point candidate
(DXGI format 34). The host output is 1920x1080. Those observations cannot
establish which upscaler is active or whether these resources are its inputs.
Addresses are session-local.

In diagnostics builds, the loader now observes the five public ffx API
exports in the title's loader, upscaler and frame-generation Xbox DLLs,
both when imported through the IAT and through HLE GetProcAddress. Each
hook calls the original unchanged, with the original context/arguments,
and preserves its result. Distinct module slots avoid nested loader/provider
calls recursing into themselves. Calls 1–8 and powers of two are logged
separately per module and API, plus sampled errors. Known descriptor IDs
are decoded conservatively against the common public SDK ABI. Xbox-specific
IDs are reported without decoding. Diagnostic reads use ReadProcessMemory;
no arbitrary resource pointer is dereferenced. Set VICTORY_FFX_AUDIT=0 to
disable the instrumentation before launch.

Run into the prologue for at least 15 seconds and send launch.log. No
render-target capture or SDK probe is necessary this time. Look for
"FFX audit: title call", "FFX audit: title result", "FFX audit: upscale
dispatch inputs", and "FFX audit: upscale resource". A successful upscale
dispatch means the original implementation is being invoked, not proof
that all translated GPU commands execute correctly. No new upscaler,
frame generation, quality override or replacement backend is enabled.
The next step depends on the actual observed API and resource identity.

Public reference: https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK/tree/v1.1.4/ffx-api/include/ffx_api
