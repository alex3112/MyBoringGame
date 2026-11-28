//! VUM 1: a dedicated XMem allocation backed by one committed section.
//! The CPU mapping and a single native buffer use the same physical pages.
//! Logical buffers/windows retain their bounds and share the native buffer.
//! VUM 2 adds generation-stable physical section identities and a bounded,
//! opt-in direct backing for committed pool segments. Normal remappable
//! pool rendering stays on the legacy path pending descriptor rebinding.

pub(crate) mod virtual_address;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use windows::Win32::Graphics::Direct3D12::*;
use windows_sys::Win32::System::Memory as mem;
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};

const PAGE: usize = 64 << 10;
const MIN: usize = 1 << 20;
const MAX: usize = 64 << 20;
const LIMIT: u64 = 256 << 20;

static NEXT: AtomicU64 = AtomicU64::new(1);
static CHARGED: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicU64 = AtomicU64::new(0);
static COUNT: AtomicU64 = AtomicU64::new(0);
// allocation attempts, admitted, driver refusals, binding views,
// eliminated refresh requests and their logical bytes, published ranges.
static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

pub(crate) struct Backing {
    pub(crate) id: u64,
    pub(crate) base: usize,
    bytes: usize,
    heap: ID3D12Heap,
    buffer: ID3D12Resource,
    gpu: u64,
    physical: Option<(usize, u64)>,
}
impl Drop for Backing {
    fn drop(&mut self) {
        if self.physical.is_some() {
            POOL_CHARGED.fetch_sub(self.bytes as u64, Relaxed);
        } else {
            CHARGED.fetch_sub(self.bytes as u64, Relaxed);
        }
    }
}

struct Allocation {
    handle: usize,
    backing: Arc<Backing>,
    // Original XMem attributes, not guessed host memory permissions.
    attributes: u64,
}

static ALLOCATIONS: Mutex<BTreeMap<usize, Allocation>> = Mutex::new(BTreeMap::new());

fn enabled() -> bool {
    crate::toggled!("VICTORY_VUM")
}

fn capacity() -> u64 {
    static CAP: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| std::env::var("VICTORY_VUM_MB").ok()
        .and_then(|v| v.parse::<u64>().ok()).map(|v| v.min(1024) << 20).unwrap_or(LIMIT))
}

/// No GPU initialization from a title allocator: only use an existing host.
/// If any operation fails, nothing is published and the caller uses its
/// old private allocation path.
pub fn allocate(size: usize, attributes: u64) -> Option<usize> {
    if !enabled() || !(MIN..=MAX).contains(&size) { return None; }
    let bytes = size.checked_next_multiple_of(PAGE)?;
    if bytes > MAX { return None; }
    STATS[0].fetch_add(1, Relaxed);
    let host = crate::host::existing_host()?;
    CHARGED.fetch_update(Relaxed, Relaxed, |n| n.checked_add(bytes as u64).filter(|&v| v <= capacity())).ok()?;
    let make = || -> Option<(usize, usize, ID3D12Heap, ID3D12Resource)> {
        let handle = unsafe { mem::CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(),
            mem::PAGE_READWRITE | mem::SEC_COMMIT, 0, bytes as u32, std::ptr::null()) };
        if handle.is_null() { return None; }
        let base = unsafe { mem::MapViewOfFile(handle, mem::FILE_MAP_WRITE, 0, 0, bytes) }.Value as usize;
        if base == 0 {
            unsafe { CloseHandle(handle); }
            return None;
        }
        let native = crate::inplace::open(&host.device, crate::inplace::From::Section(handle as usize))
            .and_then(|heap| crate::inplace::place(&host.device, &heap, 0, bytes as u64).map(|buffer| (heap, buffer)));
        match native {
            Ok((heap, buffer)) => Some((base, handle as usize, heap, buffer)),
            Err(e) => {
                if STATS[2].fetch_add(1, Relaxed) == 0 {
                    tracing::warn!(error = %e, "VUM: driver refused section backing; legacy allocation");
                }
                unsafe {
                    mem::UnmapViewOfFile(mem::MEMORY_MAPPED_VIEW_ADDRESS { Value: base as *mut _ });
                    CloseHandle(handle);
                }
                None
            }
        }
    };
    let Some((base, handle, heap, buffer)) = make() else {
        CHARGED.fetch_sub(bytes as u64, Relaxed);
        return None;
    };
    let id = NEXT.fetch_add(1, Relaxed);
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    crate::host::name(&buffer, || format!("VUM section {id} ({bytes:#x} bytes)"));
    let backing = Arc::new(Backing { id, base, bytes, heap, buffer, gpu, physical: None });
    // Register the CPU mapping with existing alias-aware guards so that a
    // legacy texture or crossing view can still observe updates.
    {
        let _mapping = crate::watch::title_memory_remapping();
        crate::watch::title_memory_remapped(base, bytes);
        crate::inplace::title_memory_section(handle, bytes as u64, true);
        crate::inplace::title_memory_view(base, bytes, handle, 0);
        ALLOCATIONS.lock().unwrap_or_else(|e| e.into_inner())
            .insert(base, Allocation { handle, backing, attributes });
        LIVE.fetch_add(bytes as u64, Relaxed);
        COUNT.fetch_add(1, Relaxed);
    }
    STATS[1].fetch_add(1, Relaxed);
    tracing::info!(id, base = format_args!("{base:#x}"), bytes, attributes = format_args!("{attributes:#x}"), "VUM: dedicated shared allocation");
    Some(base)
}

/// Release the CPU view. The native heap is retained by recorded lists,
/// and the section remains alive through their native references.
pub fn release(base: usize) -> bool {
    let _mapping = crate::watch::title_memory_remapping();
    let allocation = ALLOCATIONS.lock().unwrap_or_else(|e| e.into_inner()).remove(&base);
    let Some(allocation) = allocation else { return false; };
    let bytes = allocation.backing.bytes;
    crate::watch::title_memory_unmapping(base, bytes);
    crate::shadow::in_place_let_go(base as u64, bytes as u64);
    let unmapped = unsafe { mem::UnmapViewOfFile(mem::MEMORY_MAPPED_VIEW_ADDRESS { Value: base as *mut _ }) };
    if unmapped == 0 {
        // Keep ownership rather than closing a section still mapped.
        crate::inplace::title_memory_view(base, bytes, allocation.handle, 0);
        ALLOCATIONS.lock().unwrap_or_else(|e| e.into_inner()).insert(base, allocation);
        tracing::error!(base, "VUM: unable to unmap dedicated allocation");
        return false;
    }
    crate::inplace::title_memory_section_gone(allocation.handle);
    unsafe { CloseHandle(allocation.handle as _); }
    LIVE.fetch_sub(bytes as u64, Relaxed);
    COUNT.fetch_sub(1, Relaxed);
    tracing::debug!(id = allocation.backing.id, attributes = format_args!("{:#x}", allocation.attributes), "VUM: CPU view released");
    crate::host::retire(allocation.backing);
    true
}

pub(crate) fn place_for(start: u64, len: u64) -> Option<(crate::inplace::Placed, Arc<Backing>)> {
    if len == 0 { return None; }
    let start = usize::try_from(start).ok()?;
    let end = start.checked_add(usize::try_from(len).ok()?)?;
    let dedicated = if COUNT.load(Relaxed) == 0 { None } else {
        let map = ALLOCATIONS.lock().unwrap_or_else(|e| e.into_inner());
        map.range(..=start).next_back().and_then(|(&base,a)| {
            (end <= base.checked_add(a.backing.bytes)?).then(|| (a.backing.clone(), (start-base) as u64))
        })
    };
    let (backing, bias) = dedicated.or_else(|| pool_backing(start, end - start))?;
    let placed = crate::inplace::Placed { buffer: backing.buffer.clone(), heap: backing.heap.clone(),
        gpu: backing.gpu + bias, bias };
    STATS[3].fetch_add(1, Relaxed);
    Some((placed, backing))
}

pub(crate) fn overlaps(from: usize, to: usize) -> bool {
    if from >= to { return false; }
    if COUNT.load(Relaxed) != 0 {
        let map = ALLOCATIONS.lock().unwrap_or_else(|e| e.into_inner());
        if map.range(..to).next_back().is_some_and(|(&base,a)| base + a.backing.bytes > from) {
            return true;
        }
    }
    // A legacy copy crossing a pool alias must also wait for a direct
    // physical writer. Conservatively include partial mapped ranges.
    physical_experiment() && POOL_COUNT.load(Relaxed) != 0
        && crate::inplace::shown_bytes(from,to-from).is_some_and(|n| n != 0)
}

pub(crate) fn intersections(ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    if COUNT.load(Relaxed) == 0 { return Vec::new(); }
    let map = ALLOCATIONS.lock().unwrap_or_else(|e| e.into_inner());
    let mut result = Vec::new();
    for &(from,to) in ranges {
        if from >= to { continue; }
        let first = map.range(..=from).next_back().map(|(&base,_)| base).unwrap_or(from);
        for (&base,a) in map.range(first..to) {
            let at = base.max(from);
            let end = (base + a.backing.bytes).min(to);
            if at < end { result.push((at,end)); }
        }
    }
    result.sort_unstable();
    result
}

pub(crate) fn refresh_omitted(len: usize) {
    STATS[4].fetch_add(1, Relaxed);
    STATS[5].fetch_add(len as u64, Relaxed);
}

/// Called only after the actual native fence completes, before publishing
/// the guest fence. The bytes are already in the CPU mapping; only old
/// fallback copies/texture views need a journal notification. Never mark
/// a recycled virtual address from an old flight.
pub(crate) fn landed(backing: &Backing, guest_start: u64, bias: u64, ranges: &[(usize, usize)]) {
    let _mapping = crate::watch::title_memory_reading();
    if let Some((section,generation)) = backing.physical {
        // Use the physical coordinates captured by the recording, not the
        // mapping currently visible at its original guest address.
        let mut aliases = Vec::new();
        for &(from,to) in ranges {
            let from = (from as u64).max(guest_start);
            if from >= to as u64 { continue; }
            let Some(offset) = bias.checked_add(from-guest_start) else { continue; };
            let len = (to as u64-from).min((backing.bytes as u64).saturating_sub(offset));
            aliases.extend(crate::inplace::physical_aliases(section,generation,offset,len));
        }
        aliases.sort_unstable();
        let mut joined: Vec<(usize,usize)> = Vec::new();
        for (a,b) in aliases {
            if let Some(last) = joined.last_mut() && a <= last.1 { last.1 = last.1.max(b); }
            else { joined.push((a,b)); }
        }
        for (a,b) in joined {
            crate::watch::wrote(a,b-a);
            crate::descriptor::host::remapped(a,b-a);
            POOL_STATS[5].fetch_add(1, Relaxed);
        }
        return;
    }
    let current = ALLOCATIONS.lock().unwrap_or_else(|e| e.into_inner())
        .get(&backing.base).is_some_and(|a| a.backing.id == backing.id);
    if !current { return; }
    let mut spans = ranges.to_vec();
    spans.sort_unstable();
    let mut joined: Vec<(usize,usize)> = Vec::new();
    for (from,to) in spans {
        let from = from.max(backing.base);
        let to = to.min(backing.base + backing.bytes);
        if from >= to { continue; }
        if let Some(last) = joined.last_mut() && from <= last.1 {
            last.1 = last.1.max(to);
        } else { joined.push((from,to)); }
    }
    for (from,to) in joined {
        let pages: Vec<usize> = (from / 4096..to.div_ceil(4096)).collect();
        crate::watch::landed_beside(&pages, &[(from,to-from)]);
        STATS[6].fetch_add(1, Relaxed);
    }
}

// Separate budget: pool segments must not evict dedicated VUM1 allocations.
// Only the experiment creates committed pool segments and admits them.
static POOL_CHARGED: AtomicU64 = AtomicU64::new(0);
static POOL_STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
struct PoolEntry {
    section: usize,
    bytes: u64,
    backing: Option<Arc<Backing>>,
    refused: bool,
}
static POOLS: Mutex<BTreeMap<u64, PoolEntry>> = Mutex::new(BTreeMap::new());
static POOL_VIEWS: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());
static POOL_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn physical_experiment() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| enabled() && std::env::var_os("VICTORY_VUM_PHYSICAL").is_some_and(|v| v == "1"))
}

pub fn physical_budget() -> u64 {
    static CAP: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| std::env::var("VICTORY_VUM_PHYSICAL_MB").ok()
        .and_then(|v| v.parse::<u64>().ok()).map(|n| n.min(512) << 20).unwrap_or(128 << 20))
}

/// Register only committed segments deliberately created by XMem, not the
/// dedicated VUM1 allocations or the legacy multi-GiB reserved sections.
pub fn register_pool(section: usize, bytes: u64) {
    if !physical_experiment() { return; }
    let Some(generation) = crate::inplace::section_generation(section) else { return; };
    POOLS.lock().unwrap_or_else(|e| e.into_inner()).insert(generation,
        PoolEntry { section, bytes, backing: None, refused: false });
    POOL_COUNT.fetch_add(1, Relaxed);
    POOL_STATS[0].fetch_add(1, Relaxed);
}

pub(crate) fn pool_gone(generation: u64) {
    if POOL_COUNT.load(Relaxed) == 0 { return; }
    let gone = POOLS.lock().unwrap_or_else(|e| e.into_inner()).remove(&generation);
    if let Some(mut entry) = gone {
        POOL_COUNT.fetch_sub(1, Relaxed);
        if let Some(backing) = entry.backing.take() { crate::host::retire(backing); }
    }
}

fn pool_backing(start: usize, len: usize) -> Option<(Arc<Backing>, u64)> {
    if POOL_COUNT.load(Relaxed) == 0 || !physical_experiment() { return None; }
    let run = crate::inplace::physical_run(start,len)?;
    if !run.committed || run.bytes > MAX as u64 { return None; }
    let mut pools = POOLS.lock().unwrap_or_else(|e| e.into_inner());
    let entry = pools.get_mut(&run.generation)?;
    if entry.section != run.section || entry.bytes != run.bytes || entry.refused { return None; }
    let backing = if let Some(backing) = &entry.backing {
        POOL_STATS[3].fetch_add(1, Relaxed);
        backing.clone()
    } else {
        let host = crate::host::existing_host()?;
        POOL_STATS[1].fetch_add(1, Relaxed);
        POOL_CHARGED.fetch_update(Relaxed, Relaxed, |n| n.checked_add(run.bytes)
            .filter(|&v| v <= physical_budget())).ok()?;
        let native = crate::inplace::open(&host.device, crate::inplace::From::Section(run.section))
            .and_then(|heap| crate::inplace::place(&host.device, &heap, 0, run.bytes).map(|buffer| (heap,buffer)));
        let (heap, buffer) = match native {
            Ok(native) => native,
            Err(e) => {
                POOL_CHARGED.fetch_sub(run.bytes, Relaxed);
                entry.refused = true;
                POOL_STATS[2].fetch_add(1, Relaxed);
                tracing::warn!(error = %e, generation = run.generation, "VUM 2: native physical segment refused");
                return None;
            }
        };
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        let id = NEXT.fetch_add(1, Relaxed);
        let backing = Arc::new(Backing { id, base: 0, bytes: run.bytes as usize, heap,buffer,gpu,
            physical: Some((run.section,run.generation)) });
        entry.backing = Some(backing.clone());
        tracing::info!(id, generation = run.generation, bytes = run.bytes, "VUM 2: physical segment opened once");
        backing
    };
    drop(pools);
    let mut bound = POOL_VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    bound.entry(start).and_modify(|end| *end = (*end).max(start+len)).or_insert(start+len);
    POOL_STATS[4].fetch_add(1, Relaxed);
    Some((backing, run.offset))
}

/// Only used by the experimental direct pool path. Remove VA associations
/// on remap, but retain the physical backing itself and in-flight resources.
pub(crate) fn pool_remapping(at: usize,len: usize) -> bool {
    if !physical_experiment() || len == 0 { return false; }
    let end = at.saturating_add(len);
    let mut bound = POOL_VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    let old: Vec<usize> = bound.range(..end).filter(|(_,b)| **b > at).map(|(&a,_)| a).collect();
    if old.is_empty() { return false; }
    for a in old { bound.remove(&a); }
    if POOL_STATS[6].fetch_add(1, Relaxed) == 0 {
        tracing::warn!("VUM 2 experimental: a bound pool view was remapped; old recorded GPU addresses are not retargeted");
    }
    true
}

/// Immutable identity of a GPU binding, including its actual offset in
/// the native buffer. Never resolve a recorded GPU address from its VA
/// alone: a remap can keep the same resource but change the offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MappingTicket {
    pub(crate) section: usize,
    pub(crate) generation: u64,
    pub(crate) start: u64,
    pub(crate) len: u64,
    pub(crate) offset: u64,
}

impl MappingTicket {
    pub(crate) fn current(self) -> bool {
        crate::inplace::physical_run(self.start as usize,self.len as usize)
            .is_some_and(|r| r.section == self.section && r.generation == self.generation && r.offset == self.offset)
    }
    pub(crate) fn span(self, from: usize, to: usize) -> Option<PhysicalSpan> {
        let a = (from as u64).max(self.start);
        let b = (to as u64).min(self.start.checked_add(self.len)?);
        if a >= b { return None; }
        Some(PhysicalSpan { section:self.section,generation:self.generation,
            from:self.offset.checked_add(a-self.start)?, to:self.offset.checked_add(b-self.start)? })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PhysicalSpan {
    section: usize,
    generation: u64,
    from: u64,
    to: u64,
}
impl PhysicalSpan {
    pub(crate) fn aliases(self) -> Vec<(usize,usize)> {
        crate::inplace::physical_aliases(self.section,self.generation,self.from,self.to-self.from)
    }
}

pub(crate) fn join_physical(spans: &mut Vec<PhysicalSpan>) {
    spans.sort_unstable();
    let mut kept = 0;
    for i in 0..spans.len() {
        let next = spans[i];
        if kept != 0 && spans[kept-1].section == next.section
            && spans[kept-1].generation == next.generation && next.from <= spans[kept-1].to {
            spans[kept-1].to = spans[kept-1].to.max(next.to);
        } else {
            spans[kept] = next;
            kept += 1;
        }
    }
    spans.truncate(kept);
}

pub(crate) fn ticket(backing: &Backing,start: u64,len: u64,bias: u64) -> Option<MappingTicket> {
    let (section,generation) = backing.physical?;
    Some(MappingTicket { section,generation,start,len,offset:bias })
}

pub(crate) fn mapping_current(backing: &Backing, start: u64, len: u64, bias: u64) -> bool {
    ticket(backing,start,len,bias).is_none_or(MappingTicket::current)
}

// Exact capture from descriptor creation, not a second guest lookup.
// Only persistent heap descriptors use this capture. Dynamic slots are
// private to their recording; their retained shadows are validated below.
thread_local! {
    static CAPTURE: std::cell::Cell<Option<MappingTicket>> = const { std::cell::Cell::new(None) };
}
pub(crate) fn capture_begin() { CAPTURE.set(None); }
pub(crate) fn capture_ticket(ticket: Option<MappingTicket>) { CAPTURE.set(ticket); }
pub(crate) fn capture_end() -> Option<MappingTicket> { CAPTURE.take() }

static REMAP_STATS: [AtomicU64;4] = [const { AtomicU64::new(0) };4];
pub(crate) fn checked(n: usize) { REMAP_STATS[0].fetch_add(n as u64,Relaxed); }
pub(crate) fn rebound(n: usize) { REMAP_STATS[1].fetch_add(n as u64,Relaxed); }
pub(crate) fn alias_ranges(n: usize) { REMAP_STATS[2].fetch_add(n as u64,Relaxed); }
pub(crate) fn rejected() { REMAP_STATS[3].fetch_add(1,Relaxed); }

#[cfg(test)]
#[test]
fn physical_ranges_join_only_within_the_same_generation() {
    let t = MappingTicket { section:7,generation:3,start:100,len:100,offset:200 };
    let a = t.span(90,140).unwrap();
    let b = t.span(130,300).unwrap();
    let other = PhysicalSpan { generation:4,..a };
    assert!(t.span(200,201).is_none());
    let mut ranges = vec![other,b,a];
    join_physical(&mut ranges);
    assert_eq!(ranges, vec![PhysicalSpan { section:7,generation:3,from:200,to:300 },other]);
}

pub(crate) fn log(frames: f64) {
    virtual_address::log(frames);
    let n = STATS.each_ref().map(|v| v.swap(0, Relaxed) as f64 / frames);
    tracing::info!("VUM 1 per frame (VICTORY_VUM={}): {} live sections, {:.2} MiB live / {:.2} MiB charged; {:.2} alloc attempts, {:.2} admitted, {:.2} driver refusals, {:.2} logical views, {:.0} refresh steps omitted ({:.2} MiB requested), {:.0} GPU visibility notifications",
        u8::from(enabled()), COUNT.load(Relaxed), LIVE.load(Relaxed) as f64 / 1048576.0,
        CHARGED.load(Relaxed) as f64 / 1048576.0, n[0],n[1],n[2],n[3],n[4],n[5]/1048576.0,n[6]);
    let p = POOL_STATS.each_ref().map(|n| n.swap(0,Relaxed) as f64 / frames);
    tracing::info!("VUM 2 physical (VICTORY_VUM_PHYSICAL={}): {} committed candidate segments, {:.2} MiB native charged; per frame {:.2} registered, {:.2} opens, {:.2} refusals, {:.2} backing reuses, {:.2} binds, {:.2} alias publications, {:.2} bound remaps",
        u8::from(physical_experiment()), POOL_COUNT.load(Relaxed), POOL_CHARGED.load(Relaxed) as f64/1048576.0,
        p[0],p[1],p[2],p[3],p[4],p[5],p[6]);
    let q = REMAP_STATS.each_ref().map(|n| n.swap(0,Relaxed) as f64 / frames);
    tracing::info!("VUM 2 mapping validation per frame: {:.0} tickets checked, {:.0} persistent descriptors rebound, {:.0} projected dependency ranges, {:.0} rejected stale recordings",
        q[0],q[1],q[2],q[3]);
}
