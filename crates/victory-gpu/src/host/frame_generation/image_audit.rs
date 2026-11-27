//! Small, read-only pixel probes of AMD's actual interpolation output.
//! Diagnostics builds only. Two consecutive samples every 16 title frames.
//! All copies execute on AMD's interpolation list after dispatch; the SDK
//! restores input/output to their declared states. Read only after wait.
use super::{Host, Resource, transition};
use std::sync::Mutex;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

const PATCH: u32 = 8;
const GRID: u32 = 4;
const PATCHES: usize = (GRID * GRID) as usize;
const PITCH: usize = 256;
const STRIDE: usize = PITCH * PATCH as usize;
const IMAGE: usize = STRIDE * PATCHES;
const TOTAL: usize = IMAGE * 2;

pub(super) struct Audit {
    buffer: ID3D12Resource,
    state: Mutex<State>,
}
#[derive(Default)]
struct State {
    pending: Option<u64>,
    previous: Option<(u64, Vec<u8>)>,
}
impl Audit {
    pub(super) fn new(host: &Host) -> Option<Self> {
        if !cfg!(feature = "diagnostics") || std::env::var_os("VICTORY_FG_PIXEL_AUDIT").is_some_and(|v| v == "0") {
            return None;
        }
        Some(Self { buffer: host.read_back_buffer(TOTAL as u64)?, state: Mutex::new(State::default()) })
    }
    pub(super) fn reset(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.previous = None;
    }
    pub(super) fn record(&self, list: &ID3D12GraphicsCommandList, frame: u64, native: &Resource, generated: &Resource) {
        if frame % 16 > 1 { return; }
        // SDK's replacement backbuffer is PRESENT and the interpolation
        // destination is UAV. Other formats/states are not probed.
        if native.state != 128 || generated.state != 2 || native.format != 10 || generated.format != 10
            || native.width != generated.width || native.height != generated.height
            || native.width < 64 || native.height < 64 || native.address == generated.address
        { return; }
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.pending.is_some() { return; }
        let Some(a) = (unsafe { ID3D12Resource::from_raw_borrowed(&native.address) }) else { return };
        let Some(b) = (unsafe { ID3D12Resource::from_raw_borrowed(&generated.address) }) else { return };
        if [a,b].iter().any(|r| {
            let d = unsafe { r.GetDesc() };
            d.Format != DXGI_FORMAT_R8G8B8A8_UNORM || d.SampleDesc.Count != 1
                || d.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
                || d.Width != u64::from(native.width) || d.Height != native.height
        }) { return; }
        for (index, (src, state)) in [(a,D3D12_RESOURCE_STATE_PRESENT),(b,D3D12_RESOURCE_STATE_UNORDERED_ACCESS)].into_iter().enumerate() {
            unsafe {
                list.ResourceBarrier(&[transition(src,state,D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                let from = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(src),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
                };
                for i in 0..PATCHES {
                    // Central 60% of the screen, avoiding our HUD/diagnostics.
                    let x = native.width * (2 + 2 * (i as u32 % GRID)) / 10 - PATCH / 2;
                    let y = native.height * (2 + 2 * (i as u32 / GRID)) / 10 - PATCH / 2;
                    let to = D3D12_TEXTURE_COPY_LOCATION {
                        pResource: std::mem::transmute_copy(&self.buffer),
                        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                            PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                                Offset: (index * IMAGE + i * STRIDE) as u64,
                                Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                                    Width: PATCH, Height: PATCH, Depth: 1, RowPitch: PITCH as u32 },
                            },
                        },
                    };
                    let region = D3D12_BOX { left: x, top: y, front: 0, right: x+PATCH, bottom: y+PATCH, back: 1 };
                    list.CopyTextureRegion(&to, 0, 0, 0, &from, Some(&region));
                }
                list.ResourceBarrier(&[transition(src,D3D12_RESOURCE_STATE_COPY_SOURCE,state)]);
            }
        }
        s.pending = Some(frame);
    }
    pub(super) fn collect(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(frame) = s.pending.take() else { return; };
        let mut ptr = std::ptr::null_mut();
        let data = unsafe {
            if self.buffer.Map(0, Some(&D3D12_RANGE { Begin: 0, End: TOTAL }), Some(&mut ptr)).is_err() || ptr.is_null() {
                tracing::warn!(frame, "FFX FG: pixel probe map failed");
                return;
            }
            let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), TOTAL);
            let images = [0,1].map(|index| {
                let mut out = Vec::with_capacity(PATCHES * PATCH as usize * PATCH as usize * 3);
                for i in 0..PATCHES {
                    for row in 0..PATCH as usize {
                        let at = index * IMAGE + i * STRIDE + row * PITCH;
                        for p in bytes[at..at + PATCH as usize * 4].chunks_exact(4) {
                            out.extend_from_slice(&p[..3]);
                        }
                    }
                }
                out
            });
            self.buffer.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
            images
        };
        let [native, generated] = data;
        let (current_mae, current_changed) = difference(&native, &generated);
        let previous = s.previous.as_ref().filter(|(f,_)| f.checked_add(1) == Some(frame));
        let to_previous = previous.map(|(_,p)| difference(p,&generated).0);
        let native_motion = previous.map(|(_,p)| difference(p,&native).0);
        tracing::info!(frame, sampled_pixels = native.len()/3, current_mae, current_changed,
            previous_mae = ?to_previous, native_motion_mae = ?native_motion,
            native_hash = %blake3::hash(&native).to_hex(), generated_hash = %blake3::hash(&generated).to_hex(),
            "FFX FG: actual interpolation pixels (RGB sample, not full frame)");
        s.previous = Some((frame,native));
    }
}
fn difference(a: &[u8], b: &[u8]) -> (f64, usize) {
    let sum: u64 = a.iter().zip(b).map(|(&x,&y)| u64::from(x.abs_diff(y))).sum();
    let changed = a.chunks_exact(3).zip(b.chunks_exact(3)).filter(|(x,y)| x != y).count();
    (sum as f64 / a.len().max(1) as f64, changed)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn rgb_comparison() {
        assert_eq!(difference(&[1,2,3,4,5,6], &[1,2,3,4,5,6]),(0.0,0));
        assert_eq!(difference(&[0,0,0,0,0,0], &[6,0,0,0,0,0]),(1.0,1));
    }
}
