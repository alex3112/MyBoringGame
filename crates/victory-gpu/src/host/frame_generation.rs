//! Experimental native FSR 3.1 frame interpolation. Uses the AMD pacing
//! swapchain, original title depth/MVs, and the same direct queue as the
//! translated title. No AFMF, duplicated presents or second upscaler.
//! SDK ABI: ffx_api.h, ffx_framegeneration.h and DX12 headers.
use std::collections::BTreeMap;
use std::ffi::{CStr, c_char, c_void};
use std::ptr::null_mut;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use victory_core::fsr::{self, Temporal};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;
use super::{Host, HostList, transition};
use super::fidelityfx::{Header, Library};
use crate::resource::HostTexture;

mod image_audit;

type Ptr = *mut c_void;

// All FPS values describe a measured interval, not a theoretical multiplier.
// DXGI's submitted-present count is not a physical scanout counter.
#[derive(Clone, Copy, Default)]
pub(super) struct Metrics {
    pub native: f64,
    pub generated: f64,
    pub submitted: Option<f64>,
    pub completed: Option<f64>,
}
static METRICS: Mutex<Option<(Instant, Metrics)>> = Mutex::new(None);
pub(super) fn metrics() -> Option<Metrics> {
    METRICS.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        .filter(|(at, _)| at.elapsed().as_secs_f64() < 2.5).map(|(_, m)| *m)
}
pub(super) fn state_label() -> &'static str {
    let status = fsr::generation_status();
    if status.starts_with("Unavailable") { return "UNAVAILABLE"; }
    if !fsr::generation_requested() { return "OFF"; }
    if status.starts_with("On - native") {
        if metrics().is_some_and(|m| m.generated > 0.0) { "ACTIVE" } else { "WARMUP" }
    } else { "PAUSED" }
}

/// DXGI refresh statistics only expose the latest sample. We observe them
/// once per native present, so multi-present intervals are aggregated.
/// SyncQPCTime is the refresh synchronization clock, not a per-image timestamp.
struct Pacing {
    freq: i64,
    last: Option<DXGI_FRAME_STATISTICS>,
    first: Option<DXGI_FRAME_STATISTICS>,
    samples: u64,
    min: f64,
    max: f64,
    sum: f64,
    multi: u64,
    unavailable: u64,
}
impl Pacing {
    fn new() -> Self {
        let mut freq = 0i64;
        unsafe { windows_sys::Win32::System::Performance::QueryPerformanceFrequency(&mut freq); }
        Self { freq, last: None, first: None, samples: 0, min: f64::INFINITY,
            max: 0.0, sum: 0.0, multi: 0, unavailable: 0 }
    }
    fn observe(&mut self, swap: &IDXGISwapChain3) {
        let mut stats = DXGI_FRAME_STATISTICS::default();
        if self.freq <= 0 || unsafe { swap.GetFrameStatistics(&mut stats) }.is_err() {
            self.unavailable += 1;
            self.last = None;
            self.first = None;
            return;
        }
        if stats.SyncQPCTime <= 0 { return; }
        if let Some(old) = self.last {
            let count = stats.PresentCount.wrapping_sub(old.PresentCount);
            if count == 0 { return; }
            let ticks = stats.SyncQPCTime - old.SyncQPCTime;
            // A disjoint/reset or mode change cannot form a valid interval.
            if count > 120 || ticks <= 0 || ticks > self.freq * 5 {
                self.first = Some(stats);
            } else {
                let ms = ticks as f64 * 1000.0 / self.freq as f64 / f64::from(count);
                self.samples += 1;
                self.multi += u64::from(count > 1);
                self.min = self.min.min(ms);
                self.max = self.max.max(ms);
                self.sum += ms;
            }
        }
        if self.first.is_none() { self.first = Some(stats); }
        self.last = Some(stats);
    }
    fn interval(&mut self) -> Option<f64> {
        let fps = self.first.zip(self.last).and_then(|(a,b)| {
            let ticks = b.SyncQPCTime - a.SyncQPCTime;
            let count = b.PresentCount.wrapping_sub(a.PresentCount);
            (ticks > 0 && count > 0 && count <= 240).then(|| f64::from(count) * self.freq as f64 / ticks as f64)
        });
        if fsr::generation_requested() {
            tracing::info!(completed_fps = ?fps, samples = self.samples, multi_present_samples = self.multi,
                unavailable = self.unavailable,
                mean_interval_ms = (self.samples > 0).then(|| self.sum / self.samples as f64),
                min_interval_ms = (self.samples > 0).then_some(self.min),
                max_interval_ms = (self.samples > 0).then_some(self.max),
                "FFX FG: DXGI refresh timing (aggregated, not individual scanout intervals)");
        }
        self.first = self.last;
        self.samples = 0; self.multi = 0; self.unavailable = 0;
        self.min = f64::INFINITY; self.max = 0.0; self.sum = 0.0;
        fps
    }
}

type Result<T> = std::result::Result<T, String>;
type Create = unsafe extern "C" fn(*mut Ptr, *mut Header, *const c_void) -> u32;
type Destroy = unsafe extern "C" fn(*mut Ptr, *const c_void) -> u32;
type Call = unsafe extern "C" fn(*mut Ptr, *const Header) -> u32;
type Query = unsafe extern "C" fn(*mut Ptr, *mut Header) -> u32;

const CREATE_FG: u64 = 0x20001;
const CONFIGURE_FG: u64 = 0x20002;
const DISPATCH_FG: u64 = 0x20003;
const PREPARE_FG: u64 = 0x20004;
const CREATE_SWAP: u64 = 0x30006;
const WAIT_SWAP: u64 = 0x30007;
const BACKEND_DX12: u64 = 2;
fn header(kind: u64) -> Header { Header { kind, next: null_mut() } }

struct Api {
    library: Option<Library>,
    create: Create,
    destroy: Destroy,
    configure: Call,
    dispatch: Call,
    query: Query,
}
impl Api {
    fn new() -> Result<Self> {
        let lib = Library::open()?;
        // SAFETY: the official ABI exports have the signatures above.
        let create = unsafe { std::mem::transmute::<_, Create>(lib.symbol(c"ffxCreateContext")?) };
        let destroy = unsafe { std::mem::transmute::<_, Destroy>(lib.symbol(c"ffxDestroyContext")?) };
        let configure = unsafe { std::mem::transmute::<_, Call>(lib.symbol(c"ffxConfigure")?) };
        let dispatch = unsafe { std::mem::transmute::<_, Call>(lib.symbol(c"ffxDispatch")?) };
        let query = unsafe { std::mem::transmute::<_, Query>(lib.symbol(c"ffxQuery")?) };
        Ok(Self { library: Some(lib), create, destroy, configure, dispatch, query })
    }
    fn create(&self, desc: &mut Header) -> Result<usize> {
        let mut ctx = null_mut();
        let rc = unsafe { (self.create)(&mut ctx, desc, std::ptr::null()) };
        if rc != 0 || ctx.is_null() { return Err(format!("ffxCreateContext {:#x}: {rc}", desc.kind)); }
        Ok(ctx as usize)
    }
    fn call(&self, ctx: usize, desc: &Header, f: Call) -> Result<()> {
        let mut ctx = ctx as Ptr;
        let rc = unsafe { f(&mut ctx, desc) };
        if rc == 0 { Ok(()) } else { Err(format!("FidelityFX descriptor {:#x}: {rc}", desc.kind)) }
    }
    fn destroy(&self, ctx: usize) {
        let mut ptr = ctx as Ptr;
        let rc = unsafe { (self.destroy)(&mut ptr, std::ptr::null()) };
        if rc != 0 { tracing::warn!(rc, "FFX FG: destroy failed"); }
    }
    fn provider(&self, host: &Host, kind: u64, prefix: &str) -> Result<u64> {
        #[repr(C)]
        struct Versions {
            header: Header, kind: u64, device: Ptr, count: *mut u64,
            ids: *mut u64, names: *mut *const c_char,
        }
        let mut count = 0u64;
        let mut desc = Versions { header: header(4), kind, device: host.device.as_raw(),
            count: &mut count, ids: null_mut(), names: null_mut() };
        let rc = unsafe { (self.query)(null_mut(), &mut desc.header) };
        if rc != 0 || count == 0 || count > 128 { return Err(format!("provider query {kind:#x}: {rc}, count {count}")); }
        let mut ids = vec![0u64; count as usize];
        let mut names = vec![std::ptr::null(); count as usize];
        desc.ids = ids.as_mut_ptr();
        desc.names = names.as_mut_ptr();
        let rc = unsafe { (self.query)(null_mut(), &mut desc.header) };
        if rc != 0 || count as usize > ids.len() { return Err(format!("provider enumeration {kind:#x}: {rc}")); }
        for (&id, &name) in ids.iter().zip(&names).take(count as usize) {
            if name.is_null() { continue; }
            let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
            tracing::info!(kind, id, %name, "FFX FG: available provider");
            if name.contains(prefix) { return Ok(id); }
        }
        Err(format!("FidelityFX {prefix} provider not available"))
    }
}
#[repr(C)]
struct Override { header: Header, id: u64 }
#[repr(C)]
struct Backend { header: Header, device: Ptr }
#[repr(C)]
struct CreateFg { header: Header, flags: u32, display: [u32; 2], render: [u32; 2], format: u32 }
#[repr(C)]
struct CreateSwap {
    header: Header, swap: *mut Ptr, hwnd: HWND, desc: *mut DXGI_SWAP_CHAIN_DESC1,
    fullscreen: Ptr, factory: Ptr, queue: Ptr,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Resource {
    address: Ptr, kind: u32, format: u32, width: u32, height: u32, depth: u32,
    mips: u32, flags: u32, usage: u32, state: u32,
}
fn resource(texture: &ID3D12Resource, format: u32, state: u32) -> Resource {
    let d = unsafe { texture.GetDesc() };
    Resource { address: texture.as_raw(), kind: 2, format, width: d.Width as u32,
        height: d.Height, depth: u32::from(d.DepthOrArraySize), mips: u32::from(d.MipLevels),
        flags: 0, usage: if d.Flags.contains(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS) { 2 } else { 0 }, state }
}
type CallbackFn = unsafe extern "C" fn(*mut Generation, Ptr) -> u32;
#[repr(C)]
struct Configure {
    header: Header, swap: Ptr, present_callback: Ptr, present_user: Ptr,
    generate_callback: Option<CallbackFn>, generate_user: Ptr,
    enabled: bool, async_work: bool, hudless: Resource, flags: u32,
    only_generated: bool, rect: [i32; 4], frame: u64,
}
#[repr(C)]
struct Prepare {
    header: Header, frame: u64, flags: u32, list: Ptr, render: [u32; 2],
    jitter: [f32; 2], motion_scale: [f32; 2], dt: f32, unused_reset: bool,
    near: f32, far: f32, fov: f32, meters: f32, depth: Resource, motion: Resource,
}
#[repr(C)]
struct Generation {
    header: Header, list: Ptr, color: Resource, outputs: [Resource; 4],
    count: u32, reset: bool, transfer: u32, luminance: [f32; 2],
    rect: [i32; 4], frame: u64,
}

struct Inputs { data: Temporal, depth: Arc<HostTexture>, motion: Arc<HostTexture> }
// One successful title upscale per presentation. Never use a previous frame
// as a substitute. Retain the native resource owners (not raw title pointers).
static INPUTS: Mutex<BTreeMap<u64, Option<Inputs>>> = Mutex::new(BTreeMap::new());
static INPUT_COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
pub(super) fn capture(data: Temporal) {
    if data.upscale_flags == u32::MAX || data.upscale_flags & 2 != 0
        || data.render.contains(&0) || data.output.contains(&0)
        || !data.jitter.iter().chain(&data.motion_scale).all(|v| v.is_finite())
        || ![data.dt_ms, data.near, data.far, data.fov, data.meters].iter().all(|v| v.is_finite())
        || data.dt_ms <= 0.0 || data.fov <= 0.0 || data.fov >= std::f32::consts::PI
    { INPUT_COUNTS[1].fetch_add(1, Ordering::Relaxed); return; }
    let get = |id: usize, fmt: DXGI_FORMAT| {
        let title = id as crate::object::This;
        if !crate::resource::is_resource(title) { return None; }
        let copy = crate::resource::made_copy(title)?;
        let d = unsafe { copy.texture.GetDesc() };
        (d.Format == fmt && d.Width == u64::from(data.render[0]) && d.Height == data.render[1]
            && d.SampleDesc.Count == 1 && d.DepthOrArraySize == 1 && d.MipLevels == 1
            && copy.base == 0 && copy.tiled.is_none()).then_some(copy)
    };
    let frame = crate::queue::presented().saturating_add(1);
    let inputs = get(data.depth, DXGI_FORMAT_R32G8X24_TYPELESS).zip(get(data.motion, DXGI_FORMAT_R16G16_FLOAT))
        .map(|(depth, motion)| Inputs { data, depth, motion });
    INPUT_COUNTS[usize::from(inputs.is_none())].fetch_add(1, Ordering::Relaxed);
    let mut map = INPUTS.lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|&id, _| id >= frame.saturating_sub(8));
    use std::collections::btree_map::Entry;
    match map.entry(frame) {
        Entry::Vacant(v) => { v.insert(inputs); }
        Entry::Occupied(mut o) => {
            INPUT_COUNTS[2].fetch_add(1, Ordering::Relaxed);
            o.insert(None);
        } // ambiguous: do not interpolate
    }
}
fn take(frame: u64) -> Option<Inputs> {
    let mut map = INPUTS.lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|&id, _| id >= frame);
    map.remove(&frame).flatten()
}

struct Timer {
    heap: ID3D12QueryHeap, readback: ID3D12Resource, frequency: u64,
}
impl Timer {
    fn new(host: &Host) -> Option<Self> {
        let frequency = unsafe { host.queue().GetTimestampFrequency() }.ok()?;
        if frequency == 0 { return None; }
        let mut heap: Option<ID3D12QueryHeap> = None;
        unsafe { host.device.CreateQueryHeap(&D3D12_QUERY_HEAP_DESC {
            Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: 4, NodeMask: 0,
        }, &mut heap).ok()?; }
        Some(Self { heap: heap?, readback: host.read_back_buffer(32)?, frequency })
    }
    fn begin(&self, list: &ID3D12GraphicsCommandList, i: u32) {
        unsafe { list.EndQuery(&self.heap, D3D12_QUERY_TYPE_TIMESTAMP, i); }
    }
    fn end(&self, list: &ID3D12GraphicsCommandList, i: u32) {
        unsafe {
            list.EndQuery(&self.heap, D3D12_QUERY_TYPE_TIMESTAMP, i + 1);
            list.ResolveQueryData(&self.heap, D3D12_QUERY_TYPE_TIMESTAMP, i, 2, &self.readback, u64::from(i) * 8);
        }
    }
    fn read(&self, interpolation: bool) -> (f64, Option<f64>) {
        let mut mapped = null_mut();
        let range = D3D12_RANGE { Begin: 0, End: if interpolation { 32 } else { 16 } };
        unsafe {
            if self.readback.Map(0, Some(&range), Some(&mut mapped)).is_err() || mapped.is_null() {
                return (0.0, None);
            }
            let stamps = std::slice::from_raw_parts(mapped.cast::<u64>(), if interpolation { 4 } else { 2 });
            let delta = |i: usize| stamps[i+1].saturating_sub(stamps[i]) as f64 * 1000.0 / self.frequency as f64;
            let out = (delta(0), interpolation.then(|| delta(2)));
            self.readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
            out
        }
    }
}
struct Callback {
    ctx: usize, dispatch: Call, reset_frame: AtomicU64,
    generated: AtomicU64, failed: AtomicU64, measured: AtomicU64,
    timer: Option<Timer>,
    audit: Option<image_audit::Audit>,
}
unsafe extern "C" fn interpolate(desc: *mut Generation, user: Ptr) -> u32 {
    if desc.is_null() || user.is_null() { return 1; }
    let cb = unsafe { &*user.cast::<Callback>() };
    let d = unsafe { &mut *desc };
    if d.header.kind != DISPATCH_FG || d.list.is_null() { return 1; }
    if cb.reset_frame.load(Ordering::Acquire) == d.frame { d.reset = true; }
    // Borrow the SDK-owned command list without AddRef/Release. Async
    // interpolation is disabled, so this is the game's direct queue clock.
    let list = unsafe { ID3D12GraphicsCommandList::from_raw_borrowed(&d.list) };
    if let (Some(timer), Some(list)) = (&cb.timer, list) { timer.begin(list, 2); }
    let mut ctx = cb.ctx as Ptr;
    let rc = unsafe { (cb.dispatch)(&mut ctx, &d.header) };
    if let (Some(timer), Some(list)) = (&cb.timer, list) {
        timer.end(list, 2);
        cb.measured.store(if rc == 0 { d.frame } else { 0 }, Ordering::Release);
    }
    if rc == 0 {
        if !d.reset {
            cb.generated.fetch_add(u64::from(d.count), Ordering::Relaxed);
            if d.count > 0 && let (Some(audit), Some(list)) = (&cb.audit, list) {
                audit.record(list, d.frame, &d.color, &d.outputs[0]);
            }
        }
    } else {
        cb.failed.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(frame = d.frame, rc, "FFX FG: interpolation dispatch failed");
    }
    rc
}
struct Session {
    ctx: usize, callback: Box<Callback>, display: [u32; 2], render: [u32; 2],
    flags: u32, hudless: Option<ID3D12Resource>, last: u64,
}
pub(super) struct Target<'a> {
    pub host: &'a Host, pub copy: &'a HostList, pub swap: &'a IDXGISwapChain3,
    pub size: [u32; 2], pub format: DXGI_FORMAT,
    pub hudless: Option<(&'a ID3D12Resource, D3D12_RESOURCE_STATES)>,
}
pub(super) struct Proxy {
    api: Api, swap_ctx: usize, swap_address: usize, provider: u64, session: Option<Session>,
    held: Option<Inputs>, active: bool, last_frame: u64, failed: bool,
    meter: Instant, native: u64, last_present: Option<u32>,
    pacing: Pacing,
    generated_base: u64, failure_base: u64, prepare_ms: f64, interpolate_ms: f64, samples: u64,
}
impl Proxy {
    pub(super) fn new(host: &Host, factory: &IDXGIFactory2, hwnd: HWND, desc: &DXGI_SWAP_CHAIN_DESC1) -> Result<(IDXGISwapChain3, Self)> {
        if std::env::var_os("VICTORY_FG").is_some_and(|v| v == "0") { return Err("disabled by VICTORY_FG=0".into()); }
        let api = Api::new()?;
        let provider = api.provider(host, CREATE_FG, "3.1.6")?;
        let swap_provider = api.provider(host, CREATE_SWAP, "3.1.")?;
        let mut version = Override { header: header(5), id: swap_provider };
        let mut desc = *desc;
        let mut raw = null_mut();
        let mut create = CreateSwap {
            header: Header { kind: CREATE_SWAP, next: &mut version.header },
            swap: &mut raw, hwnd, desc: &mut desc, fullscreen: null_mut(),
            factory: factory.as_raw(), queue: host.queue().as_raw(),
        };
        let swap_ctx = api.create(&mut create.header)?;
        if raw.is_null() { api.destroy(swap_ctx); return Err("SDK returned no swapchain".into()); }
        let swap4 = unsafe { IDXGISwapChain4::from_raw(raw) };
        let swap = match swap4.cast::<IDXGISwapChain3>() {
            Ok(swap) => swap,
            Err(e) => { api.destroy(swap_ctx); return Err(e.to_string()); }
        };
        fsr::set_generation_status("Off - AMD pacing swapchain ready");
        tracing::info!("FFX FG: native pacing swapchain created (interpolation off)");
        let swap_address = swap.as_raw() as usize;
        Ok((swap, Self { api, swap_ctx, swap_address, provider, session: None, held: None, active: false,
            last_frame: 0, failed: false, meter: Instant::now(), native: 0, last_present: None, pacing: Pacing::new(),
            generated_base: 0, failure_base: 0, prepare_ms: 0.0, interpolate_ms: 0.0, samples: 0 }))
    }
    fn wait(&self) -> Result<()> { self.api.call(self.swap_ctx, &header(WAIT_SWAP), self.api.dispatch) }
    pub(super) fn begin_frame(&mut self) -> bool {
        if !self.active { return true; }
        // Keep callback/context and timing resources valid until AMD has
        // completed the previous interpolation/presents. Conservative first
        // implementation; never overlap the SDK prepare with its callback.
        if let Err(e) = self.wait() {
            self.failed = true;
            fsr::request_generation(false);
            tracing::warn!(%e, "FFX FG: wait for previous presents failed");
            fsr::set_generation_status("Unavailable - SDK wait failed; restart");
            return false;
        }
        if let Some(session) = &self.session {
            if session.callback.failed.load(Ordering::Acquire) != 0 {
                self.failed = true;
                fsr::request_generation(false);
            }
            if let Some(audit) = &session.callback.audit { audit.collect(); }
            if let Some(timer) = &session.callback.timer {
                let measured = session.callback.measured.load(Ordering::Acquire) == self.last_frame;
                let (prepare, interpolation) = timer.read(measured);
                if let Some(interpolation) = interpolation {
                    self.prepare_ms += prepare;
                    self.interpolate_ms += interpolation;
                    self.samples += 1;
                }
            }
        }
        self.held = None;
        true
    }
    fn disable(&mut self, swap: &IDXGISwapChain3, frame: u64, status: &str) {
        if self.active && let Some(session) = &self.session {
            let config = Self::config(session, swap, frame, false, Resource::default());
            if let Err(e) = self.api.call(session.ctx, &config.header, self.api.configure) {
                tracing::warn!(%e, "FFX FG: disable failed");
                self.failed = true;
                fsr::request_generation(false);
                fsr::set_generation_status("Unavailable - SDK disable failed; restart");
                // Do not call Present with the old interpolation callback
                // still enabled and without inputs for this frame.
                return;
            }
        }
        self.active = false;
        if let Some(s) = &self.session { if let Some(audit) = &s.callback.audit { audit.reset(); } }
        fsr::set_generation_status(status);
    }
    pub(super) fn blocked(&self) -> bool { self.active && self.failed }
    pub(super) fn before_resize(&mut self, swap: &IDXGISwapChain3, frame: u64) -> bool {
        if !self.begin_frame() { return false; }
        self.disable(swap, frame, "Off - swapchain resizing");
        if self.blocked() { return false; }
        if !self.failed && let Some(session) = self.session.take() {
            self.api.destroy(session.ctx);
        }
        self.held = None;
        self.generated_base = 0;
        self.failure_base = 0;
        self.last_frame = 0;
        self.pacing = Pacing::new();
        self.last_present = None;
        self.native = 0;
        self.meter = Instant::now();
        *METRICS.lock().unwrap_or_else(|e| e.into_inner()) = None;
        true
    }
    fn config(s: &Session, swap: &IDXGISwapChain3, frame: u64, enabled: bool, hudless: Resource) -> Configure {
        Configure { header: header(CONFIGURE_FG), swap: swap.as_raw(), present_callback: null_mut(), present_user: null_mut(),
            generate_callback: enabled.then_some(interpolate as CallbackFn), generate_user: if enabled { std::ptr::from_ref(s.callback.as_ref()).cast_mut().cast() } else { null_mut() },
            enabled, async_work: false, hudless, flags: 0, only_generated: false,
            rect: [0, 0, s.display[0] as i32, s.display[1] as i32], frame }
    }
    fn session(&self, host: &Host, size: [u32; 2], render: [u32; 2], flags: u32) -> Result<Session> {
        let mut backend = Backend { header: header(BACKEND_DX12), device: host.device.as_raw() };
        let mut version = Override { header: Header { kind: 5, next: &mut backend.header }, id: self.provider };
        let mut desc = CreateFg { header: Header { kind: CREATE_FG, next: &mut version.header },
            flags, display: size, render, format: 10 };
        let ctx = self.api.create(&mut desc.header)?;
        let callback = Box::new(Callback { ctx, dispatch: self.api.dispatch, reset_frame: AtomicU64::new(0),
            generated: AtomicU64::new(0), failed: AtomicU64::new(0), measured: AtomicU64::new(0), timer: Timer::new(host),
            audit: image_audit::Audit::new(host) });
        tracing::info!(?size, ?render, flags, "FFX FG: native 3.1.6 context created");
        Ok(Session { ctx, callback, display: size, render, flags, hudless: None, last: 0 })
    }
    pub(super) fn prepare(&mut self, frame: u64, t: Target<'_>) {
        let inputs = take(frame);
        if self.failed {
            self.disable(t.swap, frame, "Unavailable - SDK error; restart");
            return;
        }
        if !fsr::generation_requested() {
            self.disable(t.swap, frame, "Off - AMD pacing swapchain ready");
            return;
        }
        if super::settings_menu::is_open() {
            self.disable(t.swap, frame, "On - paused while settings are open");
            return;
        }
        let Some(inputs) = inputs else {
            INPUT_COUNTS[3].fetch_add(1, Ordering::Relaxed);
            self.disable(t.swap, frame, "On - waiting for matching depth and motion vectors");
            return;
        };
        if t.format != DXGI_FORMAT_R8G8B8A8_UNORM || inputs.data.output != t.size {
            self.disable(t.swap, frame, "On - unsupported presentation format or resolution");
            return;
        }
        // These three flags share the public upscale/FG bit positions.
        // HDR is determined by the actual presentation format (SDR here).
        let flags = inputs.data.upscale_flags & 0x1c;
        if self.session.as_ref().is_some_and(|s| (s.display, s.render, s.flags) != (t.size, inputs.data.render, flags)) {
            self.disable(t.swap, frame, "On - rebuilding context");
            if self.failed { return; }
            if let Some(s) = self.session.take() { self.api.destroy(s.ctx); }
            self.generated_base = 0;
            self.failure_base = 0;
        }
        if self.session.is_none() {
            match self.session(t.host, t.size, inputs.data.render, flags) {
                Ok(s) => self.session = Some(s),
                Err(e) => { self.fail(t.swap, frame, e); return; }
            }
        }
        let Some(s) = self.session.as_mut() else { return; };
        let list = &t.copy.list;
        // The two-plane compositor already separates the base scene. Copy it
        // to an SDK-owned-lifetime texture so next frame's title writes cannot
        // overwrite the HUD-less color while AMD is interpolating.
        let hudless = if let Some((base, state)) = t.hudless {
            match copy_hudless(t.host, t.copy, s, base, state) {
                Ok(r) => r,
                Err(e) => { self.fail(t.swap, frame, e); return; }
            }
        } else { Resource::default() };
        let config = Self::config(s, t.swap, frame, true, hudless);
        if let Err(e) = self.api.call(s.ctx, &config.header, self.api.configure) {
            self.fail(t.swap, frame, e); return;
        }
        self.active = true;
        let data = inputs.data;
        let reset = data.reset || s.last.checked_add(1) != Some(frame) || s.last == 0;
        s.callback.reset_frame.store(if reset { frame } else { 0 }, Ordering::Release);
        let desired = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
        let mut barriers = Vec::new();
        let mut restore = Vec::new();
        for r in [&inputs.depth, &inputs.motion] {
            t.copy.keep(&r.texture);
            let state = D3D12_RESOURCE_STATES(r.state.load(Ordering::Acquire) as i32);
            if state != desired {
                barriers.push(transition(&r.texture, state, desired));
                restore.push(transition(&r.texture, desired, state));
            }
        }
        unsafe { if !barriers.is_empty() { list.ResourceBarrier(&barriers); } }
        if let Some(timer) = &s.callback.timer { timer.begin(list, 0); }
        let prep = Prepare { header: header(PREPARE_FG), frame, flags: 0, list: list.as_raw(),
            render: data.render, jitter: data.jitter, motion_scale: data.motion_scale,
            dt: data.dt_ms, unused_reset: false, near: data.near, far: data.far, fov: data.fov,
            meters: if data.meters > 0.0 { data.meters } else { 1.0 },
            depth: resource(&inputs.depth.texture, 28, 4), motion: resource(&inputs.motion.texture, 18, 4) };
        let result = self.api.call(s.ctx, &prep.header, self.api.dispatch);
        if let Some(timer) = &s.callback.timer { timer.end(list, 0); }
        unsafe { if !restore.is_empty() { list.ResourceBarrier(&restore); } }
        // Even a failed dispatch may have recorded work: retain until the
        // next SDK wait / presenter shutdown, rather than freeing early.
        self.held = Some(inputs);
        if let Err(e) = result { self.fail(t.swap, frame, e); return; }
        s.last = frame;
        self.last_frame = frame;
        fsr::set_generation_status("On - native FSR 3.1.6");
        if reset { tracing::info!(frame, "FFX FG: temporal history reset"); }
    }
    fn fail(&mut self, swap: &IDXGISwapChain3, frame: u64, error: String) {
        tracing::warn!(frame, %error, "FFX FG: disabling after SDK error");
        self.disable(swap, frame, "Unavailable - SDK error; restart");
        self.failed = true;
        fsr::request_generation(false);
    }
    pub(super) fn presented(&mut self, swap: &IDXGISwapChain3, ok: bool) {
        if ok { self.native += 1; }
        self.pacing.observe(swap);
        let elapsed = self.meter.elapsed().as_secs_f64();
        if elapsed < 1.0 { return; }
        let count = unsafe { swap.GetLastPresentCount() }.ok();
        let actual = count.zip(self.last_present).map(|(a,b)| f64::from(a.wrapping_sub(b)) / elapsed);
        self.last_present = count;
        let (generated, failures) = self.session.as_ref().map_or((0,0), |s| (
            s.callback.generated.load(Ordering::Relaxed), s.callback.failed.load(Ordering::Relaxed)));
        let gen_fps = generated.saturating_sub(self.generated_base) as f64 / elapsed;
        let errors = failures.saturating_sub(self.failure_base);
        let completed = self.pacing.interval();
        *METRICS.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), Metrics {
            native: self.native as f64 / elapsed, generated: gen_fps, submitted: actual, completed,
        }));
        self.generated_base = generated;
        self.failure_base = failures;
        let [captured, rejected, ambiguous, missing] = INPUT_COUNTS.each_ref().map(|n| n.swap(0, Ordering::Relaxed));
        if fsr::generation_requested() || gen_fps > 0.0 || errors > 0 {
            tracing::info!(captured, rejected, ambiguous, missing, native_fps = self.native as f64 / elapsed, generated_dispatch_fps = gen_fps,
                dxgi_present_fps = ?actual, errors,
                prepare_gpu_ms = self.prepare_ms / self.samples.max(1) as f64,
                interpolation_gpu_ms = self.interpolate_ms / self.samples.max(1) as f64,
                timing_samples = self.samples, "FFX FG: frame rates and GPU timings");
        }
        self.native = 0;
        self.prepare_ms = 0.0;
        self.interpolate_ms = 0.0;
        self.samples = 0;
        self.meter = Instant::now();
        if errors != 0 {
            self.failed = true;
            fsr::request_generation(false);
        }
    }
}
impl Drop for Proxy {
    fn drop(&mut self) {
        // Presenter lives for the process. At shutdown the SDK owns a ref
        // to the proxy; wait for its work before destroying either context.
        let raw = self.swap_address as Ptr;
        let waited = self.wait().is_ok();
        if waited && let Some(swap) = unsafe { IDXGISwapChain3::from_raw_borrowed(&raw) } {
            self.disable(swap, self.last_frame.saturating_add(1), "Off");
            if !self.blocked() {
                if let Some(s) = self.session.take() { self.api.destroy(s.ctx); }
                self.api.destroy(self.swap_ctx);
                return;
            }
        }
        // Failure to drain/disable means the SDK can still use the
        // callback and resources. Preserve them and the loaded DLL rather
        // than causing an asynchronous use-after-free.
        if let Some(s) = self.session.take() { std::mem::forget(s); }
        if let Some(held) = self.held.take() { std::mem::forget(held); }
        if let Some(lib) = self.api.library.take() { std::mem::forget(lib); }
    }
}

fn copy_hudless(host: &Host, copy: &HostList, session: &mut Session, base: &ID3D12Resource, state: D3D12_RESOURCE_STATES) -> Result<Resource> {
    let d = unsafe { base.GetDesc() };
    if !matches!(d.Format, DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_TYPELESS)
        || d.Width != u64::from(session.display[0]) || d.Height != session.display[1]
        || d.MipLevels != 1 || d.DepthOrArraySize != 1 || d.SampleDesc.Count != 1
    { return Err("unsupported HUD-less base plane".into()); }
    let read = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
    if session.hudless.is_none() {
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC { Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            Alignment: 0, Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_NONE, ..d };
        let mut texture: Option<ID3D12Resource> = None;
        unsafe { host.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, read, None, &mut texture) }
            .map_err(|e| e.to_string())?;
        session.hudless = texture;
    }
    let target = session.hudless.as_ref().ok_or("HUD-less texture allocation returned null")?;
    copy.keep(target);
    copy.keep(base);
    let list = &copy.list;
    let mut pre = vec![transition(target, read, D3D12_RESOURCE_STATE_COPY_DEST)];
    let mut post = vec![transition(target, D3D12_RESOURCE_STATE_COPY_DEST, read)];
    if state != D3D12_RESOURCE_STATE_COPY_SOURCE {
        pre.push(transition(base, state, D3D12_RESOURCE_STATE_COPY_SOURCE));
        post.push(transition(base, D3D12_RESOURCE_STATE_COPY_SOURCE, state));
    }
    unsafe { list.ResourceBarrier(&pre); list.CopyResource(target, base); list.ResourceBarrier(&post); }
    Ok(resource(target, 10, 12))
}

// Verify layout in the regular Windows build as well as tests.
const _: () = {
    assert!(size_of::<Resource>() == 48);
    assert!(size_of::<Prepare>() == 184);
    assert!(std::mem::offset_of!(Prepare, depth) == 88);
    assert!(size_of::<Configure>() == 144);
    assert!(size_of::<Generation>() == 312);
    assert!(std::mem::offset_of!(Generation, frame) == 304);
    assert!(size_of::<CreateSwap>() == 64);
};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sdk_layout() {
        assert_eq!(size_of::<Resource>(), 48);
        assert_eq!(size_of::<Prepare>(), 184);
        assert_eq!(std::mem::offset_of!(Prepare, depth), 88);
        assert_eq!(size_of::<CreateSwap>(), 64);
        assert_eq!(std::mem::offset_of!(Generation, frame), 304);
        assert_eq!(size_of::<Configure>(), 144);
        assert_eq!(size_of::<CreateFg>(), 40);
    }
}
