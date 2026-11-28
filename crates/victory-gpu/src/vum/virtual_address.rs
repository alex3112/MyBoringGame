//! VUM stable GPU virtual addressing, local-memory backend.
//!
//! OpenExistingHeapFromFileMapping exposes cross-adapter heaps. D3D12
//! reserved resources cannot be mapped to those heaps. Do not mix the
//! VUM1/experimental VUM2 RAM path with this backend.
//!
//! A reserved buffer has one permanent native GPU address. Its 64 KiB
//! physical tiles are separate D3D12 local heaps. A mapping operation
//! names a logical tile interval and a physical heap interval separately.
//! This build maps all tiles once at creation, and preserves the existing
//! CPU shadow/relay semantics. Later migration/aliasing must be a queue-
//! ordered ownership transition, not a change to the descriptor's VA.
//! No old content is discarded or remapped in this initial integration.

use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use windows::core::{GUID, HRESULT, IUnknown, Interface};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};

const TILE: u64 = 64 << 10;
const SEGMENT: u64 = 32 << 20;
const MIN: u64 = 16 << 20;
const MAX: u64 = 256 << 20;
const DEFAULT_BUDGET: u64 = 256 << 20;
static LIVE: AtomicU64 = AtomicU64::new(0);
static LIVE_COUNT: AtomicU64 = AtomicU64::new(0);
// attempted, made, refused, mapped tiles, mapping calls, microseconds
static STATS: [AtomicU64;6] = [const { AtomicU64::new(0) };6];

fn enabled() -> bool {
    super::enabled() && crate::toggled!("VICTORY_VUM_STABLE_VA")
}
fn budget() -> u64 {
    static BUDGET: OnceLock<u64> = OnceLock::new();
    *BUDGET.get_or_init(|| std::env::var("VICTORY_VUM_STABLE_MB").ok()
        .and_then(|v| v.parse::<u64>().ok()).map(|n| n.min(1024) << 20)
        .unwrap_or(DEFAULT_BUDGET))
}

struct Charge(u64);
impl Charge {
    fn take(bytes:u64) -> Option<Self> {
        LIVE.fetch_update(Ordering::AcqRel,Ordering::Acquire,|n|
            n.checked_add(bytes).filter(|&v| v <= budget())).ok()?;
        LIVE_COUNT.fetch_add(1,Ordering::Relaxed);
        Some(Self(bytes))
    }
}
impl Drop for Charge {
    fn drop(&mut self) {
        LIVE.fetch_sub(self.0,Ordering::AcqRel);
        LIVE_COUNT.fetch_sub(1,Ordering::Relaxed);
    }
}

/// A buffer interval in 64 KiB tiles, independently of its backing heap.
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
struct Region {
    first:u32,
    count:u32,
}
fn regions(bytes:u64) -> Option<Vec<Region>> {
    if bytes == 0 || !bytes.is_multiple_of(TILE) || bytes > MAX { return None; }
    let mut at = 0u32;
    let end = u32::try_from(bytes/TILE).ok()?;
    let mut out = Vec::new();
    while at < end {
        let count = (end-at).min((SEGMENT/TILE) as u32);
        out.push(Region { first:at,count });
        at += count;
    }
    Some(out)
}

struct Segment {
    region:Region,
    heap:ID3D12Heap,
}

// Reserved resources do NOT retain the heaps mapped into them. Attach a
// COM lifetime owner (as for tiled textures) to the resource itself, not
// only its Rust Shadow. Persistent descriptors can outlive the Shadow.
// The owner must never contain a reference to the reserved resource.
#[repr(C)]
struct Lease {
    methods: &'static [*const ();3],
    references: AtomicU32,
    segments: Vec<Segment>,
    _charge: Charge,
}
struct OwnerMethods([*const ();3]);
// SAFETY: immutable function addresses.
unsafe impl Sync for OwnerMethods {}
static OWNER_METHODS: OwnerMethods = OwnerMethods([
    owner_query as *const (), owner_add_ref as *const (), owner_release as *const (),
]);
unsafe extern "system" fn owner_query(this:*mut c_void,iid:*const GUID,out:*mut *mut c_void) -> HRESULT {
    unsafe {
        if out.is_null() { return HRESULT(0x8000_4003_u32 as i32); }
        *out = std::ptr::null_mut();
        if iid.as_ref().is_some_and(|id| *id == IUnknown::IID) {
            owner_add_ref(this);
            *out = this;
            return HRESULT(0);
        }
    }
    HRESULT(0x8000_4002_u32 as i32)
}
unsafe extern "system" fn owner_add_ref(this:*mut c_void) -> u32 {
    unsafe { (*this.cast::<Lease>()).references.fetch_add(1,Ordering::Relaxed)+1 }
}
unsafe extern "system" fn owner_release(this:*mut c_void) -> u32 {
    unsafe {
        let left = (*this.cast::<Lease>()).references.fetch_sub(1,Ordering::AcqRel)-1;
        if left == 0 { drop(Box::from_raw(this.cast::<Lease>())); }
        left
    }
}

// One-time initial mappings use a private copy queue. Wait before
// publishing the resource to any recorder or descriptor. That includes
// GPU-indexed descriptors visible to already queued work. Never wait for
// the main submission/relay threads while holding the shadow-map lock.
struct Mapper {
    device:ID3D12Device,
    queue:ID3D12CommandQueue,
    fence:ID3D12Fence,
    event:HANDLE,
    value:u64,
}
// SAFETY: D3D12 interfaces are free-threaded; event/fence serialised by MAPPER.
unsafe impl Send for Mapper {}
impl Drop for Mapper {
    fn drop(&mut self) { let _ = unsafe { CloseHandle(self.event) }; }
}
static MAPPER: Mutex<Option<Mapper>> = Mutex::new(None);
impl Mapper {
    fn new(device:&ID3D12Device) -> windows::core::Result<Self> {
        let queue:ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type:D3D12_COMMAND_LIST_TYPE_COPY,..Default::default()
        })? };
        let fence = unsafe { device.CreateFence(0,D3D12_FENCE_FLAG_NONE)? };
        let event = unsafe { CreateEventW(None,false,false,None)? };
        Ok(Self {device:device.clone(),queue,fence,event,value:0})
    }

    /// Queue a mapping of logical tiles to a physical heap range. The VA
    /// belongs to the reserved resource and does not change. At migration
    /// the caller must have settled previous uses on ALL relevant queues,
    /// saved any live contents and kept the old heap until the fence.
    /// The only caller today is initial creation, with fresh local heaps.
    fn map(&self, resource:&ID3D12Resource, region:Region, heap:&ID3D12Heap, physical_first:u32) {
        let at = D3D12_TILED_RESOURCE_COORDINATE { X:region.first,Y:0,Z:0,Subresource:0 };
        let size = D3D12_TILE_REGION_SIZE {NumTiles:region.count,..Default::default()};
        let flags = D3D12_TILE_RANGE_FLAG_NONE;
        unsafe {
            self.queue.UpdateTileMappings(resource,1,Some(&raw const at),Some(&raw const size),
                heap,1,Some(&raw const flags),Some(&raw const physical_first),
                Some(&raw const region.count),D3D12_TILE_MAPPING_FLAG_NONE);
        }
        STATS[3].fetch_add(u64::from(region.count),Ordering::Relaxed);
        STATS[4].fetch_add(1,Ordering::Relaxed);
    }
    fn finish(&mut self) -> windows::core::Result<()> {
        self.value += 1;
        unsafe {
            self.queue.Signal(&self.fence,self.value)?;
            if self.fence.GetCompletedValue() < self.value {
                self.fence.SetEventOnCompletion(self.value,self.event)?;
                if WaitForSingleObject(self.event,INFINITE) != WAIT_OBJECT_0 {
                    return Err(windows::core::Error::from_thread());
                }
            }
            self.device.GetDeviceRemovedReason()?;
        }
        Ok(())
    }
}

fn support(device:&ID3D12Device) -> bool {
    // Host device is fixed for the process. Cache the feature query.
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        let ok = unsafe { device.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS,
            (&raw mut options).cast(),size_of_val(&options) as u32).is_ok() };
        let supported = ok && options.TiledResourcesTier.0 >= 2;
        tracing::info!(supported,tier=options.TiledResourcesTier.0,
            "VUM stable VA: local reserved buffers; existing shared RAM heaps excluded");
        supported
    })
}

pub(crate) fn allocate(device:&ID3D12Device,size:u64) -> Option<ID3D12Resource> {
    if !enabled() || !(MIN..=MAX).contains(&size) || !support(device) { return None; }
    let bytes = size.checked_next_multiple_of(TILE)?;
    let plan = regions(bytes)?;
    let charge = Charge::take(bytes)?;
    STATS[0].fetch_add(1,Ordering::Relaxed);
    match make(device,size,bytes,plan,charge) {
        Ok(buffer) => {
            STATS[1].fetch_add(1,Ordering::Relaxed);
            Some(buffer)
        }
        Err(e) => {
            let n = STATS[2].fetch_add(1,Ordering::Relaxed);
            if n == 0 { tracing::warn!(error=%e,size,"VUM stable VA: creation refused; ordinary local buffer"); }
            None
        }
    }
}

fn make(device:&ID3D12Device,size:u64,bytes:u64,plan:Vec<Region>,charge:Charge)
    -> windows::core::Result<ID3D12Resource> {
    let began = std::time::Instant::now();
    let desc = D3D12_RESOURCE_DESC {
        Dimension:D3D12_RESOURCE_DIMENSION_BUFFER,Width:size,Height:1,
        DepthOrArraySize:1,MipLevels:1,SampleDesc:DXGI_SAMPLE_DESC{Count:1,Quality:0},
        Layout:D3D12_TEXTURE_LAYOUT_ROW_MAJOR,Flags:D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut buffer:Option<ID3D12Resource> = None;
    unsafe { device.CreateReservedResource(&desc,D3D12_RESOURCE_STATE_COMMON,None,&mut buffer)?; }
    let buffer = buffer.ok_or_else(windows::core::Error::empty)?;
    // Verify the driver's tiling geometry before mapping anything.
    let mut total = 0u32;
    let mut count = 1u32;
    let mut tiling = D3D12_SUBRESOURCE_TILING::default();
    let mut packed = D3D12_PACKED_MIP_INFO::default();
    let mut shape = D3D12_TILE_SHAPE::default();
    unsafe { device.GetResourceTiling(&buffer,Some(&raw mut total),Some(&raw mut packed),
        Some(&raw mut shape),Some(&raw mut count),0,&raw mut tiling); }
    if u64::from(total)*TILE != bytes || count != 1
        || tiling.StartTileIndexInOverallResource != 0 || tiling.WidthInTiles != total
        || tiling.HeightInTiles != 1 || tiling.DepthInTiles != 1 {
        return Err(windows::core::Error::empty());
    }
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    if gpu == 0 { return Err(windows::core::Error::empty()); }
    let mut segments = Vec::with_capacity(plan.len());
    for region in plan {
        let desc = D3D12_HEAP_DESC {
            SizeInBytes:u64::from(region.count)*TILE,
            Properties:D3D12_HEAP_PROPERTIES{Type:D3D12_HEAP_TYPE_DEFAULT,..Default::default()},
            Alignment:0,
            Flags:D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS | crate::host::not_resident(),
        };
        let mut heap:Option<ID3D12Heap> = None;
        unsafe { device.CreateHeap::<ID3D12Heap>(&raw const desc,&raw mut heap)?; }
        let heap = heap.ok_or_else(windows::core::Error::empty)?;
        crate::host::resident(device,&heap)?;
        segments.push(Segment {region,heap});
    }
    let n = segments.len();
    let mut mapper = MAPPER.lock().unwrap_or_else(|e| e.into_inner());
    if mapper.as_ref().is_some_and(|m| m.device.as_raw()!=device.as_raw()) { *mapper = None; }
    if mapper.is_none() { *mapper = Some(Mapper::new(device)?); }
    let m = mapper.as_mut().ok_or_else(windows::core::Error::empty)?;
    // Attach before the first queue operation. No references to the buffer
    // are held by its own lease. Failure here falls back without GPU work.
    let owner = Box::into_raw(Box::new(Lease { methods:&OWNER_METHODS.0,
        references:AtomicU32::new(1),segments,_charge:charge }));
    const KEY:GUID = GUID::from_u128(0x63833fd7_b011_4af8_a083_499a10970436);
    let object = unsafe { IUnknown::from_raw(owner.cast()) };
    unsafe { buffer.SetPrivateDataInterface(&KEY,&object)?; }
    for segment in unsafe { &(*owner).segments } {
        m.map(&buffer,segment.region,&segment.heap,0);
    }
    // UpdateTileMappings returns void. After it has been queued, a failed
    // fence is not a recoverable allocation refusal: freeing the mapped
    // objects would race outstanding GPU work.
    if let Err(e) = m.finish() {
        crate::host::fail(format_args!("VUM stable VA initial mapping failed: {e}"));
    }
    drop(mapper);
    if unsafe { buffer.GetGPUVirtualAddress() } != gpu {
        crate::host::fail(format_args!("VUM stable VA changed during initial mapping"));
    }
    STATS[5].fetch_add(began.elapsed().as_micros() as u64,Ordering::Relaxed);
    tracing::info!(gpu=format_args!("{gpu:#x}"),bytes,segments=n,"VUM stable VA: local reserved buffer mapped");
    Ok(buffer)
}

pub(crate) fn log(frames:f64) {
    let n = STATS.each_ref().map(|s| s.swap(0,Ordering::Relaxed) as f64/frames);
    tracing::info!("VUM stable VA (VICTORY_VUM_STABLE_VA={}): {} live reservations, {:.2} MiB backed; per frame {:.2} attempts, {:.2} made, {:.2} refused, {:.2} tiles mapped, {:.2} mapping calls, {:.3} ms allocation/mapping",
        u8::from(enabled()),LIVE_COUNT.load(Ordering::Relaxed),LIVE.load(Ordering::Relaxed) as f64/1048576.0,
        n[0],n[1],n[2],n[3],n[4],n[5]/1000.0);
}

#[cfg(test)]
#[test]
fn logical_regions_cover_the_reservation_without_holes() {
    let plan = regions(174 << 20).unwrap();
    assert_eq!(plan.len(),6);
    assert_eq!(plan[0],Region{first:0,count:512});
    assert_eq!(plan[5],Region{first:2560,count:224});
    assert_eq!(plan.iter().map(|r|u64::from(r.count)*TILE).sum::<u64>(),174 << 20);
    assert!(regions(0).is_none());
    assert!(regions(TILE+1).is_none());
    assert!(regions(MAX+TILE).is_none());
}
