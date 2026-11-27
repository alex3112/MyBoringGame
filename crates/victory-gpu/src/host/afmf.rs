//! AMD ADLX driver-level Fluid Motion Frames control.
//! This is not the FidelityFX in-engine frame interpolation API.
//! ABI slots follow AMD's public ADLX headers: ISystem.h, I3DSettings.h,
//! I3DSettings1.h and I3DSettings3.h. No SDK or extra DLL is distributed.
use std::ffi::c_void;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};

type Ptr = *mut c_void;
type Result<T> = std::result::Result<T, String>;
type Out = unsafe extern "system" fn(Ptr, *mut Ptr) -> i32;
type Query = unsafe extern "system" fn(Ptr, *const u16, *mut Ptr) -> i32;
type ReadBool = unsafe extern "system" fn(Ptr, *mut u8) -> i32;
type WriteBool = unsafe extern "system" fn(Ptr, u8) -> i32;
type ReadEnum = unsafe extern "system" fn(Ptr, *mut i32) -> i32;
type WriteEnum = unsafe extern "system" fn(Ptr, i32) -> i32;
type Terminate = unsafe extern "C" fn() -> i32;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn check(code: i32, operation: &str) -> Result<()> {
    match code {
        0..=2 => Ok(()),
        _ => Err(format!("{operation}: ADLX result {code}")),
    }
}

// SAFETY: callers select the signature of the corresponding published slot;
// references live until their release, before ADLXTerminate.
unsafe fn slot<F: Copy>(p: Ptr, index: usize) -> F {
    let table = unsafe { *p.cast::<*const *const c_void>() };
    let address = unsafe { *table.add(index) };
    assert_eq!(size_of::<F>(), size_of::<Ptr>());
    unsafe { std::mem::transmute_copy(&address) }
}

struct Interface(Ptr);

impl Interface {
    fn new(p: Ptr) -> Result<Self> {
        if p.is_null() {
            Err("ADLX returned a null interface".into())
        } else {
            Ok(Self(p))
        }
    }

    fn query(&self, iid: &str) -> Result<Self> {
        let mut p = null_mut();
        let name = wide(iid);
        let f: Query = unsafe { slot(self.0, 2) };
        check(unsafe { f(self.0, name.as_ptr(), &mut p) }, iid)?;
        Self::new(p)
    }

    fn out(p: Ptr, index: usize, operation: &str) -> Result<Self> {
        let mut result = null_mut();
        let f: Out = unsafe { slot(p, index) };
        check(unsafe { f(p, &mut result) }, operation)?;
        Self::new(result)
    }

    fn boolean(&self, index: usize) -> Result<i32> {
        let mut value = 0u8;
        let f: ReadBool = unsafe { slot(self.0, index) };
        check(unsafe { f(self.0, &mut value) }, "read AFMF boolean")?;
        Ok(i32::from(value != 0))
    }

    fn enumeration(&self, index: usize) -> Result<i32> {
        let mut value = 0;
        let f: ReadEnum = unsafe { slot(self.0, index) };
        check(unsafe { f(self.0, &mut value) }, "read AFMF mode")?;
        Ok(value)
    }
}

impl Drop for Interface {
    fn drop(&mut self) {
        let release: unsafe extern "system" fn(Ptr) -> i32 = unsafe { slot(self.0, 1) };
        unsafe { release(self.0) };
    }
}

struct Runtime {
    dll: HMODULE,
    terminate: Option<Terminate>,
}

impl Runtime {
    fn load() -> Result<Self> {
        let name = wide("amdadlx64.dll");
        // Only load the installed driver library, never one beside the title.
        let dll = unsafe { LoadLibraryExW(name.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if dll.is_null() {
            return Err("AMD ADLX driver library not found in System32".into());
        }
        Ok(Self { dll, terminate: None })
    }

    fn symbol<F: Copy>(&self, name: &'static [u8]) -> Result<F> {
        let address = unsafe { GetProcAddress(self.dll, name.as_ptr()) }
            .ok_or_else(|| format!("ADLX export missing: {}", String::from_utf8_lossy(name)))?;
        assert_eq!(size_of::<F>(), size_of_val(&address));
        Ok(unsafe { std::mem::transmute_copy(&address) })
    }

    fn initialize(&mut self) -> Result<Ptr> {
        let version: unsafe extern "C" fn(*mut u64) -> i32 =
            self.symbol(b"ADLXQueryFullVersion\0")?;
        let init: unsafe extern "C" fn(u64, *mut Ptr) -> i32 =
            self.symbol(b"ADLXInitialize\0")?;
        let terminate: Terminate = self.symbol(b"ADLXTerminate\0")?;
        let mut reported = 0;
        check(unsafe { version(&mut reported) }, "query ADLX version")?;
        if !matches!(reported >> 48, 1 | 2) {
            return Err(format!("Unrecognized ADLX ABI: {reported:#x}"));
        }
        let mut system = null_mut();
        let status = unsafe { init(reported, &mut system) };
        // Do not terminate an ADLX instance that we did not initialize.
        if status == 0 {
            self.terminate = Some(terminate);
        }
        check(status, "initialize ADLX")?;
        if system.is_null() {
            return Err("ADLX returned no system services".into());
        }
        Ok(system)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Some(terminate) = self.terminate {
            unsafe { terminate() };
        }
        unsafe { FreeLibrary(self.dll) };
    }
}

pub(super) const N: usize = 5;
pub(super) const OPTIONS: [&[&str]; N] = [
    &["Off", "On"],
    &["Auto", "Enhanced", "Standard"],
    &["Auto", "Standard", "High"],
    &["Auto", "Quality", "Performance"],
    &["Repeat frames", "Blend frames"],
];

pub(super) struct Afmf {
    // Drop the reference-counted interfaces before unloading the runtime.
    advanced: Option<Interface>,
    base: Interface,
    _runtime: Runtime,
    pub(super) original: [Option<i32>; N],
}

impl Afmf {
    pub(super) fn connect() -> Result<Self> {
        let mut runtime = Runtime::load()?;
        let system = runtime.initialize()?;
        // IADLXSystem is not reference counted. Get3DSettingsServices is
        // slot 7. QueryInterface checks support for the extended services.
        let settings = Interface::out(system, 7, "3D settings")?;
        let extension = settings.query("IADLX3DSettingsServices1")?;
        let base = Interface::out(extension.0, 17, "Fluid Motion Frames")?;
        if base.boolean(3)? == 0 {
            return Err("AMD Fluid Motion Frames is not supported by this driver/GPU".into());
        }
        let advanced = base.query("IADLX3DAMDFluidMotionFrames1").ok();
        // Release temporary service references before transferring runtime.
        drop(extension);
        drop(settings);
        let mut result = Self { advanced, base, _runtime: runtime, original: [None; N] };
        result.original = result.read();
        tracing::info!(state = ?result.original, "AMD AFMF global driver settings detected");
        Ok(result)
    }

    pub(super) fn read(&self) -> [Option<i32>; N] {
        let mut values = [None; N];
        values[0] = self.base.boolean(4).ok();
        if let Some(a) = &self.advanced {
            if a.boolean(6).ok() == Some(1) {
                values[1] = a.enumeration(7).ok();
            }
            for (i, index) in [(2, 9), (3, 11), (4, 13)] {
                values[i] = a.enumeration(index).ok();
            }
        }
        for (i, value) in values.iter_mut().enumerate() {
            if value.is_some_and(|v| v < 0 || v as usize >= OPTIONS[i].len()) {
                *value = None;
            }
        }
        values
    }

    pub(super) fn set(&self, row: usize, value: i32) -> Result<()> {
        if row >= N || value < 0 || value as usize >= OPTIONS[row].len() {
            return Err("Invalid AFMF setting".into());
        }
        let code = if row == 0 {
            let f: WriteBool = unsafe { slot(self.base.0, 5) };
            unsafe { f(self.base.0, value as u8) }
        } else {
            let a = self.advanced.as_ref().ok_or("Advanced AFMF controls unavailable")?;
            if row == 1 && a.boolean(6)? == 0 {
                return Err("AFMF algorithm selection unsupported".into());
            }
            let f: WriteEnum = unsafe { slot(a.0, 6 + row * 2) };
            unsafe { f(a.0, value) }
        };
        tracing::info!(row, value, code, "AMD AFMF driver setting request");
        check(code, "set AFMF")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn result_codes_are_not_hresult() {
        assert!(check(0, "test").is_ok());
        assert!(check(1, "test").is_ok());
        assert!(check(12, "test").is_err());
    }
}
