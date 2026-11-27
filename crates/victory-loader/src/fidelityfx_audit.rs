//! Observe the title's original FidelityFX public API without replacing its
//! implementation. Both IAT binding and dynamic GetProcAddress are covered.
//! The documented SDK 1.1.4 and modular SDK layouts agree for these common
//! descriptor IDs. Xbox-specific/unknown IDs are logged, never interpreted.
//! ReadProcessMemory bounds reads. An explicit sharpening override uses an aligned local copy; the title descriptor is never written.
use std::ffi::c_void;
use victory_core::fsr::{self, Sharpening, Status};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const MODULES: [&str; 3] = [
    "amd_fidelityfx_loader_xbox.dll",
    "amd_fidelityfx_upscaler_xbox.dll",
    "amd_fidelityfx_framegeneration_xbox.dll",
];
const NAMES: [&str; 5] = [
    "ffxCreateContext", "ffxDestroyContext", "ffxConfigure", "ffxQuery", "ffxDispatch",
];
const CREATE: usize = 0;
const DESTROY: usize = 1;
const CONFIGURE: usize = 2;
const QUERY: usize = 3;
const DISPATCH: usize = 4;
static ORIGINAL: [[AtomicUsize; 5]; 3] = [const { [const { AtomicUsize::new(0) }; 5] }; 3];
static CALLS: [[AtomicU64; 5]; 3] = [const { [const { AtomicU64::new(0) }; 5] }; 3];
static ERRORS: [[AtomicU64; 5]; 3] = [const { [const { AtomicU64::new(0) }; 5] }; 3];
static ENABLED: OnceLock<bool> = OnceLock::new();
static CONTROLS: OnceLock<bool> = OnceLock::new();

pub fn is_module(file: &str) -> bool {
    MODULES.iter().any(|m| m.eq_ignore_ascii_case(file))
}

/// Keep distinct original exports for the loader and each provider: the
/// loader may call an intercepted provider, and must not recurse into itself.
pub fn wrap(file: &str, name: &str, original: usize) -> usize {
    if original == 0 || !*CONTROLS.get_or_init(|| std::env::var_os("VICTORY_FFX_CONTROLS").is_none_or(|v| v != "0")) {
        return original;
    }
    let Some(module) = MODULES.iter().position(|m| m.eq_ignore_ascii_case(file)) else { return original };
    let Some(op) = NAMES.iter().position(|n| *n == name) else {
        if audit_enabled() { tracing::info!(module = file, export = name, "FFX audit: other title export (unchanged)"); }
        return original;
    };
    let hook = match module {
        0 => hooks::<0>()[op],
        1 => hooks::<1>()[op],
        2 => hooks::<2>()[op],
        _ => return original,
    };
    match ORIGINAL[module][op].compare_exchange(0, original, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => tracing::info!(module = file, export = name, "FFX audit: intercepted title export"),
        Err(old) if old == original || original == hook => {}
        Err(_) => {
            tracing::warn!(module = file, export = name, "FFX audit: export address changed; leaving new address untouched");
            return original;
        }
    }
    hook
}

fn hooks<const M: usize>() -> [usize; 5] {
    [
        create::<M> as *const () as usize,
        destroy::<M> as *const () as usize,
        configure::<M> as *const () as usize,
        query::<M> as *const () as usize,
        dispatch::<M> as *const () as usize,
    ]
}

fn read<const N: usize>(address: usize) -> Option<[u8; N]> {
    if address == 0 { return None; }
    let mut bytes = [0; N];
    let mut count = 0usize;
    let ok = unsafe {
        ReadProcessMemory(GetCurrentProcess(), address as *const c_void,
            bytes.as_mut_ptr().cast(), N, &mut count)
    };
    (ok != 0 && count == N).then_some(bytes)
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
fn f32_at(b: &[u8], at: usize) -> f32 {
    f32::from_bits(u32_at(b, at))
}
fn dims(b: &[u8], at: usize) -> [u32; 2] {
    [u32_at(b, at), u32_at(b, at + 4)]
}
fn floats(b: &[u8], at: usize) -> [f32; 2] {
    [f32_at(b, at), f32_at(b, at + 4)]
}
fn valid_size(d: [u32; 2]) -> bool {
    d.into_iter().all(|v| (1..=16384).contains(&v))
}
fn context_value(context: *mut c_void) -> u64 {
    read::<8>(context as usize).map_or(0, |b| u64_at(&b, 0))
}
fn audit_enabled() -> bool {
    cfg!(feature = "diagnostics") && *ENABLED.get_or_init(|| std::env::var_os("VICTORY_FFX_AUDIT").is_none_or(|v| v != "0"))
}

fn sample(n: u64) -> bool {
    n <= 8 || n.is_power_of_two()
}

fn before<const M: usize>(op: usize, context: *mut c_void, desc: *const c_void) -> (u64, u64) {
    let n = CALLS[M][op].fetch_add(1, Ordering::Relaxed) + 1;
    if !audit_enabled() || !sample(n) { return (n, 0); }
    let header = read::<16>(desc as usize);
    let kind = header.as_ref().map_or(0, |b| u64_at(b, 0));
    let next = header.as_ref().map_or(0, |b| u64_at(b, 8));
    let next_kind = read::<16>(next as usize).map_or(0, |b| u64_at(&b, 0));
    let context_value = if op == CREATE { 0 } else { context_value(context) };
    tracing::info!(module = MODULES[M], call = NAMES[op], n,
        context_slot = context as usize, context_value = format_args!("{context_value:#x}"),
        descriptor = format_args!("{kind:#x}"), next_descriptor = format_args!("{next_kind:#x}"),
        "FFX audit: title call");
    match (op, kind) {
        (CREATE, 0x0001_0000) => {
            if let Some(b) = read::<36>(desc as usize) {
                tracing::info!(module = MODULES[M], n, flags = format_args!("{:#x}", u32_at(&b, 16)),
                    max_render = ?dims(&b, 20), max_upscale = ?dims(&b, 28),
                    "FFX audit: upscale context candidate");
            }
        }
        (DISPATCH, 0x0001_0001) => {
            if let Some(b) = read::<432>(desc as usize) {
                let render = dims(&b, 376);
                let upscale = dims(&b, 384);
                let scale = floats(&b, 368);
                // Reject an inconsistent layout instead of reporting guessed fields.
                if valid_size(render) && (valid_size(upscale) || upscale == [0, 0])
                    && scale.iter().all(|v| v.is_finite() && v.abs() <= 65536.0)
                    && b[392] <= 1 && b[408] <= 1
                {
                    tracing::info!(module = MODULES[M], n, render = ?render, upscale = ?upscale,
                        command_list = format_args!("{:#x}", u64_at(&b, 16)),
                        jitter = ?floats(&b, 360), mv_scale = ?scale,
                        sharpening = b[392] != 0, sharpness = f32_at(&b, 396),
                        dt_ms = f32_at(&b, 400), pre_exposure = f32_at(&b, 404),
                        reset = b[408] != 0, near = f32_at(&b, 412), far = f32_at(&b, 416),
                        fov_y = f32_at(&b, 420), meters = f32_at(&b, 424),
                        flags = format_args!("{:#x}", u32_at(&b, 428)),
                        "FFX audit: upscale dispatch inputs (public ABI candidate)");
                    for (i, label) in ["color", "depth", "motion_vectors", "exposure", "reactive",
                        "transparency", "output"].iter().enumerate() {
                        let at = 24 + i * 48;
                        tracing::info!(module = MODULES[M], n, input = *label,
                            resource = format_args!("{:#x}", u64_at(&b, at)),
                            resource_type = u32_at(&b, at + 8), ffx_format = u32_at(&b, at + 12),
                            size = ?dims(&b, at + 16), state = format_args!("{:#x}", u32_at(&b, at + 40)),
                            "FFX audit: upscale resource");
                    }
                } else {
                    tracing::warn!(module = MODULES[M], n, "FFX audit: upscale descriptor does not match public ABI");
                }
            } else {
                tracing::warn!(module = MODULES[M], n, "FFX audit: could not read upscale descriptor");
            }
        }
        (QUERY, 4) => {
            if let Some(b) = read::<40>(desc as usize) {
                tracing::info!(module = MODULES[M], n, effect = format_args!("{:#x}", u64_at(&b, 16)),
                    "FFX audit: provider enumeration query");
            }
        }
        (QUERY, 0x0001_0002) => {
            if let Some(b) = read::<20>(desc as usize) {
                tracing::info!(module = MODULES[M], n, quality_mode = u32_at(&b, 16), "FFX audit: quality ratio query");
            }
        }
        (QUERY, 0x0001_0003) => {
            if let Some(b) = read::<28>(desc as usize) {
                tracing::info!(module = MODULES[M], n, display = ?dims(&b, 16), quality_mode = u32_at(&b, 24),
                    "FFX audit: quality resolution query");
            }
        }
        _ => {}
    }
    (n, kind)
}

fn after<const M: usize>(op: usize, n: u64, kind: u64, context: *mut c_void, rc: u32) {
    let errors = if rc != 0 { ERRORS[M][op].fetch_add(1, Ordering::Relaxed) + 1 } else { 0 };
    if audit_enabled() && (sample(n) || (errors != 0 && sample(errors))) {
        let value = if op == CREATE && rc == 0 { context_value(context) } else { 0 };
        tracing::info!(module = MODULES[M], call = NAMES[op], n, descriptor = format_args!("{kind:#x}"),
            result = rc, errors, created_context = format_args!("{value:#x}"), "FFX audit: title result");
    }
}

type Create = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_void) -> u32;
type Destroy = unsafe extern "C" fn(*mut c_void, *const c_void) -> u32;
type Call = unsafe extern "C" fn(*mut c_void, *const c_void) -> u32;

unsafe extern "C" fn create<const M: usize>(context: *mut c_void, desc: *mut c_void, alloc: *const c_void) -> u32 {
    let f: Create = unsafe { std::mem::transmute(ORIGINAL[M][CREATE].load(Ordering::Acquire)) };
    let previous_error = unsafe { GetLastError() };
    let (n, kind) = before::<M>(CREATE, context, desc);
    unsafe { SetLastError(previous_error) };
    let rc = unsafe { f(context, desc, alloc) };
    let last_error = unsafe { GetLastError() };
    after::<M>(CREATE, n, kind, context, rc);
    unsafe { SetLastError(last_error) };
    rc
}
unsafe extern "C" fn destroy<const M: usize>(context: *mut c_void, alloc: *const c_void) -> u32 {
    let f: Destroy = unsafe { std::mem::transmute(ORIGINAL[M][DESTROY].load(Ordering::Acquire)) };
    let previous_error = unsafe { GetLastError() };
    let (n, kind) = before::<M>(DESTROY, context, std::ptr::null());
    unsafe { SetLastError(previous_error) };
    let rc = unsafe { f(context, alloc) };
    let last_error = unsafe { GetLastError() };
    after::<M>(DESTROY, n, kind, context, rc);
    if M == 0 && rc == 0 { fsr::clear(); }
    unsafe { SetLastError(last_error) };
    rc
}
fn invoke<const M: usize, const OP: usize>(context: *mut c_void, desc: *const c_void) -> u32 {
    let f: Call = unsafe { std::mem::transmute(ORIGINAL[M][OP].load(Ordering::Acquire)) };
    let previous_error = unsafe { GetLastError() };
    let (n, kind) = before::<M>(OP, context, desc);
    unsafe { SetLastError(previous_error) };
    let rc = unsafe { f(context, desc) };
    let last_error = unsafe { GetLastError() };
    after::<M>(OP, n, kind, context, rc);
    if OP == QUERY && audit_enabled() && sample(n) && rc == 0 {
        if kind == 6 {
            if let Some(b) = read::<32>(desc as usize) {
                let address = u64_at(&b, 24) as usize;
                let mut bytes = Vec::new();
                for i in 0..96 {
                    let Some(at) = address.checked_add(i) else { break };
                    let Some([byte]) = read::<1>(at) else { break };
                    if byte == 0 { break; }
                    bytes.push(byte);
                }
                tracing::info!(module = MODULES[M], n,
                    provider_id = format_args!("{:#x}", u64_at(&b, 16)),
                    provider = %String::from_utf8_lossy(&bytes), "FFX audit: provider version");
            }
        } else if kind == 0x0001_0003
            && let Some(b) = read::<48>(desc as usize)
        {
            let width = read::<4>(u64_at(&b, 32) as usize).map(|p| u32_at(&p, 0));
            let height = read::<4>(u64_at(&b, 40) as usize).map(|p| u32_at(&p, 0));
            tracing::info!(module = MODULES[M], n, ?width, ?height, "FFX audit: queried render resolution");
        }
    }
    unsafe { SetLastError(last_error) };
    rc
}
unsafe extern "C" fn configure<const M: usize>(context: *mut c_void, desc: *const c_void) -> u32 {
    invoke::<M, CONFIGURE>(context, desc)
}
unsafe extern "C" fn query<const M: usize>(context: *mut c_void, desc: *mut c_void) -> u32 {
    invoke::<M, QUERY>(context, desc)
}
// Layout validated by the title's build 0.131 audit. Unknown descriptor
// chains or sizes are passed through, not reinterpreted. Only the outer
// loader dispatch is modified, so nested provider calls see the same copy.
#[repr(C, align(8))]
struct UpscaleBytes([u8; 432]);

fn upscale_bytes(desc: *const c_void) -> Option<UpscaleBytes> {
    let header = read::<16>(desc as usize)?;
    if u64_at(&header, 0) != 0x0001_0001 || u64_at(&header, 8) != 0 { return None; }
    let b = read::<432>(desc as usize)?;
    let render = dims(&b, 376);
    let output = dims(&b, 384);
    if !valid_size(render) || !valid_size(output)
        || b[392] > 1 || b[408] > 1
        || !f32_at(&b, 396).is_finite()
        || [0, 1, 2, 6].into_iter().any(|i| u64_at(&b, 24 + i * 48) == 0)
    {
        return None;
    }
    Some(UpscaleBytes(b))
}

fn apply_sharpening(b: &mut [u8; 432], requested: Sharpening) {
    match requested {
        Sharpening::Game => {}
        Sharpening::Off => {
            b[392] = 0;
            b[396..400].copy_from_slice(&0.0f32.to_le_bytes());
        }
        Sharpening::On(p) => {
            b[392] = 1;
            b[396..400].copy_from_slice(&(f32::from(p.min(100)) / 100.0).to_le_bytes());
        }
    }
}

unsafe extern "C" fn dispatch<const M: usize>(context: *mut c_void, desc: *const c_void) -> u32 {
    if M != 0 {
        return invoke::<M, DISPATCH>(context, desc);
    }
    let previous_error = unsafe { GetLastError() };
    let requested = fsr::request();
    let mut snapshot = upscale_bytes(desc);
    let actual = if let Some(b) = snapshot.as_mut() {
        apply_sharpening(&mut b.0, requested);
        if requested != Sharpening::Game { std::ptr::from_ref(b).cast() } else { desc }
    } else {
        desc
    };
    unsafe { SetLastError(previous_error) };
    let rc = invoke::<M, DISPATCH>(context, actual);
    let last_error = unsafe { GetLastError() };
    if let Some(b) = snapshot {
        let state = Status {
            render: dims(&b.0, 376), upscale: dims(&b.0, 384),
            sharpening: b.0[392] != 0,
            sharpness: (f32_at(&b.0, 396).clamp(0.0, 1.0) * 100.0).round() as u8,
            applied: requested, result: rc,
        };
        if fsr::status() != Some(state) {
            tracing::info!(?state, "FFX runtime: title sharpening dispatch");
        }
        fsr::publish(state);
        let n = CALLS[M][DISPATCH].load(Ordering::Relaxed);
        if rc == 0 && audit_enabled() && sample(n) {
            let ids = [0, 1, 2, 6];
            fsr::observe(ids.map(|i| u64_at(&b.0, 24 + i * 48) as usize),
                ids.map(|i| dims(&b.0, 40 + i * 48)));
        }
    } else if read::<16>(desc as usize).is_some_and(|b| u64_at(&b, 0) == 0x0001_0001) {
        // A new incompatible upscale descriptor must not leave the menu
        // advertising an applied override from an older dispatch.
        fsr::clear();
    }
    unsafe { SetLastError(last_error) };
    rc
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_known_modules_are_intercepted() {
        assert!(is_module("AMD_FidelityFX_Loader_Xbox.dll"));
        assert!(!is_module("amd_fidelityfx_loader_dx12.dll"));
    }
    #[test]
    fn sampled_calls_are_bounded() {
        assert!(sample(1) && sample(8) && sample(1024));
        assert!(!sample(9) && !sample(1023));
    }
    #[test]
    fn sharpening_override_preserves_other_fields() {
        let mut b = [0x55; 432];
        let original = b;
        apply_sharpening(&mut b, Sharpening::Game);
        assert_eq!(b, original);
        apply_sharpening(&mut b, Sharpening::On(40));
        assert_eq!(b[392], 1);
        assert_eq!(f32_at(&b, 396), 0.4);
        assert_eq!(&b[..392], &original[..392]);
        assert_eq!(&b[400..], &original[400..]);
        apply_sharpening(&mut b, Sharpening::Off);
        assert_eq!(b[392], 0);
        assert_eq!(f32_at(&b, 396), 0.0);
    }
    #[test]
    fn diagnostic_read_of_own_memory() {
        let value = [1u8, 2, 3, 4];
        assert_eq!(read::<4>(value.as_ptr() as usize), Some(value));
        assert_eq!(read::<4>(0), None);
    }
}
