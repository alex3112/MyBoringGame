//! An owned Win32 settings window, with a separate message thread so no
//! SDK query or UI event runs under the D3D12 presenter lock.
use std::cell::Cell;
use std::ptr::{null, null_mut};
use std::time::Duration;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{COLOR_WINDOW, DEFAULT_GUI_FONT, GetStockObject};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetAsyncKeyState, GetKeyboardLayout, MapVirtualKeyExW, VkKeyScanExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CLOSE: usize = 200;
const PROBE: usize = 201;
const CAPTURE: usize = 202;
const WIDTH: i32 = 610;
const HEIGHT: i32 = 320;

thread_local! {
    static COMMAND: Cell<usize> = const { Cell::new(0) };
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

pub(super) fn start(owner: windows::Win32::Foundation::HWND) {
    // HWND is an opaque handle; move only its integer representation across
    // threads. This thread owns every control and all ADLX interfaces.
    let owner = owner.0 as usize;
    if let Err(e) = std::thread::Builder::new().name("victory-settings".into()).spawn(move || {
        run(owner as HWND);
    }) {
        tracing::warn!(error = %e, "could not start settings menu");
    }
}

extern "system" fn wnd_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match message {
        WM_CLOSE => {
            COMMAND.with(|c| c.set(CLOSE));
            0
        }
        WM_COMMAND if (wp >> 16) & 0xffff == 0 => {
            COMMAND.with(|c| c.set(wp & 0xffff));
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wp, lp) },
    }
}

fn control(parent: HWND, class: &str, title: &str, id: usize, bounds: [i32; 4]) -> HWND {
    let [x, y, width, height] = bounds;
    let class_name = wide(class);
    let text = wide(title);
    let style = WS_CHILD | WS_VISIBLE | if id != 0 { WS_TABSTOP } else { 0 };
    let hwnd = unsafe {
        CreateWindowExW(0, class_name.as_ptr(), text.as_ptr(), style,
            x, y, width, height, parent, id as _, GetModuleHandleW(null()), null())
    };
    if !hwnd.is_null() {
        unsafe { SendMessageW(hwnd, WM_SETFONT, GetStockObject(DEFAULT_GUI_FONT) as usize, 1) };
    }
    hwnd
}

fn label(hwnd: HWND, text: &str) {
    let text = wide(text);
    unsafe { SetWindowTextW(hwnd, text.as_ptr()) };
}

struct Menu {
    hwnd: HWND,
    status: HWND,
}

impl Menu {
    fn create(owner: HWND) -> Option<Self> {
        let class = wide("VIctorySettingsWindow");
        let caption = wide("VIctory | Graphics settings");
        let instance = unsafe { GetModuleHandleW(null()) };
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance,
            hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
            hbrBackground: (COLOR_WINDOW + 1) as _,
            lpszClassName: class.as_ptr(),
            ..Default::default()
        };
        unsafe { RegisterClassW(&wc) };
        let hwnd = unsafe {
            CreateWindowExW(WS_EX_TOOLWINDOW, class.as_ptr(), caption.as_ptr(),
                WS_POPUP | WS_CAPTION | WS_SYSMENU, 0, 0, WIDTH, HEIGHT,
                owner, null_mut(), instance, null())
        };
        if hwnd.is_null() {
            return None;
        }
        control(hwnd, "STATIC", "GRAPHICS / AMD FIDELITYFX FRAME GENERATION", 0, [18, 12, 550, 22]);
        control(hwnd, "STATIC", "FSR 3.1 native frame generation (VIctory only)", 0, [18, 42, 550, 22]);
        let generation = control(hwnd, "BUTTON", "Off - inputs not mapped", 0, [18, 68, 550, 32]);
        unsafe { EnableWindow(generation, 0) };
        let status = control(hwnd, "STATIC",
            "Native interpolation is not active. First identify depth and motion vectors, then connect the presentation pipeline.",
            0, [18, 113, 550, 68]);
        control(hwnd, "BUTTON", "Check FidelityFX SDK", PROBE, [18, 193, 230, 32]);
        let capture = control(hwnd, "BUTTON", "Capture FSR inputs", CAPTURE, [258, 193, 310, 32]);
        unsafe { EnableWindow(capture, i32::from(cfg!(feature = "diagnostics"))) };
        control(hwnd, "BUTTON", "Close", CLOSE, [390, 236, 178, 30]);
        if status.is_null() || generation.is_null() || capture.is_null() {
            unsafe { DestroyWindow(hwnd) };
            return None;
        }
        Some(Self { hwnd, status })
    }

    /// True means the menu should close immediately so the requested
    /// capture samples the game, rather than a window with no input focus.
    fn command(&self, id: usize) -> bool {
        match id {
            PROBE => {
                let text = match super::fidelityfx::probe() {
                    Ok(versions) => format!("FidelityFX providers: {versions}. Interpolation still off; inputs not mapped."),
                    Err(e) => e,
                };
                label(self.status, &text);
            }
            #[cfg(feature = "diagnostics")]
            CAPTURE => match crate::capture::request_fsr() {
                Ok(()) => return true,
                Err(e) => label(self.status, &e),
            },
            _ => {}
        }
        false
    }

    fn show(&self, owner: HWND) {
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(owner, &mut rect) } != 0 {
            let x = rect.left + (rect.right - rect.left - WIDTH) / 2;
            let y = rect.top + (rect.bottom - rect.top - HEIGHT) / 2;
            unsafe { SetWindowPos(self.hwnd, null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE) };
        }
        unsafe {
            ShowWindow(self.hwnd, SW_SHOW);
            SetForegroundWindow(self.hwnd);
        }
    }

    fn hide(&self, owner: HWND) {
        unsafe {
            ShowWindow(self.hwnd, SW_HIDE);
            SetForegroundWindow(owner);
        }
    }
}

fn run(owner: HWND) {
    let Some(menu) = Menu::create(owner) else {
        tracing::warn!("could not create graphics settings window");
        return;
    };
    let mut down = false;
    let mut visible = false;
    let mut escape = false;
    loop {
        if unsafe { IsWindow(owner) } == 0 {
            break;
        }
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) } != 0 {
            if msg.message == WM_QUIT {
                return;
            }
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let foreground = unsafe { GetForegroundWindow() };
        let thread = unsafe { GetWindowThreadProcessId(foreground, null_mut()) };
        let layout = unsafe { GetKeyboardLayout(thread) };
        // Resolve ² on the active layout; fallback to the physical key
        // left of 1 (grave on US keyboards). Use a rising edge.
        let mapped = unsafe { VkKeyScanExW(0x00b2, layout) };
        let vk = if mapped == -1 {
            (unsafe { MapVirtualKeyExW(0x29, 3, layout) }) as i32
        } else {
            i32::from(mapped) & 0xff
        };
        let pressed = unsafe { GetAsyncKeyState(vk) } < 0;
        let esc_pressed = unsafe { GetAsyncKeyState(0x1b) } < 0;
        let id = COMMAND.with(|c| c.replace(0));
        let toggle = pressed && !down && (foreground == owner || foreground == menu.hwnd);
        if (toggle && visible) || (visible && id == CLOSE)
            || (visible && foreground == menu.hwnd && esc_pressed && !escape)
        {
            menu.hide(owner);
            visible = false;
        } else if toggle {
            menu.show(owner);
            visible = true;
        } else if visible && id != 0 && menu.command(id) {
            menu.hide(owner);
            visible = false;
        }
        down = pressed;
        escape = esc_pressed;
        std::thread::sleep(Duration::from_millis(16));
    }
    unsafe { DestroyWindow(menu.hwnd) };
}
