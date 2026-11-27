//! Live title-side FSR controls. No SDK pointers cross threads.
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sharpening {
    #[default]
    Game,
    Off,
    On(u8),
}
impl Sharpening {
    fn encode(self) -> u32 {
        match self { Self::Game => 0, Self::Off => 1, Self::On(p) => 2 + u32::from(p.min(100)) }
    }
    fn decode(value: u32) -> Self {
        match value { 0 => Self::Game, 1 => Self::Off, n => Self::On((n - 2).min(100) as u8) }
    }
}
static REQUEST: AtomicU32 = AtomicU32::new(0);
pub fn request() -> Sharpening {
    Sharpening::decode(REQUEST.load(Ordering::Acquire))
}
pub fn set_request(value: Sharpening) {
    REQUEST.store(value.encode(), Ordering::Release);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub render: [u32; 2],
    pub upscale: [u32; 2],
    pub sharpening: bool,
    pub sharpness: u8,
    pub applied: Sharpening,
    pub result: u32,
}
static STATUS: Mutex<Option<Status>> = Mutex::new(None);
pub fn status() -> Option<Status> {
    *STATUS.lock().unwrap_or_else(|e| e.into_inner())
}
pub fn publish(value: Status) {
    *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
}
pub fn clear() {
    *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Synchronous observer: called while the original dispatch's arguments are
/// alive. These addresses are title objects, not native COM pointers.
/// The observer must not retain them or issue GPU commands.
pub type Observe = fn([usize; 4], [[u32; 2]; 4]);
static OBSERVER: OnceLock<Observe> = OnceLock::new();
pub fn register_observer(observer: Observe) {
    let _ = OBSERVER.set(observer);
}
pub fn observe(resources: [usize; 4], dimensions: [[u32; 2]; 4]) {
    if let Some(observer) = OBSERVER.get() {
        observer(resources, dimensions);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sharpening_encoding() {
        for p in 0..=100 {
            assert_eq!(Sharpening::decode(Sharpening::On(p).encode()), Sharpening::On(p));
        }
        assert_eq!(Sharpening::decode(0), Sharpening::Game);
        assert_eq!(Sharpening::decode(1), Sharpening::Off);
        assert_eq!(Sharpening::decode(u32::MAX), Sharpening::On(100));
    }
}
