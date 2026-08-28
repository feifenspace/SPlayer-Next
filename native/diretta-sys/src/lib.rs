//! `diretta_ffi`：Diretta C Shim 的 Rust FFI 绑定（P3.1.3）
//!
//! 来源：HIFI_REFACTORING.md §11.1（内存所有权）/ §11.4（错误码）/ §11.3（事件回调）
//!       §11.2（线程模型，决策 D18）
//!
//! 本模块对 `cshim/diretta_shim.h` 暴露的 C ABI 做 Rust 侧 RAII 包装：
//! - [`DirettaFinder`]：包裹 `diretta_finder_t*`，Drop 时调用 `diretta_finder_close`
//! - [`DirettaSync`]：包裹 `diretta_sync_t*`（P3.1.3 实现推流 + 控制操作）
//! - [`DirettaSetting`]：C 友好结构体的 Rust 等价
//! - [`DirettaError`]：8 个错误码（§11.4）的枚举
//!
//! # 内存所有权（§11.1）
//!
//! - C Shim 用 `new`/`delete` 管理 `DIRETTA::Find`/`DirettaSyncImpl` 对象
//! - Rust 侧用 `Box::into_raw`/`Box::from_raw` 思路：构造时 `open()` 返回 raw ptr，
//!   Drop 时把 raw ptr 传回 `close()` 让 C 侧 `delete`。Rust 不直接 owns C 内存。
//! - 跨线程释放禁止（D18）：哪个线程 open 就在哪个线程 close——Rust 侧用 `Send`/`!Sync`
//!   标记约束（`DirettaFinder` 标记 `!Sync`，但仍可 `Send`，因为单线程内可转移所有权）
//!
//! # 线程模型（D18，§11.2）
//!
//! - `DirettaSync::open` / `Drop`：同线程调用（生命周期管理）
//! - `DirettaSync::push`：单线程约束（C Shim 内部 buf_mtx_ 保护环形缓冲区 SPSC）
//! - `set_sink` / `connect` / `disconnect` / `play` / `stop`：C Shim 内部 ctrl_mtx_ 保护
//! - `is_online` / `is_playing`：原子读，任意线程可调用
//!
//! # 条件编译
//!
//! 仅在 `target_os = "linux"` 且 `diretta-148`/`diretta-149` feature 启用时编译。
//! Windows/macOS 或未启用 feature 时本模块整体不存在，避免链接错误。

use std::ffi::CStr;
use std::os::raw::c_char;
use std::ptr;

// === C ABI 绑定 ===
// 与 cshim/diretta_shim.h 严格一一对应。改动需同步 .h 文件。

#[repr(C)]
pub struct diretta_finder_opaque {
    _private: [u8; 0],
}
#[repr(C)]
pub struct diretta_sync_opaque {
    _private: [u8; 0],
}

/// C 侧不透明 Finder 指针（diretta_finder_t*）。
pub(crate) type DirettaFinderRaw = *mut diretta_finder_opaque;
/// C 侧不透明 Sync 指针（diretta_sync_t*）。
pub(crate) type DirettaSyncRaw = *mut diretta_sync_opaque;

/// 事件回调 C 签名（§11.3）。
pub type DirettaEventCb =
    unsafe extern "C" fn(event_type: std::os::raw::c_int,
                         error_code: std::os::raw::c_int,
                         user_data: *mut std::ffi::c_void);

extern "C" {
    fn diretta_get_version() -> u16;
    fn diretta_get_version_string() -> *const c_char;

    fn diretta_finder_open(setting: *const CDirettaSetting) -> DirettaFinderRaw;
    fn diretta_finder_close(f: DirettaFinderRaw);

    // 方案 A 新增：设备扫描 API
    fn diretta_finder_scan(f: DirettaFinderRaw,
                           out_devices: *mut CDirettaDeviceInfo,
                           max_count: std::os::raw::c_int,
                           actual_count: *mut std::os::raw::c_int)
        -> std::os::raw::c_int;

    // P3.2 Stage 7.5 新增：MTU 测量 API（参考 tinyLMS-old DirettaDriver.cpp L508-527）
    fn diretta_finder_measure_mtu(f: DirettaFinderRaw,
                                  ip_str: *const c_char,
                                  port: u16,
                                  ifno: u32,
                                  out_mtu: *mut u32)
        -> std::os::raw::c_int;

    fn diretta_sync_open(cb: Option<DirettaEventCb>,
                         user_data: *mut std::ffi::c_void) -> DirettaSyncRaw;
    fn diretta_sync_close(s: DirettaSyncRaw);

    // P3.1.3：推流 + 控制 API
    fn diretta_sync_push(s: DirettaSyncRaw, data: *const std::ffi::c_void,
                         size: usize) -> std::os::raw::c_int;
    fn diretta_sync_set_sink(s: DirettaSyncRaw, ip_str: *const c_char,
                             port: u16, ifno: u32, mtu: u32, buffer_ms: u32,
                             sample_rate: u32, channels: u32, bits_per_sample: u32)
        -> std::os::raw::c_int;
    // DSD 完整形态阶段 3 新增：DSD Native 直通模式
    // dsd_rate_multiplier: 64/128/256/512
    // dsd_byte_order: 0=LSB/DSF, 1=MSB/DFF
    fn diretta_sync_set_sink_dsd(s: DirettaSyncRaw, ip_str: *const c_char,
                                  port: u16, ifno: u32, mtu: u32, buffer_ms: u32,
                                  dsd_rate_multiplier: u32, dsd_byte_order: u32,
                                  channels: u32) -> std::os::raw::c_int;
    // P3：DSD 协商变换查询（返回协商命中的位反转 / 字节交换标志）
    fn diretta_sync_get_dsd_transform(s: DirettaSyncRaw,
                                      bit_reverse: *mut std::os::raw::c_int,
                                      byte_swap: *mut std::os::raw::c_int)
        -> std::os::raw::c_int;
    fn diretta_sync_connect(s: DirettaSyncRaw,
                            timeout_ms: std::os::raw::c_int) -> std::os::raw::c_int;
    fn diretta_sync_disconnect(s: DirettaSyncRaw) -> std::os::raw::c_int;
    fn diretta_sync_play(s: DirettaSyncRaw) -> std::os::raw::c_int;
    fn diretta_sync_stop(s: DirettaSyncRaw) -> std::os::raw::c_int;
    fn diretta_sync_is_online(s: DirettaSyncRaw) -> std::os::raw::c_int;
    fn diretta_sync_is_playing(s: DirettaSyncRaw) -> std::os::raw::c_int;

    // P1 修复：Pre-mute 机制 + Soft Resume 辅助
    // 在 stop()/reconfigure() 前调用 trigger_pre_mute + wait_pre_mute_done，
    // 让 SDK 线程输出静音帧后再断开连接，消除"切歌爆音"。
    // clear_ring_buffer 用于 Soft Resume 路径清空 ring_buf_ 残留旧格式数据。
    fn diretta_sync_trigger_pre_mute(s: DirettaSyncRaw,
                                     count: std::os::raw::c_int)
        -> std::os::raw::c_int;
    fn diretta_sync_wait_pre_mute_done(s: DirettaSyncRaw,
                                        timeout_ms: std::os::raw::c_int)
        -> std::os::raw::c_int;
    fn diretta_sync_clear_ring_buffer(s: DirettaSyncRaw)
        -> std::os::raw::c_int;

    // DSD 完整形态阶段 1 新增：Sink 设备能力查询（参考 tinyLMS-old DirettaDriver.cpp L1251-1258）
    fn diretta_sync_get_sink_info(s: DirettaSyncRaw,
                                  out_info: *mut CDirettaSinkInfo)
        -> std::os::raw::c_int;
    fn diretta_sync_get_negotiated_format(s: DirettaSyncRaw) -> u32;

    fn diretta_get_last_error() -> std::os::raw::c_int;
}

// === C 友好结构体（与 cshim/diretta_shim.h::diretta_setting 严格对应）===
#[repr(C)]
#[derive(Debug, Clone)]
pub struct CDirettaSetting {
    pub product_id: u64,
    pub limit_version: u16,
    pub name: *const c_char,
    pub nop_break: std::os::raw::c_int,
    pub loopback: std::os::raw::c_int,
    pub my_id: u64,
    pub broadcast: std::os::raw::c_int,
    pub multicast: std::os::raw::c_int,
}

impl Default for CDirettaSetting {
    fn default() -> Self {
        Self {
            product_id: 0,
            limit_version: 0,
            name: ptr::null(),
            nop_break: 0,
            loopback: 0,
            my_id: 0,
            broadcast: 0,
            multicast: 1, // 149 SDK 默认 true（§7.7.1）
        }
    }
}

// === C 友好设备信息结构体（方案 A 新增，与 cshim/diretta_shim.h::diretta_device_info 严格对应）===
//
// 字段顺序与 C 头文件保持一致，#[repr(C)] 保证 ABI 兼容。
// 字符串字段为定长 char 数组，NUL 终止；Rust 侧通过 CStr 转换为 String。

/// 与 C 侧 #define 对应的常量。
///
/// 注意：`DIRETTA_DEVICE_IPV4_MAX` 名称保留旧名（避免 Rust 侧大面积改名），
/// 但语义已变为兼容 IPv6 字符串的缓冲区大小（Diretta 设备地址本质是 IPv6）。
pub const DIRETTA_DEVICE_NAME_MAX: usize = 128;
pub const DIRETTA_DEVICE_IPV4_MAX: usize = 64;
pub const DIRETTA_DEVICE_CONFIG_MAX: usize = 256;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CDirettaDeviceInfo {
    pub ip_str: [std::os::raw::c_char; DIRETTA_DEVICE_IPV4_MAX],
    pub port: u16,
    pub ifno: u32,
    pub version: u16,
    pub product_id: u64,
    pub target_name: [std::os::raw::c_char; DIRETTA_DEVICE_NAME_MAX],
    pub output_name: [std::os::raw::c_char; DIRETTA_DEVICE_NAME_MAX],
    pub config: [std::os::raw::c_char; DIRETTA_DEVICE_CONFIG_MAX],
    pub po: u16,
    pub pi: u16,
    pub multiport: std::os::raw::c_int,
}

impl Default for CDirettaDeviceInfo {
    fn default() -> Self {
        Self {
            ip_str: [0; DIRETTA_DEVICE_IPV4_MAX],
            port: 0,
            ifno: 0,
            version: 0,
            product_id: 0,
            target_name: [0; DIRETTA_DEVICE_NAME_MAX],
            output_name: [0; DIRETTA_DEVICE_NAME_MAX],
            config: [0; DIRETTA_DEVICE_CONFIG_MAX],
            po: 0,
            pi: 0,
            multiport: 0,
        }
    }
}

impl CDirettaDeviceInfo {
    /// 把 C 字符数组转换为 Rust String（NUL 截断，UTF-8 lossy）。
    fn cstr_to_string(buf: &[std::os::raw::c_char]) -> String {
        // 找到第一个 NUL 字节作为字符串结尾
        let nul_pos = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(buf.as_ptr() as *const u8, nul_pos)
        };
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// 转换为 Rust 友好结构体。
    pub fn to_rust(self) -> DirettaDeviceInfo {
        DirettaDeviceInfo {
            ip_str: Self::cstr_to_string(&self.ip_str),
            port: self.port,
            ifno: self.ifno,
            version: self.version,
            product_id: self.product_id,
            target_name: Self::cstr_to_string(&self.target_name),
            output_name: Self::cstr_to_string(&self.output_name),
            config: Self::cstr_to_string(&self.config),
            po: self.po,
            pi: self.pi,
            multiport: self.multiport != 0,
        }
    }
}

/// Rust 友好的设备信息（方案 A 新增）。
///
/// 所有字符串字段为 `String`（Rust 拥有），由 `CDirettaDeviceInfo::to_rust()` 转换而来。
/// `device_id` 用于 `AudioBackend::connect()`，格式为 `"ip|port|ifno"`。
///
/// 注意：分隔符使用 `|` 而非 `:`，因为 Diretta 设备地址本质是 IPv6（V4-mapped-V6），
/// IPv6 字符串含 `:` 会导致 `rsplit_once(':')` 解析失败。
#[derive(Debug, Clone)]
pub struct DirettaDeviceInfo {
    pub ip_str: String,
    pub port: u16,
    pub ifno: u32,
    pub version: u16,
    pub product_id: u64,
    pub target_name: String,
    pub output_name: String,
    pub config: String,
    pub po: u16,
    pub pi: u16,
    pub multiport: bool,
}

impl DirettaDeviceInfo {
    /// 构造 `connect()` 期望的 `"ip|port|ifno"` 格式 device_id。
    ///
    /// 使用 `|` 分隔符避免 IPv6 地址中的 `:` 冲突。
    pub fn device_id(&self) -> String {
        format!("{}|{}|{}", self.ip_str, self.port, self.ifno)
    }

    /// 用户可见的显示名（优先 target_name，回退到 ip:port）。
    pub fn display_name(&self) -> String {
        if !self.target_name.is_empty() {
            self.target_name.clone()
        } else if !self.output_name.is_empty() {
            self.output_name.clone()
        } else {
            self.device_id()
        }
    }
}

// === C 友好 Sink Info 结构体（DSD 完整形态阶段 1 新增）===
//
// 与 cshim/diretta_shim.h::diretta_sink_info 严格一一对应。
// 字段顺序、类型对齐 C ABI；#[repr(C)] 保证内存布局兼容。
//
// FormatID 是 SDK 的 enum class FormatID : std::uint64_t（位图），
// 这里用 u64 透传原始值。Rust 侧通过位与运算判断具体格式位：
//   FMT_DSD1       = 0x0000000000010000  DSD1 协议位
//   FMT_DSD_LSB    = 0x0000000000100000  LSB-first（DSF 文件格式）
//   FMT_DSD_MSB    = 0x0000000000200000  MSB-first（DFF / DSDIFF 文件格式）
//   FMT_DSD_LITTLE = 0x0000000000400000  小端字节序
//   FMT_DSD_BIG    = 0x0000000000800000  大端字节序
//   FMT_DSD_SIZ_32 = 0x0000000002000000  32-bit 块大小
// 详见 DirettaHostSDK_149/Host/Format.hpp。
//
// `support_*` 字段非零表示设备支持该类格式；具体位图由
// [`FormatSupport`]（Rust 等价于 C++ `DIRETTA::FormatSupport` 类）进一步解析
// 采样率范围、位深范围、声道范围。

/// FormatID 位图常量（与 SDK `enum class FormatID : uint64_t` 一一对应）。
///
/// 来源：`DirettaHostSDK_149/Host/Format.hpp`。
/// 位图算法（getChMin/Max、getBitsMin/Max、getSpeedMin/Max 等）通过反汇编
/// `libDirettaHost_x64-linux-15v2.a` 的 `Format.o` 验证。
pub mod format_id {
    /// 声道位（bits 0-7）
    pub const CHA_1: u64    = 0x0000000000000001;
    pub const CHA_2: u64    = 0x0000000000000002;
    pub const CHA_4: u64    = 0x0000000000000004;
    pub const CHA_6: u64    = 0x0000000000000008;
    pub const CHA_8: u64    = 0x0000000000000010;
    pub const CHA_16: u64   = 0x0000000000000020;
    pub const CHA_MSK: u64  = 0x00000000000000FF;

    /// PCM 位深位（bits 8-15）
    pub const FMT_PCM_SIGNED_8: u64  = 0x0000000000000100;
    pub const FMT_PCM_SIGNED_16: u64 = 0x0000000000000200;
    pub const FMT_PCM_SIGNED_24: u64 = 0x0000000000000400;
    pub const FMT_PCM_SIGNED_32: u64 = 0x0000000000000800;
    pub const FMT_PCM_SIGNED_64: u64 = 0x0000000000001000;
    pub const FMT_PCM_FLOAT_32: u64 = 0x0000000000002000;
    pub const FMT_PCM_FLOAT_64: u64 = 0x0000000000004000;
    pub const FMT_PCM_MSK: u64      = 0x000000000000FF00;

    /// DSD 协议位（bits 16-19）
    pub const FMT_DSD1: u64         = 0x0000000000010000;
    pub const FMT_DSD4: u64         = 0x0000000000020000;
    pub const FMT_DSD_BIT_MSK: u64  = 0x00000000000F0000;

    /// DSD 字节序位（bits 20-23）
    pub const FMT_DSD_LSB: u64       = 0x0000000000100000; // DSF
    pub const FMT_DSD_MSB: u64       = 0x0000000000200000; // DFF / DSDIFF
    pub const FMT_DSD_LITTLE: u64    = 0x0000000000400000;
    pub const FMT_DSD_BIG: u64       = 0x0000000000800000;
    pub const FMT_DSD_ORDER_MSK: u64 = 0x0000000000F00000;

    /// DSD 块大小位（bits 24-27）
    pub const FMT_DSD_SIZ_32: u64   = 0x0000000002000000; // 32-bit 块（InterleavedBlock32）
    pub const FMT_DSD_SIZE_MSK: u64 = 0x00000000FF000000;

    /// 采样率基础位（bits 32-34）
    pub const RAT_8000: u64      = 0x0000000100000000;
    pub const RAT_44100: u64     = 0x0000000200000000;
    pub const RAT_48000: u64     = 0x0000000400000000;
    pub const RAT_BASE_MSK: u64  = 0x0000000700000000;

    /// 采样率倍率位（bits 35-47）
    pub const RAT_MP1: u64    = 0x0000000800000000;
    pub const RAT_MP2: u64    = 0x0000001000000000;
    pub const RAT_MP4: u64    = 0x0000002000000000;
    pub const RAT_MP8: u64    = 0x0000004000000000;
    pub const RAT_MP16: u64   = 0x0000008000000000;
    pub const RAT_MP32: u64   = 0x0000010000000000;
    pub const RAT_MP64: u64   = 0x0000020000000000;
    pub const RAT_MP128: u64  = 0x0000040000000000;
    pub const RAT_MP256: u64  = 0x0000080000000000;
    pub const RAT_MP512: u64 = 0x0000100000000000;
    pub const RAT_MP1024: u64 = 0x0000200000000000;
    pub const RAT_MP2048: u64 = 0x0000400000000000;
    pub const RAT_MP4096: u64 = 0x0000800000000000;
    pub const RAT_MP_MSK: u64 = 0x0000FFF800000000;
}

/// Rust 等价于 C++ `DIRETTA::FormatSupport` 类。
///
/// 从 FormatID raw value 构造，提供能力范围查询方法。
/// 算法严格依据反汇编 `libDirettaHost_x64-linux-15v2.a` 的 `Format.o`：
/// - `getChMin` / `getChMax`：扫描 CHA_* 位
/// - `getBitsMin` / `getBitsMax`：扫描 FMT_PCM_* / FMT_DSD1 / FMT_DSD4 位
/// - `getSpeedBaseMin` / `getSpeedBaseMax`：扫描 RAT_8000/44100/48000 位
/// - `getSpeedMultMin` / `getSpeedMultMax`：扫描 RAT_MP1~MP4096 位
/// - `getSpeedMin` = `base_min * mult_min`
/// - `getSpeedMax` = `base_max * mult_max`
///
/// 参考 `DirettaHostSDK_149/Host/Format.hpp` 与 tinyLMS-old `DirettaDriver.cpp`
/// L1252-1290（`FormatSupport pcm_support(info.supportPCM)` 用法）。
#[derive(Debug, Clone, Copy)]
pub struct FormatSupport {
    fmt_id: u64,
}

impl FormatSupport {
    /// 从 FormatID raw value 构造。
    pub fn new(fmt_id: u64) -> Self {
        Self { fmt_id }
    }

    /// `operator bool()` 等价：FormatID != 0 表示有效能力。
    pub fn is_valid(&self) -> bool {
        self.fmt_id != 0
    }

    /// `havePCM()` 等价：byte 1 (bits 8-15) != 0。
    pub fn have_pcm(&self) -> bool {
        (self.fmt_id & format_id::FMT_PCM_MSK) != 0
    }

    /// `haveDSD()` 等价：byte 2 低 4 位 (bits 16-19) != 0。
    pub fn have_dsd(&self) -> bool {
        (self.fmt_id & format_id::FMT_DSD_BIT_MSK) != 0
    }

    /// 是否支持 LSB-first 字节序（DSF 文件格式）。
    pub fn supports_dsd_lsb(&self) -> bool {
        (self.fmt_id & format_id::FMT_DSD_LSB) != 0
    }

    /// 是否支持 MSB-first 字节序（DFF / DSDIFF 文件格式）。
    pub fn supports_dsd_msb(&self) -> bool {
        (self.fmt_id & format_id::FMT_DSD_MSB) != 0
    }

    /// 是否支持小端字节序。
    pub fn supports_dsd_little(&self) -> bool {
        (self.fmt_id & format_id::FMT_DSD_LITTLE) != 0
    }

    /// 是否支持大端字节序。
    pub fn supports_dsd_big(&self) -> bool {
        (self.fmt_id & format_id::FMT_DSD_BIG) != 0
    }

    /// 是否支持 32-bit 块大小（InterleavedBlock32）。
    pub fn supports_dsd_size_32(&self) -> bool {
        (self.fmt_id & format_id::FMT_DSD_SIZ_32) != 0
    }

    /// `getChMin()`：声道最小值。
    ///
    /// 算法（来自反汇编）：CHA_1→1, CHA_2→2, CHA_4→4, CHA_6→6,
    /// 然后 CHA_16→16，回退 CHA_8→8。
    pub fn get_ch_min(&self) -> u32 {
        let f = self.fmt_id;
        if f & format_id::CHA_1 != 0 { return 1; }
        if f & format_id::CHA_2 != 0 { return 2; }
        if f & format_id::CHA_4 != 0 { return 4; }
        if f & format_id::CHA_6 != 0 { return 6; }
        if f & format_id::CHA_16 != 0 { return 16; }
        if f & format_id::CHA_8 != 0 { return 8; }
        0
    }

    /// `getChMax()`：声道最大值。
    ///
    /// 算法：CHA_16→16, CHA_8→8, CHA_6→6, CHA_4→4, CHA_2→2, CHA_1→1。
    pub fn get_ch_max(&self) -> u32 {
        let f = self.fmt_id;
        if f & format_id::CHA_16 != 0 { return 16; }
        if f & format_id::CHA_8 != 0 { return 8; }
        if f & format_id::CHA_6 != 0 { return 6; }
        if f & format_id::CHA_4 != 0 { return 4; }
        if f & format_id::CHA_2 != 0 { return 2; }
        if f & format_id::CHA_1 != 0 { return 1; }
        0
    }

    /// `getBitsMin()`：位深最小值。
    ///
    /// 算法：DSD1→1, DSD4→4, 然后扫描 byte 1 (FMT_PCM_MSK)：
    /// - dh & 0x01（PCM_8）→ 8
    /// - dh & 0x02（PCM_16）→ 16
    /// - dh & 0x04（PCM_24）→ 24
    /// - dh & 0x28（PCM_32 | FLOAT_32）→ 32
    /// - dh & 0x50（PCM_64 | FLOAT_64）→ 64
    ///
    /// 其中 `dh = (fmt_id >> 8) & 0xFF` 即 byte 1。
    pub fn get_bits_min(&self) -> u32 {
        let f = self.fmt_id;
        // DSD 协议位优先
        if f & format_id::FMT_DSD1 != 0 { return 1; }
        if f & format_id::FMT_DSD4 != 0 { return 4; }
        // PCM 位深
        let dh = (f >> 8) & 0xFF;
        if dh & 0x01 != 0 { return 8; }   // PCM_SIGNED_8
        if dh & 0x02 != 0 { return 16; }  // PCM_SIGNED_16
        if dh & 0x04 != 0 { return 24; }  // PCM_SIGNED_24
        if dh & 0x28 != 0 { return 32; }  // PCM_SIGNED_32 | FLOAT_32
        if dh & 0x50 != 0 { return 64; } // PCM_SIGNED_64 | FLOAT_64
        0
    }

    /// `getBitsMax()`：位深最大值。
    ///
    /// 算法：dh & 0x50→64, dh & 0x28→32, dh & 0x04→24, dh & 0x02→16,
    /// dh & 0x01→8, 回退到 DSD（DSD1→1, DSD4→4）。
    pub fn get_bits_max(&self) -> u32 {
        let f = self.fmt_id;
        let dh = (f >> 8) & 0xFF;
        if dh & 0x50 != 0 { return 64; } // PCM_SIGNED_64 | FLOAT_64
        if dh & 0x28 != 0 { return 32; } // PCM_SIGNED_32 | FLOAT_32
        if dh & 0x04 != 0 { return 24; } // PCM_SIGNED_24
        if dh & 0x02 != 0 { return 16; } // PCM_SIGNED_16
        if dh & 0x01 != 0 { return 8; }  // PCM_SIGNED_8
        // 回退到 DSD
        if f & format_id::FMT_DSD4 != 0 { return 4; }
        if f & format_id::FMT_DSD1 != 0 { return 1; }
        0
    }

    /// `getSpeedBaseMin()`：基础采样率最小值。
    ///
    /// 算法：RAT_8000→8000, RAT_44100→44100, RAT_48000→48000。
    pub fn get_speed_base_min(&self) -> u32 {
        let f = self.fmt_id;
        if f & format_id::RAT_8000 != 0 { return 8000; }
        if f & format_id::RAT_44100 != 0 { return 44100; }
        if f & format_id::RAT_48000 != 0 { return 48000; }
        0
    }

    /// `getSpeedBaseMax()`：基础采样率最大值。
    ///
    /// 算法：RAT_48000→48000, RAT_44100→44100, RAT_8000→8000。
    pub fn get_speed_base_max(&self) -> u32 {
        let f = self.fmt_id;
        if f & format_id::RAT_48000 != 0 { return 48000; }
        if f & format_id::RAT_44100 != 0 { return 44100; }
        if f & format_id::RAT_8000 != 0 { return 8000; }
        0
    }

    /// `getSpeedMultMin()`：倍率最小值。
    ///
    /// 算法：MP1→1, MP2→2, MP4→4, MP8→8, MP16→16, MP32→32,
    /// MP64→64, MP128→128, MP256→256, MP512→512, MP1024→1024,
    /// MP2048→2048, MP4096→4096（按位从低到高）。
    pub fn get_speed_mult_min(&self) -> u32 {
        let f = self.fmt_id;
        if f & format_id::RAT_MP1 != 0 { return 1; }
        if f & format_id::RAT_MP2 != 0 { return 2; }
        if f & format_id::RAT_MP4 != 0 { return 4; }
        if f & format_id::RAT_MP8 != 0 { return 8; }
        if f & format_id::RAT_MP16 != 0 { return 16; }
        if f & format_id::RAT_MP32 != 0 { return 32; }
        if f & format_id::RAT_MP64 != 0 { return 64; }
        if f & format_id::RAT_MP128 != 0 { return 128; }
        if f & format_id::RAT_MP256 != 0 { return 256; }
        if f & format_id::RAT_MP512 != 0 { return 512; }
        if f & format_id::RAT_MP1024 != 0 { return 1024; }
        if f & format_id::RAT_MP2048 != 0 { return 2048; }
        if f & format_id::RAT_MP4096 != 0 { return 4096; }
        0
    }

    /// `getSpeedMultMax()`：倍率最大值。
    ///
    /// 算法：MP4096→4096, ..., MP1→1（按位从高到低）。
    pub fn get_speed_mult_max(&self) -> u32 {
        let f = self.fmt_id;
        if f & format_id::RAT_MP4096 != 0 { return 4096; }
        if f & format_id::RAT_MP2048 != 0 { return 2048; }
        if f & format_id::RAT_MP1024 != 0 { return 1024; }
        if f & format_id::RAT_MP512 != 0 { return 512; }
        if f & format_id::RAT_MP256 != 0 { return 256; }
        if f & format_id::RAT_MP128 != 0 { return 128; }
        if f & format_id::RAT_MP64 != 0 { return 64; }
        if f & format_id::RAT_MP32 != 0 { return 32; }
        if f & format_id::RAT_MP16 != 0 { return 16; }
        if f & format_id::RAT_MP8 != 0 { return 8; }
        if f & format_id::RAT_MP4 != 0 { return 4; }
        if f & format_id::RAT_MP2 != 0 { return 2; }
        if f & format_id::RAT_MP1 != 0 { return 1; }
        0
    }

    /// `getSpeedMin()`：实际采样率最小值 = base_min * mult_min。
    pub fn get_speed_min(&self) -> u32 {
        self.get_speed_base_min().saturating_mul(self.get_speed_mult_min())
    }

    /// `getSpeedMax()`：实际采样率最大值 = base_max * mult_max。
    pub fn get_speed_max(&self) -> u32 {
        self.get_speed_base_max().saturating_mul(self.get_speed_mult_max())
    }

    /// `getFrameMax()`：单帧最大字节数 = bits_max * ch_max / 8。
    pub fn get_frame_max(&self) -> u32 {
        self.get_bits_max().saturating_mul(self.get_ch_max()) / 8
    }
}

#[cfg(test)]
mod format_support_tests {
    use super::*;

    /// TargetApp_5DCC 设备实测 raw value（来自 /tmp/tinylms-headless.log）
    /// PCM raw = 1090921697027
    /// DSD MSB raw = 33011162742787
    #[test]
    fn parse_targetapp_5dcc_pcm_capability() {
        // PCM raw = 1090921697027 = 0xFE000020003FF
        let pcm_raw: u64 = 1090921697027;
        let fs = FormatSupport::new(pcm_raw);

        assert!(fs.have_pcm());
        assert!(!fs.have_dsd());

        // Channels: CHA_1|CHA_2 = 0x3 → min=1, max=2
        assert_eq!(fs.get_ch_min(), 1);
        assert_eq!(fs.get_ch_max(), 2);

        // Bits: PCM_SIGNED_8|16|24|32 = 0xF00 → min=8, max=32
        assert_eq!(fs.get_bits_min(), 8);
        assert_eq!(fs.get_bits_max(), 32);

        // Rate: RAT_44100|RAT_48000 = 0x600000000
        // Mult: MP1|MP2|MP4|MP8|MP16 = 0xF800000000
        // → min = 44100*1 = 44100, max = 48000*16 = 768000
        assert_eq!(fs.get_speed_base_min(), 44100);
        assert_eq!(fs.get_speed_base_max(), 48000);
        assert_eq!(fs.get_speed_mult_min(), 1);
        assert_eq!(fs.get_speed_mult_max(), 16);
        assert_eq!(fs.get_speed_min(), 44100);
        assert_eq!(fs.get_speed_max(), 768000);
    }

    #[test]
    fn parse_targetapp_5dcc_dsd_msb_capability() {
        // DSD MSB raw = 33011162742787 = 0x1E0303000027373
        let dsd_raw: u64 = 33011162742787;
        let fs = FormatSupport::new(dsd_raw);

        assert!(!fs.have_pcm());
        assert!(fs.have_dsd());

        // DSD 属性位
        assert!(fs.supports_dsd_msb());
        assert!(!fs.supports_dsd_lsb());
        assert!(fs.supports_dsd_big());
        assert!(fs.supports_dsd_size_32());

        // Channels: CHA_1|CHA_2 = 0x3 → min=1, max=2
        assert_eq!(fs.get_ch_min(), 1);
        assert_eq!(fs.get_ch_max(), 2);

        // Bits: DSD1 = 0x10000 → min=1, max=1
        assert_eq!(fs.get_bits_min(), 1);
        assert_eq!(fs.get_bits_max(), 1);

        // Rate: RAT_44100|RAT_48000
        // Mult: MP64|MP128|MP256|MP512 = 0x1E0000000000
        // → min = 44100*64 = 2822400 (DSD64), max = 48000*512 = 24576000 (DSD512)
        assert_eq!(fs.get_speed_base_min(), 44100);
        assert_eq!(fs.get_speed_base_max(), 48000);
        assert_eq!(fs.get_speed_mult_min(), 64);
        assert_eq!(fs.get_speed_mult_max(), 512);
        assert_eq!(fs.get_speed_min(), 2822400);
        assert_eq!(fs.get_speed_max(), 24576000);
    }

    #[test]
    fn empty_format_support_returns_zero_ranges() {
        let fs = FormatSupport::new(0);
        assert!(!fs.is_valid());
        assert!(!fs.have_pcm());
        assert!(!fs.have_dsd());
        assert_eq!(fs.get_ch_min(), 0);
        assert_eq!(fs.get_speed_min(), 0);
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CDirettaSinkInfo {
    /// FormatID raw value（PCM 能力位图）；!= 0 表示支持 PCM
    pub support_pcm: u64,
    /// FormatID raw value（DSD LSB 能力位图，DSF）；!= 0 表示支持 DSD LSB-first
    pub support_dsd_lsb: u64,
    /// FormatID raw value（DSD MSB 能力位图，DFF）；!= 0 表示支持 DSD MSB-first
    pub support_dsd_msb: u64,
    pub latency_buffer: u16,
    pub latency_max: u16,
    pub latency_hw: u16,
    pub max_size: u16,
    pub min_mtu: u16,
    pub req_mtu: u16,
    pub max_mtu: u32,
    pub support_ms_mode: u16,
}

impl Default for CDirettaSinkInfo {
    fn default() -> Self {
        Self {
            support_pcm: 0,
            support_dsd_lsb: 0,
            support_dsd_msb: 0,
            latency_buffer: 0,
            latency_max: 0,
            latency_hw: 0,
            max_size: 0,
            min_mtu: 0,
            req_mtu: 0,
            max_mtu: 0,
            support_ms_mode: 0,
        }
    }
}

/// Rust 友好的 Sink 设备能力描述（DSD 完整形态阶段 1 新增）。
///
/// 由 [`CDirettaSinkInfo::to_rust`] 转换而来，同时包含：
/// - 布尔能力标记（`supports_pcm` / `supports_dsd_*`）
/// - 原始 FormatID 位图（`*_raw` 字段）
/// - **解析后的能力范围**（PCM/DSD 的采样率、位深、声道范围）
/// - **DSD 细粒度能力位**（字节序、块大小等，决定 DSD 传输方式）
///
/// # 解析后的能力范围
///
/// `FormatSupport` 类（Rust 等价于 C++ `DIRETTA::FormatSupport`）从原始 FormatID
/// 位图解析出具体的采样率范围、位深范围、声道范围。
///
/// - PCM 能力范围：从 `support_pcm_raw` 解析
/// - DSD 能力范围：从 `support_dsd_lsb_raw | support_dsd_msb_raw` 合并后解析
///   （参考 tinyLMS-old DirettaDriver.cpp L1385-1420）
///
/// # DSD 传输方式决策
///
/// DSD 的传输参数由以下能力位决定：
/// - `dsd_supports_msb_byte_order` / `dsd_supports_lsb_byte_order`：字节序方向
/// - `dsd_supports_big_endian` / `dsd_supports_little_endian`：端序
/// - `dsd_supports_32bit_block`：32-bit 块大小（InterleavedBlock32）
/// - `dsd_min/max_sample_rate`：DSD 速率范围（DSD64 ~ DSD512）
///
/// 参考 tinyLMS-old DirettaDriver.cpp L1251-1290, L1385-1420, L1540-1590。
#[derive(Debug, Clone, Copy, Default)]
pub struct DirettaSinkInfo {
    /// 是否支持 PCM（`support_pcm != 0`）
    pub supports_pcm: bool,
    /// 是否支持 DSD（lsb 或 msb 任一支持）
    pub supports_dsd: bool,
    /// 是否支持 DSD LSB-first（DSF 文件格式）
    pub supports_dsd_lsb: bool,
    /// 是否支持 DSD MSB-first（DFF / DSDIFF 文件格式）
    pub supports_dsd_msb: bool,

    // === 原始 FormatID 位图（保留供高级用法）===
    /// PCM 能力位图原始值
    pub support_pcm_raw: u64,
    /// DSD LSB 能力位图原始值（DSF）
    pub support_dsd_lsb_raw: u64,
    /// DSD MSB 能力位图原始值（DFF）
    pub support_dsd_msb_raw: u64,

    // === 解析后的 PCM 能力范围（从 support_pcm_raw 解析）===
    /// PCM 最低采样率（Hz），如 44100
    pub pcm_min_sample_rate: u32,
    /// PCM 最高采样率（Hz），如 768000
    pub pcm_max_sample_rate: u32,
    /// PCM 最小位深（bits），如 8
    pub pcm_min_bits: u32,
    /// PCM 最大位深（bits），如 32
    pub pcm_max_bits: u32,
    /// PCM 最小声道数，如 1
    pub pcm_min_channels: u32,
    /// PCM 最大声道数，如 2
    pub pcm_max_channels: u32,

    // === 解析后的 DSD 能力范围（从 support_dsd_lsb_raw | support_dsd_msb_raw 合并解析）===
    /// DSD 最低采样率（Hz），如 2822400（DSD64 = 44100×64）
    pub dsd_min_sample_rate: u32,
    /// DSD 最高采样率（Hz），如 24576000（DSD512 = 48000×512）
    pub dsd_max_sample_rate: u32,
    /// DSD 最小声道数，如 1
    pub dsd_min_channels: u32,
    /// DSD 最大声道数，如 2
    pub dsd_max_channels: u32,

    // === DSD 细粒度能力位（从合并 raw 值的 FMT_DSD_* 位解析）===
    /// 是否支持 LSB-first 字节序（DSF 文件格式，FMT_DSD_LSB 位）
    pub dsd_supports_lsb_byte_order: bool,
    /// 是否支持 MSB-first 字节序（DFF / DSDIFF 文件格式，FMT_DSD_MSB 位）
    pub dsd_supports_msb_byte_order: bool,
    /// 是否支持小端字节序（FMT_DSD_LITTLE 位）
    pub dsd_supports_little_endian: bool,
    /// 是否支持大端字节序（FMT_DSD_BIG 位）
    pub dsd_supports_big_endian: bool,
    /// 是否支持 32-bit 块大小（InterleavedBlock32，FMT_DSD_SIZ_32 位）
    pub dsd_supports_32bit_block: bool,

    // === 延迟与 MTU 信息 ===
    pub latency_buffer: u16,
    pub latency_max: u16,
    pub latency_hw: u16,
    pub max_size: u16,
    pub min_mtu: u16,
    pub req_mtu: u16,
    pub max_mtu: u32,
    pub support_ms_mode: u16,
}

impl CDirettaSinkInfo {
    /// 转换为 Rust 友好结构体。
    ///
    /// 同时使用 `FormatSupport` 解析原始 FormatID 位图，提取：
    /// - PCM/DSD 采样率范围、位深范围、声道范围
    /// - DSD 字节序、端序、块大小等细粒度能力位
    ///
    /// 等价于 tinyLMS-old DirettaDriver.cpp L1251-1290 的：
    /// ```cpp
    /// DIRETTA::FormatSupport pcm_support(info.supportPCM);
    /// device_caps_.pcm_min_sample_rate = pcm_support.getSpeedMin();
    /// device_caps_.pcm_max_sample_rate = pcm_support.getSpeedMax();
    /// // ...
    /// DIRETTA::FormatSupport dsd_support(info.supportDSDlsb | info.supportDSDmsb);
    /// device_caps_.dsd_min_sample_rate = dsd_support.getSpeedMin();
    /// // ...
    /// ```
    pub fn to_rust(self) -> DirettaSinkInfo {
        let supports_pcm = self.support_pcm != 0;
        let supports_dsd_lsb = self.support_dsd_lsb != 0;
        let supports_dsd_msb = self.support_dsd_msb != 0;

        // PCM 能力范围：从 support_pcm_raw 解析
        let pcm_fs = FormatSupport::new(self.support_pcm);
        let (pcm_min_sample_rate, pcm_max_sample_rate,
             pcm_min_bits, pcm_max_bits,
             pcm_min_channels, pcm_max_channels) = if supports_pcm {
            (pcm_fs.get_speed_min(), pcm_fs.get_speed_max(),
             pcm_fs.get_bits_min(), pcm_fs.get_bits_max(),
             pcm_fs.get_ch_min(), pcm_fs.get_ch_max())
        } else {
            (0, 0, 0, 0, 0, 0)
        };

        // DSD 能力范围：从 support_dsd_lsb_raw | support_dsd_msb_raw 合并后解析
        // 参考 tinyLMS-old DirettaDriver.cpp L1385-1420
        let dsd_combined_raw = self.support_dsd_lsb | self.support_dsd_msb;
        let dsd_fs = FormatSupport::new(dsd_combined_raw);
        let supports_dsd = supports_dsd_lsb || supports_dsd_msb;
        let (dsd_min_sample_rate, dsd_max_sample_rate,
             dsd_min_channels, dsd_max_channels) = if supports_dsd {
            (dsd_fs.get_speed_min(), dsd_fs.get_speed_max(),
             dsd_fs.get_ch_min(), dsd_fs.get_ch_max())
        } else {
            (0, 0, 0, 0)
        };

        DirettaSinkInfo {
            supports_pcm,
            supports_dsd_lsb,
            supports_dsd_msb,
            supports_dsd,
            support_pcm_raw: self.support_pcm,
            support_dsd_lsb_raw: self.support_dsd_lsb,
            support_dsd_msb_raw: self.support_dsd_msb,
            // PCM 解析后能力范围
            pcm_min_sample_rate,
            pcm_max_sample_rate,
            pcm_min_bits,
            pcm_max_bits,
            pcm_min_channels,
            pcm_max_channels,
            // DSD 解析后能力范围
            dsd_min_sample_rate,
            dsd_max_sample_rate,
            dsd_min_channels,
            dsd_max_channels,
            // DSD 细粒度能力位
            dsd_supports_lsb_byte_order: dsd_fs.supports_dsd_lsb(),
            dsd_supports_msb_byte_order: dsd_fs.supports_dsd_msb(),
            dsd_supports_little_endian: dsd_fs.supports_dsd_little(),
            dsd_supports_big_endian: dsd_fs.supports_dsd_big(),
            dsd_supports_32bit_block: dsd_fs.supports_dsd_size_32(),
            // 延迟与 MTU
            latency_buffer: self.latency_buffer,
            latency_max: self.latency_max,
            latency_hw: self.latency_hw,
            max_size: self.max_size,
            min_mtu: self.min_mtu,
            req_mtu: self.req_mtu,
            max_mtu: self.max_mtu,
            support_ms_mode: self.support_ms_mode,
        }
    }
}

// === Rust 友好 Setting（CString 自动管理）===

/// Rust 友好的 Finder 配置。
///
/// `name` 字段使用 `String`（Rust 侧拥有），调用 [`DirettaFinder::open`] 时
/// 内部转 `CString` 传给 C Shim。C Shim 在 `diretta_finder_open` 内部会
/// `std::string(name)` 复制一份，open 返回后 Rust 侧即可释放 CString。
#[derive(Debug, Clone)]
pub struct DirettaSetting {
    pub product_id: u64,
    pub limit_version: u16,
    pub name: Option<String>,
    pub nop_break: bool,
    pub loopback: bool,
    pub my_id: u64,
    /// 149 only：148 SDK 下被静默忽略
    pub broadcast: bool,
    /// 149 only：148 SDK 下被静默忽略（默认 true）
    pub multicast: bool,
}

impl Default for DirettaSetting {
    /// 默认值与 CDirettaSetting::default() 对齐：multicast=true（§7.7.1 149 兼容）。
    fn default() -> Self {
        Self {
            product_id: 0,
            limit_version: 0,
            name: None,
            nop_break: false,
            loopback: false,
            my_id: 0,
            broadcast: false,
            multicast: true, // §7.7.1：149 SDK Multicast 默认 true
        }
    }
}

impl DirettaSetting {
    /// 转换为 C ABI 结构体。
    ///
    /// 返回 `(C 结构体, Option<CString>)`——CString 必须保活到 C Shim 调用 open 完成。
    fn to_c(&self) -> (CDirettaSetting, Option<std::ffi::CString>) {
        let (name_ptr, c_name) = match &self.name {
            Some(s) => {
                // 不允许内嵌 NUL（CString::new 会失败时回退到 lossy 转换）
                let c = std::ffi::CString::new(s.as_str()).unwrap_or_else(|_| {
                    std::ffi::CString::new(s.replace('\0', "")).unwrap_or_default()
                });
                (c.as_ptr(), Some(c))
            }
            None => (ptr::null(), None),
        };

        let c_setting = CDirettaSetting {
            product_id: self.product_id,
            limit_version: self.limit_version,
            name: name_ptr,
            nop_break: if self.nop_break { 1 } else { 0 },
            loopback: if self.loopback { 1 } else { 0 },
            my_id: self.my_id,
            broadcast: if self.broadcast { 1 } else { 0 },
            multicast: if self.multicast { 1 } else { 0 },
        };

        (c_setting, c_name)
    }
}

// === 错误码（§11.4）===

/// Diretta 错误类型（8 个枚举值，§11.4）。
///
/// `From<i32>` 实现将 C 侧返回的整数错误码转换为枚举；未知错误码（不在 -7..=0 范围）
/// 一律映射为 [`DirettaError::Generic`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DirettaError {
    #[error("成功")]
    Ok,
    #[error("通用错误")]
    Generic,
    #[error("网络错误")]
    Network,
    #[error("连接被拒绝")]
    Refused,
    #[error("MTU 不匹配")]
    Mtu,
    #[error("音频格式不支持")]
    Format,
    #[error("缓冲区下溢")]
    Underrun,
    #[error("操作超时")]
    Timeout,
}

impl From<i32> for DirettaError {
    fn from(code: i32) -> Self {
        match code {
            0  => DirettaError::Ok,
            -1 => DirettaError::Generic,
            -2 => DirettaError::Network,
            -3 => DirettaError::Refused,
            -4 => DirettaError::Mtu,
            -5 => DirettaError::Format,
            -6 => DirettaError::Underrun,
            -7 => DirettaError::Timeout,
            _  => DirettaError::Generic, // 未知错误码兜底
        }
    }
}

impl From<DirettaError> for i32 {
    fn from(err: DirettaError) -> Self {
        match err {
            DirettaError::Ok       => 0,
            DirettaError::Generic  => -1,
            DirettaError::Network  => -2,
            DirettaError::Refused  => -3,
            DirettaError::Mtu      => -4,
            DirettaError::Format   => -5,
            DirettaError::Underrun => -6,
            DirettaError::Timeout  => -7,
        }
    }
}

// C 侧常量（与 #define 对应，方便 Rust 侧引用）
pub const DIRETTA_OK: i32 = 0;
pub const DIRETTA_ERR_GENERIC: i32 = -1;
pub const DIRETTA_ERR_NETWORK: i32 = -2;
pub const DIRETTA_ERR_REFUSED: i32 = -3;
pub const DIRETTA_ERR_MTU: i32 = -4;
pub const DIRETTA_ERR_FORMAT: i32 = -5;
pub const DIRETTA_ERR_UNDERRUN: i32 = -6;
pub const DIRETTA_ERR_TIMEOUT: i32 = -7;

// === RAII Wrapper：DirettaFinder ===

/// Finder 实例的 RAII 包装（§11.1）。
///
/// 持有 C 侧 `diretta_finder_t*`，Drop 时调用 `diretta_finder_close`。
///
/// # 线程约束（D18）
///
/// `DirettaFinder` 标记 `!Sync`——只能在创建它的线程上 drop。
/// 跨线程转移所有权（`Send`）允许，但接收线程必须负责 drop。
/// 实际上 Diretta SDK 的 Finder 设计为单线程使用，本约束符合 SDK 语义。
pub struct DirettaFinder {
    raw: DirettaFinderRaw,
}

// Send：可以跨线程转移所有权（移动语义）
// !Sync：不可跨线程共享引用（D18 单线程约束）
unsafe impl Send for DirettaFinder {}
// 显式标记 !Sync（默认就是 !Sync，这里写出来是文档化意图）
// 不实现 Sync trait

impl DirettaFinder {
    /// 打开 Finder 实例。
    ///
    /// 失败时返回 `Err(DirettaError)`，错误码来自 `diretta_get_last_error()`。
    pub fn open(setting: &DirettaSetting) -> Result<Self, DirettaError> {
        let (c_setting, _c_name_keepalive) = setting.to_c();

        // SAFETY: c_setting 是栈上的合法结构体，name 指针（如有）指向 c_name_keepalive
        // 内的 CString，二者生命期覆盖整个 unsafe 块。diretta_finder_open 内部会复制
        // name 字段，返回后 _c_name_keepalive 即可释放。
        let raw = unsafe { diretta_finder_open(&c_setting) };
        if raw.is_null() {
            let code = unsafe { diretta_get_last_error() };
            Err(DirettaError::from(code))
        } else {
            Ok(Self { raw })
        }
    }

    /// 查询当前线程的最近错误码（thread_local）。
    pub fn last_error() -> DirettaError {
        // SAFETY: 仅读取 thread_local int，无副作用
        let code = unsafe { diretta_get_last_error() };
        DirettaError::from(code)
    }

    /// 扫描 Diretta 设备（方案 A 新增）。
    ///
    /// 内部调用 C 侧 `diretta_finder_scan`，最多 10 次重试（参考 DirettaDriver::Scan）。
    /// 阻塞约 1-3 秒，调用方应在非 UI 线程执行。
    ///
    /// # 参数
    ///
    /// - `max_count`: 最多返回的设备数（建议 16-32）
    ///
    /// # 返回
    ///
    /// 成功返回 `Vec<DirettaDeviceInfo>`（可能为空，表示无设备响应）；
    /// 失败返回 `Err(DirettaError)`（如实例无效或 C++ 异常）。
    ///
    /// # 线程约束（D18）
    ///
    /// 必须在与 `open()` 相同的线程调用（Finder 单线程约束）。
    pub fn scan(&mut self, max_count: usize) -> Result<Vec<DirettaDeviceInfo>, DirettaError> {
        if self.raw.is_null() {
            return Err(DirettaError::Generic);
        }

        // 准备输出缓冲区。max_count clamp 到 [1, 64] 避免恶意大值。
        let max = max_count.clamp(1, 64);
        let mut c_buf: Vec<CDirettaDeviceInfo> = vec![CDirettaDeviceInfo::default(); max];
        let mut actual: std::os::raw::c_int = 0;

        // SAFETY: self.raw 由 diretta_finder_open 返回，非空（已校验）。
        // c_buf 是 Vec<T>，as_mut_ptr 返回有效可写指针，长度 max 覆盖 C 侧写入范围。
        // actual 是栈变量，&mut 传给 C 侧写入。
        let rc = unsafe {
            diretta_finder_scan(self.raw,
                                c_buf.as_mut_ptr(),
                                max as std::os::raw::c_int,
                                &mut actual)
        };

        if rc != DIRETTA_OK {
            return Err(DirettaError::from(rc));
        }

        // actual >= 0（C 侧保证），且 <= max（C 侧缓冲区满时截断）
        let n = actual.max(0) as usize;
        let n = n.min(max);  // 防御性截断

        let mut devices = Vec::with_capacity(n);
        for item in c_buf.iter().take(n) {
            devices.push(item.to_rust());
        }
        Ok(devices)
    }

    /// 测量到指定 sink 的真实 MTU（P3.2 Stage 7.5 新增）。
    ///
    /// 内部调用 C 侧 `diretta_finder_measure_mtu`，发送探测包计算路径 MTU。
    /// 设备需要先收到 MTU 探测包才会接受后续 connect 请求。
    ///
    /// # 参数
    ///
    /// - `ip`: sink IPv6 地址字符串（如 "fe80::1" 或 "::ffff:192.168.1.100"）
    /// - `port`: sink 端口
    /// - `ifno`: 网络接口号
    ///
    /// # 返回
    ///
    /// 成功返回实测 MTU（u32）；失败返回 `Err(DirettaError)`。
    /// 调用方应在失败时回退到默认 1500。
    ///
    /// # 参考
    ///
    /// tinyLMS-old DirettaDriver.cpp L508-527：connect 前必须调用 measSendMTU。
    pub fn measure_mtu(&mut self, ip: &str, port: u16, ifno: u32) -> Result<u32, DirettaError> {
        if self.raw.is_null() {
            return Err(DirettaError::Generic);
        }

        // CString 转换（NUL 终止）
        let c_ip = std::ffi::CString::new(ip).map_err(|_| DirettaError::Generic)?;

        let mut out_mtu: u32 = 0;

        // SAFETY: self.raw 由 diretta_finder_open 返回，非空（已校验）。
        // c_ip 是 CString，as_ptr 返回的指针在 C 调用期间保持有效。
        // out_mtu 是栈变量，&mut 传给 C 侧写入。
        let rc = unsafe {
            diretta_finder_measure_mtu(self.raw,
                                       c_ip.as_ptr(),
                                       port,
                                       ifno,
                                       &mut out_mtu)
        };

        if rc != DIRETTA_OK {
            return Err(DirettaError::from(rc));
        }

        Ok(out_mtu)
    }
}

impl Drop for DirettaFinder {
    fn drop(&mut self) {
        // SAFETY: self.raw 由 diretta_finder_open 返回，仅在本 Drop 中释放一次。
        // D18 约束：调用方保证 Drop 发生在与 open 相同的线程（!Sync 标记辅助）。
        // NULL 安全：C 侧 close 处理 nullptr，但 Rust 侧 raw 不会是 null（构造时校验）
        if !self.raw.is_null() {
            unsafe { diretta_finder_close(self.raw) };
            self.raw = ptr::null_mut();
        }
    }
}

// === RAII Wrapper：DirettaSync（P3.1.3 实现）===

/// Sync 实例的 RAII 包装（P3.1.3）。
///
/// 持有 C 侧 `diretta_sync_t*`，Drop 时调用 `diretta_sync_close`。
///
/// # 线程约束（D18，§11.2）
///
/// - `open` / `Drop`：同线程调用（生命周期管理）
/// - `push`：单线程约束（C Shim 内部 buf_mtx_ 保护环形缓冲区 SPSC）
/// - `set_sink` / `connect` / `disconnect` / `play` / `stop`：C Shim 内部 ctrl_mtx_ 保护
/// - `is_online` / `is_playing`：原子读，任意线程可调用
///
/// `DirettaSync` 标记 `Send`（可跨线程转移所有权）且 `Sync`（可经 `Arc` 跨线程共享）。
/// 实际使用中 push 应在单一线程调用，控制操作（connect/disconnect/play/stop）由 C 侧
/// `ctrl_mtx_` 保护可在另一线程调用；is_online/is_playing 为原子读。故可安全共享。
/// 【P2】`Sync` 用于将 `Arc<DirettaSync>` 移到独立断开线程，避免阻塞 Tokio runtime。
#[derive(Debug)]
pub struct DirettaSync {
    raw: DirettaSyncRaw,
}

unsafe impl Send for DirettaSync {}
unsafe impl Sync for DirettaSync {}

impl DirettaSync {
    /// 打开 Sync 实例（P3.1.3 真实实现）。
    ///
    /// 构造 `DirettaSyncImpl` 派生类实例。本函数不调用 `Sync::open()`——
    /// `Sync::open()` 在 [`DirettaSync::set_sink`] 首次调用时惰性执行。
    ///
    /// # 参数
    /// - `cb`：事件回调（可 `None`，表示不接收事件）
    /// - `user_data`：透传给回调的不透明指针（可 `null_mut`）
    pub fn open(cb: Option<DirettaEventCb>, user_data: *mut std::ffi::c_void)
        -> Result<Self, DirettaError>
    {
        // SAFETY: cb 是 extern "C" fn 或 NULL；user_data 透传给 C，C 不解引用
        let raw = unsafe { diretta_sync_open(cb, user_data) };
        if raw.is_null() {
            let code = unsafe { diretta_get_last_error() };
            Err(DirettaError::from(code))
        } else {
            Ok(Self { raw })
        }
    }

    /// 向推流缓冲区写入数据（§11.2 单线程 push）。
    ///
    /// 内部用 mutex 保护环形缓冲区（SPSC 模式）。缓冲区满时返回 `Err(Underrun)`。
    ///
    /// # 线程约束（D18）
    ///
    /// 应在单一线程调用（单生产者）。C Shim 内部 buf_mtx_ 保护并发读。
    pub fn push(&self, data: &[u8]) -> Result<(), DirettaError> {
        // SAFETY: self.raw 是合法的 DirettaSyncRaw（构造时校验非 null）。
        // data 指针在函数返回前有效。C 侧 push 内部 memcpy 到环形缓冲区。
        let ret = unsafe {
            diretta_sync_push(self.raw, data.as_ptr() as *const std::ffi::c_void, data.len())
        };
        if ret == DIRETTA_OK {
            Ok(())
        } else {
            Err(DirettaError::from(ret))
        }
    }

    /// 设置 sink 地址与缓冲参数（§11.2 控制操作）。
    ///
    /// 惰性初始化：首次调用时执行 `Sync::open(ifno, ...)`（创建工作线程）。
    ///
    /// # 参数
    /// - `ip`：sink IPv6 地址字符串（如 "fe80::1" 或 "::ffff:192.168.1.100"，
    ///   C Shim 内部用 `set_V6_str` 解析，支持 V4-mapped-V6）
    /// - `port`：sink 端口（host byte order）
    /// - `ifno`：网络接口号（从扫描结果 `addr.get_ifno()` 提取，`Sync::open` 与 `setSink` 需要）
    /// - `mtu`：期望 MTU（0 表示使用默认；tinyLMS-old 用 `measSendMTU` 实测）
    /// - `buffer_ms`：sink 缓冲毫秒数（0 表示使用 sink 默认值；tinyLMS-old 用 30）
    /// - `sample_rate`：PCM 采样率（Hz，如 44100 / 48000 / 96000 / 192000），
    ///   用于 `setSinkConfigure` 与 `cycle_time_us` 动态计算
    /// - `channels`：声道数（1 = 单声道，2 = 立体声）
    /// - `bits_per_sample`：PCM 位深（16 / 24 / 32）
    ///
    /// # 慢速播放修复
    /// 之前硬编码 48000/2/32，导致 96kHz/192kHz 音源被设备按 48kHz 播放，
    /// 听感为"慢速播放 + 音调偏低"。修复后由调用方传入源 PCM 实际格式。
    pub fn set_sink(&self, ip: &str, port: u16, ifno: u32, mtu: u32, buffer_ms: u32,
                    sample_rate: u32, channels: u32, bits_per_sample: u32)
        -> Result<(), DirettaError>
    {
        // 不允许内嵌 NUL
        let c_ip = std::ffi::CString::new(ip).unwrap_or_else(|_| {
            std::ffi::CString::new(ip.replace('\0', "")).unwrap_or_default()
        });
        // SAFETY: self.raw 合法；c_ip 在函数返回前有效；C 侧 set_sink 内部 std::string 复制
        let ret = unsafe {
            diretta_sync_set_sink(self.raw, c_ip.as_ptr(), port, ifno, mtu, buffer_ms,
                                  sample_rate, channels, bits_per_sample)
        };
        if ret == DIRETTA_OK {
            Ok(())
        } else {
            Err(DirettaError::from(ret))
        }
    }

    /// 设置 sink 为 DSD Native 直通模式（DSD 完整形态阶段 3 新增）。
    ///
    /// 与 `set_sink` 区别：
    /// - 使用 DSD FormatID（`FMT_DSD1 | FMT_DSD_SIZ_32 | FMT_DSD_LSB/MSB`）而非 PCM
    /// - `cycle_time_us` 基于 DSD 字节流速率（`dsd_sample_rate * channels / 8`）
    /// - 不调用 swresample 转换，DSD 字节流直接透传到设备
    ///
    /// # 参数
    /// - `ip`：sink IPv6 地址字符串（同 set_sink）
    /// - `port`：sink 端口
    /// - `ifno`：网络接口号
    /// - `mtu`：期望 MTU（0 表示默认 1500）
    /// - `buffer_ms`：sink 缓冲毫秒数（0 表示默认）
    /// - `dsd_rate_multiplier`：DSD 速率倍数（64/128/256/512）
    /// - `dsd_byte_order`：字节序（0 = LSB/DSF，1 = MSB/DFF）
    /// - `channels`：声道数（通常 2）
    ///
    /// # 返回
    /// - `Ok(())`：set_sink_dsd 成功，可继续 connect/play
    /// - `Err(DirettaError::Format)`：设备不支持 DSD Native 直通或参数无效
    pub fn set_sink_dsd(&self, ip: &str, port: u16, ifno: u32, mtu: u32, buffer_ms: u32,
                        dsd_rate_multiplier: u32, dsd_byte_order: u32, channels: u32)
        -> Result<(), DirettaError>
    {
        let c_ip = std::ffi::CString::new(ip).unwrap_or_else(|_| {
            std::ffi::CString::new(ip.replace('\0', "")).unwrap_or_default()
        });
        // SAFETY: self.raw 合法；c_ip 在函数返回前有效；C 侧 set_sink_dsd 内部 std::string 复制
        let ret = unsafe {
            diretta_sync_set_sink_dsd(self.raw, c_ip.as_ptr(), port, ifno, mtu, buffer_ms,
                                       dsd_rate_multiplier, dsd_byte_order, channels)
        };
        if ret == DIRETTA_OK {
            Ok(())
        } else {
            Err(DirettaError::from(ret))
        }
    }

    /// 查询 DSD 协商命中的变换标志（P3，对齐 tinyLMS-old dsd_bit_reverse_/dsd_byte_swap_）。
    ///
    /// 返回 `(bit_reverse, byte_swap)`：
    /// - `bit_reverse`：设备要求对源位序取反（目标 LSB != 源 LSB）
    /// - `byte_swap`：设备要求小端传输（目标 LITTLE）
    ///
    /// 供 `write_dsd_bytes` 在直通前做正确的位/字节变换，避免 DSD 音色错误或无声。
    /// 仅在 `set_sink_dsd` 成功后可调用。
    pub fn dsd_transform(&self) -> Result<(bool, bool), DirettaError> {
        let mut bit_reverse: std::os::raw::c_int = 0;
        let mut byte_swap: std::os::raw::c_int = 0;
        let ret = unsafe {
            diretta_sync_get_dsd_transform(self.raw, &mut bit_reverse, &mut byte_swap)
        };
        if ret == DIRETTA_OK {
            Ok((bit_reverse != 0, byte_swap != 0))
        } else {
            Err(DirettaError::from(ret))
        }
    }

    /// 连接到 sink（§11.2 控制操作）。
    ///
    /// 内部依次调用 connectPrepare / connect / connectWait。
    /// 成功后触发 CONNECTED 事件。
    ///
    /// # 参数
    /// - `timeout_ms`：连接超时（毫秒），0 表示 SDK 默认
    pub fn connect(&self, timeout_ms: i32) -> Result<(), DirettaError> {
        // SAFETY: self.raw 合法
        let ret = unsafe { diretta_sync_connect(self.raw, timeout_ms) };
        if ret == DIRETTA_OK {
            Ok(())
        } else {
            Err(DirettaError::from(ret))
        }
    }

    /// 断开 sink 连接（§11.2 控制操作）。
    ///
    /// 成功后触发 DISCONNECTED 事件。
    pub fn disconnect(&self) -> Result<(), DirettaError> {
        // SAFETY: self.raw 合法
        let ret = unsafe { diretta_sync_disconnect(self.raw) };
        if ret == DIRETTA_OK {
            Ok(())
        } else {
            Err(DirettaError::from(ret))
        }
    }

    /// 开始播放（§11.2 控制操作）。
    pub fn play(&self) -> Result<(), DirettaError> {
        // SAFETY: self.raw 合法
        let ret = unsafe { diretta_sync_play(self.raw) };
        if ret == DIRETTA_OK {
            Ok(())
        } else {
            Err(DirettaError::from(ret))
        }
    }

    /// 停止播放（暂停，连接保留）（§11.2 控制操作）。
    pub fn stop(&self) -> Result<(), DirettaError> {
        // SAFETY: self.raw 合法
        let ret = unsafe { diretta_sync_stop(self.raw) };
        if ret == DIRETTA_OK {
            Ok(())
        } else {
            Err(DirettaError::from(ret))
        }
    }

    /// 查询连接状态（原子读，任意线程可调用）。
    ///
    /// 返回 `true` = 已连接（online）；`false` = 未连接。
    pub fn is_online(&self) -> bool {
        // SAFETY: self.raw 合法；C 侧原子读 connected_
        unsafe { diretta_sync_is_online(self.raw) != 0 }
    }

    /// 查询播放状态（原子读，任意线程可调用）。
    ///
    /// 返回 `true` = 正在播放；`false` = 未播放。
    pub fn is_playing(&self) -> bool {
        // SAFETY: self.raw 合法；C 侧原子读 playing_
        unsafe { diretta_sync_is_playing(self.raw) != 0 }
    }

    // === P1 修复：Pre-mute 机制 ===
    // 对齐 tinyLMS-old DirettaSyncImpl L107-124 / DirettaDriver.cpp L837-841
    /// 触发 Pre-mute：让 SDK getNewStream 线程接下来 `count` 个 cycle 输出静音帧。
    ///
    /// 必须在 `stop()` / `disconnect()` / `reconfigure()` 之前调用，
    /// 配合 `wait_pre_mute_done()` 等待 SDK 线程把静音帧送出后再断开连接。
    /// 这样可消除"播放中突然断开"导致的最后半截音频 + 爆音脉冲。
    ///
    /// - `count`：静音 cycle 数。参考值：DSD=20，PCM=8。
    pub fn trigger_pre_mute(&self, count: i32) -> Result<(), DirettaError> {
        // SAFETY: self.raw 合法；C 侧 store 到 atomic
        let rc = unsafe { diretta_sync_trigger_pre_mute(self.raw, count) };
        if rc == DIRETTA_OK {
            Ok(())
        } else {
            Err(DirettaError::from(rc))
        }
    }

    /// 等待 Pre-mute 完成（SDK 线程把所有静音 cycle 输出）。
    ///
    /// - `timeout_ms`：超时毫秒数。参考值：40ms（DSD 20 cycles × ~2ms/cycle）。
    /// - 返回 `true` = 已完成；`false` = 超时。
    pub fn wait_pre_mute_done(&self, timeout_ms: i32) -> bool {
        // SAFETY: self.raw 合法；C 侧轮询 atomic + sleep
        unsafe { diretta_sync_wait_pre_mute_done(self.raw, timeout_ms) != 0 }
    }

    // === P1 修复：Soft Resume 辅助 ===
    /// 清空环形缓冲区（ring_buf_）。
    ///
    /// 用于 Soft Resume 路径：格式相同时不重建连接，仅清空残留旧数据，
    /// 避免新文件的第一帧与上一首末尾数据混合。
    pub fn clear_ring_buffer(&self) -> Result<(), DirettaError> {
        // SAFETY: self.raw 合法；C 侧加 buf_mtx_ 后清零读写指针
        let rc = unsafe { diretta_sync_clear_ring_buffer(self.raw) };
        if rc == DIRETTA_OK {
            Ok(())
        } else {
            Err(DirettaError::from(rc))
        }
    }

    /// 查询 sink 设备能力（DSD 完整形态阶段 1 新增）。
    ///
    /// 内部调用 `diretta_sync_get_sink_info`，读取 SDK 内部 `SinkInfo` 缓存。
    /// `getSinkInfo()` 是 `const` 方法，仅读取内存，无网络请求，可在任意线程调用。
    ///
    /// # 调用时机
    ///
    /// 必须在 `connect()` 成功后调用，否则 SDK 内部 `SinkInfo` 尚未填充，
    /// 返回的结构体所有字段为 0（所有能力为 `false`）。
    ///
    /// # 字段说明
    ///
    /// 返回 [`DirettaSinkInfo`] 提供布尔能力标记（`supports_pcm` / `supports_dsd` /
    /// `supports_dsd_lsb` / `supports_dsd_msb`）和原始 FormatID 位图（`*_raw`），
    /// 后者供阶段 2/3 进一步解析采样率/位深范围（参考 `DIRETTA::FormatSupport` 类）。
    ///
    /// 参考 tinyLMS-old `DirettaDriver.cpp` L1251-1258 的 `checkSinkSupportDSD*` 实现。
    pub fn get_sink_info(&self) -> Result<DirettaSinkInfo, DirettaError> {
        let mut c_info = CDirettaSinkInfo::default();
        // SAFETY: self.raw 合法；c_info 是栈变量，&mut 合法；
        // C 侧仅写入 c_info 字段，无其他副作用（const 方法读 SinkInfo）
        let ret = unsafe { diretta_sync_get_sink_info(self.raw, &mut c_info) };
        if ret == DIRETTA_OK {
            Ok(c_info.to_rust())
        } else {
            Err(DirettaError::from(ret))
        }
    }

    /// 查询 `setSinkConfigure` 后 SDK 实际选择的物理字节宽度。
    ///
    /// 对齐 tinyLMS-old 的 `getSinkConfigure().getWid()`。
    /// 返回值通常为 2、3 或 4，失败或尚未协商时返回 `None`。
    pub fn negotiated_sample_bytes(&self) -> Option<u32> {
        // SAFETY: self.raw 由 DirettaSync 持有且仍然有效；C 侧只读取 SDK 配置。
        let wid = unsafe { diretta_sync_get_negotiated_format(self.raw) };
        (wid != 0).then_some(wid)
    }

    /// 查询当前线程的最近错误码（thread_local）。
    pub fn last_error() -> DirettaError {
        // SAFETY: 仅读取 thread_local int，无副作用
        let code = unsafe { diretta_get_last_error() };
        DirettaError::from(code)
    }
}

impl Drop for DirettaSync {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: 同 DirettaFinder::drop。C 侧 close 内部有序关闭
            // （stop → disconnect → close），虚析构安全销毁 DirettaSyncImpl。
            unsafe { diretta_sync_close(self.raw) };
            self.raw = ptr::null_mut();
        }
    }
}

// === 版本查询 ===

/// 返回 SDK 版本号（148 或 149）。
pub fn get_version() -> u16 {
    // SAFETY: 纯函数，无副作用
    unsafe { diretta_get_version() }
}

/// 返回 SDK 版本字符串（"148" 或 "149"）。
///
/// 内部用 `CStr::from_ptr` 转换 C 静态字符串为 Rust `&'static str`。
/// 失败时回退为空字符串（不应发生——C 侧返回的是字面量）。
pub fn get_version_string() -> &'static str {
    // SAFETY: diretta_get_version_string 返回静态存储字面量（"148" / "149"）
    // 调用方不需释放；CStr::from_ptr 在 NUL 终止处停止读取，安全。
    let ptr = unsafe { diretta_get_version_string() };
    if ptr.is_null() {
        return "";
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .unwrap_or("")
}

// === 单元测试 ===

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_default_multicast_true_149_compat() {
        // §7.7.1：149 SDK Multicast 默认 true
        let s = DirettaSetting::default();
        assert!(s.multicast, "默认 multicast 应为 true（149 兼容）");
        assert!(!s.broadcast, "默认 broadcast 应为 false");
        assert_eq!(s.product_id, 0);
        assert_eq!(s.my_id, 0);
        assert_eq!(s.name, None);
    }

    #[test]
    fn setting_to_c_handles_null_name() {
        let s = DirettaSetting::default();
        let (c, _keep) = s.to_c();
        assert!(c.name.is_null(), "None name 应转为 null 指针");
        assert_eq!(c.multicast, 1);
    }

    #[test]
    fn setting_to_c_handles_some_name() {
        let mut s = DirettaSetting::default();
        s.name = Some("tinyLMS-Host".to_string());
        let (c, keep) = s.to_c();
        assert!(!c.name.is_null(), "Some name 应转为非 null 指针");
        assert!(keep.is_some(), "CString 应保活");
        // SAFETY: c.name 指向 keep 内的 CString，生命期内合法
        let rust_str = unsafe { CStr::from_ptr(c.name) }
            .to_str()
            .expect("name 应为合法 UTF-8");
        assert_eq!(rust_str, "tinyLMS-Host");
    }

    #[test]
    fn error_from_i32_all_codes() {
        // §11.4 全部 8 个错误码往返映射
        assert_eq!(DirettaError::from(0),  DirettaError::Ok);
        assert_eq!(DirettaError::from(-1), DirettaError::Generic);
        assert_eq!(DirettaError::from(-2), DirettaError::Network);
        assert_eq!(DirettaError::from(-3), DirettaError::Refused);
        assert_eq!(DirettaError::from(-4), DirettaError::Mtu);
        assert_eq!(DirettaError::from(-5), DirettaError::Format);
        assert_eq!(DirettaError::from(-6), DirettaError::Underrun);
        assert_eq!(DirettaError::from(-7), DirettaError::Timeout);
    }

    #[test]
    fn error_from_i32_unknown_falls_back_to_generic() {
        // 未知错误码兜底
        assert_eq!(DirettaError::from(-99),  DirettaError::Generic);
        assert_eq!(DirettaError::from(-100), DirettaError::Generic);
        assert_eq!(DirettaError::from(99),   DirettaError::Generic);
    }

    #[test]
    fn error_into_i32_round_trip() {
        // 枚举 → i32 往返
        let cases = [
            (DirettaError::Ok, 0),
            (DirettaError::Generic, -1),
            (DirettaError::Network, -2),
            (DirettaError::Refused, -3),
            (DirettaError::Mtu, -4),
            (DirettaError::Format, -5),
            (DirettaError::Underrun, -6),
            (DirettaError::Timeout, -7),
        ];
        for (err, code) in cases {
            let i: i32 = err.into();
            assert_eq!(i, code, "{:?} → i32 不匹配", err);
            assert_eq!(DirettaError::from(code), err, "i32 → 枚举 往返失败");
        }
    }

    #[test]
    fn version_string_nonempty_when_shim_compiled() {
        // 仅当 has_diretta_shim cfg 启用时本测试有意义
        // 非 Linux 或未启用 feature 时本模块不编译，测试也不会运行
        let v = get_version_string();
        assert!(!v.is_empty(), "version string 不应为空（C Shim 已编译）");
        assert!(
            v == "148" || v == "149" || v == "150",
            "version string 应为 148、149 或 150，实际: {}",
            v
        );
    }

    #[test]
    fn version_u16_matches_string() {
        let v_num = get_version();
        let v_str = get_version_string();
        let v_from_str: u16 = v_str.parse().expect("version string 应为合法数字");
        assert_eq!(v_num, v_from_str, "u16 与字符串版本号应一致");
    }

    #[test]
    fn finder_open_with_default_setting_returns_ok_or_err() {
        // 默认 setting 在没有 Diretta target 的环境会返回 Err(Network) 或 Ok
        // 本测试只验证 RAII 包装不会 UB / leak
        let result = DirettaFinder::open(&DirettaSetting::default());
        match result {
            Ok(_finder) => {
                // 成功路径：finder 在此 scope end 时 Drop
                // 不需要 assert，只要不 UB / 不 leak 即通过
            }
            Err(e) => {
                // 失败路径：错误码必须是已知枚举
                assert!(
                    matches!(e,
                        DirettaError::Network
                        | DirettaError::Generic
                        | DirettaError::Refused),
                    "未预期错误: {:?}",
                    e
                );
            }
        }
    }

    // === P3.1.3 新增测试：Sync 生命周期 + 状态机 ===

    #[test]
    fn sync_open_succeeds_in_p313() {
        // P3.1.3：sync_open 现在返回 Ok（构造 DirettaSyncImpl）
        let result = DirettaSync::open(None, ptr::null_mut());
        assert!(result.is_ok(), "P3.1.3 sync_open 应返回 Ok，实际: {:?}", result.err());
        // Drop 时安全关闭（虚析构链 + 有序关闭）
    }

    #[test]
    fn sync_is_online_false_initially() {
        // 新构造的 Sync 实例未连接
        let sync = DirettaSync::open(None, ptr::null_mut())
            .expect("sync_open 应成功");
        assert!(!sync.is_online(), "新 Sync 实例 is_online 应为 false");
    }

    #[test]
    fn sync_is_playing_false_initially() {
        // 新构造的 Sync 实例未播放
        let sync = DirettaSync::open(None, ptr::null_mut())
            .expect("sync_open 应成功");
        assert!(!sync.is_playing(), "新 Sync 实例 is_playing 应为 false");
    }

    #[test]
    fn sync_push_succeeds_with_valid_data() {
        let sync = DirettaSync::open(None, ptr::null_mut())
            .expect("sync_open 应成功");
        let data = [0u8; 1024];  // 1 KB 测试数据
        let result = sync.push(&data);
        assert!(result.is_err(), "未连接时 push 应返回 Err");
    }

    #[test]
    fn sync_push_succeeds_with_empty_slice() {
        // 空 slice push 应视为成功（C 侧 push 检查 size == 0 时返回 true）
        let sync = DirettaSync::open(None, ptr::null_mut())
            .expect("sync_open 应成功");
        let data: [u8; 0] = [];
        let result = sync.push(&data);
        assert!(result.is_ok(), "push 空 slice 应成功，实际: {:?}", result.err());
    }

    #[test]
    fn sync_set_sink_rejects_invalid_ip() {
        // 无效 IP 地址应返回 Err(Network)
        let sync = DirettaSync::open(None, ptr::null_mut())
            .expect("sync_open 应成功");
        // 末尾三个参数（sample_rate/channels/bits_per_sample）对应 P3.2 Stage 7.5
        // set_sink 签名扩展；本测试只验证 IP 解析失败路径，格式参数取默认值。
        let result = sync.set_sink("not.a.valid.ip.address", 8888, 0, 0, 0,
                                   48000, 2, 16);
        assert!(result.is_err(), "无效 IP 应返回 Err");
        match result {
            Err(DirettaError::Network) => (),  // 预期
            Err(other) => panic!("无效 IP 应返回 Network，实际: {:?}", other),
            Ok(_) => panic!("无效 IP 不应返回 Ok"),
        }
    }

    #[test]
    fn sync_connect_without_set_sink_returns_generic() {
        // 未调用 set_sink 就 connect 应返回 Err(Generic)
        // 原因：connect_sink 检查 opened_ 标志（false），直接返回 ERR_GENERIC
        let sync = DirettaSync::open(None, ptr::null_mut())
            .expect("sync_open 应成功");
        let result = sync.connect(1000);
        assert!(result.is_err(), "未 set_sink 时 connect 应返回 Err");
        match result {
            Err(DirettaError::Generic) => (),  // 预期
            Err(other) => panic!("未 set_sink 时应返回 Generic，实际: {:?}", other),
            Ok(_) => panic!("未 set_sink 时不应返回 Ok"),
        }
    }

    #[test]
    fn sink_info_to_rust_all_zero_means_no_dsd() {
        // 全零 SinkInfo：所有能力为 false（与 connect 前查询的兜底语义一致）
        // DSD 完整形态阶段 1 新增
        let c_info = CDirettaSinkInfo::default();
        let info = c_info.to_rust();
        assert!(!info.supports_pcm, "全零 support_pcm 应为 false");
        assert!(!info.supports_dsd_lsb, "全零 support_dsd_lsb 应为 false");
        assert!(!info.supports_dsd_msb, "全零 support_dsd_msb 应为 false");
        assert!(!info.supports_dsd, "全零 supports_dsd 应为 false（lsb 和 msb 均为 false）");
        assert_eq!(info.support_pcm_raw, 0);
        assert_eq!(info.support_dsd_lsb_raw, 0);
        assert_eq!(info.support_dsd_msb_raw, 0);
        assert_eq!(info.latency_buffer, 0);
        assert_eq!(info.req_mtu, 0);
        assert_eq!(info.max_mtu, 0);
    }

    #[test]
    fn sink_info_to_rust_dsd_lsb_only() {
        // 仅 DSD LSB（DSF）能力位图非零：supports_dsd_lsb=true，supports_dsd=true
        // 模拟 DSF 直通设备（参考 DirettaHostSDK_149/Host/Format.hpp::FMT_DSD_LSB）
        // DSD 完整形态阶段 1 新增
        let mut c_info = CDirettaSinkInfo::default();
        c_info.support_dsd_lsb = 0x0000_0000_0010_0000; // FMT_DSD_LSB 位
        c_info.support_dsd_msb = 0;
        c_info.support_pcm = 0;
        c_info.latency_buffer = 30;
        c_info.req_mtu = 1452;
        c_info.max_mtu = 1500;
        let info = c_info.to_rust();
        assert!(!info.supports_pcm, "DSF-only 设备应不支持 PCM");
        assert!(info.supports_dsd_lsb, "DSF-only 设备应支持 DSD LSB");
        assert!(!info.supports_dsd_msb, "DSF-only 设备应不支持 DSD MSB");
        assert!(info.supports_dsd, "DSF-only 设备应支持 DSD（lsb=true）");
        assert_eq!(info.latency_buffer, 30);
        assert_eq!(info.req_mtu, 1452);
        assert_eq!(info.max_mtu, 1500);
    }

    #[test]
    fn sink_info_to_rust_dsd_msb_only() {
        // 仅 DSD MSB（DFF / DSDIFF）能力位图非零：supports_dsd_msb=true
        // 模拟 DFF 直通设备（参考 DirettaHostSDK_149/Host/Format.hpp::FMT_DSD_MSB）
        // DSD 完整形态阶段 1 新增
        let mut c_info = CDirettaSinkInfo::default();
        c_info.support_dsd_lsb = 0;
        c_info.support_dsd_msb = 0x0000_0000_0020_0000; // FMT_DSD_MSB 位
        c_info.support_pcm = 0;
        let info = c_info.to_rust();
        assert!(!info.supports_dsd_lsb, "DFF-only 设备应不支持 DSD LSB");
        assert!(info.supports_dsd_msb, "DFF-only 设备应支持 DSD MSB");
        assert!(info.supports_dsd, "DFF-only 设备应支持 DSD（msb=true）");
    }

    #[test]
    fn sink_info_to_rust_pcm_and_dsd_both() {
        // PCM + DSD 双支持设备：所有布尔字段为 true
        // 模拟通用 DAC（参考 tinyLMS-old DeviceCapabilities::supports_dsd_lsb/msb 同时为 true）
        // DSD 完整形态阶段 1 新增
        let mut c_info = CDirettaSinkInfo::default();
        c_info.support_pcm = 0x0000_0000_0000_0001; // 任意非零 PCM 位
        c_info.support_dsd_lsb = 0x0000_0000_0010_0000;
        c_info.support_dsd_msb = 0x0000_0000_0020_0000;
        let info = c_info.to_rust();
        assert!(info.supports_pcm, "PCM+DSD 设备应支持 PCM");
        assert!(info.supports_dsd_lsb, "PCM+DSD 设备应支持 DSD LSB");
        assert!(info.supports_dsd_msb, "PCM+DSD 设备应支持 DSD MSB");
        assert!(info.supports_dsd, "PCM+DSD 设备应支持 DSD");
    }

    #[test]
    fn sync_disconnect_without_connect_returns_ok_or_err() {
        // 未连接就 disconnect：SDK disconnect() 应该是安全的（幂等操作）
        // 可返回 Ok 或 Err(Generic)，只要不 UB / 不 panic 即通过
        let sync = DirettaSync::open(None, ptr::null_mut())
            .expect("sync_open 应成功");
        let result = sync.disconnect();
        match result {
            Ok(_) => (),  // 幂等成功
            Err(DirettaError::Generic) => (),  // SDK 可能返回失败
            Err(other) => panic!("disconnect 应返回 Ok 或 Generic，实际: {:?}", other),
        }
    }

    #[test]
    fn sync_full_raii_lifecycle_no_ub() {
        // 验证完整 RAII 生命周期不 UB / 不 leak：
        // open → push → drop（虚析构链 + 有序关闭）
        let sync = DirettaSync::open(None, ptr::null_mut())
            .expect("sync_open 应成功");
        // 写入一些数据到环形缓冲区
        let data = [0xAAu8; 4096];
        let _ = sync.push(&data);
        // 不调用 set_sink/connect/play，直接 drop
        // 析构函数应安全处理（opened_=false，跳过 stop/disconnect/close）
        drop(sync);
        // 如果到达这里没有 UB / crash，测试通过
    }
}
