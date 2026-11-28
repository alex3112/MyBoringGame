//! Diagnostics: which host copies' bytes cross the bus, and which way: per
//! copy (title address, bytes) what was sent up and what shaders wrote that
//! was fetched, a frame; and how many different 64 KiB pieces of it. Every
//! five seconds, the eight that move most. For placing resources: a copy whose
//! pieces are fetched every frame and never sent up is written by shaders
//! alone in observed traffic, but this does not prove that the native CPU
//! never reads or writes it. Candidates are evidence for future work only.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// Of a copy, each way (up, fetched): bytes, and the pieces (64 KiB).
#[derive(Default)]
struct Moved {
    bytes: [u64; 2],
    span: (u64, u64),
    local: bool,
    pieces: [HashSet<u32>; 2],
}

pub(super) const UP: usize = 0;
pub(super) const FETCHED: usize = 1;

static MOVED: Mutex<Option<HashMap<u64, Moved>>> = Mutex::new(None);

/// Offsets `ranges` of the copy of `span` (title address, bytes) went `way`.
pub(super) fn moved(way: usize, shadow: &crate::shadow::Shadow, ranges: impl Iterator<Item = (usize, usize)>) {
    let mut all = MOVED.lock().unwrap_or_else(|e| e.into_inner());
    let m = all.get_or_insert_with(Default::default).entry(shadow.id()).or_insert_with(|| Moved {
        span: shadow.span(), local: shadow.card_local(), ..Default::default()
    });
    // (Thousands of runs lie in one piece, one after the other.)
    let mut last = u32::MAX;
    for (from, to) in ranges.filter(|r| r.0 < r.1) {
        m.bytes[way] += (to - from) as u64;
        let (first, end) = ((from >> 16) as u32, ((to - 1) >> 16) as u32);
        if (first, end) != (last, last) {
            m.pieces[way].extend(first..=end);
        }
        last = end;
    }
}

// Conservative *traffic* shortlist only, never an ownership permission.
fn candidate(m: &Moved) -> bool {
    m.local && m.span.1 >= (1 << 20) && m.bytes[UP] == 0 && m.bytes[FETCHED] >= (1 << 20)
}

/// For the log, over `frames` frames.
pub(super) fn line(frames: f64) {
    let all = MOVED.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
    let mut all: Vec<_> = all.into_iter().collect();
    all.sort_by_key(|(_, m)| std::cmp::Reverse(m.bytes[0] + m.bytes[1]));
    let mb = |b: u64| b as f64 / 1e6 / frames.max(1.0);
    let sum = |way: usize| all.iter().map(|(_, m)| m.bytes[way]).sum::<u64>();
    // Copies that only move one way.
    let only = |way: usize| all.iter().filter(|(_, m)| m.bytes[1 - way] == 0).map(|(_, m)| m.bytes[way]).sum::<u64>();
    tracing::info!(
        "landings in title memory looked at by refreshes per frame (VICTORY_LAND_ECHO=0: sent up): {:.1} MB held already and left, \
         {:.1} MB the title's too",
        mb(crate::shadow::ECHO[0].swap(0, std::sync::atomic::Ordering::Relaxed)),
        mb(crate::shadow::ECHO[1].swap(0, std::sync::atomic::Ordering::Relaxed)),
    );
    tracing::info!(
        "bytes of host copies over the bus per frame: {:.1} MB up ({:.1} of copies nothing was fetched from), {:.1} MB fetched \
         ({:.1} of copies nothing was sent up to), {} copies; the most:",
        mb(sum(UP)),
        mb(only(UP)),
        mb(sum(FETCHED)),
        mb(only(FETCHED)),
        all.len()
    );
    if crate::land::gpu_homes_on() {
        let candidates: Vec<_> = all.iter().filter(|(_, m)| candidate(m)).collect();
        let size: u64 = candidates.iter().map(|(_, m)| m.span.1).sum();
        let fetched: u64 = candidates.iter().map(|(_, m)| m.bytes[FETCHED]).sum();
        tracing::info!("GPU homes candidate census: {} local allocation identities, {:.2} MiB nominal allocation coverage, {:.2} MiB/frame requested readback; zero observed uploads in interval, native CPU reads unproven",
            candidates.len(), size as f64 / 1048576.0, fetched as f64 / 1048576.0 / frames.max(1.0));
        for (id, m) in candidates.into_iter().take(8) {
            let (start, len) = m.span;
            tracing::info!("  GPU home candidate id={id} {start:#x}+{len:#x}: {:.2} MiB/frame requested readback, {} different pieces",
                m.bytes[FETCHED] as f64 / 1048576.0 / frames.max(1.0), m.pieces[FETCHED].len());
        }
    }
    for (id, m) in all.into_iter().take(8) {
        let (start, len) = m.span;
        let both = m.pieces[0].intersection(&m.pieces[1]).count();
        tracing::info!(
            "  id={id} {start:#x}+{len:#x}: {:.2} MB up in {} different pieces, {:.2} MB fetched in {}, {both} of them both ways",
            mb(m.bytes[0]),
            m.pieces[0].len(),
            mb(m.bytes[1]),
            m.pieces[1].len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn candidate_is_not_a_cpu_access_proof() {
        let mut m = Moved { local: true, span: (0x10000, 2 << 20), bytes: [0, 1 << 20], ..Default::default() };
        assert!(candidate(&m));
        m.bytes[UP] = 4;
        assert!(!candidate(&m));
        m.bytes[UP] = 0;
        m.local = false;
        assert!(!candidate(&m));
        m.local = true;
        m.span.1 = 64 << 10;
        assert!(!candidate(&m));
    }
}
