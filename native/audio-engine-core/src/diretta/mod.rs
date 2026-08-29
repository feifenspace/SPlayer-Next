use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;
use tracing::{info, warn};

pub use diretta_sys::{
    format_id, CDirettaDeviceInfo, CDirettaSetting, CDirettaSinkInfo, DirettaDeviceInfo,
    DirettaError, DirettaEventCb, DirettaFinder as SysFinder, DirettaSetting, DirettaSinkInfo,
    DirettaSync, FormatSupport,
};

/// Diretta Target 设备信息（对外序列化兼容）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DirettaTarget {
    pub ipv6_addr: String,
    pub full_addr: String,
    pub if_idx: i32,
    pub target_name: String,
    pub output_name: String,
    pub model_name: String,
    pub mtu: u32,
}

use parking_lot::Mutex;

/// Diretta 运行时状态快照。
///
/// 由 `AudioOutput` owner 线程在 Diretta 流创建/销毁时更新，
/// 供 headless-server 在任意线程查询当前连接/播放状态。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DirettaRuntimeState {
    /// 当前是否活跃输出为 Diretta（true = diretta backend 正在运行）
    pub is_diretta_active: bool,
    /// 当前是否已连接到 Target 并处于 online 状态
    pub is_online: bool,
    /// 当前是否正在推流播放
    pub is_playing: bool,
    /// 当前连接的 Target 地址
    pub target_addr: String,
    /// 当前协商的采样率
    pub sample_rate: u32,
    /// 当前协商的声道数
    pub channels: u16,
    /// 累计 underrun 次数（Diretta SDK 在环形缓冲区空时递增）
    pub underrun_count: usize,
    /// 是否为 DSD Native 模式
    pub is_dsd: bool,
    /// 最后一次错误描述
    #[serde(skip)]
    pub last_error: Option<String>,
}

/// 进程全局 Diretta 运行时状态。
///
/// 使用 `once_cell` 懒初始化 + `Mutex` 保护，避免在 crate 初始化阶段
/// 访问 `parking_lot` 静态锁可能带来的顺序问题。
static DIRETTA_STATE: once_cell::sync::Lazy<std::sync::Mutex<DirettaRuntimeState>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(DirettaRuntimeState::default()));

/// 设备能力缓存，按 `target_addr%if_idx` 缓存握手结果，避免每次能力查询都新建 Finder+Sync 重新握手。
static CAPABILITY_CACHE: once_cell::sync::Lazy<
    parking_lot::Mutex<std::collections::HashMap<String, DirettaSinkInfo>>,
> = once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// 清空设备能力缓存（设备能力变化/重连后调用）。
pub fn clear_capability_cache() {
    CAPABILITY_CACHE.lock().clear();
}

/// 读取当前 Diretta 运行时状态快照。
///
/// # 说明
///
/// 此函数可在任意线程调用，无需锁定 player。
/// 当 Diretta 后端未激活时返回 `is_diretta_active: false`。
pub fn runtime_state() -> DirettaRuntimeState {
    DIRETTA_STATE
        .lock()
        .expect("Diretta runtime state mutex poisoned")
        .clone()
}

fn update_runtime_state(update: impl FnOnce(&mut DirettaRuntimeState)) {
    let mut guard = DIRETTA_STATE
        .lock()
        .expect("Diretta runtime state mutex poisoned");
    update(&mut guard);
}

/// 标记 Diretta 后端开始连接。
pub fn runtime_state_begin(target_addr: &str, sample_rate: u32, channels: u16, is_dsd: bool) {
    update_runtime_state(|state| {
        *state = DirettaRuntimeState {
            is_diretta_active: true,
            target_addr: target_addr.to_string(),
            sample_rate,
            channels,
            is_dsd,
            ..DirettaRuntimeState::default()
        };
    });
}

/// 更新 Diretta 后端连接/播放状态。
pub fn runtime_state_set_connection(online: bool, playing: bool) {
    update_runtime_state(|state| {
        state.is_online = online;
        state.is_playing = playing;
    });
}

/// 记录一次 Diretta 推流失败。
pub fn runtime_state_record_failure(error: &str) {
    update_runtime_state(|state| {
        state.underrun_count = state.underrun_count.saturating_add(1);
        state.last_error = Some(error.to_string());
    });
}

/// 清除 Diretta 后端运行状态。
pub fn runtime_state_reset() {
    update_runtime_state(|state| *state = DirettaRuntimeState::default());
}

/// Diretta 目标扫描与探测器
pub struct DirettaFinder {
    inner: Mutex<SysFinder>,
}

unsafe impl Send for DirettaFinder {}
unsafe impl Sync for DirettaFinder {}

impl DirettaFinder {
    pub fn new() -> Result<Self> {
        let setting = DirettaSetting::default();
        let inner = SysFinder::open(&setting)
            .map_err(|e| anyhow::anyhow!("Failed to create DirettaFinder: {:?}", e))?;
        Ok(Self {
            inner: Mutex::new(inner),
        })
    }

    /// 扫描局域网内的 Diretta Target
    pub fn scan(&self, _retry_count: i32) -> Vec<DirettaTarget> {
        let mut inner = self.inner.lock();
        let devices = match inner.scan(16) {
            Ok(devs) => devs,
            Err(e) => {
                warn!("Diretta scan failed: {:?}", e);
                return Vec::new();
            }
        };

        let mut result = Vec::new();
        for d in devices {
            let full_addr = if d.port > 0 {
                format!("{}%{},{}", d.ip_str, d.ifno, d.port)
            } else {
                format!("{}%{}", d.ip_str, d.ifno)
            };

            result.push(DirettaTarget {
                ipv6_addr: d.ip_str,
                full_addr,
                if_idx: d.ifno as i32,
                target_name: d.target_name.clone(),
                output_name: d.output_name.clone(),
                model_name: if !d.target_name.is_empty() {
                    d.target_name
                } else {
                    d.output_name
                },
                mtu: 1500,
            });
        }
        result
    }

    /// 测定指定 Target 的网络 MTU
    pub fn measure_mtu(&self, ipv6_addr: &str, port: u16, if_idx: i32) -> u32 {
        let mut inner = self.inner.lock();
        inner
            .measure_mtu(ipv6_addr, port, if_idx as u32)
            .unwrap_or(1500)
    }
}

/// Diretta 传输流控制器
pub struct DirettaStream {
    _finder: Option<DirettaFinder>,
    sync: Arc<DirettaSync>,
    target_addr: String,
    sample_rate: u32,
    channels: u16,
    bit_depth: AtomicU32,
    is_dsd: bool,
    mtu: u32,
    is_connected: AtomicBool,
    is_playing: AtomicBool,
    push_failure_count: AtomicUsize,
    /// 复用 write_samples 的 f32→bytes 转换缓冲，避免每帧堆分配（P1，对齐 tinyLMS-old 复用 tmp_stream_data_）
    scratch: Arc<parking_lot::Mutex<Vec<u8>>>,
    /// 连接建立时刻，供 watchdog "宽容窗口" 判定（P2，对齐 tinyLMS-old last_hard_reset_time_ 10s 宽容期）
    connected_at: std::time::Instant,
}

unsafe impl Send for DirettaStream {}
unsafe impl Sync for DirettaStream {}

impl DirettaStream {
    /// 创建并连接到指定的 Diretta Target
    pub fn connect(
        target_addr: &str,
        if_idx: i32,
        sample_rate: u32,
        channels: u16,
        bit_depth: u8,
        is_dsd: bool,
        mtu: u32,
    ) -> Result<Self> {
        Self::connect_with_dsd_order(
            target_addr,
            if_idx,
            sample_rate,
            channels,
            bit_depth,
            is_dsd,
            mtu,
            0,
        )
    }

    /// 创建并连接到指定的 Diretta Target，并显式指定 DSD 字节序。
    /// `dsd_byte_order` 为 0（LSB/DSF）或 1（MSB/DFF）。
    pub fn connect_with_dsd_order(
        target_addr: &str,
        if_idx: i32,
        sample_rate: u32,
        channels: u16,
        bit_depth: u8,
        is_dsd: bool,
        mtu: u32,
        dsd_byte_order: u32,
    ) -> Result<Self> {
        runtime_state_begin(target_addr, sample_rate, channels, is_dsd);

        let sync = match DirettaSync::open(None, std::ptr::null_mut()) {
            Ok(sync) => sync,
            Err(error) => {
                runtime_state_record_failure(&error.to_string());
                runtime_state_reset();
                return Err(anyhow::anyhow!("DirettaSync::open failed: {:?}", error));
            }
        };

        // 解析 IP, port, ifno
        let mut ip_str = target_addr.to_string();
        let mut port = 19644u16;
        let mut ifno = if_idx.max(0) as u32;

        if let Some((ip, p_str)) = ip_str.split_once(',') {
            if let Ok(p) = p_str.parse::<u16>() {
                port = p;
            }
            ip_str = ip.to_string();
        }

        if let Some((ip, if_str)) = ip_str.split_once('%') {
            if let Ok(i) = if_str.parse::<u32>() {
                ifno = i;
            }
            ip_str = ip.to_string();
        }

        info!(
            target_addr,
            ip = %ip_str,
            port,
            ifno,
            sample_rate,
            channels,
            bit_depth,
            is_dsd,
            mtu,
            "DirettaStream::connect: Setting sink"
        );

        // 关键步骤（DirettaHostSDK & tinyLMS-old 要求）：
        // 1. 设备必须在 connect 前收到 MTU 探测包 (measSendMTU)，否则设备拒绝响应 0x48 CR 导致 connectWait 挂起/失败
        // 2. Finder 实例必须在整个会话生命周期内保持打开（维持组播监听与底层 socket 绑定）
        let (finder_opt, measured_mtu) = match DirettaFinder::new() {
            Ok(mut finder) => {
                let _ = finder.scan(8);
                let m = finder.measure_mtu(&ip_str, port, ifno as i32);
                info!(ip = %ip_str, port, ifno, measured_mtu = m, "DirettaFinder::measure_mtu 测量完成");
                (Some(finder), m)
            }
            Err(e) => {
                warn!(error = ?e, "DirettaFinder::new 失败（用于 measure_mtu），回退到 1500");
                (None, 1500)
            }
        };

        let actual_mtu = if mtu > 0 && mtu != 1500 {
            mtu
        } else {
            measured_mtu
        };

        if is_dsd {
            let mult = match Self::dsd_rate_multiplier(sample_rate) {
                Some(m) => m,
                None => {
                    return Err(anyhow::anyhow!(
                        "DSD sample rate {} is not a valid DSD rate (DSD64/128/256/512)",
                        sample_rate
                    ))
                }
            };
            info!(
                sample_rate,
                mult,
                dsd_byte_order = dsd_byte_order,
                "DirettaStream::connect: DSD Native, mult={} byte_order={}",
                mult,
                dsd_byte_order
            );
            sync.set_sink_dsd(
                &ip_str,
                port,
                ifno,
                actual_mtu,
                100, // 官方 SinHost_push.cpp L118: buffer 请求 100ms
                mult,
                dsd_byte_order,
                channels as u32,
            )
            .map_err(|e| anyhow::anyhow!("set_sink_dsd failed: {:?}", e))?;
        } else {
            // 官方 SinHost_push.cpp L118: setSink(addr, Clock::MilliSeconds(100), false, mtu)
            // buffer_ms=100 严格对齐官方 push 模式示例。
            // tinyLMS-old 总是请求 32-bit PCM 物理槽位（L793-832），这是 DSD 兼容的通用选择。
            sync.set_sink(
                &ip_str,
                port,
                ifno,
                actual_mtu,
                100,
                sample_rate,
                channels as u32,
                32, // tinyLMS-old: 始终优先请求 32-bit 物理槃位，规避 3-byte 对齐问题
            )
            .map_err(|e| anyhow::anyhow!("set_sink failed: {:?}", e))?;
        }

        // SDK SinHost.cpp L186: synk.connect(0)
        info!("DirettaStream::connect: Connecting to sink");
        sync.connect(5000)
            .map_err(|e| anyhow::anyhow!("connect_sink failed: {:?}", e))?;

        // 查询实际协商的物理字节宽度（Diretta 可能强制 32-bit）
        // tinyLMS-old DirettaDriver.cpp L842-849: getSinkConfigure().getWid()
        let actual_wid = sync.negotiated_sample_bytes().unwrap_or(4); // fallback to 32-bit if query fails
        let actual_bit_depth = actual_wid * 8;
        info!(
            "DirettaStream::connect: Negotiated physical bit depth = {}-bit",
            actual_bit_depth
        );

        info!("DirettaStream::connect: Calling play");
        sync.play()
            .map_err(|e| anyhow::anyhow!("play_sink failed: {:?}", e))?;

        runtime_state_set_connection(sync.is_online(), sync.is_playing());

        Ok(Self {
            _finder: finder_opt,
            sync: Arc::new(sync),
            target_addr: target_addr.to_string(),
            sample_rate,
            channels,
            bit_depth: AtomicU32::new(actual_bit_depth),
            is_dsd,
            mtu: actual_mtu,
            is_connected: AtomicBool::new(true),
            is_playing: AtomicBool::new(true),
            push_failure_count: AtomicUsize::new(0),
            scratch: Arc::new(parking_lot::Mutex::new(Vec::new())),
            connected_at: std::time::Instant::now(),
        })
    }

    /// 获取当前连接的 Diretta Target DAC 硬件音频支持能力
    pub fn get_sink_capabilities(&self) -> Option<DirettaSinkInfo> {
        self.sync.get_sink_info().ok()
    }

    /// 将 DSD 采样率映射到标准倍率（64/128/256/512）。
    ///
    /// DSD 采样率 = 44100 × 64 = DSD64（2822400 Hz）
    ///                 44100 × 128 = DSD128（5644800 Hz）
    ///                 44100 × 256 = DSD256（11289600 Hz）
    ///                 44100 × 512 = DSD512（22579200 Hz）
    ///
    /// 也接受 48000×N 形式（DSD 专业速率），因为设备能力查询中常见。
    fn dsd_rate_multiplier(sample_rate: u32) -> Option<u32> {
        const VALID_MULTS: &[(u32, u32)] = &[
            (64, 2822400),   // 44100 * 64
            (128, 5644800),  // 44100 * 128
            (256, 11289600), // 44100 * 256
            (512, 22579200), // 44100 * 512
        ];
        for &(mult, rate) in VALID_MULTS {
            if sample_rate == rate {
                return Some(mult);
            }
        }
        // 48000基数变体
        for &(_mult, _rate) in VALID_MULTS {
            if sample_rate == 48000 * _mult {
                return Some(_mult);
            }
        }
        None
    }

    /// 写入 f32 交错 PCM 数据，转换为协商后的物理位深。
    /// 缓冲区满时返回 `Err(DirettaError::Underrun)`。
    pub fn write_samples(&self, samples: &[f32]) -> Result<(), DirettaError> {
        let bit_depth = self.bit_depth.load(Ordering::Relaxed);
        let bytes_per_sample = (bit_depth as usize / 8).max(1);
        let cap = samples.len() * bytes_per_sample;

        let mut byte_buf = self.scratch.lock();
        byte_buf.clear();
        let cur_cap = byte_buf.capacity();
        if cur_cap < cap {
            byte_buf.reserve(cap - cur_cap);
        }

        match bit_depth {
            16 => {
                for &s in samples {
                    let val = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                    byte_buf.extend_from_slice(&val.to_le_bytes());
                }
            }
            24 => {
                for &s in samples {
                    let val = (s.clamp(-1.0, 1.0) * 8388607.0) as i32;
                    byte_buf.extend_from_slice(&val.to_le_bytes()[..3]);
                }
            }
            32 => {
                for &s in samples {
                    let scaled = (s * 0.999).clamp(-1.0, 1.0);
                    let val = (scaled * 2147483000.0) as i32;
                    byte_buf.extend_from_slice(&val.to_le_bytes());
                }
            }
            _ => return Err(DirettaError::Format),
        }

        // push 同步拷贝进 SDK 环形缓冲；byte_buf 在锁释放后即可安全复用
        let result = self.sync.push(&byte_buf[..]);
        drop(byte_buf); // 提前释放写侧锁，避免不必要的长持锁
        if let Err(error) = &result {
            self.push_failure_count.fetch_add(1, Ordering::AcqRel);
            runtime_state_record_failure(&error.to_string());
        }
        result
    }

    /// 向 Diretta 流写入 DSD 字节（直通模式，并在需要时应用硬件协商的位反转与字节交换）。
    ///
    /// 适用于 SACD DST 解码后的 DSD 流。
    pub fn write_dsd_bytes(&self, dsd_bytes: &[u8]) -> Result<(), DirettaError> {
        if !self.is_dsd {
            warn!("write_dsd_bytes called on non-DSD stream");
            return Err(DirettaError::Format);
        }

        const BIT_REVERSE_LUT: [u8; 256] = {
            let mut lut = [0u8; 256];
            let mut i = 0;
            while i < 256 {
                let mut b = i as u8;
                b = ((b & 0xF0) >> 4) | ((b & 0x0F) << 4);
                b = ((b & 0xCC) >> 2) | ((b & 0x33) << 2);
                b = ((b & 0xAA) >> 1) | ((b & 0x55) << 1);
                lut[i] = b;
                i += 1;
            }
            lut
        };

        // 如果硬件协商要求位反转或字节交换，应用转换
        let result = if let Ok((bit_reverse, byte_swap)) = self.sync.dsd_transform() {
            if bit_reverse || byte_swap {
                let mut scratch = self.scratch.lock();
                scratch.clear();
                scratch.reserve(dsd_bytes.len());

                if byte_swap && (dsd_bytes.len() % 2 == 0) {
                    for chunk in dsd_bytes.chunks_exact(2) {
                        let b0 = if bit_reverse { BIT_REVERSE_LUT[chunk[0] as usize] } else { chunk[0] };
                        let b1 = if bit_reverse { BIT_REVERSE_LUT[chunk[1] as usize] } else { chunk[1] };
                        scratch.push(b1);
                        scratch.push(b0);
                    }
                } else if bit_reverse {
                    for &b in dsd_bytes {
                        scratch.push(BIT_REVERSE_LUT[b as usize]);
                    }
                } else {
                    scratch.extend_from_slice(dsd_bytes);
                }
                self.sync.push(&scratch[..])
            } else {
                self.sync.push(dsd_bytes)
            }
        } else {
            self.sync.push(dsd_bytes)
        };

        if let Err(error) = &result {
            self.push_failure_count.fetch_add(1, Ordering::AcqRel);
            runtime_state_record_failure(&error.to_string());
        }
        result
    }

    /// 开始播放
    pub fn play(&mut self) -> Result<()> {
        if !self.is_playing.load(Ordering::Acquire) {
            self.sync
                .play()
                .map_err(|e| anyhow::anyhow!("Diretta play failed: {:?}", e))?;
            self.is_playing.store(true, Ordering::Release);
        }
        runtime_state_set_connection(self.is_online(), self.is_playing());
        Ok(())
    }

    /// 停止播放（执行优雅 Pre-Mute 预静音，防止 DAC 爆音）
    pub fn stop(&mut self) -> Result<()> {
        if self.is_playing.load(Ordering::Acquire) {
            let _ = self.sync.trigger_pre_mute(8);
            self.sync.wait_pre_mute_done(40);
            let _ = self.sync.stop();
            self.is_playing.store(false, Ordering::Release);
        }
        Ok(())
    }

    /// 检查连接状态
    pub fn is_connected(&self) -> bool {
        self.is_connected.load(Ordering::Acquire) && self.sync.is_online()
    }

    /// 判断是否处于连接"宽容期"内（P2，对齐 tinyLMS-old last_hard_reset_time_ 10s 宽容期）。
    /// 刚连接建立后的短暂抖动（SDK 状态同步滞后）不应触发 watchdog 误杀。
    pub fn just_connected(&self, within: std::time::Duration) -> bool {
        self.connected_at.elapsed() < within
    }

    /// 查询 DSD 协商命中的位/字节变换标志（P3，暴露给上层做直通前变换）。
    pub fn dsd_bit_transform(&self) -> Option<(bool, bool)> {
        if !self.is_dsd {
            return None;
        }
        self.sync.dsd_transform().ok()
    }

    /// 查询 SDK 层在线状态（任意线程可调）
    pub fn is_online(&self) -> bool {
        self.sync.is_online()
    }

    /// 查询 SDK 层播放状态（任意线程可调）
    pub fn is_playing(&self) -> bool {
        self.sync.is_playing()
    }

    /// 累计推流失败次数（含缓冲区满/underrun）
    pub fn push_failures(&self) -> usize {
        self.push_failure_count.load(Ordering::Acquire)
    }

    /// 采样率
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// 声道数
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// 目标地址
    pub fn target_addr(&self) -> &str {
        &self.target_addr
    }
}

/// 查询 Diretta Target 能力，不启动播放流。
///
/// 该路径只建立一次 SDK 握手读取 SinkInfo，然后立即断开，避免创建
/// `DirettaStream` 时触发播放状态、环形缓冲区和 Tokio 运行时相关副作用。
pub fn query_sink_info(target_addr: &str, if_idx: i32, mtu: u32) -> Result<DirettaSinkInfo> {
    let mut ip = target_addr.to_string();
    let mut port = 19644u16;
    let mut ifno = if_idx.max(0) as u32;

    if let Some((value, port_str)) = ip.split_once(',') {
        port = port_str.parse().unwrap_or(19644);
        ip = value.to_string();
    }
    if let Some((value, ifno_str)) = ip.split_once('%') {
        ifno = ifno_str.parse().unwrap_or(ifno);
        ip = value.to_string();
    }

    // 能力缓存：命中直接返回，避免重建 Finder+Sync 的重复握手
    let cache_key = format!("{}%{}.{}", ip, ifno, port);
    if let Some(info) = CAPABILITY_CACHE.lock().get(&cache_key).cloned() {
        return Ok(info);
    }

    let finder = DirettaFinder::new()?;
    let actual_mtu = if mtu > 0 {
        mtu
    } else {
        finder.measure_mtu(&ip, port, ifno as i32)
    };
    let sync = DirettaSync::open(None, std::ptr::null_mut())
        .map_err(|e| anyhow::anyhow!("DirettaSync::open failed: {e:?}"))?;

    sync.set_sink(&ip, port, ifno, actual_mtu, 100, 48000, 2, 32)
        .map_err(|e| anyhow::anyhow!("Diretta set_sink failed: {e:?}"))?;
    
    // 播放路径严格对齐官方示例（SinHost_push.cpp）不再内联 inquiry；
    // 能力查询路径在此显式查询 Target 支持格式以填充 SinkInfo
    let _ = sync.inquiry_support_format(&ip, port, ifno);
    let info = sync
        .get_sink_info()
        .map_err(|e| anyhow::anyhow!("Diretta sink info unavailable: {e:?}"));
    let _ = sync.disconnect();

    if let Ok(cached) = &info {
        CAPABILITY_CACHE.lock().insert(cache_key, cached.clone());
    }
    info
}

#[cfg(test)]
mod tests {
    use super::DirettaStream;

    #[test]
    fn dsd_rate_multiplier_accepts_standard_rates() {
        assert_eq!(DirettaStream::dsd_rate_multiplier(2_822_400), Some(64));
        assert_eq!(DirettaStream::dsd_rate_multiplier(5_644_800), Some(128));
        assert_eq!(DirettaStream::dsd_rate_multiplier(11_289_600), Some(256));
        assert_eq!(DirettaStream::dsd_rate_multiplier(22_579_200), Some(512));
    }

    #[test]
    fn dsd_rate_multiplier_rejects_non_dsd_rates() {
        assert_eq!(DirettaStream::dsd_rate_multiplier(44_100), None);
        assert_eq!(DirettaStream::dsd_rate_multiplier(3_000_000), None);
    }
}

impl Drop for DirettaStream {
    fn drop(&mut self) {
        let _ = self.stop();
        let sync = self.sync.clone();
        self.is_connected.store(false, Ordering::Release);
        runtime_state_reset();
        let _ = std::thread::Builder::new()
            .name("diretta-disconnect".to_string())
            .spawn(move || {
                let _ = sync.disconnect();
            });
    }
}
