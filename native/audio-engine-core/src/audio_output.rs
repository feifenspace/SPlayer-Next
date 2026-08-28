//! 跨线程安全的音频输出
//!
//! `cpal::Stream`（以及包装它的 `rodio::MixerDeviceSink`）是 `!Send` 的——
//! cpal 文档明确要求 Stream 的创建、持有和 drop 都在同一线程上完成
//! （macOS CoreAudio 是真雷区，Windows WASAPI / Linux ALSA 是契约要求）。
//!
//! 但 NAPI 的 async fn 跑在多线程 tokio runtime 上，`.await` 后 Future
//! 可能在任意 worker thread 恢复，原本通过 `unsafe impl Send` 绕过类型系统的
//! 做法在 macOS 上是真 UB，其它平台属于"现在凑合能跑"的契约违反。
//!
//! 本模块的做法：开一个专用 `audio-output-owner` 线程独占持有 `MixerDeviceSink`，
//! 对外只暴露可跨线程克隆的 `Mixer`。
//! Stream 在该线程上创建，在该线程上 drop，永远不会被跨线程访问。

use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use rodio::cpal::{self, traits::DeviceTrait, traits::HostTrait};
use rodio::{mixer::Mixer, DeviceSinkBuilder, MixerDeviceSink};
use tracing::{debug, info, warn};

use crate::error::{AudioErrorKind, AudioResultExt};
use crate::priority;

/// 持有音频输出的跨线程句柄。`Send`，可放进 `InnerPlayer` 而不需 `unsafe impl Send`。
///
/// 内部专用线程独占 `MixerDeviceSink`，drop 这个结构会通过 channel 通知线程退出，
/// 线程退出时 drop `MixerDeviceSink`——确保 `cpal::Stream` 创建和销毁都在同一线程。
///
/// # Examples
///
/// ```ignore
/// // 走系统默认设备
/// let output = AudioOutput::new(None, None, None)?;
/// let player = Player::connect_new(output.mixer());
/// // player 可在任意线程上使用；output 持有的 cpal::Stream 始终在专用线程上
/// ```
pub struct AudioOutput {
    mixer: Mixer,
    /// 实际打开的输出流采样率
    sample_rate: u32,
    /// drop 这个 sender 会让 owner 线程的 recv 返回 Err，从而退出并释放 Stream
    /// 包成 Option 是为了 Drop 里能 take() 出来显式 drop，从而在 join 前先关闭 channel
    shutdown: Option<mpsc::Sender<()>>,
    /// owner 线程句柄，Drop 时 join 等待 cpal stream 在该线程真正释放
    thread: Option<JoinHandle<()>>,
}

impl AudioOutput {
    /// 在专用线程上创建音频输出
    ///
    /// # Arguments
    /// * `device_name` - 输出设备名，`None` 走系统默认设备
    /// * `sample_rate` - 目标采样率（Diretta 分支专用，若为 `None` 则使用默认 48000）
    /// * `channels` - 目标声道数（Diretta 分支专用，若为 `None` 则使用默认 2）
    ///
    /// # Errors
    /// - 找不到指定设备
    /// - 无可用音频设备
    /// - 专用线程 spawn 失败
    pub fn new(
        device_name: Option<&str>,
        sample_rate: Option<u32>,
        channels: Option<u16>,
    ) -> Result<Self> {
        let device_name = device_name.map(String::from);

        // 把构建结果回传给调用线程；用 sync_channel 容量 1 避免发送方阻塞
        let (result_tx, result_rx) = mpsc::sync_channel::<Result<(Mixer, u32)>>(1);
        // 调用方 drop AudioOutput 时关闭，触发 owner 线程退出
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

        let thread = thread::Builder::new()
            .name("audio-output-owner".to_string())
            .spawn(move || {
                priority::boost_current_audio_thread("audio-output-owner");
                debug!(device = ?device_name, "audio-output-owner: starting");

                // 检查是否为 Diretta Target 设备 (前缀 "diretta:" 或 "diretta@")
                if let Some(ref d_name) = device_name {
                    if let Some(target_addr) = d_name.strip_prefix("diretta:").or_else(|| d_name.strip_prefix("diretta@")) {
                        info!(target_addr, "audio-output-owner: 启动 Diretta 网络音频输出后端");
                        // Diretta 连接必须在创建 mixer 前完成。
                        // Diretta 当前接收 f32 PCM；保持播放器默认输出格式。
                        // tinyLMS-old 的格式协商在 C shim 中优先选择 32-bit 物理槽位。
                        // 44.1k 速率假设验证:对齐 test_sin/RAT_44100
                        // 新官方 Target 固件在 48k 时 clock lock 后 Refused;先验证 44.1k 能否 connect
                        let sample_rate = sample_rate.unwrap_or(44100);
                        let channels = channels.unwrap_or(2);
                        let bit_depth = 32;

                        // 解析复合地址格式: ip[%ifno][,port]
                        let (_ip_part, if_idx) = {
                            let mut ip_part = target_addr;
                            if let Some((ip, _port_str)) = ip_part.split_once(',') {
                                ip_part = ip;
                            }
                            if let Some((ip, if_str)) = ip_part.split_once('%') {
                                let if_no = if_str.parse::<i32>().unwrap_or(0);
                                (ip, if_no)
                            } else {
                                (ip_part, 0)
                            }
                        };

                        match crate::diretta::DirettaStream::connect(
                            target_addr,
                            if_idx,
                            sample_rate,
                            channels,
                            bit_depth,
                            false,
                            1500,
                        ) {
                            Ok(mut stream) => {
                                let ch_nz = std::num::NonZeroU16::new(channels).unwrap();
                                let rate_nz = std::num::NonZeroU32::new(sample_rate).unwrap();
                                let (mixer, mut source) = rodio::mixer::mixer(ch_nz, rate_nz);

                                if result_tx.send(Ok((mixer, sample_rate))).is_err() {
                                    let _ = stream.stop();
                                    return;
                                }

                                let chunk_samples = (sample_rate as usize * channels as usize * 10) / 1000; // 10ms chunk
                                let mut sample_buf = vec![0.0f32; chunk_samples];
                                let step_duration = std::time::Duration::from_millis(10);

                                // 启动预填充：预填 10 个 10ms 块（共 100ms 缓冲池），消除起播阶段与时钟抖动引起的爆音和丢包
                                for _ in 0..10 {
                                    for item in sample_buf.iter_mut() {
                                        *item = source.next().unwrap_or(0.0);
                                    }
                                    let _ = stream.write_samples(&sample_buf);
                                }

                                // P1 Watchdog（对齐 tinyLMS-old WatchdogLoop）：
                                const WATCHDOG_GRACE_TICKS: u32 = 50; // 500ms 宽容窗口
                                const WATCHDOG_DEAD_TICKS: u32 = 40;   // 400ms 连续离线阈值
                                let mut watchdog_grace_remaining = WATCHDOG_GRACE_TICKS;
                                let mut watchdog_offline_ticks = 0u32;
                                let mut next_tick = std::time::Instant::now();

                                loop {
                                    next_tick += step_duration;

                                    match shutdown_rx.try_recv() {
                                        Ok(_) | Err(mpsc::TryRecvError::Disconnected) => {
                                            debug!("audio-output-owner: Diretta shutting down");
                                            let _ = stream.stop();
                                            break;
                                        }
                                        Err(mpsc::TryRecvError::Empty) => {}
                                    }

                                    let mut max_amp = 0.0f32;
                                    for item in sample_buf.iter_mut() {
                                        let sample = source.next().unwrap_or(0.0);
                                        let abs = sample.abs();
                                        if abs > max_amp {
                                            max_amp = abs;
                                        }
                                        *item = sample;
                                    }

                                    static TICK_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                                    let tick = TICK_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    if tick < 5 || tick % 100 == 0 {
                                        info!(
                                            tick = tick,
                                            max_amp = max_amp,
                                            is_online = stream.is_online(),
                                            is_playing = stream.is_playing(),
                                            "Diretta 推流状态诊断"
                                        );
                                    }

                                    if let Err(error) = stream.write_samples(&sample_buf) {
                                        warn!(error = ?error, "Diretta 推流失败，停止当前输出线程");
                                        crate::diretta::runtime_state_record_failure(&error.to_string());
                                        break;
                                    }
                                    crate::diretta::runtime_state_set_connection(
                                        stream.is_online(),
                                        stream.is_playing(),
                                    );

                                    // P1 Watchdog：在线健康监控（对齐 tinyLMS-old WatchdogLoop）。
                                    if watchdog_grace_remaining > 0 {
                                        watchdog_grace_remaining -= 1;
                                        watchdog_offline_ticks = 0;
                                    } else if !stream.is_online() {
                                        watchdog_offline_ticks += 1;
                                        if watchdog_offline_ticks >= WATCHDOG_DEAD_TICKS {
                                            warn!(
                                                "Diretta: is_online 连续 {} ticks (~{}ms) 为 false，链路 stall，干净退出避免静音挂死",
                                                watchdog_offline_ticks,
                                                watchdog_offline_ticks * 10
                                            );
                                            crate::diretta::runtime_state_record_failure(
                                                "Diretta link stall (watchdog)",
                                            );
                                            break;
                                        }
                                    } else {
                                        watchdog_offline_ticks = 0;
                                    }

                                    let now = std::time::Instant::now();
                                    if next_tick > now {
                                        thread::sleep(next_tick - now);
                                    } else if now.duration_since(next_tick) > std::time::Duration::from_millis(50) {
                                        next_tick = now;
                                    }
                                }
                                return;
                            }
                            Err(err) => {
                                warn!(error = %err, target_addr, "Diretta 连接失败，回退到系统默认声卡");
                            }
                        }
                    }
                }

                let build_result = build_output_sink(device_name.as_deref());
                match build_result {
                    Ok((mut sink, sample_rate)) => {
                        sink.log_on_drop(false);
                        if result_tx
                            .send(Ok((sink.mixer().clone(), sample_rate)))
                            .is_err()
                        {
                            // 调用方已放弃接收：在本线程 drop sink 后退出
                            warn!("audio-output-owner: receiver dropped before handshake");
                            drop(sink);
                            return;
                        }
                        // 持有 sink，等待 shutdown 信号或 channel 关闭
                        let _ = shutdown_rx.recv();
                        debug!("audio-output-owner: shutting down, dropping cpal stream");
                        drop(sink);
                    }
                    Err(err) => {
                        warn!(error = %err, "audio-output-owner: 未检测到物理声卡，启用 Headless 虚拟音频时钟");
                        let sample_rate = 48000;
                        let channels = std::num::NonZeroU16::new(2).unwrap();
                        let rate = std::num::NonZeroU32::new(sample_rate).unwrap();
                        let (mixer, mut source) = rodio::mixer::mixer(channels, rate);
                        if result_tx.send(Ok((mixer, sample_rate))).is_err() {
                            return;
                        }

                        // 虚拟音频时钟循环，按真实采样速率消费样本以驱动解码与播放进度
                        let chunk_samples = (sample_rate as usize * 2 * 20) / 1000;
                        let mut buffer = vec![0.0f32; chunk_samples];
                        let step_duration = std::time::Duration::from_millis(20);

                        loop {
                            let start = std::time::Instant::now();
                            match shutdown_rx.try_recv() {
                                Ok(_) | Err(mpsc::TryRecvError::Disconnected) => {
                                    debug!("audio-output-owner: virtual clock shutting down");
                                    break;
                                }
                                Err(mpsc::TryRecvError::Empty) => {}
                            }
                            for item in buffer.iter_mut() {
                                if let Some(s) = source.next() {
                                    *item = s;
                                }
                            }
                            let elapsed = start.elapsed();
                            if let Some(rem) = step_duration.checked_sub(elapsed) {
                                thread::sleep(rem);
                            }
                        }
                    }
                }
            })
            .context("failed to spawn audio-output-owner thread")
            .with_audio_kind(AudioErrorKind::Device)?;

        let (mixer, sample_rate) = result_rx
            .recv()
            .context("audio output owner thread terminated unexpectedly")
            .with_audio_kind(AudioErrorKind::Device)??;

        Ok(Self {
            mixer,
            sample_rate,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        })
    }

    /// 借出输出混音器，用于连接 `Player`
    pub fn mixer(&self) -> &Mixer {
        &self.mixer
    }

    /// 输出流采样率，作为播放重采样目标
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

impl Drop for AudioOutput {
    /// 确定性释放：先 drop 发送端通知 owner 线程退出，再 join 等待 cpal stream 真正释放
    ///
    /// 这样 `set_output_device` 等场景里新旧 stream 不会重叠占用设备，
    /// 在 macOS / Linux 上避免 "device busy" 风险
    fn drop(&mut self) {
        // 先 drop sender 让 owner 线程的 shutdown_rx.recv() 返回 Err 退出
        drop(self.shutdown.take());
        if let Some(thread) = self.thread.take() {
            // 忽略 join 错误：owner 线程已经在 stream drop 时尽力清理过了
            let _ = thread.join();
        }
    }
}

/// 构建 cpal/rodio 输出流；**仅在 `audio-output-owner` 线程内调用**，
/// 保证 `MixerDeviceSink` 的创建、持有和 drop 都发生在同一线程上
///
/// 始终使用设备默认配置打开流，返回实际采样率供播放重采样器与 DSP 使用。
fn build_output_sink(device_name: Option<&str>) -> Result<(MixerDeviceSink, u32)> {
    let host = cpal::default_host();
    match device_name {
        Some(name) => {
            let device = host
                .output_devices()
                .context("Failed to enumerate output devices")?
                .find(|device| persisted_device_name(device).as_deref() == Some(name))
                .with_context(|| format!("Output device '{}' not found", name))
                .with_audio_kind(AudioErrorKind::Device)?;
            open_device_with_default_config(&device)
        }
        None => {
            let sink = DeviceSinkBuilder::open_default_sink()
                .context("Failed to open default output device")
                .with_audio_kind(AudioErrorKind::Device)?;
            let sample_rate = sink.config().sample_rate().get();
            info!(sample_rate, "使用系统默认音频输出配置");
            Ok((sink, sample_rate))
        }
    }
}

/// 设备名已被设置持久化为选择键，继续沿用旧 API 的值以避免升级后已有配置失效
#[allow(deprecated)]
fn persisted_device_name(device: &cpal::Device) -> Option<String> {
    device.name().ok()
}

/// 使用设备默认配置创建输出流
fn open_device_with_default_config(device: &cpal::Device) -> Result<(MixerDeviceSink, u32)> {
    let sink = DeviceSinkBuilder::from_device(device.clone())
        .context("Failed to get default output config")?
        .open_sink_or_fallback()
        .context("Failed to open output device")
        .with_audio_kind(AudioErrorKind::Device)?;
    let sample_rate = sink.config().sample_rate().get();
    info!(sample_rate, "使用设备默认音频输出配置");
    Ok((sink, sample_rate))
}

/// 枚举所有输出设备，返回 `(name, is_default)` 列表
/// 纯查询，不涉及 `!Send` 状态，调用方任意线程都能用
pub fn list_output_devices() -> Vec<(String, bool)> {
    let host = cpal::default_host();
    let default_name = host
        .default_output_device()
        .and_then(|device| persisted_device_name(&device));
    host.output_devices()
        .map(|devices| {
            devices
                .filter_map(|device| {
                    let name = persisted_device_name(&device)?;
                    let is_default = default_name.as_ref() == Some(&name);
                    Some((name, is_default))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 取系统默认输出设备名
pub fn default_device_name() -> Option<String> {
    cpal::default_host()
        .default_output_device()
        .and_then(|device| persisted_device_name(&device))
}
