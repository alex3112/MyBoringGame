//! An owned Win32 settings window, with a separate message thread so no
//! driver call or UI event runs under the D3D12 presenter lock.
use super::afmf::{Afmf, N, OPTIONS};
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

const ROWS: [&str; N] = [
    "Frame generation (AFMF)",
    "Algorithm",
    "Search mode",
    "Performance mode",
    "Fast motion response",
];
const FIRST: usize = 100;
const CLOSE: usize = 200;
const REFRESH: usize = 201;
const RESTORE: usize = 202;
const WIDTH: i32 = 610;
const HEIGHT: i32 = 414;

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
    buttons: [HWND; N],
    status: HWND,
    restore: HWND,
    session: Option<Afmf>,
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
        control(hwnd, "STATIC", "GRAPHICS / AMD FRAME GENERATION", 0, [18, 12, 550, 22]);
        control(hwnd, "STATIC",
            "Global AMD driver profile. Changes persist and may affect other games.",
            0, [18, 38, 560, 22]);
        let mut buttons = [null_mut(); N];
        for i in 0..N {
            let y = 70 + 37 * i as i32;
            control(hwnd, "STATIC", ROWS[i], 0, [18, y + 6, 260, 24]);
            buttons[i] = control(hwnd, "BUTTON", "Unavailable", FIRST + i, [290, y, 275, 30]);
        }
        let status = control(hwnd, "STATIC", "", 0, [18, 260, 560, 48]);
        let restore = control(hwnd, "BUTTON", "Restore initial settings", RESTORE, [18, 318, 200, 30]);
        control(hwnd, "BUTTON", "Refresh", REFRESH, [230, 318, 145, 30]);
        control(hwnd, "BUTTON", "Close", CLOSE, [390, 318, 175, 30]);
        if status.is_null() || restore.is_null() || buttons.iter().any(|b| b.is_null()) {
            unsafe { DestroyWindow(hwnd) };
            return None;
        }
        Some(Self { hwnd, buttons, status, restore, session: None })
    }

    fn refresh(&mut self, connect: bool, message: Option<&str>) {
        if connect && self.session.is_none() {
            match Afmf::connect() {
                Ok(session) => self.session = Some(session),
                Err(e) => {
                    tracing::warn!(error = %e, "AMD AFMF unavailable");
                    label(self.status, &e);
                }
            }
        }
        let state = self.session.as_ref().map_or([None; N], Afmf::read);
        for (i, value) in state.into_iter().enumerate() {
            let caption = value.map_or("Unavailable", |v| OPTIONS[i][v as usize]);
            label(self.buttons[i], caption);
            unsafe { EnableWindow(self.buttons[i], i32::from(value.is_some())) };
        }
        unsafe { EnableWindow(self.restore, i32::from(self.session.is_some())) };
        if let Some(message) = message {
            label(self.status, message);
        } else if self.session.is_some() {
            label(self.status, "Click a value to cycle. On = driver setting, not confirmed interpolation. Low native FPS may cause artifacts.");
        }
    }

    fn command(&mut self, id: usize) {
        if id == REFRESH {
            self.refresh(true, None);
            return;
        }
        let Some(session) = self.session.as_ref() else { return; };
        let result = if (FIRST..FIRST + N).contains(&id) {
            let row = id - FIRST;
            let Some(current) = session.read()[row] else { return; };
            let next = (current + 1) % OPTIONS[row].len() as i32;
            session.set(row, next).and_then(|()| {
                if session.read()[row] == Some(next) { Ok(()) }
                else { Err("Driver did not confirm the requested setting. Refresh or check Adrenalin.".into()) }
            })
        } else if id == RESTORE {
            // Restore advanced options before the initial on/off state.
            let mut result = Ok(());
            for row in [1, 2, 3, 4, 0] {
                if let Some(value) = session.original[row]
                    && session.read()[row] != Some(value)
                    && let Err(e) = session.set(row, value)
                {
                    result = Err(e);
                }
            }
            if result.is_ok() && session.read() != session.original {
                result = Err("Driver did not confirm all initial settings".into());
            }
            result
        } else {
            return;
        };
        match result {
            Ok(()) => self.refresh(false, Some("Driver settings confirmed. Return to the game to test AFMF.")),
            Err(e) => {
                tracing::warn!(error = %e, "AFMF settings change failed");
                self.refresh(false, Some(&e));
            }
        }
    }

    fn show(&mut self, owner: HWND) {
        self.refresh(true, None);
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
    let Some(mut menu) = Menu::create(owner) else {
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
        } else if visible && id != 0 {
            menu.command(id);
        }
        down = pressed;
        escape = esc_pressed;
        std::thread::sleep(Duration::from_millis(16));
    }
    unsafe { DestroyWindow(menu.hwnd) };
}
