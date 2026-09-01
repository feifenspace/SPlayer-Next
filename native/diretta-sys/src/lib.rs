use std::ffi::{c_char, c_void};

pub const TEXT_CAPACITY: usize = 256;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SPlayerDirettaDevice {
    pub id: [c_char; TEXT_CAPACITY],
    pub name: [c_char; TEXT_CAPACITY],
    pub ipv6_addr: [c_char; TEXT_CAPACITY],
    pub full_addr: [c_char; TEXT_CAPACITY],
    pub if_idx: i32,
    pub target_name: [c_char; TEXT_CAPACITY],
    pub output_name: [c_char; TEXT_CAPACITY],
    pub model_name: [c_char; TEXT_CAPACITY],
    pub mtu: u32,
}

impl Default for SPlayerDirettaDevice {
    fn default() -> Self {
        Self {
            id: [0; TEXT_CAPACITY],
            name: [0; TEXT_CAPACITY],
            ipv6_addr: [0; TEXT_CAPACITY],
            full_addr: [0; TEXT_CAPACITY],
            if_idx: 0,
            target_name: [0; TEXT_CAPACITY],
            output_name: [0; TEXT_CAPACITY],
            model_name: [0; TEXT_CAPACITY],
            mtu: 0,
        }
    }
}

pub type NextBlockCallback =
    unsafe extern "C" fn(context: *mut c_void, data: *mut *const u8, len: *mut usize) -> bool;
pub type ReleaseBlockCallback = unsafe extern "C" fn(context: *mut c_void);

#[cfg(diretta_sdk_enabled)]
unsafe extern "C" {
    pub fn splayer_diretta_last_error() -> *const c_char;
    pub fn splayer_diretta_scan(devices: *mut SPlayerDirettaDevice, capacity: usize) -> usize;
    pub fn splayer_diretta_open_direct(
        target_id: *const c_char,
        sample_rate: u32,
        channels: u16,
        storage_bits: u8,
        source_context: *mut c_void,
        next_block: NextBlockCallback,
        release_block: ReleaseBlockCallback,
    ) -> *mut c_void;
    pub fn splayer_diretta_open_dsd_direct(
        target_id: *const c_char,
        bit_rate: u32,
        channels: u16,
        source_lsb_first: bool,
        wire_lsb_first: *mut bool,
        source_context: *mut c_void,
        next_block: NextBlockCallback,
        release_block: ReleaseBlockCallback,
    ) -> *mut c_void;
    pub fn splayer_diretta_play(handle: *mut c_void) -> bool;
    pub fn splayer_diretta_pause(handle: *mut c_void) -> bool;
    pub fn splayer_diretta_close(handle: *mut c_void);
}

#[cfg(not(diretta_sdk_enabled))]
pub unsafe fn splayer_diretta_last_error() -> *const c_char {
    std::ptr::null()
}

#[cfg(not(diretta_sdk_enabled))]
pub unsafe fn splayer_diretta_scan(_devices: *mut SPlayerDirettaDevice, _capacity: usize) -> usize {
    0
}

#[cfg(not(diretta_sdk_enabled))]
pub unsafe fn splayer_diretta_open_direct(
    _target_id: *const c_char,
    _sample_rate: u32,
    _channels: u16,
    _storage_bits: u8,
    _source_context: *mut c_void,
    _next_block: NextBlockCallback,
    _release_block: ReleaseBlockCallback,
) -> *mut c_void {
    std::ptr::null_mut()
}

#[cfg(not(diretta_sdk_enabled))]
pub unsafe fn splayer_diretta_open_dsd_direct(
    _target_id: *const c_char,
    _bit_rate: u32,
    _channels: u16,
    _source_lsb_first: bool,
    _wire_lsb_first: *mut bool,
    _source_context: *mut c_void,
    _next_block: NextBlockCallback,
    _release_block: ReleaseBlockCallback,
) -> *mut c_void {
    std::ptr::null_mut()
}

#[cfg(not(diretta_sdk_enabled))]
pub unsafe fn splayer_diretta_play(_handle: *mut c_void) -> bool {
    false
}

#[cfg(not(diretta_sdk_enabled))]
pub unsafe fn splayer_diretta_pause(_handle: *mut c_void) -> bool {
    false
}

#[cfg(not(diretta_sdk_enabled))]
pub unsafe fn splayer_diretta_close(_handle: *mut c_void) {}
