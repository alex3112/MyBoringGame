//! Read-only memory accounting, independent of GPU submissions.
//! Counters are not additive: logical spans and pools can overlap OS totals.
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct Meter {
    live: AtomicU64,
    peak: AtomicU64,
    added: AtomicU64,
    removed: AtomicU64,
    longest_ms: AtomicU64,
}
impl Meter {
    const fn new() -> Self {
        Self { live: AtomicU64::new(0), peak: AtomicU64::new(0), added: AtomicU64::new(0), removed: AtomicU64::new(0), longest_ms: AtomicU64::new(0) }
    }
    pub fn add(&self, bytes: u64) {
        let live = self.live.fetch_add(bytes, Relaxed).saturating_add(bytes);
        self.peak.fetch_max(live, Relaxed);
        self.added.fetch_add(bytes, Relaxed);
    }
    pub fn remove(&self, bytes: u64) {
        self.live.fetch_sub(bytes, Relaxed);
        self.removed.fetch_add(bytes, Relaxed);
    }
    pub fn lease(&'static self, bytes: u64) -> Lease {
        self.add(bytes);
        Lease { meter: self, bytes, start: Instant::now() }
    }
    fn sample(&self) -> [u64; 5] {
        [self.live.load(Relaxed), self.peak.load(Relaxed), self.added.load(Relaxed), self.removed.load(Relaxed), self.longest_ms.load(Relaxed)]
    }
}
pub struct Lease { meter: &'static Meter, bytes: u64, start: Instant }
impl Drop for Lease {
    fn drop(&mut self) {
        self.meter.remove(self.bytes);
        self.meter.longest_ms.fetch_max(self.start.elapsed().as_millis() as u64, Relaxed);
    }
}
pub static TITLE_HEAP: Meter = Meter::new();
pub static TITLE_SECTION_RESERVED: Meter = Meter::new();
pub static TITLE_SECTION_COMMIT: Meter = Meter::new();
pub static SNAPSHOT: Meter = Meter::new();
pub static STORAGE: Meter = Meter::new();
pub static DESCRIPTOR_NATIVE: Meter = Meter::new();
pub static DESCRIPTOR_CPU: Meter = Meter::new();
pub static VIRTUAL_COMMIT_CALL_BYTES: AtomicU64 = AtomicU64::new(0);
pub static VIRTUAL_FREE_CALL_BYTES: AtomicU64 = AtomicU64::new(0);
pub static VIRTUAL_FREE_CALLS: AtomicU64 = AtomicU64::new(0);

static METERS: [(&str, &Meter); 7] = [
    ("title_default_heap", &TITLE_HEAP), ("title_section_capacity", &TITLE_SECTION_RESERVED),
    ("title_section_commit_observed", &TITLE_SECTION_COMMIT), ("snapshot_checked_out_capacity", &SNAPSHOT),
    ("dstorage_temporary_capacity", &STORAGE), ("descriptor_native_span", &DESCRIPTOR_NATIVE),
    ("descriptor_fixed_cpu_estimate", &DESCRIPTOR_CPU),
];
const NAMES: &[&str] = &[
    "unix_ms", "elapsed_ms", "frame", "physical_total_bytes", "physical_available_bytes",
    "system_commit_limit_bytes", "system_commit_available_bytes", "process_working_set_bytes",
    "process_peak_working_set_bytes", "process_private_commit_bytes", "process_peak_commit_bytes",
    "dxgi_local_budget_bytes", "dxgi_local_used_bytes", "dxgi_shared_budget_bytes", "dxgi_shared_used_bytes",
    "shadow_count", "shadow_logical_span_bytes", "shadow_card_span_bytes", "shadow_card_hash_bytes",
    "snapshot_kept_span_bytes", "snapshot_idle_pool_capacity_bytes", "window_key_span_bytes",
    "window_union_span_bytes", "window_asked_span_sum_bytes", "texture_pool_block_bytes", "staging_pool_block_bytes",
    "texture_staging_logical_bytes", "host_texture_count", "host_texture_wrapper_bytes", "title_filled_texture_wrapper_bytes",
    "tiled_texture_full_span_bytes", "tiled_texture_backed_bytes", "tiled_heap_capacity_bytes",
    "scratch_all_bytes", "scratch_idle_bytes", "other_list_upload_bytes", "host_list_count",
    "host_upload_created_bytes_cumulative", "host_shared_created_bytes_cumulative", "host_local_created_bytes_cumulative",
    "shader_container_bytes", "shader_translated_bytes", "pipeline_library_file_bytes",
    "title_virtual_commit_call_bytes_cumulative", "title_virtual_free_call_bytes_cumulative", "title_virtual_free_calls_cumulative",
    "native_list_pool_idle_count", "thin_backend_pool_idle_count", "native_list_lazy_acquired_cumulative",
    "native_list_lazy_detached_cumulative", "native_list_pool_hits_cumulative",
    "partial_mip_copies_created_cumulative", "partial_mip_native_bytes_created_cumulative",
    "partial_mip_copies_grown_cumulative", "partial_mip_title_full_span_bytes_cumulative",
    "window_reuse_requests_cumulative", "window_reuse_span_bytes_cumulative",
    "relay_fetch_capacity_bytes", "relay_fetch_batches_cumulative", "relay_fetch_bytes_cumulative",
    "scratch_growth_fallbacks_cumulative", "upload_allocation_failures_cumulative", "shared_allocation_failures_cumulative",
    "scratch_page_pool_capacity_bytes", "small_staging_pool_capacity_bytes", "scratch_slices_cumulative", "scratch_slice_bytes_cumulative",
    "small_staging_slices_cumulative", "small_staging_slice_bytes_cumulative", "zero_upload_cache_capacity_bytes",
    "zero_upload_reuses_cumulative", "zero_upload_reused_bytes_cumulative",
    "record_refresh_coverage_skips_cumulative", "record_refresh_coverage_bytes_cumulative",
    "submit_refresh_coverage_skips_cumulative", "submit_refresh_coverage_bytes_cumulative",
    "journal_clean_blocks_cumulative", "journal_clean_block_bytes_cumulative",
    "journal_ahead_pass_skips_cumulative", "journal_ahead_range_skips_cumulative",
    "snapshot_new_allocations_cumulative", "snapshot_pool_hits_cumulative",
    "snapshot_pool_reused_capacity_bytes_cumulative", "snapshot_sparse_reservations_cumulative",
    "snapshot_sparse_reserved_span_bytes_cumulative", "snapshot_sparse_committed_bytes_cumulative",
    "snapshot_actual_copy_bytes_cumulative", "snapshot_fully_covered_declarations_cumulative",
];

fn sample(start: Instant) -> Vec<Option<u64>> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: zero-initialized, correctly sized output structures for the current process.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    let mut c = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    let mem = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
    let proc = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } != 0;
    let mut row = vec![
        Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64),
        Some(start.elapsed().as_millis() as u64), Some(crate::frame::current() as u64),
        mem.then_some(m.ullTotalPhys), mem.then_some(m.ullAvailPhys),
        mem.then_some(m.ullTotalPageFile), mem.then_some(m.ullAvailPageFile),
        proc.then_some(c.WorkingSetSize as u64), proc.then_some(c.PeakWorkingSetSize as u64),
        proc.then_some(c.PagefileUsage as u64), proc.then_some(c.PeakPagefileUsage as u64),
    ];
    row.extend(crate::host::made_host().and_then(|h| h.video_memory()).map_or([None; 4], |v| v.map(Some)));
    row.extend(crate::shadow::audit());
    row.extend(crate::pool::audit());
    let tex = &crate::resource::HOST_TEXTURES;
    let tiles = &crate::tiles::STATS;
    let alive = &crate::host::ALIVE;
    let made = &crate::host::MADE;
    let shaders = crate::pipeline::kept();
    row.extend([
        crate::texture::live_staging_bytes(), tex[0].load(Relaxed), tex[1].load(Relaxed),
        crate::resource::KINDS[1].load(Relaxed), tiles[1].load(Relaxed), tiles[2].load(Relaxed), tiles[3].load(Relaxed),
        alive[2].load(Relaxed), alive[4].load(Relaxed), alive[7].load(Relaxed), alive[0].load(Relaxed),
        made[1].load(Relaxed), made[3].load(Relaxed), made[5].load(Relaxed),
        shaders[1], shaders[3], shaders[4], VIRTUAL_COMMIT_CALL_BYTES.load(Relaxed),
        VIRTUAL_FREE_CALL_BYTES.load(Relaxed), VIRTUAL_FREE_CALLS.load(Relaxed)
    ].map(Some));
    row.extend(crate::list::audit());
    row.extend(crate::resource::HELD.iter().map(|n| Some(n.load(Relaxed))));
    row.extend(crate::shadow::WINDOW_REUSE.iter().map(|n| Some(n.load(Relaxed))));
    row.extend(crate::host::MEMORY_REUSE.iter().map(|n| Some(n.load(Relaxed))));
    row.extend(crate::pool::upload_audit());
    row.extend(crate::host::ZERO_REUSE.iter().map(|n| Some(n.load(Relaxed))));
    row.extend(crate::host::REFRESH_REUSE.iter().map(|n| Some(n.load(Relaxed))));
    row.extend(crate::watch::JOURNAL_REUSE.iter().map(|n| Some(n.load(Relaxed))));
    row.extend(crate::shadow::SNAPSHOT_REUSE.iter().map(|n| Some(n.load(Relaxed))));
    for (_, meter) in METERS { row.extend(meter.sample().map(Some)); }
    row
}

fn write_row(w: &mut impl Write, row: &[Option<u64>]) -> std::io::Result<()> {
    for (i, v) in row.iter().enumerate() {
        if i != 0 { write!(w, ",")?; }
        if let Some(v) = v { write!(w, "{v}")?; }
    }
    writeln!(w)?;
    w.flush()
}

/// Start before executing the title. No GPU device is created by the sampler.
/// Each row is flushed so a sudden exit retains all completed samples.
pub fn start() -> std::io::Result<()> {
    if std::env::var_os("VICTORY_MEMORY_AUDIT").is_some_and(|v| v == "0") {
        eprintln!("Memory audit disabled by VICTORY_MEMORY_AUDIT=0");
        return Ok(());
    }
    let path = std::env::var_os("VICTORY_MEMORY_AUDIT_PATH").map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.join("memory_audit.csv")))
            .unwrap_or_else(|| "memory_audit.csv".into()));
    // Resolve the output before any title image is mapped or executed.
    let path = std::path::absolute(path)?;
    eprintln!("Memory audit output: {}", path.display());
    let file = std::fs::File::create(&path).map_err(|e| {
        std::io::Error::new(e.kind(), format!("creating {}: {e}", path.display()))
    })?;
    let mut writer = BufWriter::new(file);
    let mut names: Vec<String> = NAMES.iter().map(|s| (*s).into()).collect();
    for (name, _) in METERS {
        for suffix in ["live_bytes", "peak_bytes", "added_bytes_cumulative", "removed_bytes_cumulative", "longest_released_ms"] {
            names.push(format!("{name}_{suffix}"));
        }
    }
    writeln!(writer, "{}", names.join(","))?;
    writer.flush()?;
    let start = Instant::now();
    let first = sample(start);
    if first.len() != names.len() {
        return Err(std::io::Error::other(format!("memory audit schema mismatch: {} columns, {} values", names.len(), first.len())));
    }
    write_row(&mut writer, &first)?;
    std::thread::Builder::new().name("victory-memory-audit".into()).spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let row = sample(start);
            if let Err(e) = write_row(&mut writer, &row) {
                eprintln!("Memory audit stopped: {e}");
                break;
            }
        }
    })?;
    eprintln!("Memory audit active: {} (one sample per second)", path.display());
    Ok(())
}
