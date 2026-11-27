//! Settings overlay drawn into the game's own D3D12 backbuffer. No HWND
//! is created. The CPU paints an antialiased UI only when it changes;
//! immutable upload buffers are retained by the in-flight host list.
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows_sys::Win32::Foundation::{POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyboardLayout, MapVirtualKeyExW, VkKeyScanExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CURSORINFO, CURSOR_SHOWING, GetClientRect, GetCursorInfo, GetCursorPos,
    GetForegroundWindow, GetWindowThreadProcessId,
};

use super::{Host, HostList};
use victory_core::fsr::{self, Sharpening, Status};

static OPEN: AtomicBool = AtomicBool::new(false);
pub(crate) fn is_open() -> bool {
    OPEN.load(Ordering::Acquire)
}

const W: u32 = 800;
const H: u32 = 535;
const BG: [u8; 3] = [14, 25, 45];
const BLUE: [u8; 3] = [31, 65, 123];
const BUTTON: [u8; 3] = [27, 45, 76];
const HOVER: [u8; 3] = [45, 88, 153];
const TEXT: [u8; 3] = [226, 236, 252];
const MUTED: [u8; 3] = [155, 181, 218];
const ACCENT: [u8; 3] = [140, 192, 255];

#[derive(Clone, Copy)]
struct Hit {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}
impl Hit {
    fn contains(self, (x, y): (u32, u32)) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}
const SHARP: Hit = Hit { x: 390, y: 194, w: 382, h: 37 };
const MINUS: Hit = Hit { x: 390, y: 238, w: 55, h: 37 };
const PLUS: Hit = Hit { x: 717, y: 238, w: 55, h: 37 };
const RESET: Hit = Hit { x: 390, y: 282, w: 382, h: 37 };
const PROBE: Hit = Hit { x: 390, y: 367, w: 382, h: 37 };
const CLOSE: Hit = Hit { x: 558, y: 477, w: 214, h: 40 };

#[derive(Clone, Copy, PartialEq, Eq)]
struct Layout {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}
impl Layout {
    fn new((width, height): (u32, u32)) -> Option<Self> {
        if width < 320 || height < 240 {
            return None;
        }
        let w = (width * 78 / 100).min(height * 86 / 100 * W / H).min(1600);
        let h = w * H / W;
        Some(Self { x: (width - w) / 2, y: (height - h) / 2, w, h })
    }
    fn local(self, x: u32, y: u32) -> Option<(u32, u32)> {
        let x = x.checked_sub(self.x)?;
        let y = y.checked_sub(self.y)?;
        (x < self.w && y < self.h).then(|| (x * W / self.w, y * H / self.h))
    }
    fn sx(self, x: u32) -> u32 { x * self.w / W }
    fn sy(self, y: u32) -> u32 { y * self.h / H }
}

fn key(vk: i32) -> (bool, bool) {
    let v = unsafe { GetAsyncKeyState(vk) };
    (v < 0, v & 1 != 0)
}
fn toggle_key(hwnd: HWND) -> i32 {
    let thread = unsafe { GetWindowThreadProcessId(hwnd.0, null_mut()) };
    let layout = unsafe { GetKeyboardLayout(thread) };
    let mapped = unsafe { VkKeyScanExW(0x00b2, layout) };
    if mapped == -1 {
        (unsafe { MapVirtualKeyExW(0x29, 3, layout) }) as i32
    } else {
        i32::from(mapped) & 0xff
    }
}
fn pointer(hwnd: HWND, size: (u32, u32), layout: Layout) -> Option<(u32, u32)> {
    let mut point = POINT::default();
    let mut rect = RECT::default();
    unsafe {
        if GetCursorPos(&mut point) == 0 || ScreenToClient(hwnd.0, &mut point) == 0
            || GetClientRect(hwnd.0, &mut rect) == 0 {
            return None;
        }
    }
    if rect.right <= 0 || rect.bottom <= 0 || point.x < 0 || point.y < 0
        || point.x >= rect.right || point.y >= rect.bottom {
        return None;
    }
    layout.local(
        (i64::from(point.x) * i64::from(size.0) / i64::from(rect.right)) as u32,
        (i64::from(point.y) * i64::from(size.1) / i64::from(rect.bottom)) as u32,
    )
}
fn hidden_cursor() -> bool {
    let mut info = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
    unsafe { GetCursorInfo(&mut info) != 0 && info.flags & CURSOR_SHOWING == 0 }
}

struct Canvas {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    layout: Layout,
}
impl Canvas {
    fn new(layout: Layout) -> Option<Self> {
        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = layout.w as i32;
        info.bmiHeader.biHeight = -(layout.h as i32);
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
            if dc.is_null() { return None; }
            let mut bits = null_mut();
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bitmap.is_null() || bits.is_null() {
                if !bitmap.is_null() { DeleteObject(bitmap); }
                DeleteDC(dc);
                return None;
            }
            let old = SelectObject(dc, bitmap);
            if old.is_null() || old as isize == -1 {
                DeleteObject(bitmap);
                DeleteDC(dc);
                return None;
            }
            SetBkMode(dc, TRANSPARENT as i32);
            Some(Self { dc, bitmap, old, bits: bits.cast(), layout })
        }
    }

    fn rect(&mut self, hit: Hit, rgb: [u8; 3]) {
        let l = self.layout;
        let x0 = l.sx(hit.x).min(l.w);
        let x1 = l.sx(hit.x + hit.w).min(l.w);
        let y0 = l.sy(hit.y).min(l.h);
        let y1 = l.sy(hit.y + hit.h).min(l.h);
        let pixel = [rgb[2], rgb[1], rgb[0], 255];
        unsafe {
            for y in y0..y1 {
                let row = std::slice::from_raw_parts_mut(
                    self.bits.add(((y * l.w + x0) * 4) as usize), ((x1 - x0) * 4) as usize);
                for dst in row.chunks_exact_mut(4) { dst.copy_from_slice(&pixel); }
            }
        }
    }

    fn text(&self, x: u32, y: u32, text: &str, size: u32, bold: bool, rgb: [u8; 3]) {
        let face: Vec<u16> = "Segoe UI".encode_utf16().chain(Some(0)).collect();
        let text: Vec<u16> = text.encode_utf16().collect();
        let l = self.layout;
        unsafe {
            let font = CreateFontW(-(l.sy(size).max(10) as i32), 0, 0, 0,
                (if bold { FW_BOLD } else { FW_NORMAL }) as i32, 0, 0, 0,
                u32::from(DEFAULT_CHARSET), u32::from(OUT_DEFAULT_PRECIS),
                u32::from(CLIP_DEFAULT_PRECIS), u32::from(ANTIALIASED_QUALITY),
                u32::from(DEFAULT_PITCH), face.as_ptr());
            if font.is_null() { return; }
            let old = SelectObject(self.dc, font);
            if old.is_null() || old as isize == -1 {
                DeleteObject(font);
                return;
            }
            SetTextColor(self.dc, u32::from(rgb[0]) | (u32::from(rgb[1]) << 8) | (u32::from(rgb[2]) << 16));
            TextOutW(self.dc, l.sx(x) as i32, l.sy(y) as i32, text.as_ptr(), text.len() as i32);
            SelectObject(self.dc, old);
            DeleteObject(font);
        }
    }

    fn pixels(&self) -> &[u8] {
        unsafe {
            GdiFlush();
            std::slice::from_raw_parts(self.bits, (self.layout.w * self.layout.h * 4) as usize)
        }
    }
}
impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

fn bytes_per_pixel(format: DXGI_FORMAT) -> Option<usize> {
    match format {
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_R10G10B10A2_UNORM => Some(4),
        DXGI_FORMAT_R16G16B16A16_FLOAT => Some(8),
        _ => None,
    }
}

// 8-bit unorm to IEEE half, for [0,1] only.
fn half(v: u8) -> u16 {
    if v == 0 { return 0; }
    let bits = (f32::from(v) / 255.0).to_bits();
    (((bits >> 23) - 112) << 10 | ((bits >> 13) & 1023)) as u16
}
fn convert(pixel: &[u8], dst: &mut [u8], format: DXGI_FORMAT) {
    let [b, g, r, _] = [pixel[0], pixel[1], pixel[2], pixel[3]];
    match format {
        DXGI_FORMAT_R8G8B8A8_UNORM => dst.copy_from_slice(&[r, g, b, 255]),
        DXGI_FORMAT_B8G8R8A8_UNORM => dst.copy_from_slice(&[b, g, r, 255]),
        DXGI_FORMAT_R10G10B10A2_UNORM => {
            let c = |v: u8| u32::from(v) * 1023 / 255;
            dst.copy_from_slice(&(c(r) | c(g) << 10 | c(b) << 20 | 3 << 30).to_le_bytes());
        }
        DXGI_FORMAT_R16G16B16A16_FLOAT => {
            for (p, v) in dst.chunks_exact_mut(2).zip([r, g, b, 255]) {
                p.copy_from_slice(&half(v).to_le_bytes());
            }
        }
        _ => {}
    }
}

struct Picture {
    resource: ID3D12Resource,
    layout: Layout,
    format: DXGI_FORMAT,
    pitch: u32,
    hover: u8,
    cursor: Option<(u32, u32)>,
    revision: u64,
}

pub(super) struct Menu {
    down: bool,
    mouse_down: bool,
    escape_down: bool,
    status: String,
    revision: u64,
    probe: Option<Receiver<Result<String, String>>>,
    picture: Option<Picture>,
    fsr_status: Option<Status>,
    strength: u8,
}
impl Menu {
    pub(super) fn new() -> Self {
        OPEN.store(false, Ordering::Release);
        fsr::register_observer(observe_resources);
        Self {
            down: false, mouse_down: false, escape_down: false,
            status: "FSR sharpening controls the title's original upscaler. No second upscaler.".into(),
            revision: 0, probe: None, picture: None, fsr_status: None, strength: 30,
        }
    }

    fn status(&mut self, message: String) {
        self.status = message;
        self.revision = self.revision.wrapping_add(1);
    }

    fn action(&mut self, hover: u8) {
        match hover {
            1 if self.probe.is_none() => {
                let (tx, rx) = std::sync::mpsc::channel();
                match std::thread::Builder::new().name("victory-fidelityfx-probe".into()).spawn(move || {
                    let _ = tx.send(super::fidelityfx::probe());
                }) {
                    Ok(_) => {
                        self.probe = Some(rx);
                        self.status("Checking the native FidelityFX runtime...".into());
                    }
                    Err(e) => self.status(format!("Could not start SDK check: {e}")),
                }
            }
            3 => OPEN.store(false, Ordering::Release),
            4 if self.fsr_status.is_some() => {
                let enabled = match fsr::request() {
                    Sharpening::Game => self.fsr_status.is_some_and(|s| s.sharpening),
                    Sharpening::Off => false,
                    Sharpening::On(_) => true,
                };
                fsr::set_request(if enabled { Sharpening::Off } else { Sharpening::On(self.strength) });
                self.status("Sharpening change requested; waiting for the title's next FSR dispatch.".into());
            }
            5 | 6 if self.fsr_status.is_some() => {
                let current = match fsr::request() {
                    Sharpening::On(p) => p,
                    _ => self.fsr_status.map_or(0, |s| s.sharpness),
                };
                self.strength = if hover == 5 { current.saturating_sub(5) }
                    else { current.saturating_add(5).min(100) };
                fsr::set_request(Sharpening::On(self.strength));
                self.status("Sharpness change requested; waiting for the next FSR dispatch.".into());
            }
            7 if self.fsr_status.is_some() => {
                fsr::set_request(Sharpening::Game);
                self.status("Restoring the title's original sharpening parameters.".into());
            }
            _ => {}
        }
    }

    fn poll_probe(&mut self) {
        let result = self.probe.as_ref().map(Receiver::try_recv);
        match result {
            Some(Ok(result)) => {
                self.probe = None;
                match result {
                    Ok(versions) => self.status(format!("SDK ready: {versions}. Interpolation still off.")),
                    Err(e) => {
                        tracing::warn!(error = %e, "FidelityFX SDK check failed");
                        self.status(e);
                    }
                }
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.probe = None;
                self.status("SDK check did not complete.".into());
            }
            _ => {}
        }
    }

    fn paint(&self, host: &Host, layout: Layout, format: DXGI_FORMAT, hover: u8, cursor: Option<(u32,u32)>) -> Option<Picture> {
        let mut c = Canvas::new(layout)?;
        c.rect(Hit { x: 0, y: 0, w: W, h: H }, BG);
        c.rect(Hit { x: 16, y: 55, w: 768, h: 39 }, BLUE);
        for (id, rect) in [(1, PROBE), (3, CLOSE), (4, SHARP), (5, MINUS), (6, PLUS), (7, RESET)] {
            c.rect(rect, if hover == id { HOVER } else if id == 3 { BLUE } else { BUTTON });
        }
        c.rect(Hit { x: 450, y: 238, w: 262, h: 37 }, BUTTON);
        c.rect(Hit { x: 28, y: 414, w: 744, h: 1 }, [54, 76, 110]);
        c.text(27, 14, "VIctory", 28, true, ACCENT);
        c.text(627, 23, crate::BUILD_VERSION, 16, false, MUTED);
        c.text(28, 63, "Graphics Settings", 22, true, TEXT);
        c.text(28, 103, "AMD FidelityFX Upscaling", 17, true, ACCENT);
        c.text(28, 133, "FSR Upscaling", 20, false, TEXT);
        c.text(400, 133, if self.fsr_status.is_some_and(|s| s.result == 0) { "Active (original Xbox)" } else { "Waiting for title dispatch" }, 19, false, MUTED);
        c.text(28, 164, "Render / output resolution", 20, false, TEXT);
        let dimensions = self.fsr_status.map_or("Not available".into(), |s| format!("{}x{}  /  {}x{}", s.render[0], s.render[1], s.upscale[0], s.upscale[1]));
        c.text(400, 164, &dimensions, 19, false, MUTED);
        c.text(28, 202, "Sharpening", 20, false, TEXT);
        let requested = fsr::request();
        let state = self.fsr_status;
        let mode = match requested {
            Sharpening::Game => if state.is_some_and(|s| s.sharpening) { "Game default (On)" } else { "Game default (Off)" },
            Sharpening::Off => "Off",
            Sharpening::On(_) => "On",
        };
        let suffix = if state.is_some_and(|s| s.result != 0) { " - SDK error" } else if state.is_some_and(|s| s.applied != requested) { " - pending" } else { "" };
        c.text(411, 202, &format!("{mode}{suffix}"), 19, true, if state.is_some() { TEXT } else { MUTED });
        c.text(28, 246, "Sharpness", 20, false, TEXT);
        c.text(407, 246, "-", 20, true, TEXT);
        c.text(735, 246, "+", 20, true, TEXT);
        let strength = match requested { Sharpening::On(p) => p, _ => state.map_or(0, |s| s.sharpness) };
        c.text(525, 246, &format!("{strength}%"), 20, true, TEXT);
        c.text(28, 290, "Restore title defaults", 20, false, TEXT);
        c.text(411, 290, "Reset sharpening", 19, true, TEXT);
        c.text(28, 329, "AMD Frame Generation", 17, true, ACCENT);
        c.text(28, 351, "Frame Generation", 20, false, TEXT);
        c.text(400, 351, "Off - native dispatch not integrated", 17, false, MUTED);
        c.text(28, 377, "FidelityFX SDK", 20, false, TEXT);
        c.text(411, 377, "Check SDK", 19, true, TEXT);
        // Keep arbitrary loader error messages inside the panel.
        let mut lines = vec![String::new()];
        for word in self.status.split_whitespace() {
            if lines.last().is_some_and(|s| s.chars().count() + word.chars().count() > 84) && lines.len() < 3 {
                lines.push(String::new());
            }
            if let Some(line) = lines.last_mut() {
                if !line.is_empty() { line.push(' '); }
                line.push_str(word);
            }
        }
        for (n, line) in lines.iter().enumerate() {
            let short: String = line.chars().take(92).collect();
            c.text(28, 424 + n as u32 * 17, &short, 15, false, MUTED);
        }
        c.text(28, 488, "² / Esc: close", 17, false, MUTED);
        c.text(634, 488, "Close", 19, true, TEXT);
        if let Some((x,y)) = cursor {
            c.text(x.min(W-24), y.min(H-25), "↖", 24, true, TEXT);
        }

        let bpp = bytes_per_pixel(format)?;
        let pitch = (layout.w * bpp as u32).next_multiple_of(256);
        let size = u64::from(pitch) * u64::from(layout.h);
        let (resource, mapped) = host.upload_buffer(size)?;
        let src = c.pixels();
        unsafe {
            let out = std::slice::from_raw_parts_mut(mapped, size as usize);
            out.fill(0);
            for y in 0..layout.h as usize {
                let src = &src[y * layout.w as usize * 4..(y+1) * layout.w as usize * 4];
                let dst = &mut out[y * pitch as usize..y * pitch as usize + layout.w as usize * bpp];
                for (p, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(bpp)) {
                    convert(p, d, format);
                }
            }
            resource.Unmap(0, None);
        }
        Some(Picture { resource, layout, format, pitch, hover, cursor, revision: self.revision })
    }

    pub(super) fn draw(&mut self, host: &Host, copy: &HostList, back: &ID3D12Resource,
        hwnd: HWND, size: (u32,u32), format: DXGI_FORMAT) {
        let (down, tapped) = key(toggle_key(hwnd));
        let (escape, esc_tap) = key(0x1b);
        let (mouse, mouse_tap) = key(1);
        let focused = unsafe { GetForegroundWindow() == hwnd.0 };
        if focused && (down || tapped) && !self.down {
            OPEN.fetch_xor(true, Ordering::AcqRel);
        }
        if !focused || (is_open() && ((escape || esc_tap) && !self.escape_down)) {
            OPEN.store(false, Ordering::Release);
        }
        let clicked = (mouse || mouse_tap) && !self.mouse_down;
        self.down = down;
        self.escape_down = escape;
        self.mouse_down = mouse;
        if !is_open() {
            self.picture = None;
            return;
        }
        self.poll_probe();
        let current = fsr::status();
        if self.fsr_status != current {
            self.fsr_status = current;
            if let Some(s) = current && s.applied == fsr::request() {
                self.status(if s.result == 0 { format!("Title FSR accepted sharpening: {} ({}%).", if s.sharpening { "On" } else { "Off" }, s.sharpness) } else { format!("Title FSR dispatch returned error {}.", s.result) });
            } else {
                self.revision = self.revision.wrapping_add(1);
            }
        }
        let Some(layout) = Layout::new(size) else {
            OPEN.store(false, Ordering::Release);
            return;
        };
        let pos = pointer(hwnd, size, layout);
        let hover = pos.map_or(0, |p| {
            if PROBE.contains(p) { 1 }
            else if CLOSE.contains(p) { 3 }
            else if self.fsr_status.is_some() && SHARP.contains(p) { 4 }
            else if self.fsr_status.is_some() && MINUS.contains(p) { 5 }
            else if self.fsr_status.is_some() && PLUS.contains(p) { 6 }
            else if self.fsr_status.is_some() && RESET.contains(p) { 7 }
            else { 0 }
        });
        if clicked { self.action(hover); }
        if !is_open() {
            self.picture = None;
            return;
        }
        let cursor = if hidden_cursor() { pos } else { None };
        if self.picture.as_ref().is_none_or(|p| p.layout != layout || p.format != format
            || p.hover != hover || p.cursor != cursor || p.revision != self.revision) {
            self.picture = self.paint(host, layout, format, hover, cursor);
        }
        let Some(p) = &self.picture else {
            OPEN.store(false, Ordering::Release);
            tracing::warn!("settings overlay could not allocate or render");
            return;
        };
        // The backbuffer is already COPY_DEST here. Keep this immutable
        // upload alive until the exact submission that uses it has retired.
        copy.keep(&p.resource);
        unsafe {
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&p.resource),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: format, Width: layout.w, Height: layout.h, Depth: 1, RowPitch: p.pitch,
                        },
                    },
                },
            };
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(back),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            copy.list.CopyTextureRegion(&to, layout.x, layout.y, 0, &from, None);
        }
    }
}

// Called synchronously on the title thread, while the dispatch owns these
// resources. Never allocate a texture, transition its state or retain the
// title pointer here. THIN can defer actual writes; an existing native
// texture is evidence of identity only, not a synchronization guarantee.
fn observe_resources(resources: [usize; 4], dimensions: [[u32; 2]; 4]) {
    use windows::core::Interface;
    for (i, label) in ["color", "depth", "motion_vectors", "output"].iter().enumerate() {
        let title = resources[i] as crate::object::This;
        if !crate::resource::is_resource(title) {
            tracing::warn!(input = *label, title = resources[i], "FFX bridge: not a recognized title resource");
            continue;
        }
        if let Some(copy) = crate::resource::made_copy(title) {
            let desc = unsafe { copy.texture.GetDesc() };
            let matches_size = desc.Width == u64::from(dimensions[i][0]) && desc.Height == dimensions[i][1];
            tracing::info!(input = *label, title = resources[i], native = copy.texture.as_raw() as usize,
                width = desc.Width, height = desc.Height, format = desc.Format.0, matches_size,
                "FFX bridge: existing native texture");
        } else {
            tracing::info!(input = *label, title = resources[i], "FFX bridge: native texture not yet materialized");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_and_hit_test() {
        for size in [(640, 480), (1920, 1080), (3840, 2160)] {
            let l = Layout::new(size).unwrap();
            assert!(l.x + l.w <= size.0 && l.y + l.h <= size.1);
            let x = l.x + l.sx(PROBE.x + 10);
            let y = l.y + l.sy(PROBE.y + 10);
            assert!(PROBE.contains(l.local(x,y).unwrap()));
        }
    }
    #[test]
    fn pixel_formats() {
        let mut dst = [0; 8];
        convert(&[1,2,3,0], &mut dst[..4], DXGI_FORMAT_R8G8B8A8_UNORM);
        assert_eq!(&dst[..4], &[3,2,1,255]);
        convert(&[0,0,255,0], &mut dst, DXGI_FORMAT_R16G16B16A16_FLOAT);
        assert_eq!(dst, [0,60,0,0,0,0,0,60]);
    }
}
