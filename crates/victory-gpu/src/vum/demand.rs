//! Experimental VUM demand paging for dedicated, non-imported allocations.
//! Guest memory and a private writable alias refer to one section. Only
//! the guest view is protected; the alias lets us publish the complete
//! GPU page before restoring access, never expose a half-copied page.
//! Ordinary private memory and GPU-imported pools are excluded. The
//! physical mode uses the existing section registry and all guest aliases.
//!
//! The relay may capture pages only at a completed native-queue boundary
//! with the submission lock held. An immutable local snapshot outlives
//! the guest-visible fence. A separate copy queue materializes a page;
//! it never waits on the submitting thread, relay, or guest fences.
//! The existing guard handles CPU writes after materialization.
//! This is an opt-in pilot; arbitrary kernel reads/protection changes are
//! not mediated by VEH. Do not select allocations used as OS I/O buffers.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, CloseHandle};
use windows::core::Interface;
use windows_sys::Win32::System::Memory as mem;

use crate::host::Host;
use crate::shadow::Shadow;

const PAGE: usize = 64 << 10;
const MIN: usize = 1 << 20;
const ALLOC_LIMIT: usize = 64 << 20;
const SNAP_LIMIT: u64 = 16 << 20;
const BATCH_PAGES: usize = 64;

fn compatible() -> bool {
    !crate::vum::physical_experiment() && crate::inplace::level() == 0
}

pub(crate) fn resident_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VUM_RESIDENT").is_some_and(|v| v == "1")) && compatible()
}

fn dedicated_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    !resident_on() && *ON.get_or_init(|| std::env::var_os("VICTORY_VUM_DEMAND").is_some_and(|v| v == "1")) && compatible()
}

pub(crate) fn legacy_physical_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    !resident_on() && *ON.get_or_init(|| std::env::var_os("VICTORY_VUM_PHYSICAL_DEMAND").is_some_and(|v| v == "1")) && compatible()
}

pub(crate) fn physical_on() -> bool { resident_on() || legacy_physical_on() }

pub(crate) fn enabled() -> bool { dedicated_on() || physical_on() }

// Exact XMem attribute class must be selected by the operator. Absence of
// the selector admits no memory: an unobserved CPU read is not proof.
fn selected(attributes: u64) -> bool {
    static ATTR: OnceLock<Option<u64>> = OnceLock::new();
    ATTR.get_or_init(|| std::env::var("VICTORY_VUM_DEMAND_ATTRIBUTES").ok()
        .and_then(|v| u64::from_str_radix(v.trim().trim_start_matches("0x").trim_start_matches("0X"),16).ok()))
        .is_some_and(|a| a == attributes)
}

static PENDING: AtomicU64 = AtomicU64::new(0);
static SNAP_BYTES: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static STATS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
// admissions, bytes captured, CPU faults, explicit publications,
// materialized bytes, same-home refreshes, allocation candidates, refused.

pub(crate) fn pending() -> bool { PENDING.load(Ordering::Acquire) != 0 }

static NEXT_ALLOCATION: AtomicU64 = AtomicU64::new(1);
static NEXT_ACCESS: AtomicU64 = AtomicU64::new(1);
struct Allocation { id: u64, bytes: usize, alias: usize }

/// Windows handles are reusable; section generation is the identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct PhysicalKey { generation: u64, offset: u64 }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageIdentity { Dedicated(u64), Physical(PhysicalKey) }

enum Source {
    Snapshot(Arc<Snapshot>),
    // Native Shadow already owns this device-local allocation. No second
    // GPU copy and no stale CPU mirror used as source of a GPU refresh.
    Resident,
}

struct Page {
    source: Source,
    offset: u64,
    shadow: Arc<Shadow>,
    mirror: usize,
    alias: usize,
    fence: u64,
    identity: PageIdentity,
    views: Vec<usize>,
    _armed: Option<Armed>,
}
#[derive(Clone, Copy)]
enum AccessRange { Virtual(usize,usize), Physical(u64,u64,u64) }
#[derive(Default)]
struct State {
    allocations: BTreeMap<usize, Allocation>,
    active: BTreeMap<u64, AccessRange>,
    pages: BTreeMap<usize, Page>,
    // One entry per physical page, regardless of how many guest addresses
    // show it. Virtual aliases index the same live publication.
    physical: BTreeMap<PhysicalKey, usize>,
    aliases: BTreeMap<usize, usize>,
    // A real CPU access makes that physical page a poor lazy candidate.
    // Fixed upper bound; entries expire after 120 title frames.
    hot: BTreeMap<PhysicalKey, u64>,
    engine: Option<Engine>,
    resident_engine: Option<Engine>,
}
static STATE: Mutex<Option<State>> = Mutex::new(None);

fn owner(state:&State, at:usize, len:usize) -> Option<(u64,usize)> {
    let (&base,a) = state.allocations.range(..=at).next_back()?;
    // The first page may contain the allocator header; the last incomplete
    // page is never selected by the caller's exact Shadow bounds.
    (at >= base.checked_add(PAGE)? && at.checked_add(len)? <= base.checked_add(a.bytes)?)
        .then(|| (a.id, a.alias + (at-base)))
}

pub fn allocate(size:usize, attributes:u64) -> Option<usize> {
    if !dedicated_on() || !(MIN..=ALLOC_LIMIT).contains(&size) { return None; }
    let n=STATS[6].fetch_add(1,Ordering::Relaxed)+1;
    if n <= 16 || n.is_power_of_two() {
        tracing::info!(size,attributes=format_args!("{attributes:#x}"),"VUM demand allocation candidate");
    }
    if !selected(attributes) { return None; }
    crate::host::existing_host()?;
    let bytes=size.checked_next_multiple_of(PAGE)?;
    if bytes > ALLOC_LIMIT { return None; }
    ALLOC_BYTES.fetch_update(Ordering::AcqRel,Ordering::Acquire,|v| v.checked_add(bytes as u64).filter(|&v| v<=ALLOC_LIMIT as u64)).ok()?;
    let made=unsafe {
        let handle=mem::CreateFileMappingW(windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE,
            std::ptr::null(), mem::PAGE_READWRITE | mem::SEC_COMMIT,0,bytes as u32,std::ptr::null());
        if handle.is_null() { None } else {
            let guest=mem::MapViewOfFile(handle,mem::FILE_MAP_WRITE,0,0,bytes);
            let alias=mem::MapViewOfFile(handle,mem::FILE_MAP_WRITE,0,0,bytes);
            let _=windows_sys::Win32::Foundation::CloseHandle(handle);
            if guest.Value.is_null() || alias.Value.is_null() {
                if !guest.Value.is_null() { mem::UnmapViewOfFile(guest); }
                if !alias.Value.is_null() { mem::UnmapViewOfFile(alias); }
                None
            } else { Some((guest.Value as usize,alias.Value as usize)) }
        }
    };
    let Some((base,alias))=made else {
        ALLOC_BYTES.fetch_sub(bytes as u64,Ordering::AcqRel);
        return None;
    };
    // Not registered as an importable physical section. No direct GPU
    // backing or extra guest alias can be created through VUM pool APIs.
    STATE.lock().unwrap_or_else(|e|e.into_inner()).get_or_insert_with(State::default)
        .allocations.insert(base,Allocation{id:NEXT_ALLOCATION.fetch_add(1,Ordering::Relaxed),bytes,alias});
    crate::watch::title_memory_allocated(base,bytes);
    crate::watch::install();
    tracing::info!(base=format_args!("{base:#x}"),bytes,attributes=format_args!("{attributes:#x}"),
        "VUM demand managed allocation (experimental)");
    Some(base)
}

pub fn release(base:usize) -> bool {
    let bytes={
        let state=STATE.lock().unwrap_or_else(|e|e.into_inner());
        let Some(a)=state.as_ref().and_then(|s|s.allocations.get(&base)) else { return false; };
        a.bytes
    };
    let _mapping=crate::watch::title_memory_remapping();
    let a={
        let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
        let s=state.as_mut().expect("managed allocation");
        resolve_locked(s,base,bytes,None);
        s.allocations.remove(&base).expect("managed allocation")
    };
    crate::watch::title_memory_freed(base,bytes);
    crate::watch::title_memory_unmapping(base,bytes);
    let guest=unsafe { mem::UnmapViewOfFile(mem::MEMORY_MAPPED_VIEW_ADDRESS{Value:base as *mut _}) };
    let alias=unsafe { mem::UnmapViewOfFile(mem::MEMORY_MAPPED_VIEW_ADDRESS{Value:a.alias as *mut _}) };
    if guest==0 || alias==0 { crate::host::fail(format_args!("VUM demand managed allocation could not be unmapped")); }
    ALLOC_BYTES.fetch_sub(bytes as u64,Ordering::AcqRel);
    true
}

struct Event(HANDLE);
impl Drop for Event { fn drop(&mut self) { unsafe { let _=CloseHandle(self.0); } } }

struct Snapshot { buffer: ID3D12Resource, bytes:u64 }
impl Drop for Snapshot {
    fn drop(&mut self) { SNAP_BYTES.fetch_sub(self.bytes,Ordering::AcqRel); }
}
type Piece=(Arc<Shadow>,usize,usize,u64);

fn capture(host:&Host, queue:&ID3D12CommandQueue, pieces:&[(Arc<Shadow>,usize)], bytes:u64) -> Option<Arc<Snapshot>> {
    SNAP_BYTES.fetch_update(Ordering::AcqRel,Ordering::Acquire,|v| v.checked_add(bytes).filter(|&v|v<=SNAP_LIMIT)).ok()?;
    let Some(buffer)=host.uav_buffer(bytes) else { SNAP_BYTES.fetch_sub(bytes,Ordering::AcqRel); return None; };
    let snapshot=Arc::new(Snapshot{buffer,bytes});
    // SAFETY: dedicated native objects; sources are alive and their
    // producers have completed. A command list is never reused in flight.
    unsafe {
        let alloc:ID3D12CommandAllocator=host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).ok()?;
        let list:ID3D12GraphicsCommandList=host.device.CreateCommandList(0,D3D12_COMMAND_LIST_TYPE_DIRECT,&alloc,None).ok()?;
        let fence:ID3D12Fence=host.device.CreateFence(0,D3D12_FENCE_FLAG_NONE).ok()?;
        let event=Event(CreateEventW(None,false,false,None).ok()?);
        for (i,(shadow,lo)) in pieces.iter().enumerate() {
            list.CopyBufferRegion(&snapshot.buffer,(i*PAGE) as u64,&shadow.buffer,*lo as u64,PAGE as u64);
        }
        list.Close().ok()?;
        let native=list.cast::<ID3D12CommandList>().ok()?;
        queue.ExecuteCommandLists(&[Some(native)]);
        // Once submitted, errors are fatal rather than releasing in-flight
        // storage or publishing an uninitialized snapshot.
        if queue.Signal(&fence,1).and_then(|_|fence.SetEventOnCompletion(1,event.0)).is_err()
            || WaitForSingleObject(event.0,INFINITE)!=WAIT_OBJECT_0
            || host.device.GetDeviceRemovedReason().is_err()
        { crate::host::fail(format_args!("VUM demand snapshot did not complete")); }
    }
    Some(snapshot)
}

struct Engine {
    device:ID3D12Device,
    queue:ID3D12CommandQueue,
    allocator:ID3D12CommandAllocator,
    list:ID3D12GraphicsCommandList,
    fence:ID3D12Fence,
    event:Event,
    buffer:ID3D12Resource,
    sent:u64,
}
// All native calls are serialized by STATE; event is a private kernel handle.
unsafe impl Send for Engine {}

impl Engine {
    fn new(host:&Host)->Option<Self> { Self::with_queue(host,None) }

    fn with_queue(host:&Host,direct:Option<&ID3D12CommandQueue>)->Option<Self> {
        let buffer=host.read_back_buffer(PAGE as u64)?;
        let ty=if direct.is_some() { D3D12_COMMAND_LIST_TYPE_DIRECT } else { D3D12_COMMAND_LIST_TYPE_COPY };
        unsafe {
            let allocator:ID3D12CommandAllocator=host.device.CreateCommandAllocator(ty).ok()?;
            let list:ID3D12GraphicsCommandList=host.device.CreateCommandList(0,ty,&allocator,None).ok()?;
            list.Close().ok()?;
            let queue:ID3D12CommandQueue=match direct {
                Some(queue)=>queue.clone(),
                None=>host.device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC{
                    Type:ty,..Default::default()
                }).ok()?,
            };
            let fence:ID3D12Fence=host.device.CreateFence(0,D3D12_FENCE_FLAG_NONE).ok()?;
            let event=Event(CreateEventW(None,false,false,None).ok()?);
            Some(Self{device:host.device.clone(),queue,allocator,list,fence,event,buffer,sent:0})
        }
    }
    /// Snapshot is complete and immutable. No lock of the normal relay,
    /// submission lane, guards, or resource registry is acquired here.
    fn publish(&mut self,page:&Page)->windows::core::Result<()> {
        // A CPU access can revoke a newly armed reservation before the
        // new writer has been sent. The old CPU bytes are still current.
        if matches!(&page.source,Source::Resident) && page.fence==0 {
            return Self::restore(page);
        }
        self.sent+=1;
        unsafe {
            self.allocator.Reset()?;
            self.list.Reset(&self.allocator,None)?;
            let source=match &page.source {
                Source::Snapshot(s)=>&s.buffer,
                Source::Resident=>&page.shadow.buffer,
            };
            self.list.CopyBufferRegion(&self.buffer,0,source,page.offset,PAGE as u64);
            self.list.Close()?;
            self.queue.ExecuteCommandLists(&[Some(self.list.cast::<ID3D12CommandList>()?)]);
            self.queue.Signal(&self.fence,self.sent)?;
            self.fence.SetEventOnCompletion(self.sent,self.event.0)?;
            if WaitForSingleObject(self.event.0,INFINITE)!=WAIT_OBJECT_0 {
                return Err(windows::core::Error::from_thread());
            }
            self.device.GetDeviceRemovedReason()?;
            let mut data=std::ptr::null_mut();
            self.buffer.Map(0,None,Some(&mut data))?;
            if data.is_null() { return Err(windows::core::Error::from_thread()); }
            // The guest view remains inaccessible until both the private
            // alias and the reference mirror hold the complete GPU version.
            std::ptr::copy_nonoverlapping(data.cast::<u8>(),page.alias as *mut u8,PAGE);
            std::ptr::copy_nonoverlapping(data.cast::<u8>(),page.mirror as *mut u8,PAGE);
            self.buffer.Unmap(0,None);
        }
        Self::restore(page)
    }

    fn restore(page:&Page)->windows::core::Result<()> {
        // Publish bytes through the hidden view first; every guest
        // alias then sees the same complete physical page.
        for &at in &page.views {
            let mut old=0;
            if unsafe { mem::VirtualProtect(at as *const _,PAGE,mem::PAGE_READONLY,&mut old) }==0 || old!=mem::PAGE_NOACCESS {
                return Err(windows::core::Error::from_thread());
            }
        }
        Ok(())
    }
}

fn virtual_keys(s:&State,at:usize,len:usize,except:Option<u64>)->Vec<usize> {
    if len==0 { return Vec::new(); }
    let mut keys:Vec<_>=s.aliases.range(at/PAGE*PAGE..at.saturating_add(len))
        .filter_map(|(_, &key)| s.pages.get(&key).filter(|p|except!=Some(p.shadow.id())).map(|_|key)).collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}
fn physical_keys(s:&State,generation:u64,from:u64,len:u64)->Vec<usize> {
    if len==0 { return Vec::new(); }
    let end=from.saturating_add(len);
    let from=from/PAGE as u64*PAGE as u64;
    s.physical.range(PhysicalKey{generation,offset:from}..PhysicalKey{generation,offset:end})
        .map(|(_, &key)|key).collect()
}
fn mark_hot(s:&mut State,key:PhysicalKey) {
    if s.hot.len()>=4096 && !s.hot.contains_key(&key) { s.hot.clear(); }
    s.hot.insert(key,crate::frame::current() as u64);
}
fn materialize(s:&mut State,keys:Vec<usize>,cpu:bool)->usize {
    let n=keys.len();
    for at in keys {
        let page=s.pages.remove(&at).expect("pending page");
        let resident=matches!(&page.source,Source::Resident);
        let engine=if resident { &mut s.resident_engine } else { &mut s.engine };
        if let Err(e)=engine.as_mut().expect("demand engine").publish(&page) {
            crate::host::fail(format_args!("VUM demand publication failed at {at:#x}, producer {}: {e}",page.fence));
        }
        for view in &page.views { s.aliases.remove(view); }
        let copied=!resident || page.fence!=0;
        if resident {
            if copied { RESIDENT_STATS[3].fetch_add(PAGE as u64,Ordering::Relaxed); }
            else { REVOKED.fetch_add(1,Ordering::Relaxed); }
            if cpu { RESIDENT_STATS[4].fetch_add(1,Ordering::Relaxed); }
        }
        if let PageIdentity::Physical(key)=page.identity {
            s.physical.remove(&key);
            PHYSICAL_STATS[3].fetch_add(1,Ordering::Relaxed);
            if cpu { mark_hot(s,key); }
        }
        PENDING.fetch_sub(1,Ordering::AcqRel);
        if copied { STATS[4].fetch_add(PAGE as u64,Ordering::Relaxed); }
    }
    n
}
fn resolve_locked(s:&mut State, at:usize,len:usize, except:Option<u64>) -> usize {
    let keys=virtual_keys(s,at,len,except);
    materialize(s,keys,false)
}

pub(crate) fn resolve_range(at:usize,len:usize) {
    if !might_need(at,len) { return; }
    let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
    if let Some(s)=state.as_mut() {
        STATS[3].fetch_add(resolve_locked(s,at,len,None) as u64,Ordering::Relaxed);
    }
}
/// Known kernel reads/copies are leased for the whole call, not merely
/// checked before it. Admission tests these ranges under the same mutex,
/// so a page cannot become inaccessible in the middle of a kernel read.
const STRIPES:usize=4096;
const SHIFT:usize=20;
static ACTIVE_STRIPES:[AtomicU64;STRIPES]=[const{AtomicU64::new(0)};STRIPES];
static ARM_STRIPES:[AtomicU64;STRIPES]=[const{AtomicU64::new(0)};STRIPES];
static BIG_ACTIVE:AtomicU64=AtomicU64::new(0);
static ARM_COUNT:AtomicU64=AtomicU64::new(0);
fn stripe(at:usize)->usize { (at>>SHIFT)%STRIPES }
fn stripe_bounds(at:usize,len:usize)->(usize,usize) {
    (at>>SHIFT, at.saturating_add(len.saturating_sub(1))>>SHIFT)
}
struct StripeLease { first:usize,last:usize,big:bool }
impl StripeLease {
    fn new(at:usize,len:usize)->Self {
        let (first,last)=stripe_bounds(at,len);
        let big=last-first>=32;
        if big { BIG_ACTIVE.fetch_add(1,Ordering::SeqCst); }
        else { for n in first..=last { ACTIVE_STRIPES[n%STRIPES].fetch_add(1,Ordering::SeqCst); } }
        Self{first,last,big}
    }
}
impl Drop for StripeLease {
    fn drop(&mut self) {
        if self.big { BIG_ACTIVE.fetch_sub(1,Ordering::SeqCst); }
        else { for n in self.first..=self.last { ACTIVE_STRIPES[n%STRIPES].fetch_sub(1,Ordering::SeqCst); } }
    }
}
struct Armed(Vec<usize>);
impl Armed {
    fn new(views:&[usize])->Self {
        let mut stripes:Vec<_>=views.iter().map(|&v|stripe(v)).collect();
        stripes.sort_unstable();stripes.dedup();
        ARM_COUNT.fetch_add(1,Ordering::SeqCst);
        for &n in &stripes { ARM_STRIPES[n].fetch_add(1,Ordering::SeqCst); }
        Self(stripes)
    }
    fn busy(&self)->bool {
        BIG_ACTIVE.load(Ordering::SeqCst)!=0 ||
            self.0.iter().any(|&n|ACTIVE_STRIPES[n].load(Ordering::SeqCst)!=0)
    }
}
impl Drop for Armed {
    fn drop(&mut self) {
        for &n in &self.0 { ARM_STRIPES[n].fetch_sub(1,Ordering::SeqCst); }
        ARM_COUNT.fetch_sub(1,Ordering::SeqCst);
    }
}
fn possibly_resident(at:usize,len:usize)->bool {
    if len==0 { return false; }
    let (a,b)=stripe_bounds(at,len);
    if b-a>=32 { return ARM_COUNT.load(Ordering::SeqCst)!=0; }
    (a..=b).any(|n|ARM_STRIPES[n%STRIPES].load(Ordering::SeqCst)!=0)
}
fn might_need(at:usize,len:usize)->bool {
    if resident_on() { possibly_resident(at,len) } else { pending() && len!=0 }
}

pub struct CpuAccess(u64,Option<StripeLease>);
impl Drop for CpuAccess {
    fn drop(&mut self) {
        if self.0 != 0 {
            if let Some(s)=STATE.lock().unwrap_or_else(|e|e.into_inner()).as_mut() {
                s.active.remove(&self.0);
            }
        }
    }
}
pub(crate) fn cpu_access(at:usize,len:usize)->CpuAccess {
    if len==0 { return CpuAccess(0,None); }
    if resident_on() {
        let lease=StripeLease::new(at,len);
        if possibly_resident(at,len) {
            let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
            if let Some(s)=state.as_mut() {
                let keys=virtual_keys(s,at,len,None);
                STATS[3].fetch_add(materialize(s,keys,true) as u64,Ordering::Relaxed);
            }
        }
        return CpuAccess(0,Some(lease));
    }
    if !physical_on() && ALLOC_BYTES.load(Ordering::Acquire)==0 { return CpuAccess(0,None); }
    let end=at.saturating_add(len);
    let mapped=physical_on() && crate::inplace::has_title_view(at,len);
    if !mapped && ALLOC_BYTES.load(Ordering::Acquire)==0 { return CpuAccess(0,None); }
    let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
    let s=state.get_or_insert_with(State::default);
    if !mapped && !s.allocations.range(..end).next_back().is_some_and(|(&base,a)|base.saturating_add(a.bytes)>at) {
        return CpuAccess(0,None);
    }
    let keys=virtual_keys(s,at,len,None);
    STATS[3].fetch_add(materialize(s,keys,true) as u64,Ordering::Relaxed);
    let id=NEXT_ACCESS.fetch_add(1,Ordering::Relaxed);
    s.active.insert(id,AccessRange::Virtual(at,end));
    CpuAccess(id,None)
}

pub(crate) fn resident_cpu_access(at:usize,len:usize)->CpuAccess {
    if resident_on() { cpu_access(at,len) } else { CpuAccess(0,None) }
}

/// DirectStorage uses its own writable section view, not a guest alias.
/// Lease by physical identity, including pages that have no guest view.
pub fn physical_access(section:usize,offset:u64,len:usize)->CpuAccess {
    if !physical_on() || len==0 { return CpuAccess(0,None); }
    let Some(generation)=crate::inplace::section_generation(section) else { return CpuAccess(0,None); };
    let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
    let s=state.get_or_insert_with(State::default);
    let keys=physical_keys(s,generation,offset,len as u64);
    STATS[3].fetch_add(materialize(s,keys,true) as u64,Ordering::Relaxed);
    let id=NEXT_ACCESS.fetch_add(1,Ordering::Relaxed);
    s.active.insert(id,AccessRange::Physical(generation,offset,offset.saturating_add(len as u64)));
    CpuAccess(id,None)
}

/// Called before MapViewOfFile3: a new unprotected alias must not expose
/// stale RAM. The mapping writer excludes new demand admissions.
pub fn before_physical_map(section:usize,offset:u64,len:usize) {
    if !pending() || !physical_on() { return; }
    let Some(generation)=crate::inplace::section_generation(section) else { return; };
    let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
    if let Some(s)=state.as_mut() {
        let keys=physical_keys(s,generation,offset,len as u64);
        PHYSICAL_STATS[4].fetch_add(materialize(s,keys,false) as u64,Ordering::Relaxed);
    }
}

pub(crate) fn section_gone(section:usize) {
    if !pending() || !physical_on() { return; }
    before_physical_map(section,0,usize::MAX);
}
pub(crate) fn resolve_all() { resolve_range(0,usize::MAX); }
pub(crate) fn resolve_mapping(at:usize,len:usize) {
    if len==0 { resolve_all(); } else { resolve_range(at,len); }
}
pub(crate) fn resolve_foreign(id:u64,at:usize,len:usize) {
    if !might_need(at,len) { return; }
    let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
    if let Some(s)=state.as_mut() {
        let mut keys:Vec<_>=s.aliases.range(at/PAGE*PAGE..at.saturating_add(len)).filter_map(|(&a,&primary)| {
            (a!=primary || s.pages[&primary].shadow.id()!=id).then_some(primary)
        }).collect();
        keys.sort_unstable(); keys.dedup();
        STATS[3].fetch_add(materialize(s,keys,false) as u64,Ordering::Relaxed);
    }
}

pub(crate) fn fault(at:usize,write:bool)->bool {
    if !enabled() { return false; }
    let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
    let Some(s)=state.as_mut() else { return false; };
    let keys=virtual_keys(s,at,1,None);
    if materialize(s,keys,true)!=0 {
        STATS[2].fetch_add(1,Ordering::Relaxed);
        return true;
    }
    // A second fault can have been delivered before another thread
    // finished publication. For a write to a restored read-only page,
    // let the normal write-guard handler open it instead.
    let dedicated=owner(s,at,1).is_some();
    drop(state);
    if !dedicated && (!physical_on() || !crate::inplace::has_title_view(at,1)) { return false; }
    let mut info:mem::MEMORY_BASIC_INFORMATION=unsafe { std::mem::zeroed() };
    let n=unsafe { mem::VirtualQuery(at as *const _,&mut info,std::mem::size_of_val(&info)) };
    n!=0 && info.State==mem::MEM_COMMIT && (info.Protect==mem::PAGE_READWRITE || !write && info.Protect==mem::PAGE_READONLY)
}

fn gaps(from:usize,to:usize,owned:impl Iterator<Item=usize>)->Vec<(usize,usize)> {
    let mut at=from;
    let mut out=Vec::new();
    for page in owned {
        if page>at { out.push((at,page.min(to))); }
        at=at.max(page.saturating_add(PAGE));
    }
    if at<to { out.push((at,to)); }
    out
}
pub(crate) fn refresh_covered(id:u64,from:usize,to:usize)->Option<bool> {
    if from>=to || !might_need(from,to-from) { return None; }
    let state=STATE.lock().unwrap_or_else(|e|e.into_inner());
    let s=state.as_ref()?;
    let mut pages=s.aliases.range(from/PAGE*PAGE..to).peekable();
    pages.peek()?;
    let mut at=from;
    for (&a,&primary) in pages {
        if a>at || a!=primary || s.pages[&primary].shadow.id()!=id { return Some(false); }
        at=at.max(a+PAGE);
    }
    let all=at>=to;
    if all { STATS[5].fetch_add(1,Ordering::Relaxed); }
    Some(all)
}
pub(crate) fn refresh_gaps(id:u64,from:usize,to:usize)->Option<Vec<(usize,usize)>> {
    if from>=to || !might_need(from,to-from) { return None; }
    let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
    let s=state.as_mut()?;
    let first=from/PAGE*PAGE;
    if s.aliases.range(first..to).next().is_none() { return None; }
    // Only the original native allocation can skip a RAM refresh. Another
    // GPU Shadow, including one for an alias, must first publish.
    let mut keys:Vec<_>=s.aliases.range(first..to).filter_map(|(&a,&primary)| {
        (a!=primary || s.pages[&primary].shadow.id()!=id).then_some(primary)
    }).collect();
    keys.sort_unstable(); keys.dedup();
    STATS[3].fetch_add(materialize(s,keys,false) as u64,Ordering::Relaxed);
    let out=gaps(from,to,s.aliases.range(first..to).map(|(&a,_)|a));
    STATS[5].fetch_add(1,Ordering::Relaxed);
    Some(out)
}

static PHYSICAL_STATS:[AtomicU64;7]=[const{AtomicU64::new(0)};7];
static GUARD_REFUSALS:AtomicU64=AtomicU64::new(0);
// candidate pages, admitted physical pages, hot refusals, materialized,
// published before mapping, missed submission boundary, other refusals.
pub(crate) fn boundary_missed() {
    if physical_on() { PHYSICAL_STATS[5].fetch_add(1,Ordering::Relaxed); }
}
fn active(s:&State,identity:PageIdentity,views:&[usize])->bool {
    s.active.values().any(|access|match *access {
        AccessRange::Virtual(a,b)=>views.iter().any(|&v|a<v+PAGE&&v<b),
        AccessRange::Physical(g,a,b)=>matches!(identity,PageIdentity::Physical(k)
            if k.generation==g && a<k.offset+PAGE as u64 && k.offset<b),
    })
}
struct Candidate {
    i:usize, shadow:Arc<Shadow>, lo:usize, mirror:usize,
    identity:PageIdentity, views:Vec<usize>,
}
fn admit(c:Candidate,source:Source,offset:u64,fence:u64,after:u64)->bool {
    let resident=matches!(&source,Source::Resident);
    let at=c.shadow.span().0 as usize+c.lo;
    // The caller holds MAPPING's read lock, so the complete alias list and
    // section generation cannot change until admission and journal finish.
    let physical_alias=match c.identity {
        PageIdentity::Physical(_)=>crate::inplace::beside(at,PAGE),
        PageIdentity::Dedicated(_)=>None,
    };
    let admitted=crate::watch::demand_guard_aliases(at,&c.views,|| {
        let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
        let Some(s)=state.as_mut() else { return false; };
        let alias=match c.identity {
            PageIdentity::Dedicated(allocation)=>{
                let Some((id,alias))=owner(s,at,PAGE) else { return false; };
                if id!=allocation { return false; }
                alias
            }
            PageIdentity::Physical(key)=>{
                if s.physical.contains_key(&key) { return false; }
                let Some(alias)=physical_alias else { return false; };
                alias
            }
        };
        let armed=resident.then(||Armed::new(&c.views));
        if if resident { s.resident_engine.is_none() } else { s.engine.is_none() } { return false; }
        if c.views.iter().any(|v|s.aliases.contains_key(v)) || active(s,c.identity,&c.views)
            || armed.as_ref().is_some_and(Armed::busy)
            || (resident && s.pages.len()>=1024)
            || c.shadow.demand_mirror(c.lo,c.lo+PAGE,after)!=Some(c.mirror)
        { return false; }
        for &v in &c.views {
            let mut info:mem::MEMORY_BASIC_INFORMATION=unsafe{std::mem::zeroed()};
            let n=unsafe{mem::VirtualQuery(v as *const _,&mut info,std::mem::size_of_val(&info))};
            if n==0 || info.State!=mem::MEM_COMMIT || info.Type!=mem::MEM_MAPPED || info.Protect!=mem::PAGE_READONLY
                || (info.BaseAddress as usize).saturating_add(info.RegionSize)<v+PAGE { return false; }
        }
        // Every guest alias is guarded, and both virtual and hidden
        // physical accesses are leased. The reference must match all bytes.
        let same=unsafe { std::slice::from_raw_parts(alias as *const u8,PAGE)==std::slice::from_raw_parts(c.mirror as *const u8,PAGE) };
        if !same { return false; }
        PENDING.fetch_add(1,Ordering::AcqRel);
        let mut protected=Vec::new();
        for &v in &c.views {
            let mut old=0;
            let changed=unsafe{mem::VirtualProtect(v as *const _,PAGE,mem::PAGE_NOACCESS,&mut old)}!=0;
            if !changed || old!=mem::PAGE_READONLY {
                if changed && unsafe{mem::VirtualProtect(v as *const _,PAGE,old,&mut old)}==0 {
                    crate::host::fail(format_args!("VUM demand alias rollback failed"));
                }
                for v in protected {
                    if unsafe{mem::VirtualProtect(v as *const _,PAGE,mem::PAGE_READONLY,&mut old)}==0 {
                        crate::host::fail(format_args!("VUM demand alias rollback failed"));
                    }
                }
                PENDING.fetch_sub(1,Ordering::AcqRel);
                return false;
            }
            protected.push(v);
        }
        for &v in &c.views { s.aliases.insert(v,at); }
        if let PageIdentity::Physical(key)=c.identity {
            s.physical.insert(key,at);
            PHYSICAL_STATS[1].fetch_add(1,Ordering::Relaxed);
        }
        s.pages.insert(at,Page{source,offset,shadow:c.shadow.clone(),mirror:c.mirror,alias,fence,
            identity:c.identity,views:c.views.clone(),_armed:armed});
        true
    });
    if admitted && !resident {
        let pages:Vec<usize>=(at/4096..(at+PAGE)/4096).collect();
        crate::watch::landed_beside(&pages,&[(at,PAGE)]);
    }
    admitted
}

pub(crate) fn defer(host:&Host, queue:&ID3D12CommandQueue,pieces:&mut Vec<Piece>,fence:u64,after:u64,other:&[(Arc<Shadow>,Vec<(usize,usize)>)]) {
    if (!physical_on() && ALLOC_BYTES.load(Ordering::Acquire)==0) || pieces.is_empty() { return; }
    // Never block the relay behind a mapping writer. Holding this through
    // admission pins the section and every alias, not just one address.
    let Some(_mapping)=crate::watch::title_memory_reading_now() else { boundary_missed(); return; };
    let mut candidates=Vec::new();
    {
        let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
        let s=state.get_or_insert_with(State::default);
        if s.engine.is_none() { s.engine=Engine::new(host); }
        if s.engine.is_none() { return; }
        for (i,(shadow,lo,hi,_)) in pieces.iter().enumerate() {
            let at=shadow.span().0 as usize+*lo;
            let Some(mirror)=shadow.demand_mirror(*lo,*hi,after) else { continue; };
            let (identity,views)=if let Some((allocation,_))=owner(s,at,PAGE) {
                (PageIdentity::Dedicated(allocation),vec![at])
            } else if physical_on() {
                let Some(run)=crate::inplace::physical_run(at,PAGE) else { continue; };
                // SEC_COMMIT sections include VUM1's imported GPU heaps.
                // Admit only the original SEC_RESERVE physical pools.
                if run.committed || run.offset % PAGE as u64 != 0 { continue; }
                let key=PhysicalKey{generation:run.generation,offset:run.offset};
                let mut aliases=crate::inplace::pages_of(at);
                aliases.sort_unstable(); aliases.dedup();
                // Do not truncate an alias set, or protect unknown views.
                if aliases.is_empty() || aliases.len()>crate::watch::MANY_VIEWS || !aliases.contains(&at) { continue; }
                PHYSICAL_STATS[0].fetch_add(1,Ordering::Relaxed);
                if s.hot.get(&key).is_some_and(|&f| (crate::frame::current() as u64).saturating_sub(f)<120) {
                    PHYSICAL_STATS[2].fetch_add(1,Ordering::Relaxed); continue;
                }
                if s.physical.contains_key(&key) { continue; }
                (PageIdentity::Physical(key),aliases)
            } else { continue; };
            if views.iter().any(|v|s.aliases.contains_key(v)) || active(s,identity,&views) { continue; }
            // Detect producers through all guest aliases, not just the
            // virtual address which was used by the winning Shadow.
            if views.iter().any(|&v|other.iter().any(|(s,ranges)|s.id()!=shadow.id()
                    && ranges.iter().any(|&(a,b)|a<v+PAGE&&v<b))
                || pieces.iter().enumerate().any(|(j,(other,a,b,_))|j!=i
                    && (other.span().0 as usize+*a)<v+PAGE && v<other.span().0 as usize+*b))
            { PHYSICAL_STATS[6].fetch_add(1,Ordering::Relaxed); continue; }
            candidates.push(Candidate{i,shadow:shadow.clone(),lo:*lo,mirror,identity,views});
            if candidates.len()>=BATCH_PAGES { break; }
        }
    }
    // Avoid allocating/copying a snapshot for pages already known not to
    // have a complete clean guard. Recheck under the lock at admission.
    let before=candidates.len();
    candidates.retain(|c|crate::watch::demand_guard_aliases(c.shadow.span().0 as usize+c.lo,&c.views,||true));
    GUARD_REFUSALS.fetch_add((before-candidates.len()) as u64,Ordering::Relaxed);
    if candidates.is_empty() { return; }
    let selected:Vec<_>=candidates.iter().map(|c|(c.shadow.clone(),c.lo)).collect();
    let bytes=(selected.len()*PAGE) as u64;
    let Some(snapshot)=capture(host,queue,&selected,bytes) else { STATS[7].fetch_add(candidates.len() as u64,Ordering::Relaxed); return; };
    let mut taken=Vec::new();
    for (n,c) in candidates.into_iter().enumerate() {
        let i=c.i;
        if admit(c,Source::Snapshot(snapshot.clone()),(n*PAGE) as u64,fence,after) {
            taken.push(i);
            STATS[0].fetch_add(1,Ordering::Relaxed);
            STATS[1].fetch_add(PAGE as u64,Ordering::Relaxed);
        } else { STATS[7].fetch_add(1,Ordering::Relaxed); }
    }
    let mut i=0;
    pieces.retain(|_|{let keep=taken.binary_search(&i).is_err();i+=1;keep});
}


type Declared=(Arc<Shadow>,Vec<(usize,usize)>);
const MAX_ADMIT:usize=64;
const RECENT_MAX:usize=8192;
static RECENT:Mutex<BTreeMap<(u64,usize),u64>>=Mutex::new(BTreeMap::new());
static RESIDENT_STATS:[AtomicU64;8]=[const{AtomicU64::new(0)};8];
static REVOKED:AtomicU64=AtomicU64::new(0);
static OPAQUE_DRAIN:AtomicU64=AtomicU64::new(0);
pub(crate) fn resolve_opaque() {
    if resident_on() && pending() { OPAQUE_DRAIN.fetch_add(1,Ordering::Relaxed); }
    resolve_all();
}
// eligible, armed, producer bytes skipped, materialized bytes, CPU access,
// same-home producers, earlier-writer refusals, guard/other refusals.

pub(crate) fn learn(pieces:&[Piece]) {
    if !resident_on() || pieces.is_empty() { return; }
    let now=crate::frame::current() as u64;
    let mut recent=RECENT.lock().unwrap_or_else(|e|e.into_inner());
    if recent.len()>=RECENT_MAX { recent.retain(|_,f|now.saturating_sub(*f)<120); }
    if recent.len()>=RECENT_MAX { recent.clear(); }
    for (s,lo,hi,_) in pieces {
        if hi.saturating_sub(*lo)==PAGE && s.card_local() {
            recent.insert((s.id(),*lo),now);
            if recent.len()>=RECENT_MAX { break; }
        }
    }
}

/// A same-home write is not a CPU access. Foreign allocation/alias
/// writers still materialize the previous version before preparation.
pub(crate) fn resolve_for_write(id:u64,at:usize,len:usize) {
    if resident_on() { resolve_foreign(id,at,len); }
    else { resolve_range(at,len); }
}

/// Ranges that are definitely not needed by a CPU consumer in the
/// current submission are admitted before ExecuteCommandLists. We never
/// infer absence of CPU access: every admitted physical view is protected.
/// Only new admissions check earlier eager writers. Existing resident
/// pages can remain across multiple pending producers of the same home.
pub(crate) fn prepare_resident(host:&Host,queue:&ID3D12CommandQueue,declared:&[Declared],
    earlier:&[(usize,usize)],other:&[(usize,usize)],after:u64) {
    if !resident_on() || declared.is_empty() { return; }
    for &(a,b) in other { resolve_range(a,b.saturating_sub(a)); }
    let Some(_mapping)=crate::watch::title_memory_reading_now() else { return; };
    let now=crate::frame::current() as u64;
    let keys={
        let recent=RECENT.lock().unwrap_or_else(|e|e.into_inner());
        let mut keys=Vec::new();
        for (shadow,ranges) in declared {
            let base=shadow.span().0 as usize;
            for (&(_,lo),&frame) in recent.range((shadow.id(),0)..=(shadow.id(),usize::MAX)) {
                if now.saturating_sub(frame)>120 { continue; }
                let at=base+lo;
                if ranges.iter().any(|&(a,b)|a<=at&&at+PAGE<=b) {
                    keys.push((shadow.clone(),lo));
                    if keys.len()>=MAX_ADMIT*4 { break; }
                }
            }
            if keys.len()>=MAX_ADMIT*4 { break; }
        }
        keys
    };
    if keys.is_empty() { return; }
    {
        let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
        let s=state.get_or_insert_with(State::default);
        if s.resident_engine.is_none() { s.resident_engine=Engine::with_queue(host,Some(queue)); }
        if s.resident_engine.is_none() { return; }
    }
    let mut admitted=0;
    for (shadow,lo) in keys {
        let at=shadow.span().0 as usize+lo;
        if let Some(true)=refresh_covered(shadow.id(),at,at+PAGE) { continue; }
        let Some(mirror)=shadow.demand_mirror(lo,lo+PAGE,after) else { continue; };
        let Some(run)=crate::inplace::physical_run(at,PAGE) else { continue; };
        if run.committed || run.offset%PAGE as u64!=0 { continue; }
        let key=PhysicalKey{generation:run.generation,offset:run.offset};
        let mut views=crate::inplace::pages_of(at);
        views.sort_unstable();views.dedup();
        if views.is_empty() || views.len()>crate::watch::MANY_VIEWS || !views.contains(&at) { continue; }
        RESIDENT_STATS[0].fetch_add(1,Ordering::Relaxed);
        // Earlier includes items currently being landed, not only queued
        // flights. Never replace an eager writer's still-pending merge.
        if views.iter().any(|&v|{
            let i=earlier.partition_point(|&(_,b)|b<=v);
            earlier.get(i).is_some_and(|&(a,_)|a<v+PAGE)
        }) {
            RESIDENT_STATS[6].fetch_add(1,Ordering::Relaxed);continue;
        }
        if views.iter().any(|&v|other.iter().any(|&(a,b)|a<v+PAGE&&v<b)
            || declared.iter().any(|(s,r)|s.id()!=shadow.id()&&r.iter().any(|&(a,b)|a<v+PAGE&&v<b))) {
            RESIDENT_STATS[7].fetch_add(1,Ordering::Relaxed);continue;
        }
        {
            let state=STATE.lock().unwrap_or_else(|e|e.into_inner());
            let s=state.as_ref().expect("resident state");
            if s.physical.contains_key(&key) || s.hot.get(&key).is_some_and(|&f|now.saturating_sub(f)<120) { continue; }
        }
        let c=Candidate{i:0,shadow,lo,mirror,identity:PageIdentity::Physical(key),views};
        // At this point all uploads for the next submission are queued,
        // and no new shader commands have been sent. A CPU access before
        // send may revoke this reservation and fetch the previous version.
        if admit(c,Source::Resident,lo as u64,0,after) {
            admitted+=1;
            RESIDENT_STATS[1].fetch_add(1,Ordering::Relaxed);
            if admitted>=MAX_ADMIT { break; }
        } else { RESIDENT_STATS[7].fetch_add(1,Ordering::Relaxed); }
    }
}

/// An exact producer-local proof, retained by CardPlan even if the CPU
/// requests the page before the relay processes its flags. This proof
/// cannot be reused by an unrelated producer or a remapped allocation.
#[derive(Default)]
pub(crate) struct Coverage(Vec<(u64,usize,usize)>);
impl Coverage {
    pub(crate) fn covers(&self,id:u64,lo:usize,hi:usize)->bool {
        if lo>=hi { return false; }
        let i=self.0.partition_point(|&(s,_,b)|s<id || (s==id&&b<=lo));
        self.0.get(i).is_some_and(|&(s,a,b)|s==id&&a<=lo&&hi<=b)
    }
    pub(crate) fn skipped(&self,id:u64,lo:usize,hi:usize)->bool {
        if !self.covers(id,lo,hi) { return false; }
        RESIDENT_STATS[2].fetch_add((hi-lo) as u64,Ordering::Relaxed);
        true
    }
}
/// Atomic wrt materialization: a CPU fault may only enqueue a read on
/// the direct queue before this producer, or behind the actual submitted
/// producer. It never waits on a future/unsubmitted fence. No CPU-side
/// completion wait here. All writes still use the normal native queue.
pub(crate) fn send_resident<T>(declared:&[Declared],fence:u64,send:impl FnOnce()->T)->(T,Coverage) {
    if !resident_on() { return (send(),Coverage::default()); }
    let (result,coverage,spans)={
        let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
        let Some(s)=state.as_mut() else { return (send(),Coverage::default()); };
        let mut coverage=Vec::new();
        let mut spans=Vec::new();
        for (&at,p) in &mut s.pages {
            if !matches!(&p.source,Source::Resident) { continue; }
            if declared.iter().any(|(shadow,r)|shadow.id()==p.shadow.id()&&r.iter().any(|&(a,b)|a<at+PAGE&&at<b)) {
                if p.fence!=0 { RESIDENT_STATS[5].fetch_add(1,Ordering::Relaxed); }
                p.fence=fence;
                let lo=at-p.shadow.span().0 as usize;
                coverage.push((p.shadow.id(),lo,lo+PAGE));
                spans.push((at,PAGE));
            }
        }
        coverage.sort_unstable();
        let mut joined:Vec<(u64,usize,usize)>=Vec::new();
        for (id,a,b) in coverage {
            if let Some(last)=joined.last_mut() && last.0==id && a<=last.2 { last.2=last.2.max(b); }
            else { joined.push((id,a,b)); }
        }
        (send(),Coverage(joined),spans)
    };
    // Mark logical writes after the signal has actually been queued,
    // before Flight is exposed to the relay. The normal guard/alias
    // journal invalidates other Shadows. An actual reader materializes
    // first; the original home skips its own protected refresh ranges.
    if !spans.is_empty() {
        let pages:Vec<_>=spans.iter().flat_map(|&(at,len)|at/4096..(at+len)/4096).collect();
        crate::watch::landed_beside(&pages,&spans);
    }
    (result,coverage)
}

pub(crate) fn line(frames:f64) {
    let [c,a,s,m,f,w,o,g]=RESIDENT_STATS.each_ref().map(|n|n.swap(0,Ordering::Relaxed) as f64/frames.max(1.0));
    tracing::info!("VUM resident (VICTORY_VUM_RESIDENT={}): {:.1} candidates, {:.1} new pages, {:.3} MiB eager readback requests omitted, {:.3} MiB actual demand readback, {:.1} CPU publications, {:.1} same-home producer continuations, {:.1} earlier writer refusals, {:.1} other refusals/frame; {} pending pages; {:.1} pre-send revocations, {:.1} opaque drains/frame",
        u8::from(resident_on()),c,a,s/1048576.0,m/1048576.0,f,w,o,g,PENDING.load(Ordering::Acquire),
        REVOKED.swap(0,Ordering::Relaxed) as f64/frames.max(1.0),
        OPAQUE_DRAIN.swap(0,Ordering::Relaxed) as f64/frames.max(1.0));
    let [candidates,admitted,hot,materialized,mapped,boundary,other]=PHYSICAL_STATS.each_ref().map(|n|n.swap(0,Ordering::Relaxed) as f64/frames.max(1.0));
    tracing::info!("VUM physical demand (VICTORY_VUM_PHYSICAL_DEMAND={}): {:.1} candidates, {:.1} admitted, {:.1} hot refusals, {:.1} materialized, {:.1} before new mapping, {:.1} missed boundaries, {:.1} conflicting producers, {:.1} guard refusals/frame",
        u8::from(legacy_physical_on()),candidates,admitted,hot,materialized,mapped,boundary,other,
        GUARD_REFUSALS.swap(0,Ordering::Relaxed) as f64/frames.max(1.0));
    let [a,b,f,e,m,r,c,x]=STATS.each_ref().map(|n|n.swap(0,Ordering::Relaxed) as f64/frames.max(1.0));
    tracing::info!("VUM demand (VICTORY_VUM_DEMAND={}): {:.1} pages deferred/frame, {:.3} MiB deferred, {:.1} CPU faults, {:.1} forced publications, {:.3} MiB materialized, {:.1} same-home refresh decisions, {:.1} allocation candidates, {:.1} refused; {} pending pages, {:.2} MiB snapshots, {:.2} MiB managed RAM",
        u8::from(dedicated_on()),a,b/1048576.0,f,e,m/1048576.0,r,c,x,PENDING.load(Ordering::Acquire),
        SNAP_BYTES.load(Ordering::Acquire) as f64/1048576.0,ALLOC_BYTES.load(Ordering::Acquire) as f64/1048576.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn resident_proof_is_exact_and_allocation_scoped() {
        let c=Coverage(vec![(3,0,PAGE),(3,2*PAGE,3*PAGE),(4,0,PAGE)]);
        assert!(c.covers(3,10,PAGE));
        assert!(!c.covers(3,0,2*PAGE));
        assert!(!c.covers(5,0,PAGE));
        assert!(c.covers(4,0,PAGE));
    }
    #[test] fn gaps_do_not_refresh_gpu_owned_pages() {
        assert_eq!(gaps(PAGE/2,4*PAGE,[PAGE,2*PAGE].into_iter()),[(PAGE/2,PAGE),(3*PAGE,4*PAGE)]);
        assert!(gaps(PAGE+1,2*PAGE,[PAGE].into_iter()).is_empty());
        assert_eq!(gaps(0,4*PAGE,[PAGE,3*PAGE].into_iter()),[(0,PAGE),(2*PAGE,3*PAGE)]);
    }
    #[test] fn physical_identity_and_exact_ranges() {
        let mut s=State::default();
        s.physical.insert(PhysicalKey{generation:7,offset:0},10*PAGE);
        s.physical.insert(PhysicalKey{generation:7,offset:PAGE as u64},20*PAGE);
        s.physical.insert(PhysicalKey{generation:8,offset:0},30*PAGE);
        assert_eq!(physical_keys(&s,7,PAGE as u64-1,2),vec![10*PAGE,20*PAGE]);
        assert_eq!(physical_keys(&s,7,0,PAGE as u64),vec![10*PAGE]);
        assert_eq!(physical_keys(&s,8,0,1),vec![30*PAGE]);
        s.active.insert(1,AccessRange::Physical(7,1,2));
        assert!(active(&s,PageIdentity::Physical(PhysicalKey{generation:7,offset:0}),&[10*PAGE,20*PAGE]));
        assert!(!active(&s,PageIdentity::Physical(PhysicalKey{generation:8,offset:0}),&[30*PAGE]));
        s.active.insert(2,AccessRange::Virtual(20*PAGE+3,20*PAGE+4));
        assert!(active(&s,PageIdentity::Physical(PhysicalKey{generation:8,offset:0}),&[10*PAGE,20*PAGE]));
    }
    #[test] fn owner_excludes_header_crossings_and_outside() {
        let mut s=State::default();
        s.allocations.insert(PAGE,Allocation{id:17,bytes:4*PAGE,alias:10*PAGE});
        assert!(owner(&s,PAGE,PAGE).is_none());
        assert_eq!(owner(&s,2*PAGE,PAGE),Some((17,11*PAGE)));
        assert!(owner(&s,4*PAGE,2*PAGE).is_none());
    }
}
