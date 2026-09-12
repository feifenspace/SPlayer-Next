#[cfg(any(feature = "diretta", test))]
use std::path::Path;
#[cfg(feature = "diretta")]
use std::time::Duration;

#[cfg(any(feature = "diretta", test))]
use anyhow::anyhow;
use anyhow::Result;

#[cfg(any(feature = "diretta", test))]
use crate::direct_dsd::DirectDsdFormat;
#[cfg(any(feature = "diretta", test))]
use crate::direct_pcm::DirectPcmFormat;

pub const DEVICE_PREFIX: &str = "diretta:";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirettaDevice {
    pub id: String,
    pub name: String,
    pub ipv6_addr: String,
    pub full_addr: String,
    pub if_idx: i32,
    pub target_name: String,
    pub output_name: String,
    pub model_name: String,
    pub mtu: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct DirettaTargetCapabilities {
    /// 目标设备名称
    pub target_name: String,
    /// 输出端口名称
    pub output_name: String,
    /// 固件版本字符串
    pub firmware_version: String,
    /// 设备 IPv6 地址（不含端口）
    pub ipv6_addr: String,
    /// 设备 full address（IPv6%IFNO,PORT）
    pub full_addr: String,
    /// 网络接口号
    pub if_idx: i32,

    /// 是否支持 PCM
    pub supports_pcm: bool,
    /// 最小 PCM 位深
    pub pcm_min_bits: u32,
    /// 最大 PCM 位深
    pub pcm_max_bits: u32,
    /// 最小 PCM 采样率（Hz）
    pub pcm_min_sample_rate: u32,
    /// 最大 PCM 采样率（Hz）
    pub pcm_max_sample_rate: u32,
    /// 最小 PCM 声道数
    pub pcm_min_channels: u32,
    /// 最大 PCM 声道数
    pub pcm_max_channels: u32,

    /// 是否支持 DSD
    pub supports_dsd: bool,
    /// 是否支持 DSD LSB（DSF）
    pub supports_dsd_lsb: bool,
    /// 是否支持 DSD MSB（DFF）
    pub supports_dsd_msb: bool,
    /// 最小 DSD 采样率（Hz）
    pub dsd_min_sample_rate: u32,
    /// 最大 DSD 采样率（Hz）
    pub dsd_max_sample_rate: u32,
    /// 最小 DSD 位深
    pub dsd_min_bits: u32,
    /// 最大 DSD 位深
    pub dsd_max_bits: u32,
    /// 最小 DSD 声道数
    pub dsd_min_channels: u32,
    /// 最大 DSD 声道数
    pub dsd_max_channels: u32,

    /// 实测路径 MTU
    pub mtu_measured: u32,
    /// 设备最小 MTU
    pub mtu_min: u32,
    /// 设备请求 MTU
    pub mtu_req: u32,
    /// 设备最大 MTU
    pub mtu_max: u32,
    /// 单次传输最大数据大小（字节）
    pub max_packet_size: u32,

    /// MS 模式支持位图
    pub support_ms_mode: u16,
    /// Sink 缓冲延迟（100µs 单位，B7.2）：进度按 DAC 实际出声时刻校准的数据来源
    pub latency_buffer_x100us: u16,
    /// Sink 最大延迟（100µs 单位）
    pub latency_max_x100us: u16,
    /// Sink 硬件延迟（100µs 单位）
    pub latency_hw_x100us: u16,
}

pub fn selector_target(selector: &str) -> Option<&str> {
    selector
        .strip_prefix(DEVICE_PREFIX)
        .filter(|value| !value.is_empty() && *value != "undefined" && *value != "null")
}

pub fn selector_for(target_id: &str) -> String {
    format!("{DEVICE_PREFIX}{target_id}")
}

/// 排空目标时长（µs，Phase2）：Sink 实测延迟 / 自报缓冲取大者，+25% 余量，
/// 下限 DIRECT_FADE_DRAIN_FLOOR_MICROS（默认 120ms，v12-B 收紧；
/// SPLAYER_DIRECT_DRAIN_FLOOR_MS 可覆盖），上限 1s（防异常值）。
/// 两个读数均不可用（0）时返回 0 → ring 退回旧常量，等价改动前行为
#[cfg(feature = "diretta")]
pub fn compute_drain_target_micros(latency_us: u64, buffer_us: u64) -> u64 {
    use crate::direct_runtime::DIRECT_FADE_DRAIN_FLOOR_MICROS;
    let base = latency_us.max(buffer_us);
    if base == 0 {
        return 0;
    }
    // v12-B：垫只需覆盖 Target 端缓冲深度（base=latency），1.25× 余量足够
    // 吸收拉取抖动；下限从 200ms 收紧到 120ms（可经 SPLAYER_DIRECT_DRAIN_FLOOR_MS
    // 覆盖，设 200 恢复旧行为），未注入/读数为 0 时调用方仍退回旧常量
    let floor = std::env::var("SPLAYER_DIRECT_DRAIN_FLOOR_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|ms| ms.saturating_mul(1_000))
        .filter(|micros| *micros > 0)
        .unwrap_or(DIRECT_FADE_DRAIN_FLOOR_MICROS);
    (base + base / 4).clamp(floor, 1_000_000)
}

#[cfg(feature = "diretta")]
mod imp {
    use std::ffi::{c_char, c_void, CStr, CString};
    use std::ptr::NonNull;

    use super::*;
    use crate::direct_dsd::{
        direct_dsd_next_block, direct_dsd_release_block, DirectDsdBitOrder, DirectDsdMonitor,
        DirectDsdSource, DirectDsdStageHandle,
    };
    use crate::direct_pcm::{
        direct_pcm_next_block, direct_pcm_release_block, DirectPcmMonitor, DirectPcmSource,
        DirectPcmStageHandle,
    };
    use diretta_sys::{
        splayer_diretta_arm_preroll, splayer_diretta_close, splayer_diretta_cycle_size,
        splayer_diretta_last_error, splayer_diretta_mute_byte, splayer_diretta_open_direct,
        splayer_diretta_open_dsd_direct, splayer_diretta_pause, splayer_diretta_pcm_reconfigure,
        splayer_diretta_play, splayer_diretta_query_target_caps, splayer_diretta_scan,
        splayer_diretta_sink_buffer_us, splayer_diretta_sink_latency_us, SPlayerDirettaDevice,
        SPlayerDirettaTargetCaps, TARGET_FW_MAX, TARGET_TEXT_MAX, TEXT_CAPACITY,
    };

    const MAX_SCAN_DEVICES: usize = 32;

    fn last_error(fallback: &str) -> anyhow::Error {
        let message = unsafe {
            let ptr = splayer_diretta_last_error();
            if ptr.is_null() {
                None
            } else {
                CStr::from_ptr(ptr)
                    .to_str()
                    .ok()
                    .filter(|value| !value.is_empty())
            }
        };
        anyhow!(message.unwrap_or(fallback).to_owned())
    }

    fn raw_text(value: &[c_char; TEXT_CAPACITY]) -> String {
        unsafe { CStr::from_ptr(value.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    fn raw_text_fixed<const N: usize>(value: &[c_char; N]) -> String {
        unsafe { CStr::from_ptr(value.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    pub fn available() -> bool {
        true
    }

    /// 查询指定 Diretta Target 的硬件解码能力（同步阻塞，约 2-3 秒）
    ///
    /// `target_id` 接受 IPv6,PORT 格式的 full address，或不含端口的 IPv6 字符串。
    ///
    /// # 契约
    ///
    /// 调用方不得持有 player 锁——内部通过网络探测同步阻塞 2-3s；
    /// 当前调用方（select 可达性验证 / target_info handler）均在 `spawn_isolated_blocking` 中
    pub fn query_target_caps(target_id: &str) -> Result<DirettaTargetCapabilities> {
        let target = selector_target(target_id).unwrap_or(target_id);
        let c_target = CString::new(target).map_err(|_| anyhow!("invalid Diretta target id"))?;
        let mut raw = SPlayerDirettaTargetCaps::default();
        let ok =
            unsafe { splayer_diretta_query_target_caps(c_target.as_ptr(), &mut raw as *mut _) };
        if !ok {
            return Err(last_error("failed to query Diretta target capabilities"));
        }
        Ok(DirettaTargetCapabilities {
            target_name: raw_text_fixed::<{ TARGET_TEXT_MAX }>(&raw.target_name),
            output_name: raw_text_fixed::<{ TARGET_TEXT_MAX }>(&raw.output_name),
            firmware_version: raw_text_fixed::<{ TARGET_FW_MAX }>(&raw.firmware_version),
            ipv6_addr: raw_text_fixed::<{ TARGET_TEXT_MAX }>(&raw.ipv6_addr),
            full_addr: raw_text_fixed::<{ TARGET_TEXT_MAX }>(&raw.full_addr),
            if_idx: raw.if_idx,
            supports_pcm: raw.supports_pcm != 0,
            pcm_min_bits: raw.pcm_min_bits,
            pcm_max_bits: raw.pcm_max_bits,
            pcm_min_sample_rate: raw.pcm_min_sample_rate,
            pcm_max_sample_rate: raw.pcm_max_sample_rate,
            pcm_min_channels: raw.pcm_min_channels,
            pcm_max_channels: raw.pcm_max_channels,
            supports_dsd: raw.supports_dsd != 0,
            supports_dsd_lsb: raw.supports_dsd_lsb != 0,
            supports_dsd_msb: raw.supports_dsd_msb != 0,
            dsd_min_sample_rate: raw.dsd_min_sample_rate,
            dsd_max_sample_rate: raw.dsd_max_sample_rate,
            dsd_min_bits: raw.dsd_min_bits,
            dsd_max_bits: raw.dsd_max_bits,
            dsd_min_channels: raw.dsd_min_channels,
            dsd_max_channels: raw.dsd_max_channels,
            mtu_measured: raw.mtu_measured,
            mtu_min: u32::from(raw.mtu_min),
            mtu_req: u32::from(raw.mtu_req),
            mtu_max: raw.mtu_max,
            max_packet_size: u32::from(raw.max_size),
            support_ms_mode: raw.support_ms_mode,
            latency_buffer_x100us: raw.latency_buffer_x100us,
            latency_max_x100us: raw.latency_max_x100us,
            latency_hw_x100us: raw.latency_hw_x100us,
        })
    }

    pub fn scan_devices() -> Result<Vec<DirettaDevice>> {
        let mut raw = vec![SPlayerDirettaDevice::default(); MAX_SCAN_DEVICES];
        let discovered = unsafe { splayer_diretta_scan(raw.as_mut_ptr(), raw.len()) };
        if discovered == 0 {
            let error = last_error("");
            if error.to_string().is_empty() {
                return Ok(Vec::new());
            }
            return Err(error);
        }
        let count = discovered.min(raw.len());
        Ok(raw
            .into_iter()
            .take(count)
            .filter_map(|device| {
                let id = raw_text(&device.id);
                if id.is_empty() {
                    return None;
                }
                let name = raw_text(&device.name);
                let ipv6_addr = raw_text(&device.ipv6_addr);
                let full_addr = raw_text(&device.full_addr);
                let target_name = raw_text(&device.target_name);
                let output_name = raw_text(&device.output_name);
                let model_name = raw_text(&device.model_name);
                Some(DirettaDevice {
                    id: selector_for(&id),
                    name: if name.is_empty() {
                        "Diretta Target".to_string()
                    } else {
                        name
                    },
                    ipv6_addr: if ipv6_addr.is_empty() {
                        id.clone()
                    } else {
                        ipv6_addr
                    },
                    full_addr: if full_addr.is_empty() { id } else { full_addr },
                    if_idx: device.if_idx,
                    target_name,
                    output_name,
                    model_name,
                    mtu: if device.mtu > 0 { device.mtu } else { 1500 },
                })
            })
            .collect())
    }

    pub struct DirettaDirectConnection {
        raw: NonNull<c_void>,
        source: DirectPcmSource,
    }

    // Diretta SDK handle 只由拥有者线程控制；SDK 内部发送线程仅通过回调访问 source ring。
    unsafe impl Send for DirettaDirectConnection {}

    impl DirettaDirectConnection {
        pub fn open_local(selector: &str, path: &Path) -> Result<Self> {
            let (connection, _) = Self::open_local_at(selector, path, 0.0, 0.0)?;
            Ok(connection)
        }

        pub fn open_local_at(
            selector: &str,
            path: &Path,
            position_secs: f64,
            stop_file_secs: f64,
        ) -> Result<(Self, f64)> {
            let target = selector_target(selector)
                .ok_or_else(|| anyhow!("invalid Diretta output selector"))?;
            let target = CString::new(target).map_err(|_| anyhow!("invalid Diretta target id"))?;
            let (source, actual_position) =
                DirectPcmSource::open_local_at(path, position_secs, stop_file_secs)?;
            let format = source.format();
            let raw = unsafe {
                splayer_diretta_open_direct(
                    target.as_ptr(),
                    format.sample_rate,
                    format.channels,
                    format.storage_bits,
                    source.callback_context(),
                    direct_pcm_next_block,
                    direct_pcm_release_block,
                )
            };
            let raw = NonNull::new(raw)
                .ok_or_else(|| last_error("failed to open Diretta Source Direct target"))?;
            let connection = Self { raw, source };
            connection.configure_drain_after_open(&format);
            Ok((connection, actual_position))
        }

        /// 以流式 Reader 打开（在线音源 stream 模式，wire-format 协商与本地路径一致）
        pub fn open_reader_at(
            selector: &str,
            reader: Box<dyn crate::direct_pcm::ReadSeek>,
            position_secs: f64,
            stop_file_secs: f64,
        ) -> Result<(Self, f64)> {
            let target = selector_target(selector)
                .ok_or_else(|| anyhow!("invalid Diretta output selector"))?;
            let target = CString::new(target).map_err(|_| anyhow!("invalid Diretta target id"))?;
            let (source, actual_position) =
                DirectPcmSource::open_reader_at(reader, position_secs, stop_file_secs)?;
            let format = source.format();
            let raw = unsafe {
                splayer_diretta_open_direct(
                    target.as_ptr(),
                    format.sample_rate,
                    format.channels,
                    format.storage_bits,
                    source.callback_context(),
                    direct_pcm_next_block,
                    direct_pcm_release_block,
                )
            };
            let raw = NonNull::new(raw)
                .ok_or_else(|| last_error("failed to open Diretta Source Direct target"))?;
            let connection = Self { raw, source };
            connection.configure_drain_after_open(&format);
            Ok((connection, actual_position))
        }

        pub fn format(&self) -> DirectPcmFormat {
            self.source.format()
        }

        /// 建连后采集 Sink 实测参数并注入排空目标（Phase0 诊断 + Phase2 动态排空）。
        /// PCM 静音固定 0x00（signed 零点），不采信 SDK mute_byte。
        /// 任何 FFI 读数失败均返回 0 → 排空目标退回旧常量（默认行为可回退）
        fn configure_drain_after_open(&self, wire_format: &DirectPcmFormat) {
            let latency_us = unsafe { splayer_diretta_sink_latency_us(self.raw.as_ptr()) };
            let buffer_us = unsafe { splayer_diretta_sink_buffer_us(self.raw.as_ptr()) };
            let cycle_size = unsafe { splayer_diretta_cycle_size(self.raw.as_ptr()) };
            let mute_byte = unsafe { splayer_diretta_mute_byte(self.raw.as_ptr()) };
            let drain_target_micros = compute_drain_target_micros(latency_us, buffer_us);
            self.source.set_drain_target_micros(drain_target_micros);
            let preroll_ms = ((drain_target_micros.max(150_000) + 999) / 1_000) as u32;
            let bytes_per_second = wire_format.sample_rate as f64
                * wire_format.channels as f64
                * (wire_format.storage_bits as f64 / 8.0);
            if !unsafe {
                splayer_diretta_arm_preroll(self.raw.as_ptr(), preroll_ms, bytes_per_second)
            } {
                tracing::warn!(target: "diretta_handoff", phase = "startup_preroll_failed",
                    "unable to arm Diretta PCM startup silence");
            }
            tracing::info!(
                target: "diretta_handoff",
                phase = "sink_params",
                latency_us = %latency_us,
                buffer_us = %buffer_us,
                cycle_size = %cycle_size,
                mute_byte = %mute_byte,
                drain_target_micros = %drain_target_micros,
                wire_format = ?wire_format,
                "Diretta sink 实测参数（Phase0 诊断）"
            );
        }

        pub fn play(&mut self) -> Result<()> {
            if unsafe { splayer_diretta_play(self.raw.as_ptr()) } {
                Ok(())
            } else {
                Err(last_error("failed to start Diretta Source Direct playback"))
            }
        }

        pub fn pause(&mut self) -> Result<()> {
            if unsafe { splayer_diretta_pause(self.raw.as_ptr()) } {
                Ok(())
            } else {
                Err(last_error("failed to pause Diretta Source Direct playback"))
            }
        }

        pub fn seek_while_paused(&mut self, position_secs: f64) -> Result<f64> {
            self.source.seek_while_paused(position_secs)
        }

        pub fn replace_drained_local_source(
            &mut self,
            source: &str,
            start_secs: f64,
            stop_secs: f64,
            cancel: crate::ffmpeg_audio::HttpCancelHandle,
        ) -> Result<DirectPcmFormat> {
            self.source
                .replace_drained_local(source, start_secs, stop_secs, cancel)
        }

        /// v12-A：手动 handoff 并行开源武装——详见 DirectPcmSource::arm_handoff_stage
        pub fn arm_handoff_stage(
            &self,
            path: &Path,
            start_secs: f64,
            stop_secs: f64,
            duration_secs: f64,
        ) -> Option<u64> {
            self.source
                .arm_handoff_stage(path, start_secs, stop_secs, duration_secs)
        }

        /// v12-A：装填 staged 预打开候选完成换源——详见
        /// DirectPcmSource::replace_with_staged
        pub fn replace_with_staged_source(
            &mut self,
            source: &str,
            start_secs: f64,
            stop_secs: f64,
            cancel: crate::ffmpeg_audio::HttpCancelHandle,
            expected_generation: u64,
        ) -> Result<DirectPcmFormat> {
            self.source.replace_with_staged(
                source,
                start_secs,
                stop_secs,
                cancel,
                expected_generation,
            )
        }

        /// v11-3: 武装下一次换源的跨格式旁路（消费一次自动复位），
        /// 详见 DirectPcmSource::arm_cross_format_replace
        pub fn arm_cross_format_replace(&self) {
            self.source.arm_cross_format_replace();
        }

        /// v11-3: 热重配（不拆连接）：stop → setSinkConfigure → configTransferAuto
        /// → preroll → play。仅同 wire 位深（32 容器统一后恒为 32）且 Target
        /// 在线时可行；失败由调用方回退全量重连。
        /// 实验开关：SPLAYER_DIRECT_HOT_RECONFIG=1 时编排层才走此路径
        pub fn hot_reconfigure(&self, format: &DirectPcmFormat) -> Result<()> {
            let bytes_per_second = f64::from(format.sample_rate)
                * f64::from(format.channels)
                * f64::from(format.storage_bits / 8);
            if unsafe {
                splayer_diretta_pcm_reconfigure(
                    self.raw.as_ptr(),
                    format.sample_rate,
                    format.channels,
                    format.storage_bits,
                    bytes_per_second,
                )
            } {
                Ok(())
            } else {
                Err(last_error("Diretta hot reconfigure failed"))
            }
        }

        pub fn failed(&self) -> bool {
            self.source.failed()
        }

        pub fn finished(&self) -> bool {
            self.source.finished()
        }

        pub fn consumed_position(&self) -> f64 {
            self.source.consumed_position()
        }

        pub fn monitor(&self) -> DirectPcmMonitor {
            self.source.monitor()
        }

        pub fn stage_handle(&self) -> DirectPcmStageHandle {
            self.source.stage_handle()
        }

        pub fn set_duration(&self, duration_secs: f64) {
            self.source.set_duration(duration_secs);
        }

        /// 关流/换源前源级淡出（详见 DirectPcmSource::begin_fade_out）
        pub fn begin_fade_out(&self) {
            self.source.begin_fade_out();
        }

        /// 淡出是否已生效（后续块均为数字静音）
        pub fn is_faded_out(&self) -> bool {
            self.source.is_faded_out()
        }

        /// 事件驱动排空等待：淡出完成且已交付 min_blocks 块静音，或超时
        pub fn wait_fade_drained(&self, min_blocks: u32, timeout: Duration) -> bool {
            self.source.wait_fade_drained(min_blocks, timeout)
        }

        /// 软暂停（Phase3）：淡出到零电平并交付至少一个静音块（或超时）。
        /// 供 InnerPlayer::pause 在 sync->stop 前调用，消除暂停末块阶跃
        pub fn begin_soft_pause_and_wait(&self, timeout: Duration) -> bool {
            self.source.begin_fade_out();
            self.source.wait_soft_pause(timeout)
        }

        /// 恢复播放淡入：清除软暂停静音态并从零增益渐入。
        /// 不清除则恢复后永久静音（正确性关键）
        pub fn resume_soft(&self) {
            self.source.begin_fade_in();
        }

        /// tinyLMS Quick Resume：清除静音/淡出态但不做淡入渐变（数据级
        /// 硬拼接）。不清除则残留 silent 态会永久静音（正确性关键）
        pub fn clear_fade_state(&self) {
            self.source.clear_fade_state();
        }

        /// tinyLMS Hard Reset 预静音：武装 n 周期回调层静音倒计时
        /// （详见 DirectPcmSource::trigger_forced_mute_cycles）
        pub fn trigger_pre_mute_cycles(&self, cycles: u32) {
            self.source.trigger_forced_mute_cycles(cycles);
        }

        /// 预静音倒计时是否尚未消耗完
        pub fn forced_mute_pending(&self) -> bool {
            self.source.forced_mute_pending()
        }
    }

    impl Drop for DirettaDirectConnection {
        fn drop(&mut self) {
            // 必须先关闭 SDK，确保发送线程不再持有 source callback context，再释放 source ring。
            unsafe { splayer_diretta_close(self.raw.as_ptr()) };
        }
    }

    pub struct DirettaDirectDsdConnection {
        raw: NonNull<c_void>,
        source: DirectDsdSource,
    }

    unsafe impl Send for DirettaDirectDsdConnection {}

    impl DirettaDirectDsdConnection {
        pub fn open_local(selector: &str, path: &Path) -> Result<Self> {
            let (connection, _) = Self::open_local_at(selector, path, 0.0, 0.0)?;
            Ok(connection)
        }

        pub fn open_local_at(
            selector: &str,
            path: &Path,
            position_secs: f64,
            _stop_file_secs: f64,
        ) -> Result<(Self, f64)> {
            let target = selector_target(selector)
                .ok_or_else(|| anyhow!("invalid Diretta output selector"))?;
            let target = CString::new(target).map_err(|_| anyhow!("invalid Diretta target id"))?;
            let (mut source, actual_position) =
                DirectDsdSource::open_local_at(path, position_secs)?;
            let format = source.format();
            let source_lsb_first = format.bit_order == DirectDsdBitOrder::LsbFirst;
            let mut wire_lsb_first = source_lsb_first;
            let raw = unsafe {
                splayer_diretta_open_dsd_direct(
                    target.as_ptr(),
                    format.bit_rate,
                    format.channels,
                    source_lsb_first,
                    &mut wire_lsb_first,
                    source.callback_context(),
                    direct_dsd_next_block,
                    direct_dsd_release_block,
                )
            };
            let raw = NonNull::new(raw)
                .ok_or_else(|| last_error("failed to open Diretta Native DSD target"))?;
            let wire_bit_order = if wire_lsb_first {
                DirectDsdBitOrder::LsbFirst
            } else {
                DirectDsdBitOrder::MsbFirst
            };
            if wire_bit_order != format.bit_order {
                if let Err(error) =
                    source.set_wire_bit_order_while_paused(wire_bit_order, actual_position)
                {
                    unsafe { splayer_diretta_close(raw.as_ptr()) };
                    return Err(anyhow!(
                        "failed to adapt Native DSD wire bit order: {error}"
                    ));
                }
            }
            let connection = Self { raw, source };
            connection.configure_drain_after_open(&format);
            Ok((connection, actual_position))
        }

        pub fn format(&self) -> DirectDsdFormat {
            self.source.format()
        }

        /// 建连后采集 Sink 实测参数并注入排空目标与静音字节
        /// （Phase0 诊断 + Phase2/3：DSD 静音字节以 SDK getMuteByte 为准，
        /// 读数失败返回 0 → ring 退回 0x69 硬编码；排空目标退回旧常量）
        fn configure_drain_after_open(&self, wire_format: &DirectDsdFormat) {
            let latency_us = unsafe { splayer_diretta_sink_latency_us(self.raw.as_ptr()) };
            let buffer_us = unsafe { splayer_diretta_sink_buffer_us(self.raw.as_ptr()) };
            let cycle_size = unsafe { splayer_diretta_cycle_size(self.raw.as_ptr()) };
            let mute_byte = unsafe { splayer_diretta_mute_byte(self.raw.as_ptr()) };
            let drain_target_micros = compute_drain_target_micros(latency_us, buffer_us);
            self.source.set_drain_target_micros(drain_target_micros);
            self.source.set_mute_byte(mute_byte);
            let preroll_ms = ((drain_target_micros.max(150_000) + 999) / 1_000) as u32;
            let bytes_per_second = wire_format.bit_rate as f64 * wire_format.channels as f64 / 8.0;
            if !unsafe {
                splayer_diretta_arm_preroll(self.raw.as_ptr(), preroll_ms, bytes_per_second)
            } {
                tracing::warn!(target: "diretta_dsd", phase = "startup_preroll_failed",
                    "unable to arm Diretta DSD startup silence");
            }
            tracing::info!(
                target: "diretta_dsd",
                phase = "sink_params",
                latency_us = %latency_us,
                buffer_us = %buffer_us,
                cycle_size = %cycle_size,
                mute_byte = %mute_byte,
                drain_target_micros = %drain_target_micros,
                bit_rate = %wire_format.bit_rate,
                "Diretta DSD sink 实测参数（Phase0 诊断）"
            );
        }

        pub fn play(&mut self) -> Result<()> {
            if unsafe { splayer_diretta_play(self.raw.as_ptr()) } {
                Ok(())
            } else {
                Err(last_error("failed to start Diretta Native DSD playback"))
            }
        }

        pub fn pause(&mut self) -> Result<()> {
            if unsafe { splayer_diretta_pause(self.raw.as_ptr()) } {
                Ok(())
            } else {
                Err(last_error("failed to pause Diretta Native DSD playback"))
            }
        }

        pub fn seek_while_paused(&mut self, position_secs: f64) -> Result<f64> {
            self.source.seek_while_paused(position_secs)
        }

        /// 换源/拆线前的源级排空请求（DSD 无淡出通道，排空 = 回调侧交付 0x69 静音垫）
        pub fn begin_drain(&self) {
            self.source.begin_drain();
        }

        /// 软暂停（Phase3）：交付块替换为 0x69 零电平并至少交付一个静音块
        ///（或超时）。供 InnerPlayer::pause 在 sync->stop 前调用
        pub fn begin_soft_pause_and_wait(&self, timeout: Duration) -> bool {
            self.begin_drain();
            self.source.wait_soft_pause(timeout)
        }

        /// 恢复播放：清除排空请求。不清除则恢复后永久静音（正确性关键）
        pub fn resume_soft(&self) {
            self.source.clear_drain();
        }

        pub fn replace_drained_local_source(
            &mut self,
            source: &str,
            start_secs: f64,
        ) -> Result<DirectDsdFormat> {
            self.source.replace_drained_local(source, start_secs)
        }

        pub fn failed(&self) -> bool {
            self.source.failed()
        }

        pub fn finished(&self) -> bool {
            self.source.finished()
        }

        pub fn consumed_position(&self) -> f64 {
            self.source.consumed_position()
        }

        pub fn monitor(&self) -> DirectDsdMonitor {
            self.source.monitor()
        }

        pub fn stage_handle(&self) -> DirectDsdStageHandle {
            self.source.stage_handle()
        }
    }

    impl Drop for DirettaDirectDsdConnection {
        fn drop(&mut self) {
            unsafe { splayer_diretta_close(self.raw.as_ptr()) };
        }
    }
}

#[cfg(not(feature = "diretta"))]
mod imp {
    use super::*;

    pub fn available() -> bool {
        false
    }

    pub fn scan_devices() -> Result<Vec<DirettaDevice>> {
        Ok(Vec::new())
    }

    /// 非 diretta 构建下的占位实现：能力查询依赖 Diretta SDK。
    pub fn query_target_caps(_target_id: &str) -> Result<DirettaTargetCapabilities> {
        Err(anyhow::anyhow!("Diretta SDK support is not compiled"))
    }

    #[cfg(test)]
    pub struct DirettaDirectConnection;
    #[cfg(test)]
    pub struct DirettaDirectDsdConnection;

    #[cfg(test)]
    impl DirettaDirectConnection {
        pub fn open_local(_selector: &str, _path: &Path) -> Result<Self> {
            Err(anyhow!(
                "Diretta SDK support is not compiled; set DIRETTA_SDK_DIR when building"
            ))
        }

        pub fn format(&self) -> DirectPcmFormat {
            unreachable!("Diretta SDK support is not compiled")
        }

        pub fn play(&mut self) -> Result<()> {
            Err(anyhow!("Diretta SDK support is not compiled"))
        }

        pub fn pause(&mut self) -> Result<()> {
            Err(anyhow!("Diretta SDK support is not compiled"))
        }

        pub fn failed(&self) -> bool {
            true
        }

        pub fn finished(&self) -> bool {
            false
        }
    }

    #[cfg(test)]
    impl DirettaDirectDsdConnection {
        pub fn open_local(_selector: &str, _path: &Path) -> Result<Self> {
            Err(anyhow!("Diretta SDK support is not compiled"))
        }

        pub fn format(&self) -> DirectDsdFormat {
            unreachable!("Diretta SDK support is not compiled")
        }

        pub fn play(&mut self) -> Result<()> {
            Err(anyhow!("Diretta SDK support is not compiled"))
        }

        pub fn pause(&mut self) -> Result<()> {
            Err(anyhow!("Diretta SDK support is not compiled"))
        }

        pub fn failed(&self) -> bool {
            true
        }

        pub fn finished(&self) -> bool {
            false
        }
    }
}

pub use imp::{query_target_caps, scan_devices};
#[cfg(feature = "diretta")]
pub use imp::{DirettaDirectConnection, DirettaDirectDsdConnection};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_pull_callback_stays_copy_free_and_lock_free() {
        let bridge = include_str!("../../diretta-sys/src/bridge.cpp");
        let callback = bridge
            .split("bool getNewStream(diretta_stream& stream) override")
            .nth(1)
            .and_then(|tail| tail.split("private:").next())
            .expect("Diretta pull callback body should exist");
        for forbidden in [
            "memcpy",
            "setStream",
            "resize(",
            "mutex",
            "new ",
            "findOutput",
            "setSink(",
        ] {
            assert!(
                !callback.contains(forbidden),
                "Diretta SDK real-time callback must not contain {forbidden}"
            );
        }
        assert!(callback.contains("stream.Data.P"));
        assert!(callback.contains("stream.Size"));
    }

    #[test]
    fn native_dsd_bridge_selects_a_supported_native_wire_bit_order_only() {
        let bridge = include_str!("../../diretta-sys/src/bridge.cpp");
        let dsd_open = bridge
            .split("void* splayer_diretta_open_dsd_direct(")
            .nth(1)
            .and_then(|tail| tail.split("bool splayer_diretta_play").next())
            .expect("Native DSD bridge entry should exist");
        assert!(dsd_open.contains("FMT_DSD1"));
        assert!(dsd_open.contains("FMT_DSD_SIZ_32"));
        assert!(dsd_open.contains("FMT_DSD_BIG"));
        assert!(dsd_open.contains("FMT_DSD_LITTLE"));
        assert!(dsd_open.contains("FMT_DSD_LSB"));
        assert!(dsd_open.contains("FMT_DSD_MSB"));
        assert!(dsd_open.contains("source_lsb_first"));
        assert!(dsd_open.contains("wire_lsb_first"));
        // R8 位图协商：LSB/MSB × BIG/LITTLE 四组合按偏好顺序构建候选表，
        // 委托 open_direct_with_format 逐个 checkSinkSupport 探测
        // （源位序优先，避免 Rust 侧位重排；探测为本地校验，无网络往返）
        assert!(dsd_open.contains("bit_orders"));
        assert!(dsd_open.contains("byte_orders"));
        assert!(dsd_open.contains("candidates"));
        assert!(dsd_open.contains("open_direct_with_format"));
        for forbidden in ["DSD2PCM", "DoP", "memcpy", "reverse_bits"] {
            assert!(
                !dsd_open.contains(forbidden),
                "Native DSD bridge must not perform sample-domain conversion {forbidden}"
            );
        }
    }

    #[test]
    fn bridge_keeps_connection_completion_separate_from_playback_online_state() {
        let bridge = include_str!("../../diretta-sys/src/bridge.cpp");
        let connection = bridge
            .split("struct DirettaConnection")
            .nth(1)
            .and_then(|tail| tail.split("bool discover").next())
            .expect("Diretta connection cleanup should exist");
        assert!(connection.contains("~DirettaConnection()"));
        assert!(connection.contains("shutdown();"));
        assert!(connection.contains("disconnect_flgset();"));
        // db127f7（lifecycle 对齐 SDK148）：断连发起与等待拆分——
        // disconnect(false) 不在内部阻塞等待，由显式 disconnectWait() 兜底，
        // 保证清理路径每一步都可见、无隐藏超时
        assert!(connection.contains("disconnect(false);"));
        assert!(connection.contains("disconnectWait();"));
        assert!(connection.contains("sync->close();"));
        assert!(!connection.contains("disconnect(true);"));
        assert!(!connection.contains("kDisconnectTimeout"));

        let open = bridge
            .split("void* open_direct_with_format(")
            .nth(1)
            .and_then(|tail| tail.split("} // namespace").next())
            .expect("Diretta open path should exist");
        assert!(open.contains("connectPrepare(true)"));
        assert!(open.contains("connect(0)"));
        assert!(open.contains("connectWait()"));
        assert!(!open.contains("is_online()"));
        assert!(!open.contains("->play()"));

        let close = bridge
            .split("void splayer_diretta_close(void* opaque)")
            .nth(1)
            .expect("Diretta close entry should exist");
        assert!(close.contains("unique_ptr<DirettaConnection>"));
    }

    #[cfg(not(feature = "diretta"))]
    #[test]
    fn builds_without_sdk_and_reports_no_discoverable_targets() {
        assert!(!imp::available());
        assert!(imp::scan_devices().unwrap().is_empty());
    }

    #[test]
    fn selectors_keep_diretta_targets_in_the_existing_device_id_space() {
        let selector = selector_for("fe80::1234%2");
        assert_eq!(selector, "diretta:fe80::1234%2");
        assert_eq!(selector_target(&selector), Some("fe80::1234%2"));
        assert_eq!(selector_target("default_output"), None);
        assert_eq!(selector_target("diretta:"), None);
    }
}
