//! Diretta 网络音频输出后端。
//!
//! cpal 重构（纯 cpal 输出流）后恢复的 Diretta 集成：`diretta:` / `diretta@`
//! 前缀的设备选择器不再进入 cpal 设备查找，而是路由到本模块的常驻推流线程。
//!
//! 与旧版（rodio mixer owner 线程）行为对齐的关键点：
//! - 连接长持：`DirettaStream` 按目标地址缓存复用，切歌/seek 只替换推流源，
//!   不断开 Target 连接，避免时钟断流导致 Target 端 Refused
//! - 10ms 节拍推流 + 100ms 预填充，消除起播爆音与时钟抖动
//! - P1 watchdog：链路 stall 时干净退出，避免静音挂死
//! - 运行期失败通过 `OutputFailureCallback` 上报（与 cpal 流错误同一事件链）

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use tracing::{info, warn};

use crate::audio_output::OutputFailureCallback;
use crate::diretta::{
    runtime_state_record_failure, runtime_state_set_connection, DirettaStream,
};
use crate::priority::boost_current_audio_thread;
use crate::source::DecoderSource;

/// 推流节拍：10ms 一批，对齐旧版 Diretta owner 线程
const PUSH_STEP: Duration = Duration::from_millis(10);
/// 预填充 30 个 10ms 块（共 300ms 缓冲池），对齐起播平滑策略
const PREFILL_CHUNKS: usize = 30;
/// watchdog 宽容窗口 500ms（连接初期 is_online 可能尚未就绪）
const WATCHDOG_GRACE_TICKS: u32 = 50;
/// watchdog 连续离线阈值 400ms，超过则判定链路 stall
const WATCHDOG_DEAD_TICKS: u32 = 40;
/// 推流线程落后节拍超过该值时重置节拍基准，避免追赶式连续推流
const TICK_CATCHUP_LIMIT: Duration = Duration::from_millis(50);

/// 一次 attach 交给推流线程的播放资源。
///
/// `volume`/`stopped`/`paused` 与 cpal 分支的 `PlaybackHandle` 共享同一原子标志，
/// 播放控制语义完全一致；`source` 被线程独占消费。
struct DirettaSourceParams {
    source: DecoderSource,
    volume: Arc<AtomicU32>,
    stopped: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
}

enum DirettaCmd {
    SetSource(DirettaSourceParams),
}

/// 线程与缓存共享的内部状态。
///
/// 推流线程持有本 Arc；`cmd_tx` 刻意放在外层 [`DirettaShared`]——
/// 所有 sender 都 drop 时线程循环读到 `Disconnected` 自然退出。
struct DirettaInner {
    /// 线程是否仍在运行（write 失败 / stall / sender 断开都会置 false）
    alive: AtomicBool,
    /// 实际连接的采样率（重采样目标以此为准，而非请求值）
    sample_rate: u32,
    channels: u16,
    /// 当前活跃输出的失败回调，attach 时更新，线程失败时触发
    failure_slot: Mutex<Option<OutputFailureCallback>>,
}

/// 可克隆、可缓存的 Diretta 连接句柄。
#[derive(Clone)]
pub(crate) struct DirettaShared {
    inner: Arc<DirettaInner>,
    cmd_tx: Sender<DirettaCmd>,
}

impl DirettaShared {
    /// 连接 Target 并启动常驻推流线程。
    ///
    /// 采样率沿用音源原始采样率（无值时 44100）；声道固定 2、32-bit PCM，
    /// 均与旧版 Diretta 分支一致。
    fn connect(
        target_addr: &str,
        if_idx: i32,
        requested_sample_rate: Option<u32>,
        on_failure: OutputFailureCallback,
    ) -> Result<Self> {
        // 新官方 Target 固件在 48k clock lock 后易 Refused；固定使用 44.1k 系列标准速率（无值或 48k 系列默认 44.1k）
        // 解码侧 DecoderData 会根据输出速率（44.1k）自动将 48k 等音源重采样，确保时钟锁定理性稳定
        let sample_rate = match requested_sample_rate {
            Some(sr) if sr == 44100 || sr == 88200 || sr == 176400 || sr == 352800 => sr,
            _ => 44100,
        };
        let channels = 2u16;
        let bit_depth = 8u8 * 4;

        info!(
            target_addr,
            sample_rate, channels, "连接 Diretta Target（网络音频输出后端）"
        );
        let stream = DirettaStream::connect(target_addr, if_idx, sample_rate, channels, bit_depth, false, 1500)
            .with_context(|| format!("Diretta Target '{target_addr}' 连接失败"))?;

        let (cmd_tx, cmd_rx) = channel();
        let inner = Arc::new(DirettaInner {
            alive: AtomicBool::new(true),
            sample_rate: stream.sample_rate(),
            channels: stream.channels(),
            failure_slot: Mutex::new(Some(on_failure)),
        });

        let thread_inner = Arc::clone(&inner);
        let thread_name = format!("diretta-push-{}", short_target(target_addr));
        let handle = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || run_push_loop(stream, cmd_rx, thread_inner))
            .context("启动 Diretta 推流线程失败")?;
        info!(target_addr, "Diretta 推流线程已启动");

        // 循环自身管理退出，join 句柄直接 drop（drop 即 detach）
        drop(handle);
        Ok(Self { inner, cmd_tx })
    }

    /// 把新的播放资源交给推流线程（替换旧源，连接保持不断开）。
    ///
    /// 同时更新失败回调为本次输出代次的回调。
    pub(crate) fn attach_source(
        &self,
        source: DecoderSource,
        volume: Arc<AtomicU32>,
        stopped: Arc<AtomicBool>,
        paused: Arc<AtomicBool>,
        on_failure: OutputFailureCallback,
    ) -> Result<()> {
        if !self.is_alive() {
            anyhow::bail!("Diretta 推流线程已退出，需要重建输出");
        }
        *self.inner.failure_slot.lock() = Some(on_failure);
        self.cmd_tx
            .send(DirettaCmd::SetSource(DirettaSourceParams {
                source,
                volume,
                stopped,
                paused,
            }))
            .map_err(|_| anyhow::anyhow!("Diretta 推流线程已退出，发送播放资源失败"))
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        self.inner.sample_rate
    }

    pub(crate) fn channels(&self) -> u16 {
        self.inner.channels
    }

    fn is_alive(&self) -> bool {
        self.inner.alive.load(Ordering::Acquire)
    }
}

/// 进程级连接缓存：按完整目标地址复用推流线程。
///
/// `AudioOutput` 每次 load 都会重建，但对 Diretta 而言断开重连会造成
/// Target 时钟断流，因此连接归属缓存而非单个 `AudioOutput`。
static DIRETTA_CACHE: once_cell::sync::Lazy<Mutex<HashMap<String, DirettaShared>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

/// 获取（或建立）指定目标的 Diretta 输出连接。
///
/// 缓存命中且线程存活时直接复用（速率以现有连接为准）；否则新建连接。
/// 顺手清理已无主（仅剩推流线程自己引用）的其他缓存条目，避免切换
/// 目标后旧连接永久占用带宽。
pub(crate) fn acquire_diretta_output(
    target_addr: &str,
    if_idx: i32,
    requested_sample_rate: Option<u32>,
    on_failure: OutputFailureCallback,
) -> Result<DirettaShared> {
    let mut cache = DIRETTA_CACHE.lock();
    if let Some(existing) = cache.get(target_addr) {
        if existing.is_alive() {
            // 复用现有连接：采样率不匹配也不断开，解码侧重采样目标跟随连接实际速率
            return existing.attach_source_silent(on_failure);
        }
    }
    // 清理无主连接：Arc 仅剩推流线程自身持有（strong_count == 1）
    cache.retain(|addr, shared| {
        addr == target_addr || Arc::strong_count(&shared.inner) > 1
    });

    let shared = DirettaShared::connect(target_addr, if_idx, requested_sample_rate, on_failure)?;
    cache.insert(target_addr.to_string(), shared.clone());
    Ok(shared)
}

impl DirettaShared {
    /// 缓存命中时的轻量 attach：只更新失败回调，播放资源由随后的 `attach_source` 下发
    fn attach_source_silent(&self, on_failure: OutputFailureCallback) -> Result<DirettaShared> {
        *self.inner.failure_slot.lock() = Some(on_failure);
        Ok(self.clone())
    }
}

/// 从 `diretta:` / `diretta@` 前缀后的地址解析接口索引。
///
/// 复合格式 `ip[%ifno][,port]`，对齐旧版解析逻辑；
/// 地址串本身（含 `%if` 与端口）原样传给 `DirettaStream::connect`。
pub(crate) fn parse_interface_index(target_addr: &str) -> i32 {
    let mut addr = target_addr;
    if let Some((ip, _port)) = addr.split_once(',') {
        addr = ip;
    }
    if let Some((_ip, if_str)) = addr.split_once('%') {
        if_str.parse::<i32>().unwrap_or(0)
    } else {
        0
    }
}

/// 日志用短目标名：截到最后一个 `%` / `,` 之前，避免长 IPv6 刷屏
fn short_target(target_addr: &str) -> String {
    let end = target_addr
        .find(['%', ','])
        .unwrap_or(target_addr.len());
    target_addr[..end].to_string()
}

/// 常驻推流循环：预填充静音后按 10ms 节拍消费播放源并推给 Target。
///
/// 退出条件（均置 `alive = false` 并触发失败回调）：
/// - 所有 `DirettaShared` 被 drop（channel Disconnected）
/// - `write_samples` 失败（Target 离线 / 网络故障）
/// - watchdog 判定链路 stall（is_online 连续 400ms 为 false）
fn run_push_loop(mut stream: DirettaStream, cmd_rx: Receiver<DirettaCmd>, inner: Arc<DirettaInner>) {
    boost_current_audio_thread("diretta-push");

    let sample_rate = stream.sample_rate();
    let channels = stream.channels();
    let chunk_samples = (sample_rate as usize * channels as usize * 10) / 1000;
    let mut sample_buf = vec![0.0f32; chunk_samples];

    // 预填充 300ms 静音，消除起播阶段与时钟抖动引起的爆音和丢包
    for _ in 0..PREFILL_CHUNKS {
        let _ = stream.write_samples(&sample_buf);
    }

    let mut current: Option<DirettaSourceParams> = None;
    let mut watchdog_grace_remaining = WATCHDOG_GRACE_TICKS;
    let mut watchdog_offline_ticks = 0u32;
    let mut write_consecutive_errors = 0u32;
    let mut next_tick = Instant::now();
    let mut tick: u64 = 0;

    loop {
        next_tick += PUSH_STEP;

        match cmd_rx.try_recv() {
            Ok(DirettaCmd::SetSource(params)) => {
                info!(target_addr = stream.target_addr(), "Diretta 推流源已更新");
                current = Some(params);
            }
            // 所有句柄 drop：正常停机路径
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                info!("Diretta 推流线程退出（连接句柄已全部释放）");
                break;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }

        // 填充本节拍样本：无源 / 已停止 / 已暂停时推静音，保持 Target 时钟在线
        let mut max_amp = 0.0f32;
        let mut playing = false;
        if let Some(params) = current.as_mut() {
            let gain = f32::from_bits(params.volume.load(Ordering::Relaxed));
            let active = !params.stopped.load(Ordering::Acquire)
                && !params.paused.load(Ordering::Acquire);
            for item in sample_buf.iter_mut() {
                let sample = if active {
                    playing = true;
                    params.source.next().unwrap_or(0.0) * gain
                } else {
                    0.0
                };
                let abs = sample.abs();
                if abs > max_amp {
                    max_amp = abs;
                }
                *item = sample;
            }
        } else {
            sample_buf.fill(0.0);
        }

        if let Err(error) = stream.write_samples(&sample_buf) {
            write_consecutive_errors += 1;
            // 允许短时间（如启动初期时钟锁定）的瞬态抖动，连续失败多次后再退出
            if write_consecutive_errors >= 10 {
                warn!(error = %error, consecutive = write_consecutive_errors, "Diretta 推流连续失败，退出推流线程");
                runtime_state_record_failure(&error.to_string());
                fire_failure(&inner);
                break;
            }
        } else {
            write_consecutive_errors = 0;
        }
        runtime_state_set_connection(stream.is_online(), playing);

        // P1 watchdog：连接初期宽容，之后 is_online 持续离线判定链路 stall，
        // 干净退出避免静音挂死（对齐 tinyLMS-old WatchdogLoop）
        if watchdog_grace_remaining > 0 {
            watchdog_grace_remaining -= 1;
            watchdog_offline_ticks = 0;
        } else if !stream.is_online() {
            watchdog_offline_ticks += 1;
            if watchdog_offline_ticks >= WATCHDOG_DEAD_TICKS {
                warn!(
                    offline_ms = watchdog_offline_ticks * 10,
                    "Diretta: is_online 持续离线，链路 stall，退出推流线程"
                );
                runtime_state_record_failure("Diretta link stall (watchdog)");
                fire_failure(&inner);
                break;
            }
        } else {
            watchdog_offline_ticks = 0;
        }

        // 诊断日志：前 5 拍 + 每 500 拍（~5s）一次
        tick += 1;
        if tick < 5 || tick % 500 == 0 {
            info!(
                tick,
                max_amp,
                is_online = stream.is_online(),
                playing,
                "Diretta 推流状态"
            );
        }

        let now = Instant::now();
        if next_tick > now {
            std::thread::sleep(next_tick - now);
        } else if now.duration_since(next_tick) > TICK_CATCHUP_LIMIT {
            next_tick = now;
        }
    }

    let _ = stream.stop();
    inner.alive.store(false, Ordering::Release);
}

/// 触发当前输出代次的失败回调（不持锁调用）
fn fire_failure(inner: &DirettaInner) {
    let callback = inner.failure_slot.lock().clone();
    if let Some(callback) = callback {
        callback();
    }
}
