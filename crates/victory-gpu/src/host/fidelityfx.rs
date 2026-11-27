//! Native FidelityFX API discovery for FSR 3.1, independently of Adrenalin.
//! ABI from AMD's SDK v1.1.4 ffx_api.h and ffx_framegeneration.h.
//! No generation is enabled before real depth/motion inputs are identified.
use std::ffi::{CStr, c_char, c_void};
use std::os::windows::ffi::OsStrExt;
use std::ptr::null_mut;
use windows::core::Interface;
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
    LoadLibraryExW,
};

type Ptr = *mut c_void;
type Result<T> = std::result::Result<T, String>;

#[repr(C)]
struct Header {
    kind: u64,
    next: *mut Header,
}

#[repr(C)]
struct QueryVersions {
    header: Header,
    create_desc_type: u64,
    device: Ptr,
    output_count: *mut u64,
    version_ids: *mut u64,
    version_names: *mut *const c_char,
}

const GET_VERSIONS: u64 = 4;
const CREATE_FRAME_GENERATION: u64 = 0x0002_0001;
type Query = unsafe extern "C" fn(*mut Ptr, *mut Header) -> u32;

struct Library(HMODULE);

impl Library {
    fn open() -> Result<Self> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let path = exe.with_file_name("amd_fidelityfx_dx12.dll");
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let dll = unsafe {
            LoadLibraryExW(
                wide.as_ptr(), null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        };
        if dll.is_null() {
            return Err(format!("Install the official FidelityFX SDK 1.1.4 DX12 DLL beside victory.exe: {}", path.display()));
        }
        Ok(Self(dll))
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        unsafe { FreeLibrary(self.0) };
    }
}

pub(super) fn probe() -> Result<String> {
    let host = super::host().ok_or("DX12 host not initialized")?;
    let dll = Library::open()?;
    // Verify the runtime implements all five functions required by the
    // native FSR API before querying providers on the real host device.
    for name in [
        b"ffxCreateContext\0".as_slice(),
        b"ffxDestroyContext\0".as_slice(),
        b"ffxConfigure\0".as_slice(),
        b"ffxDispatch\0".as_slice(),
    ] {
        if unsafe { GetProcAddress(dll.0, name.as_ptr()) }.is_none() {
            return Err(format!("FidelityFX export missing: {}", String::from_utf8_lossy(&name[..name.len()-1])));
        }
    }
    let raw = unsafe { GetProcAddress(dll.0, c"ffxQuery".as_ptr().cast()) }
        .ok_or("FidelityFX export missing: ffxQuery")?;
    let query: Query = unsafe { std::mem::transmute(raw) };
    let mut count = 0u64;
    let mut desc = QueryVersions {
        header: Header { kind: GET_VERSIONS, next: null_mut() },
        create_desc_type: CREATE_FRAME_GENERATION,
        device: host.device.as_raw(),
        output_count: &mut count,
        version_ids: null_mut(),
        version_names: null_mut(),
    };
    let rc = unsafe { query(null_mut(), &mut desc.header) };
    if rc != 0 || count == 0 || count > 128 {
        return Err(format!("No usable FidelityFX frame-generation provider (result {rc}, count {count})"));
    }
    let mut ids = vec![0u64; count as usize];
    let mut names = vec![std::ptr::null(); count as usize];
    desc.version_ids = ids.as_mut_ptr();
    desc.version_names = names.as_mut_ptr();
    let rc = unsafe { query(null_mut(), &mut desc.header) };
    if rc != 0 || count == 0 || count as usize > ids.len() {
        return Err(format!("FidelityFX provider enumeration failed (result {rc})"));
    }
    let versions = names.iter().zip(ids.iter()).take(count as usize).map(|(&name, &id)| {
        if name.is_null() {
            format!("{id:#x}")
        } else {
            // SDK owns this string until the DLL is unloaded.
            unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned()
        }
    }).collect::<Vec<_>>().join(", ");
    tracing::info!(%versions, "native FidelityFX frame-generation providers on host GPU; interpolation disabled");
    Ok(versions)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sdk_abi_layout() {
        assert_eq!(size_of::<Header>(), 16);
        assert_eq!(size_of::<QueryVersions>(), 56);
        assert_eq!(std::mem::offset_of!(QueryVersions, device), 24);
        assert_eq!(std::mem::offset_of!(QueryVersions, version_names), 48);
    }
}
