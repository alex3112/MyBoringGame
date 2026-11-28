//! Experimental VUM demand paging for dedicated, non-imported allocations.
//! Guest memory and a private writable alias refer to one section. Only
//! the guest view is protected; the alias lets us publish the complete
//! GPU page before restoring access, never expose a half-copied page.
//! No ordinary private memory or remappable pool is admitted.
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

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VUM_DEMAND").is_some_and(|v| v == "1"))
        && !crate::vum::physical_experiment() && crate::inplace::level() == 0
}

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
struct Page {
    snapshot: Arc<Snapshot>,
    offset: u64,
    shadow: Arc<Shadow>,
    mirror: usize,
    alias: usize,
    fence: u64,
}
#[derive(Default)]
struct State {
    allocations: BTreeMap<usize, Allocation>,
    active: BTreeMap<u64, (usize,usize)>,
    pages: BTreeMap<usize, Page>,
    engine: Option<Engine>,
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
    if !enabled() || !(MIN..=ALLOC_LIMIT).contains(&size) { return None; }
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
    fn new(host:&Host)->Option<Self> {
        let buffer=host.read_back_buffer(PAGE as u64)?;
        unsafe {
            let allocator:ID3D12CommandAllocator=host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY).ok()?;
            let list:ID3D12GraphicsCommandList=host.device.CreateCommandList(0,D3D12_COMMAND_LIST_TYPE_COPY,&allocator,None).ok()?;
            list.Close().ok()?;
            let queue:ID3D12CommandQueue=host.device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC{
                Type:D3D12_COMMAND_LIST_TYPE_COPY,..Default::default()
            }).ok()?;
            let fence:ID3D12Fence=host.device.CreateFence(0,D3D12_FENCE_FLAG_NONE).ok()?;
            let event=Event(CreateEventW(None,false,false,None).ok()?);
            Some(Self{device:host.device.clone(),queue,allocator,list,fence,event,buffer,sent:0})
        }
    }
    /// Snapshot is complete and immutable. No lock of the normal relay,
    /// submission lane, guards, or resource registry is acquired here.
    fn publish(&mut self,page:&Page,at:usize)->windows::core::Result<()> {
        self.sent+=1;
        unsafe {
            self.allocator.Reset()?;
            self.list.Reset(&self.allocator,None)?;
            self.list.CopyBufferRegion(&self.buffer,0,&page.snapshot.buffer,page.offset,PAGE as u64);
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
            let mut old=0;
            if mem::VirtualProtect(at as *const _,PAGE,mem::PAGE_READONLY,&mut old)==0 || old!=mem::PAGE_NOACCESS {
                return Err(windows::core::Error::from_thread());
            }
        }
        Ok(())
    }
}

fn resolve_locked(s:&mut State, at:usize,len:usize, except:Option<u64>) -> usize {
    if len==0 { return 0; }
    let end=at.saturating_add(len);
    let first=at/PAGE*PAGE;
    let keys:Vec<usize>=s.pages.range(first..end).filter(|(_,p)|except!=Some(p.shadow.id())).map(|(&a,_)|a).collect();
    for a in &keys {
        let page=s.pages.remove(a).expect("pending page");
        if let Err(e)=s.engine.as_mut().expect("demand engine").publish(&page,*a) {
            crate::host::fail(format_args!("VUM demand publication failed at {a:#x}, producer {}: {e}",page.fence));
        }
        PENDING.fetch_sub(1,Ordering::AcqRel);
        STATS[4].fetch_add(PAGE as u64,Ordering::Relaxed);
    }
    keys.len()
}

pub(crate) fn resolve_range(at:usize,len:usize) {
    if !pending() || len==0 { return; }
    let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
    if let Some(s)=state.as_mut() {
        STATS[3].fetch_add(resolve_locked(s,at,len,None) as u64,Ordering::Relaxed);
    }
}
/// Known kernel reads/copies are leased for the whole call, not merely
/// checked before it. Admission tests these ranges under the same mutex,
/// so a page cannot become inaccessible in the middle of a kernel read.
pub(crate) struct CpuAccess(u64);
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
    if len==0 || ALLOC_BYTES.load(Ordering::Acquire)==0 { return CpuAccess(0); }
    let end=at.saturating_add(len);
    let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
    let Some(s)=state.as_mut() else { return CpuAccess(0); };
    if !s.allocations.range(..end).next_back().is_some_and(|(&base,a)|base.saturating_add(a.bytes)>at) {
        return CpuAccess(0);
    }
    STATS[3].fetch_add(resolve_locked(s,at,len,None) as u64,Ordering::Relaxed);
    let id=NEXT_ACCESS.fetch_add(1,Ordering::Relaxed);
    s.active.insert(id,(at,end));
    CpuAccess(id)
}
pub(crate) fn resolve_all() { resolve_range(0,usize::MAX); }
pub(crate) fn resolve_mapping(at:usize,len:usize) {
    if len==0 { resolve_all(); } else { resolve_range(at,len); }
}
pub(crate) fn resolve_foreign(id:u64,at:usize,len:usize) {
    if !pending() || len==0 { return; }
    let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
    if let Some(s)=state.as_mut() {
        STATS[3].fetch_add(resolve_locked(s,at,len,Some(id)) as u64,Ordering::Relaxed);
    }
}

pub(crate) fn fault(at:usize,write:bool)->bool {
    if !enabled() { return false; }
    let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
    let Some(s)=state.as_mut() else { return false; };
    if resolve_locked(s,at,1,None)!=0 {
        STATS[2].fetch_add(1,Ordering::Relaxed);
        return true;
    }
    // A second fault can have been delivered before another thread
    // finished publication. For a write to a restored read-only page,
    // let the normal write-guard handler open it instead.
    if owner(s,at,1).is_none() { return false; }
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
    if !pending() || from>=to { return None; }
    let state=STATE.lock().unwrap_or_else(|e|e.into_inner());
    let s=state.as_ref()?;
    let mut pages=s.pages.range(from/PAGE*PAGE..to).peekable();
    pages.peek()?;
    let mut at=from;
    for (&a,p) in pages {
        if a>at || p.shadow.id()!=id { return Some(false); }
        at=at.max(a+PAGE);
    }
    let all=at>=to;
    if all { STATS[5].fetch_add(1,Ordering::Relaxed); }
    Some(all)
}
pub(crate) fn refresh_gaps(id:u64,from:usize,to:usize)->Option<Vec<(usize,usize)>> {
    if !pending() || from>=to { return None; }
    let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
    let s=state.as_mut()?;
    let first=from/PAGE*PAGE;
    if s.pages.range(first..to).next().is_none() { return None; }
    STATS[3].fetch_add(resolve_locked(s,from,to-from,Some(id)) as u64,Ordering::Relaxed);
    let out=gaps(from,to,s.pages.range(first..to).map(|(&a,_)|a));
    STATS[5].fetch_add(1,Ordering::Relaxed);
    Some(out)
}

fn admit(shadow:Arc<Shadow>,lo:usize,mirror:usize,snapshot:Arc<Snapshot>,offset:u64,fence:u64,allocation:u64)->bool {
    let at=shadow.span().0 as usize+lo;
    let admitted=crate::watch::demand_guard(at,|| {
        let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
        let Some(s)=state.as_mut() else { return false; };
        let Some((id,alias))=owner(s,at,PAGE) else { return false; };
        if id!=allocation || s.engine.is_none() || s.pages.contains_key(&at)
            || s.active.values().any(|&(a,b)|a<at+PAGE&&at<b)
        { return false; }
        let mut info:mem::MEMORY_BASIC_INFORMATION=unsafe{std::mem::zeroed()};
        let n=unsafe{mem::VirtualQuery(at as *const _,&mut info,std::mem::size_of_val(&info))};
        if n==0 || info.State!=mem::MEM_COMMIT || info.Type!=mem::MEM_MAPPED || info.Protect!=mem::PAGE_READONLY
            || (info.BaseAddress as usize).saturating_add(info.RegionSize)<at+PAGE { return false; }
        // The ordinary guard excludes CPU stores here. Require every
        // reference byte to agree with the alias before taking the page.
        // No driver-imported memory, no address guessed from traffic.
        let same=unsafe { std::slice::from_raw_parts(alias as *const u8,PAGE)==std::slice::from_raw_parts(mirror as *const u8,PAGE) };
        if !same { return false; }
        let mut old=0;
        // Advertise pending before protecting, while STATE is held.
        // A fault arriving before insertion waits on this same lock.
        PENDING.fetch_add(1,Ordering::AcqRel);
        if unsafe{mem::VirtualProtect(at as *const _,PAGE,mem::PAGE_NOACCESS,&mut old)}==0 {
            PENDING.fetch_sub(1,Ordering::AcqRel);return false;
        }
        if old!=mem::PAGE_READONLY {
            unsafe{mem::VirtualProtect(at as *const _,PAGE,old,&mut old)};
            PENDING.fetch_sub(1,Ordering::AcqRel);return false;
        }
        s.pages.insert(at,Page{snapshot,offset,shadow,mirror,alias,fence});
        true
    });
    if admitted {
        // Logical write is visible now, even while physical bytes are in
        // VRAM. Other copies see the journal, resolve on access and refresh.
        let pages:Vec<usize>=(at/4096..(at+PAGE)/4096).collect();
        crate::watch::landed_beside(&pages,&[(at,PAGE)]);
    }
    admitted
}

pub(crate) fn defer(host:&Host, queue:&ID3D12CommandQueue,pieces:&mut Vec<Piece>,fence:u64,after:u64,other:&[(Arc<Shadow>,Vec<(usize,usize)>)]) {
    if ALLOC_BYTES.load(Ordering::Acquire)==0 || pieces.is_empty() { return; }
    // One bounded batch. Keep the original eager path for partial pages,
    // multiple writers/aliases, newer CPU bytes, budget/guard failures.
    let mut candidates=Vec::new();
    {
        let mut state=STATE.lock().unwrap_or_else(|e|e.into_inner());
        let Some(s)=state.as_mut() else { return; };
        if s.engine.is_none() { s.engine=Engine::new(host); }
        if s.engine.is_none() { return; }
        for (i,(shadow,lo,hi,_)) in pieces.iter().enumerate() {
            let at=shadow.span().0 as usize+*lo;
            let Some((allocation,_))=owner(s,at,PAGE) else { continue; };
            if s.pages.contains_key(&at) || s.active.values().any(|&(a,b)|a<at+PAGE&&at<b) { continue; }
            let Some(mirror)=shadow.demand_mirror(*lo,*hi,after) else { continue; };
            let end=at+PAGE;
            if other.iter().any(|(_,ranges)|ranges.iter().any(|&(a,b)|a<end&&at<b))
                || pieces.iter().enumerate().any(|(j,(other,a,b,_))|j!=i
                    && (other.span().0 as usize+*a)<end && at<other.span().0 as usize+*b)
            { continue; }
            candidates.push((i,shadow.clone(),*lo,mirror,allocation));
            if candidates.len()>=BATCH_PAGES { break; }
        }
    }
    if candidates.is_empty() { return; }
    let selected:Vec<_>=candidates.iter().map(|(_,s,lo,_,_)|(s.clone(),*lo)).collect();
    let bytes=(selected.len()*PAGE) as u64;
    let Some(snapshot)=capture(host,queue,&selected,bytes) else { STATS[7].fetch_add(candidates.len() as u64,Ordering::Relaxed); return; };
    let mut taken=Vec::new();
    for (n,(i,shadow,lo,mirror,allocation)) in candidates.into_iter().enumerate() {
        if admit(shadow,lo,mirror,snapshot.clone(),(n*PAGE) as u64,fence,allocation) {
            taken.push(i);
            STATS[0].fetch_add(1,Ordering::Relaxed);
            STATS[1].fetch_add(PAGE as u64,Ordering::Relaxed);
        } else { STATS[7].fetch_add(1,Ordering::Relaxed); }
    }
    // Preserve the original order of every remaining eager writeback.
    let mut i=0;
    pieces.retain(|_|{let keep=taken.binary_search(&i).is_err();i+=1;keep});
}

pub(crate) fn line(frames:f64) {
    let [a,b,f,e,m,r,c,x]=STATS.each_ref().map(|n|n.swap(0,Ordering::Relaxed) as f64/frames.max(1.0));
    tracing::info!("VUM demand (VICTORY_VUM_DEMAND={}): {:.1} pages deferred/frame, {:.3} MiB deferred, {:.1} CPU faults, {:.1} forced publications, {:.3} MiB materialized, {:.1} same-home refresh decisions, {:.1} allocation candidates, {:.1} refused; {} pending pages, {:.2} MiB snapshots, {:.2} MiB managed RAM",
        u8::from(enabled()),a,b/1048576.0,f,e,m/1048576.0,r,c,x,PENDING.load(Ordering::Acquire),
        SNAP_BYTES.load(Ordering::Acquire) as f64/1048576.0,ALLOC_BYTES.load(Ordering::Acquire) as f64/1048576.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn gaps_do_not_refresh_gpu_owned_pages() {
        assert_eq!(gaps(PAGE/2,4*PAGE,[PAGE,2*PAGE].into_iter()),[(PAGE/2,PAGE),(3*PAGE,4*PAGE)]);
        assert!(gaps(PAGE+1,2*PAGE,[PAGE].into_iter()).is_empty());
        assert_eq!(gaps(0,4*PAGE,[PAGE,3*PAGE].into_iter()),[(0,PAGE),(2*PAGE,3*PAGE)]);
    }
    #[test] fn owner_excludes_header_crossings_and_outside() {
        let mut s=State::default();
        s.allocations.insert(PAGE,Allocation{id:17,bytes:4*PAGE,alias:10*PAGE});
        assert!(owner(&s,PAGE,PAGE).is_none());
        assert_eq!(owner(&s,2*PAGE,PAGE),Some((17,11*PAGE)));
        assert!(owner(&s,4*PAGE,2*PAGE).is_none());
    }
}
