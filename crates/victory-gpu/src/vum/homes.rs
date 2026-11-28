//! Persistent device-local homes and versioned CPU publication.
//!
//! The native resource lives with its Shadow and the lists that bind it.
//! This module owns the RAM mirror, dirty CPU intervals, submitted upload
//! versions and exact writeback proofs for that resource. It is not keyed
//! by a guest virtual address that can be reused. GPU ownership is still
//! bounded by the existing CPU-visible fences until raw CPU access can be
//! mediated. No imported page protections are changed here.
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, Weak};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

/// Common native-allocation ownership. The lock order is pending then
/// flights; a completion releases a range only after all its producers
/// have published. Legacy CPU snapshots use the same ownership table.
#[derive(Default)]
pub(crate) struct Coherence {
    pub(crate) pending: Mutex<Ownership>,
    pub(crate) flights: Mutex<Flights>,
}

/// The live GPU-write ownership of one native allocation. The exact
/// declarations are retained for Flights' producer counts. The disjoint
/// coverage is derived from those declarations at the same lock boundary,
/// never inferred from a guest address or from a completed GPU fence.
#[derive(Default)]
pub(crate) struct Ownership {
    exact: Vec<(usize, usize)>,
    covered: Vec<(usize, usize)>,
}

impl Ownership {
    pub(crate) fn push(&mut self, range: (usize, usize)) {
        self.exact.push(range);
        let (mut from, mut to) = range;
        if from >= to { return; }
        let first = self.covered.partition_point(|r| r.1 < from);
        let mut last = first;
        while last < self.covered.len() && self.covered[last].0 <= to {
            from = from.min(self.covered[last].0);
            to = to.max(self.covered[last].1);
            last += 1;
        }
        self.covered.splice(first..last, [(from, to)]);
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&(usize, usize)) -> bool) {
        let before = self.exact.len();
        self.exact.retain(|r| keep(r));
        if self.exact.len() != before {
            self.covered = joined(self.exact.iter().copied().filter(|r| r.0 < r.1).collect());
        }
    }

    pub(crate) fn iter(&self) -> std::slice::Iter<'_, (usize, usize)> { self.exact.iter() }
    pub(crate) fn contains(&self, range: &(usize, usize)) -> bool { self.exact.contains(range) }
    pub(crate) fn is_empty(&self) -> bool { self.exact.is_empty() }

    pub(crate) fn overlaps(&self, from: usize, to: usize) -> &[(usize, usize)] {
        let first = self.covered.partition_point(|r| r.1 <= from);
        let last = first + self.covered[first..].partition_point(|r| r.0 < to);
        &self.covered[first..last]
    }

    pub(crate) fn gaps(&self, from: usize, to: usize) -> Vec<(usize, usize)> {
        let mut gaps = Vec::new();
        let mut at = from;
        for &(a, b) in self.overlaps(from, to) {
            if at < a { gaps.push((at, a.min(to))); }
            at = at.max(b);
        }
        if at < to { gaps.push((at, to)); }
        gaps
    }
}


/// With [`relay`]: how many submissions that are not copied back yet
/// declared each range shaders may be writing (range, count, the last
/// submission that declared it).
///
/// Without it a range declared by two flights was one entry, and the end
/// of the first flight's copy-back removed it while the second was still
/// writing: the next refresh would have put title bytes over what the
/// second flight's shaders were writing.
#[derive(Default)]
pub(crate) struct Flights(std::collections::HashMap<(usize, usize), (u32, u64)>);

impl Flights {
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool { self.0.is_empty() }

    /// Submission `number` declares `range`: counted once for it, however
    /// often it declares it.
    pub(crate) fn declare(&mut self, range: (usize, usize), number: u64) {
        let f = self.0.entry(range).or_insert((0, number));
        if f.0 == 0 || f.1 != number {
            *f = (f.0 + 1, number);
        }
    }

    /// One submission that declared `ranges` (each once) is copied back:
    /// the ranges no other submission still holds, in order.
    pub(crate) fn done(&mut self, ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
        let mut free = Vec::new();
        for range in ranges {
            match self.0.get_mut(range) {
                Some(f) if f.0 > 1 => f.0 -= 1,
                Some(_) => {
                    self.0.remove(range);
                    free.push(*range);
                }
                // Not counted (declared before anything counted): free.
                None => free.push(*range),
            }
        }
        free.sort_unstable();
        free
    }
}


pub(crate) struct Home {
    pub(crate) local: bool,
    pub(crate) staging: ID3D12Resource,
    pub(crate) before: Mutex<Option<ID3D12Resource>>,
    pub(crate) dirty: Mutex<Vec<(usize, usize)>>,
    pub(crate) sent: Mutex<SentRuns>,
    pub(crate) touched: AtomicBool,
    pub(crate) owner: OnceLock<Weak<crate::shadow::Shadow>>,
    pub(crate) size: u64,
    // At most one exact proof per 64 KiB piece, lazily allocated, no
    // references to descriptors or other resources. Bound by home size.
    publication: Mutex<Publication>,
}

impl Home {
    pub(crate) fn new(local: bool, staging: ID3D12Resource, size: u64) -> Self {
        Self { local, staging, before: Mutex::default(), dirty: Mutex::default(),
            sent: Mutex::default(), touched: AtomicBool::new(false), owner: OnceLock::new(),
            size, publication: Mutex::default() }
    }

    pub(crate) fn cpu_changed(&self, runs: impl Iterator<Item=(usize,usize)>, on_edge: impl FnOnce()) {
        let mut dirty = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
        for (from,to) in runs {
            if from >= to { continue; }
            match dirty.last_mut() {
                Some(last) if from <= last.1 && to >= last.0 => *last = (last.0.min(from), last.1.max(to)),
                _ => dirty.push((from,to)),
            }
        }
        // Publish the dirty edge while still holding the range lock.
        if !self.touched.swap(true, Ordering::AcqRel) { on_edge(); }
    }

    pub(crate) fn take_uploads(&self, relay: bool, sent_as: u64, fetched: u64) -> Vec<(usize,usize)> {
        let mut dirty = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
        self.touched.store(false, Ordering::Release);
        let ranges = joined(std::mem::take(&mut *dirty));
        if relay {
            let mut sent = self.sent.lock().unwrap_or_else(|e| e.into_inner());
            sent.forget(fetched);
            sent.note_joined(sent_as, ranges.clone());
        }
        ranges
    }

    /// A GPU snapshot predates these CPU writes. Preserve them in both
    /// the mirror and the title memory when merging GPU output.
    pub(crate) fn newer_cpu_bytes(&self, after: u64, from: usize, to: usize) -> Vec<(usize,usize)> {
        let dirty = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
        let sent = self.sent.lock().unwrap_or_else(|e| e.into_inner());
        let mut skip = dirty.iter().copied().filter(|&(a,b)| a < to && b > from).collect();
        sent.near(after, from, to, &mut skip);
        skip
    }

    pub(crate) fn publish(&self, piece: u32, lo: usize, hi: usize, epoch: PublicationEpoch) {
        if epoch.current() {
            self.publication.lock().unwrap_or_else(|e| e.into_inner()).put(piece, lo, hi, epoch);
        }
    }

    pub(crate) fn published(&self, piece: u32, lo: usize, hi: usize, fence: u64) -> bool {
        // A mapping change, even at an unrelated address, conservatively
        // invalidates all certificates. Never skip for a new producer.
        let token = token();
        self.publication.lock().unwrap_or_else(|e| e.into_inner()).covers(piece, lo, hi, fence, token)
            && token == self::token()
    }
}

/// What went up, by send: its number, and the offsets, in order and joined
/// (a refresh sends thousands of runs of a few words).
#[derive(Default)]
pub(crate) struct SentRuns(Vec<(u64, Vec<(usize, usize)>)>);

impl SentRuns {
    #[cfg(test)]
    pub(crate) fn note(&mut self, number: u64, ranges: Vec<(usize, usize)>) {
        self.note_joined(number, joined(ranges));
    }

    /// `ranges` are in order and joined.
    pub(crate) fn note_joined(&mut self, number: u64, ranges: Vec<(usize, usize)>) {
        if ranges.is_empty() {
            return;
        }
        match self.0.iter_mut().find(|s| s.0 == number) {
            Some((_, all)) => *all = joined([std::mem::take(all), ranges].concat()),
            None => self.0.push((number, ranges)),
        }
    }

    /// Sends up to `number` are of no use any more.
    pub(crate) fn forget(&mut self, number: u64) {
        self.0.retain(|s| s.0 > number);
    }

    /// Adds to `out` what sends after `number` took of `[from, to)`.
    pub(crate) fn near(&self, number: u64, from: usize, to: usize, out: &mut Vec<(usize, usize)>) {
        for (_, ranges) in self.0.iter().filter(|s| s.0 > number) {
            let first = ranges.partition_point(|r| r.1 <= from);
            out.extend(ranges[first..].iter().take_while(|r| r.0 < to).copied());
        }
    }

    #[cfg(test)]
    pub(crate) fn of(all: &[(usize, usize, u64)]) -> SentRuns {
        let mut sent = SentRuns::default();
        for &(a, b, number) in all {
            sent.note(number, vec![(a, b)]);
        }
        sent
    }
}


fn joined(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    // (The sort that finds what is in order already: they come as a few
    // stretches of tens of thousands in order, put behind one another.)
    ranges.sort();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (from, to) in ranges {
        match out.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    out
}

static INVALIDATION: AtomicU64 = AtomicU64::new(0);
pub(crate) fn invalidate() { INVALIDATION.fetch_add(1, Ordering::SeqCst); }
type MappingToken = (u64,u64);
fn token() -> MappingToken {
    (crate::watch::changes(), INVALIDATION.load(Ordering::SeqCst))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PublicationEpoch {
    fence: u64,
    mapping: MappingToken,
}
impl PublicationEpoch {
    pub(crate) fn new(fence: u64) -> Option<Self> {
        let mapping = token();
        (fence != 0 && mapping.0 % 2 == 0).then_some(Self { fence, mapping })
    }
    fn current(self) -> bool { self.mapping.0 % 2 == 0 && self.mapping == token() }
}

#[derive(Default)]
struct Publication {
    pieces: HashMap<u32, (usize,usize,PublicationEpoch)>,
}
impl Publication {
    fn put(&mut self, piece:u32, lo:usize, hi:usize, epoch:PublicationEpoch) {
        if lo >= hi { return; }
        // Do not combine disjoint subranges into a fictitious full
        // coverage. Prefer the newer proof; discard the old one.
        self.pieces.insert(piece,(lo,hi,epoch));
    }
    fn covers(&self,piece:u32,lo:usize,hi:usize,fence:u64,mapping:MappingToken)->bool {
        lo < hi && fence != 0 && mapping.0 % 2 == 0 &&
            self.pieces.get(&piece).is_some_and(|&(a,b,e)| a <= lo && hi <= b && e.fence >= fence && e.mapping == mapping)
    }
}

/// One local writeback snapshot for the relay. The direct queue copies
/// from the homes into this resource, signals ready; the copy queue waits
/// and reads only this snapshot. The relay waits its existing fetch fence
/// before the snapshot or its allocator can be reused. No direct-queue
/// wait for the copy queue and no simultaneous reads of changing homes.
pub(crate) struct Snapshot {
    buffer: ID3D12Resource,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    ready: ID3D12Fence,
    capacity: u64,
}
impl Snapshot {
    pub(crate) fn new(device:&ID3D12Device,capacity:u64,make:impl FnOnce()->Option<ID3D12Resource>)->Option<Self> {
        let buffer = make()?;
        // SAFETY: create private native objects, no outstanding commands.
        let made = unsafe {
            (|| -> windows::core::Result<Self> {
                let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                let list: ID3D12GraphicsCommandList = device.CreateCommandList(0,D3D12_COMMAND_LIST_TYPE_DIRECT,&allocator,None)?;
                list.Close()?;
                let ready: ID3D12Fence = device.CreateFence(0,D3D12_FENCE_FLAG_NONE)?;
                Ok(Self{buffer,allocator,list,ready,capacity})
            })()
        };
        match made {
            Ok(s) => { tracing::info!(capacity, "VUM homes: ordered local writeback snapshot created"); Some(s) }
            Err(e) => { tracing::warn!(error=%e,"VUM homes: snapshot unavailable; ordinary relay retained"); None }
        }
    }
    pub(crate) fn capacity(&self)->u64 { self.capacity }
    pub(crate) fn buffer(&self)->&ID3D12Resource { &self.buffer }
    pub(crate) fn list(&self)->&ID3D12GraphicsCommandList { &self.list }
    /// SAFETY: caller has waited for the preceding fetch fence.
    pub(crate) unsafe fn begin(&self)->windows::core::Result<()> {
        unsafe { self.allocator.Reset()?; self.list.Reset(&self.allocator,None) }
    }
    pub(crate) unsafe fn close(&self)->windows::core::Result<()> { unsafe { self.list.Close() } }
    /// The caller holds QUEUE_ORDER. All buffers are in implicit COMMON
    /// between ExecuteCommandLists calls, as for the existing relay fetch.
    pub(crate) unsafe fn enqueue(&self,direct:&ID3D12CommandQueue,copy:&ID3D12CommandQueue,sequence:u64)->windows::core::Result<()> {
        unsafe {
            direct.ExecuteCommandLists(&[Some(self.list.cast::<ID3D12CommandList>()?)]);
            direct.Signal(&self.ready,sequence)?;
            copy.Wait(&self.ready,sequence)
        }
    }
}

pub(crate) fn writeback_on()->bool { crate::toggled!("VICTORY_VUM_HOME_WRITEBACK") }
static STATS:[AtomicU64;4]=[const{AtomicU64::new(0)};4];
pub(crate) fn snapshot_sent(bytes:u64) {
    STATS[0].fetch_add(1,Ordering::Relaxed);
    STATS[1].fetch_add(bytes,Ordering::Relaxed);
}
pub(crate) fn reused(bytes:u64) {
    STATS[2].fetch_add(1,Ordering::Relaxed);
    STATS[3].fetch_add(bytes,Ordering::Relaxed);
}
pub(crate) fn line(frames:f64) {
    let [snapshots,bytes,hits,saved]=STATS.each_ref().map(|n| n.swap(0,Ordering::Relaxed) as f64/frames.max(1.0));
    tracing::info!("VUM home publication (VICTORY_VUM_HOME_WRITEBACK={}): {:.1} ordered snapshots, {:.2} MiB snapshot data, {:.1} redundant readbacks omitted, {:.2} MiB omitted/frame; CPU-visible fences remain eager",
        u8::from(writeback_on()),snapshots,bytes/1048576.0,hits,saved/1048576.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_publication_requires_generation_and_producer() {
        let mut p=Publication::default();
        let e=PublicationEpoch{fence:7,mapping:(2,1)};
        p.put(3,10,20,e);
        assert!(p.covers(3,11,19,7,(2,1)));
        assert!(!p.covers(3,9,19,7,(2,1)));
        assert!(!p.covers(3,11,19,8,(2,1)));
        assert!(!p.covers(3,11,19,7,(4,1)));
        assert!(!p.covers(3,11,19,7,(2,2)));
        assert!(!p.covers(4,11,19,7,(2,1)));
        p.put(3,30,40,PublicationEpoch{fence:8,mapping:(2,1)});
        assert!(!p.covers(3,10,40,7,(2,1)));
        assert!(p.covers(3,30,40,8,(2,1)));
    }
}
