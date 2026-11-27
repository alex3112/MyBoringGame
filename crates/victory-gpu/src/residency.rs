//! Host-memory admission for speculative texture work and reusable upload storage.
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
static PRESSURED: AtomicBool = AtomicBool::new(false);
static HEADROOM: AtomicU64 = AtomicU64::new(u64::MAX);

fn memory_status() -> Option<MEMORYSTATUSEX> {
    // SAFETY: the API initializes the fields of a correctly sized structure.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status)
}

// The texture sweep may run for seconds inside a single submitted frame.
static NEXT_COMMIT_SAMPLE: AtomicU64 = AtomicU64::new(0);
static AVAILABLE_COMMIT: AtomicU64 = AtomicU64::new(u64::MAX);
static AVAILABLE_PHYSICAL: AtomicU64 = AtomicU64::new(u64::MAX);
static LAST_REPORT: AtomicU64 = AtomicU64::new(0);
static RETRY_UPLOAD_AT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn allocation_error(error: &windows::core::Error) {
    if error.code() == windows::Win32::Foundation::E_OUTOFMEMORY {
        RETRY_UPLOAD_AT.fetch_max(crate::host::coarse_ms().saturating_add(500), Ordering::Relaxed);
    }
}

thread_local! {
    static STAGING_DEFERRED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn note_staging_deferred() {
    STAGING_DEFERRED.set(true);
}

pub(crate) fn take_staging_deferred() -> bool {
    STAGING_DEFERRED.replace(false)
}

pub(crate) fn upload_refused() -> bool {
    crate::host::coarse_ms() < RETRY_UPLOAD_AT.load(Ordering::Relaxed)
}

fn sample_commit(now: u64) -> u64 {
    let next = NEXT_COMMIT_SAMPLE.load(Ordering::Relaxed);
    if now >= next
        && NEXT_COMMIT_SAMPLE
            .compare_exchange(next, now.saturating_add(32), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        && let Some(status) = memory_status()
    {
        AVAILABLE_COMMIT.store(status.ullAvailPageFile, Ordering::Relaxed);
        AVAILABLE_PHYSICAL.store(status.ullAvailPhys, Ordering::Relaxed);
        HEADROOM.store(status.ullAvailPhys.min(status.ullAvailPageFile), Ordering::Relaxed);
    }
    AVAILABLE_COMMIT.load(Ordering::Relaxed)
}

fn has_upload_reserve(commit: u64, bake: bool) -> bool {
    // Keep a protective floor against Rust/game OOM aborts without starving
    // bindless world textures or one-shot ped decoration bakes when steady-state
    // commit headroom sits in the 1-4 GiB range on 32 GiB hosts.
    commit >= if bake { 256 * MIB } else { 512 * MIB }
}

fn has_growth_reserve(commit: u64, bake: bool) -> bool {
    // Growing a fresh 64 MiB upload block for speculative bindless prefetch
    // requires more headroom than reusing slices already committed inside the
    // staging pool.
    commit >= if bake { 256 * MIB } else { 1024 * MIB }
}

/// Leave commit for game allocations and submissions already in flight while
/// still letting bindless textures and one-shot material bakes upload.
pub(crate) fn admit_texture_pass(bake: bool) -> bool {
    let now = crate::host::coarse_ms();
    if now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) {
        return false;
    }
    let commit = sample_commit(now);
    has_upload_reserve(commit, bake)
        && (bake || AVAILABLE_PHYSICAL.load(Ordering::Relaxed) >= 512 * MIB)
}

/// Whether the staging allocator may commit a brand-new OS upload buffer
/// (reusing existing free ranges in already-committed pool blocks stays open
/// whenever `admit_texture_pass` is true).
pub(crate) fn allow_staging_growth(bake: bool) -> bool {
    let now = crate::host::coarse_ms();
    if now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) {
        return false;
    }
    let commit = sample_commit(now);
    has_growth_reserve(commit, bake)
        && (bake || AVAILABLE_PHYSICAL.load(Ordering::Relaxed) >= GIB)
}

pub(crate) fn limited_host_ram() -> bool {
    static LIMITED: OnceLock<bool> = OnceLock::new();
    *LIMITED.get_or_init(|| memory_status().is_some_and(|s| s.ullTotalPhys <= 32 * GIB))
}

/// Smaller hosts need proactive cache limits, even with a large pagefile.
pub(crate) fn compact_host() -> bool {
    static COMPACT: OnceLock<bool> = OnceLock::new();
    *COMPACT.get_or_init(|| memory_status().is_some_and(|s| s.ullTotalPhys <= 24 * GIB))
}

/// Maximum optional texture budget as a share of the live DXGI budget.
pub(crate) fn texture_percent(configured: u64) -> u64 {
    let cap = if pressured() { 80 } else if compact_host() { 85 } else { 97 };
    configured.min(cap)
}

/// The optional local copies share a live DXGI ceiling, independent of
/// the host-memory texture staging throttle.
pub(crate) fn adaptive_vram() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ADAPTIVE_VRAM").is_none_or(|v| v != "0"))
}

pub(crate) fn local_target_percent() -> u64 {
    static TARGET: OnceLock<u64> = OnceLock::new();
    *TARGET.get_or_init(|| std::env::var("VICTORY_VRAM_TARGET_PERCENT").ok()
        .and_then(|v| v.parse::<u64>().ok()).map_or(92, |v| v.clamp(80, 95)))
}

// Serialize shadow admissions through creation. DXGI and texture
// allocations may still change residency independently.
pub(crate) fn local_shadow_lock() -> std::sync::MutexGuard<'static, ()> {
    static ADMISSION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ADMISSION.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn local_shadow_room(memory: Option<[u64; 4]>, size: u64, legacy_percent: u64, optional: bool) -> bool {
    if adaptive_vram() && optional && critical_memory() {
        return false;
    }
    let percent = if adaptive_vram() { local_target_percent() } else { legacy_percent };
    local_room_at(memory, size, percent, adaptive_vram() && optional)
}

fn local_room_at(memory: Option<[u64; 4]>, size: u64, percent: u64, require_budget: bool) -> bool {
    match memory {
        Some([budget, used, ..]) if budget != 0 => used.saturating_add(size) <= budget / 100 * percent,
        _ => !require_budget,
    }
}

#[cfg(test)]
#[test]
fn local_shadow_admission_respects_live_budget_and_unknown_budget() {
    assert!(local_room_at(Some([1000 * MIB, 900 * MIB, 0, 0]), 20 * MIB, 92, true));
    assert!(!local_room_at(Some([1000 * MIB, 900 * MIB, 0, 0]), 21 * MIB, 92, true));
    assert!(!local_room_at(None, MIB, 92, true));
    assert!(local_room_at(None, MIB, 92, false));
}

/// Limit idle caches, not snapshots still required by submitted work.
pub(crate) fn cache_limit(normal: u64, compact: u64) -> u64 {
    if pressured() {
        0
    } else if compact_host() {
        normal.min(compact)
    } else {
        normal
    }
}

pub(crate) fn critical_memory() -> bool {
    let _ = sample_commit(crate::host::coarse_ms());
    critical_commit() || AVAILABLE_PHYSICAL.load(Ordering::Relaxed) < GIB
}

fn next_pressure(was: bool, physical: u64, commit: u64) -> bool {
    if was {
        physical < 5 * GIB || commit < 6 * GIB
    } else {
        physical <= 3 * GIB || commit <= 4 * GIB
    }
}

/// Called by the once-per-second submission sweep. A failed sample keeps the last good state.
pub(crate) fn refresh() {
    let Some(status) = memory_status() else { return };
    let (physical, commit) = (status.ullAvailPhys, status.ullAvailPageFile);
    HEADROOM.store(physical.min(commit), Ordering::Relaxed);
    AVAILABLE_PHYSICAL.store(physical, Ordering::Relaxed);
    AVAILABLE_COMMIT.store(commit, Ordering::Relaxed);
    let was = pressured();
    // Enter at 4 GiB free on a compact host, recover at 6 GiB.
    let pressure = next_pressure(was, physical.saturating_sub(if compact_host() { GIB } else { 0 }), commit);
    let now = crate::host::coarse_ms();
    let last = LAST_REPORT.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= 5000
        && LAST_REPORT.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok()
    {
        let video = crate::host::host().and_then(|h| h.video_memory()).unwrap_or([0; 4]);
        tracing::info!(
            frame = crate::frame::current(),
            physical_free_mib = physical >> 20,
            available_commit_mib = commit >> 20,
            local_budget_mib = video[0] >> 20,
            local_used_mib = video[1] >> 20,
            shared_used_mib = video[3] >> 20,
            buffer_mirror_span_mib = crate::shadow::live_span_bytes() >> 20,
            local_shadow_buffers_mib = crate::shadow::local_copy_bytes()[0] >> 20,
            local_shadow_windows_mib = crate::shadow::local_copy_bytes()[1] >> 20,
            local_mapped_windows_mib = crate::shadow::local_copy_bytes()[2] >> 20,
            window_admission_refusals = ?crate::shadow::local_window_refusals(),
            vram_target_percent = local_target_percent(),
            texture_staging_mib = crate::texture::live_staging_bytes() >> 20,
            spare_scratch_mib = crate::host::ALIVE[4].load(Ordering::Relaxed) >> 20,
            tiled_gpu_texture_mib = crate::tiles::given_total() >> 20,
            pressure,
            "memory budget sample (MiB)"
        );
    }
    if pressure != was {
        PRESSURED.store(pressure, Ordering::Relaxed);
        if pressure {
            tracing::warn!(
                pid = std::process::id(),
                frame = crate::frame::current(),
                physical_free_mib = physical >> 20,
                available_commit_mib = commit >> 20,
                buffer_mirror_span_mib = crate::shadow::live_span_bytes() >> 20,
            local_shadow_buffers_mib = crate::shadow::local_copy_bytes()[0] >> 20,
            local_shadow_windows_mib = crate::shadow::local_copy_bytes()[1] >> 20,
            local_mapped_windows_mib = crate::shadow::local_copy_bytes()[2] >> 20,
            window_admission_refusals = ?crate::shadow::local_window_refusals(),
            vram_target_percent = local_target_percent(),
                texture_staging_mib = crate::texture::live_staging_bytes() >> 20,
                spare_scratch_mib = crate::host::ALIVE[4].load(Ordering::Relaxed) >> 20,
                game_filled_gpu_texture_mib = crate::resource::KINDS[1].load(Ordering::Relaxed) >> 20,
                tiled_gpu_texture_mib = crate::tiles::given_total() >> 20,
                "host memory pressure: limiting texture prefetch and idle upload storage"
            );
        } else {
            tracing::info!(physical_free_mib = physical >> 20, available_commit_mib = commit >> 20, "host memory pressure cleared");
        }
    }
}

pub(crate) fn headroom() -> Option<u64> {
    let bytes = HEADROOM.load(Ordering::Relaxed);
    (bytes != u64::MAX).then_some(bytes)
}

pub(crate) fn pressured() -> bool {
    PRESSURED.load(Ordering::Relaxed)
}

/// True when OS commit headroom is critically low (< 1.5 GiB) or an allocation
/// just failed with `E_OUTOFMEMORY`, requiring emergency throttling.
pub(crate) fn critical_commit() -> bool {
    let now = crate::host::coarse_ms();
    now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) || sample_commit(now) < 1536 * MIB
}

/// A soft per-frame admission limit. A single texture can cross it. The
/// queue retains unserved slots so they can be retried on later frames.
pub(crate) fn prefetch_limit(bytes: u64, pressure: bool, critical: bool, limited: bool, staging: u64) -> u64 {
    if critical {
        bytes.min(16 << 20)
    } else if pressure || (limited && staging >= GIB) {
        bytes.min(64 << 20)
    } else {
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bake_and_lookahead_leave_room_for_live_game_allocations() {
        for (commit, bake, lookahead, growth) in [
            (128 * MIB, false, false, false),
            (256 * MIB, true, false, false),
            (512 * MIB, true, true, false),
            (768 * MIB, true, true, false),
            (1024 * MIB, true, true, true),
        ] {
            assert_eq!(has_upload_reserve(commit, true), bake);
            assert_eq!(has_upload_reserve(commit, false), lookahead);
            assert_eq!(has_growth_reserve(commit, false), growth);
        }
    }

    #[test]
    fn either_constraint_enters_pressure_and_both_must_recover() {
        assert!(next_pressure(false, 12 * GIB, 4 * GIB));
        assert!(next_pressure(false, 3 * GIB, 16 * GIB));
        assert!(next_pressure(true, 4 * GIB, 7 * GIB));
        assert!(next_pressure(true, 6 * GIB, 5 * GIB));
        assert!(!next_pressure(true, 5 * GIB, 6 * GIB));
        assert!(!next_pressure(false, 4 * GIB, 5 * GIB));
    }

    #[test]
    fn prefetch_respects_existing_budgets_and_resumes_after_recovery() {
        assert_eq!(prefetch_limit(512 << 20, false, false, true, GIB), 64 << 20);
        assert_eq!(prefetch_limit(512 << 20, false, false, true, GIB - 1), 512 << 20);
        assert_eq!(prefetch_limit(512 << 20, false, false, false, 2 * GIB), 512 << 20);
        assert_eq!(prefetch_limit(0, true, true, false, 0), 0);
        assert_eq!(prefetch_limit(8 << 20, true, true, false, 0), 8 << 20);
        assert_eq!(prefetch_limit(512 << 20, true, false, false, 0), 64 << 20);
        assert_eq!(prefetch_limit(512 << 20, true, true, false, 0), 16 << 20);
        assert_eq!(prefetch_limit(192 << 20, false, false, false, 0), 192 << 20);
    }
}
