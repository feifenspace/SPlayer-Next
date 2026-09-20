//! ALSA MMAP 零拷贝直出后端（蓝图 §3.2，L1 本地位纯真直出）。
//!
//! - hw 直开、无 plug 转换：采样率 `ValueOr::Exact` 精确匹配（拒绝静默重采样，
//!   不支持即报错，由调用方降级 cpal）；
//! - MMAP Interleaved：直接写内核 DMA 缓冲映射，应用侧零拷贝；
//! - XRUN 先 `prepare` 再重试并计数（欠载是可恢复状态，不是致命错误）；
//! - 写循环线程 RT 纪律：SCHED_FIFO + 性能核/隔离核绑定，事件驱动等待无忙等；
//! - 位纯真条件：`gain == 1.0` 且采样率精确匹配（音量/DSP 门槛由调用方把关）。
//!
//! 声道数固定 2（headless 立体声规范）；首版数据面沿用 `DecoderSource` 的
//! f32 拉取（16/24-bit 源在 gain=1 时 f32 往返无损），原生源格式直通解码
//! 归入后续迭代。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use alsa::pcm::{Access, Format, State};
use alsa::{Direction, ValueOr, PCM};
use anyhow::{anyhow, ensure, Context, Result};
use tracing::{info, warn};

use crate::audio_output::OutputFailureCallback;
use crate::priority::{bind_current_thread_to_performance_cores, boost_current_audio_thread};
use crate::source::{DecoderSource, IntegerDecoderSource};

/// 全进程累计 XRUN 计数（B9.6）：供 B2.2 观测钩子周期采样，
/// 汇入 §六.2 拷机判据（xrun_count=0）
static XRUN_TOTAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 进程启动以来累计的 ALSA MMAP XRUN 次数
pub fn xrun_total() -> u64 {
    XRUN_TOTAL.load(Ordering::Acquire)
}

/// 连续致命错误阈值：超过后判定输出链路故障并上报（触发 OutputStalled 重建链路）
const MAX_CONTIGUOUS_ERRORS: u32 = 50;
/// 设备事件等待上限（毫秒）：决定 play/pause 指令的响应延迟上界
const WAIT_CEILING_MS: u32 = 50;

/// 等待已经提交给 ALSA DMA 的旧音频自然播放完，再释放 PCM。
///
/// 直接 drop 会截断 hw/appl queue 的尾部，跨曲目或跨采样率重建时容易
/// 把截断边沿送到 DAC。这里不再写入新数据，只等待硬件消耗现有队列；
/// 上限保证设备异常时停止不会卡死。
fn drain_before_drop(pcm: &PCM, format: Format, hardware_paused: bool, rate: u32) {
    if hardware_paused {
        return;
    }

    // 模仿 Diretta 的 pre-mute：在旧 DMA 队列后追加一小段数字静音，
    // 让 DAC 在关闭/重配前回到零电平，而不是停在最后一个真实样本上。
    let mut remaining = (rate as usize / 200).max(1); // 5 ms
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    while remaining > 0 && std::time::Instant::now() < deadline {
        let avail = match pcm.avail_update() {
            Ok(avail) => avail as usize,
            Err(_) => break,
        };
        if avail == 0 {
            let _ = pcm.wait(Some(WAIT_CEILING_MS));
            continue;
        }
        let frames = avail.min(remaining).min(4096);
        let written = if format == Format::s16() {
            match pcm.io_i16().and_then(|io| io.mmap(frames, |buf: &mut [i16]| {
                buf.fill(0);
                frames
            })) {
                Ok(n) => n,
                Err(_) => break,
            }
        } else if format == Format::s24() {
            match pcm.io_i32_s24().and_then(|io| io.mmap(frames, |buf: &mut [i32]| {
                    buf.fill(0);
                    frames
                })) {
                Ok(n) => n,
                Err(_) => break,
            }
        } else {
            match pcm.io_i32().and_then(|io| io.mmap(frames, |buf: &mut [i32]| {
                buf.fill(0);
                frames
            })) {
                Ok(n) => n,
                Err(_) => break,
            }
        };
        remaining = remaining.saturating_sub(written);
    }

    if remaining > 0 {
        warn!(remaining, rate, "ALSA MMAP 停止静音垫未完全写入，回退强制释放");
    } else if let Err(error) = pcm.drain() {
        warn!(%error, rate, "ALSA MMAP 正常 drain 失败，回退强制释放");
    }
}

/// hw buffer 预填高水位（毫秒）。写循环每轮只把 hw 已填水位补到该上限，
/// 而非"有空间就灌"：消费节奏贴回真实时，解码 Shared 队列得以保留网络
/// 缓冲垫，position 平滑前进，watchdog 不再误判流媒体"输出停滞"。
/// 预填 200ms 远超 RT 内核调度毛刺 + 已提权音频线程的最坏等待，无 xrun
/// 风险；本地文件场景同步受益（队列缓冲垫保留）。
/// env `SPLAYER_ALSAMMAP_WATERMARK_MS` 可覆盖（毫秒；0 = 恢复旧行为不限速）。
const HW_HIGH_WATERMARK_MS: u64 = 200;

/// 直出采样格式优先级（容器精度从高到低）
const FORMAT_PRIORITY: [Format; 3] = [Format::s32(), Format::s24(), Format::s16()];

/// ALSA MMAP 直出采样率安全上限。正常 PCM 最高 768k；DSD 解码 PCM 最低
/// DSD64 = 2.8224 MHz。高于 1 MHz 的请求只可能来自 DSD 源解码——直接拒绝，
/// 防止 USB isoc 超高带宽请求拖垮 snd-usb-audio/xHCI 导致整机死机
/// （2026-09-11 播 DSD 经 ALSA MMAP 整机 hang 的防护）。
pub(crate) const MAX_SAFE_SAMPLE_RATE: u32 = 1_000_000;

/// 协商 hw 参数：采样率必须精确命中，声道固定 2。
/// 格式按 [`FORMAT_PRIORITY`] 逐个探测。返回（格式，采样率，声道数）
fn negotiate_hw_params(pcm: &PCM, requested_rate: Option<u32>) -> Result<(Format, u32, u16)> {
    let mut h = alsa::pcm::HwParams::any(pcm)?;
    h.set_access(Access::MMapInterleaved)?;
    h.set_channels(2)?;

    let mut chosen = None;
    for &format in &FORMAT_PRIORITY {
        if h.test_format(format).is_ok() {
            h.set_format(format)?;
            chosen = Some(format);
            break;
        }
    }
    let format = chosen.ok_or_else(|| anyhow!("设备不支持 S32/S24/S16 任一直出格式"))?;

    match requested_rate {
        Some(rate) => {
            ensure!(
                rate <= MAX_SAFE_SAMPLE_RATE,
                "采样率 {rate} Hz 超出 ALSA MMAP 直出安全上限（{MAX_SAFE_SAMPLE_RATE} Hz）。                 DSD 源请使用 Diretta 输出（支持 native DSD/DoP）；本地声卡直出不支持 DSD 解码流"
            );
            // 蓝图示例缺陷 1 修正：crate 无 Exact，用 Nearest 设置后校验实际
            // 速率必须精确命中——静默邻居速率会触发重采样破坏位纯真
            h.set_rate(rate, ValueOr::Nearest)?;
            let actual = h.get_rate()?;
            ensure!(
                actual == rate,
                "设备不支持精确采样率 {rate}（最近支持 {actual}），拒绝静默重采样"
            );
        }
        None => {
            h.set_rate_near(48_000, ValueOr::Nearest)?;
        }
    }
    h.set_period_size_near(1024, ValueOr::Nearest)?;
    pcm.hw_params(&mut h)?;

    let rate = h.get_rate()?;
    let channels = h.get_channels()?;
    Ok((format, rate, channels as u16))
}

fn set_sw_params(pcm: &PCM) -> Result<()> {
    let mut sw = pcm.sw_params_current()?;
    let (buffer_frames, period_frames) = pcm.get_params()?;
    // 起播阈值：攒到约一个周期即开始，留足欠载余量
    sw.set_start_threshold(buffer_frames.min(period_frames * 2) as i64)?;
    sw.set_avail_min(period_frames as i64)?;
    pcm.sw_params(&mut sw)?;
    Ok(())
}

pub(crate) fn open_pcm(
    device: &str,
    requested_rate: Option<u32>,
) -> Result<(PCM, Format, u32, u16, bool)> {
    let pcm = PCM::new(device, Direction::Playback, false)
        .with_context(|| format!("打开 ALSA 设备失败: {device}"))?;
    let (format, rate, channels) = negotiate_hw_params(&pcm, requested_rate)?;
    set_sw_params(&pcm)?;
    let can_pause = pcm.hw_params_current()?.can_pause();
    info!(
        device = %device,
        ?format,
        rate,
        channels,
        can_pause,
        "ALSA MMAP 输出已协商"
    );
    Ok((pcm, format, rate, channels, can_pause))
}

/// ALSA MMAP 输出流句柄：drop 即停流并回收写循环线程。
/// play/pause 经原子标志驱动写循环（硬件支持 pause 时原位冻结，否则 drop/prepare）
pub struct AlsaMmapStream {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    sample_rate: u32,
    channels: u16,
}

impl AlsaMmapStream {
    /// 打开设备并启动写循环。返回（句柄，实际采样率，声道数）。
    /// 采样率无法精确满足时直接报错（调用方降级 cpal 路径）
    pub fn open(
        device: &str,
        requested_sample_rate: Option<u32>,
        source: DecoderSource,
        volume: Arc<AtomicU32>,
        stopped: Arc<AtomicBool>,
        on_failure: OutputFailureCallback,
    ) -> Result<(Self, u32, u16)> {
        // 先协商拿到真实参数（随即释放），正式独占打开在写循环线程内进行
        let (_, _, rate, channels, _) = open_pcm(device, requested_sample_rate)?;
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));

        let device_owned = device.to_string();
        let worker_stop = Arc::clone(&stop);
        let worker_paused = Arc::clone(&paused);
        let worker = std::thread::Builder::new()
            .name("alsa-mmap-output".into())
            .spawn(move || {
                boost_current_audio_thread("alsa-mmap-output");
                bind_current_thread_to_performance_cores("alsa-mmap-output");
                if let Err(error) = write_loop(
                    &device_owned,
                    requested_sample_rate,
                    source,
                    volume,
                    stopped,
                    worker_paused,
                    worker_stop,
                    on_failure,
                ) {
                    warn!(error = %error, "ALSA MMAP 写循环退出");
                }
            })
            .context("启动 ALSA MMAP 写循环线程失败")?;

        Ok((
            Self {
                stop,
                paused,
                worker: Some(worker),
                sample_rate: rate,
                channels,
            },
            rate,
            channels,
        ))
    }

    /// 打开整数 PCM 写入线程。只由后续位纯真路径调用；现有 f32 open 保持不变。
    pub fn open_integer(
        device: &str,
        requested_sample_rate: Option<u32>,
        source: IntegerDecoderSource,
        volume: Arc<AtomicU32>,
        stopped: Arc<AtomicBool>,
        on_failure: OutputFailureCallback,
    ) -> Result<(Self, u32, u16)> {
        let (_, _, rate, channels, _) = open_pcm(device, requested_sample_rate)?;
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let device_owned = device.to_string();
        let worker_stop = Arc::clone(&stop);
        let worker_paused = Arc::clone(&paused);
        let worker = std::thread::Builder::new()
            .name("alsa-mmap-output-i32".into())
            .spawn(move || {
                boost_current_audio_thread("alsa-mmap-output-i32");
                bind_current_thread_to_performance_cores("alsa-mmap-output-i32");
                if let Err(error) = write_loop_integer(
                    &device_owned,
                    requested_sample_rate,
                    source,
                    volume,
                    stopped,
                    worker_paused,
                    worker_stop,
                    on_failure,
                ) {
                    warn!(error = %error, "ALSA MMAP i32 写循环退出");
                }
            })
            .context("启动 ALSA MMAP i32 写循环线程失败")?;
        Ok((
            Self {
                stop,
                paused,
                worker: Some(worker),
                sample_rate: rate,
                channels,
            },
            rate,
            channels,
        ))
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn play(&self) {
        self.paused.store(false, Ordering::Release);
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }
}

impl Drop for AlsaMmapStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// 写循环：poll 等待空间（事件驱动，无忙等）→ mmap 直写 DMA 映射。
/// XRUN 恢复（prepare + 计数）；连续致命错误超限上报 OutputFailureCallback
#[allow(clippy::too_many_arguments)]
fn write_loop(
    device: &str,
    requested_rate: Option<u32>,
    mut source: DecoderSource,
    volume: Arc<AtomicU32>,
    stopped: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    on_failure: OutputFailureCallback,
) -> Result<()> {
    let (pcm, format, rate, _channels, can_pause) = open_pcm(device, requested_rate)?;
    // ALSA hw 参数在本次流的生命周期内保持不变。提前缓存 buffer_frames，
    // 避免实时写循环每轮再次进入 ALSA 控制层查询参数。
    let (buffer_frames, _period_frames) = pcm.get_params()?;
    // v9e 诊断：一次性打印协商几何 + 启动阈值
    {
        let (buf, per) = pcm.get_params()?;
        let start_th = pcm
            .sw_params_current()
            .and_then(|sw| {
                use alsa::pcm::SwParams;
                SwParams::get_start_threshold(&sw)
            })
            .unwrap_or(-1);
        info!(
            buf, per, start_th, rate,
            "ALSA MMAP 写循环几何"
        );
    }
    let mut hardware_paused = false;
    let mut pause_draining = false;
    let mut pause_silence_remaining = 0usize;
    let mut startup_silence_remaining = (rate as usize / 200).max(1); // 5 ms
    let mut contiguous_errors: u32 = 0;
    let mut xrun_count: u64 = 0;
    // v9e 诊断：状态轨迹（前 15s 或非 Running 时每秒一条）
    let diag_started = std::time::Instant::now();
    let mut diag_last = std::time::Instant::now();
    let mut diag_writes: u64 = 0;
    let mut diag_frames: u64 = 0;
    // 配置只在输出线程启动时读取一次；实时写循环内不做环境变量查询和字符串解析。
    let watermark_ms = std::env::var("SPLAYER_ALSAMMAP_WATERMARK_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(HW_HIGH_WATERMARK_MS);
    let watermark_frames = watermark_ms.saturating_mul(rate as u64) / 1000;

    // 暂停/停止时送数字静音：f32 静音帧（写入时按格式转换）
    macro_rules! fill_frame {
        ($frame:expr) => {{
            let gain = f32::from_bits(volume.load(Ordering::Relaxed));
            if startup_silence_remaining > 0 {
                $frame[0] = 0.0;
                $frame[1] = 0.0;
                startup_silence_remaining -= 1;
            } else if stopped.load(Ordering::Acquire) || paused.load(Ordering::Acquire) {
                $frame[0] = 0.0;
                $frame[1] = 0.0;
            } else if gain.to_bits() == 1.0_f32.to_bits() {
                // 100% 音量是 ALSA MMAP 位纯真门槛；跳过无意义的 f32 乘法，
                // 保持解码器输出到容器转换之间最短的样本路径。
                $frame[0] = source.next().unwrap_or(0.0);
                $frame[1] = source.next().unwrap_or(0.0);
            } else {
                $frame[0] = source.next().map(|v| v * gain).unwrap_or(0.0);
                $frame[1] = source.next().map(|v| v * gain).unwrap_or(0.0);
            }
        }};
    }

    loop {
        if stop.load(Ordering::Acquire) {
            drain_before_drop(&pcm, format, hardware_paused, rate);
            let _ = pcm.drop();
            info!(xrun_count, "ALSA MMAP 写循环停止（已排空旧 DMA 队列）");
            return Ok(());
        }

        let want_paused = paused.load(Ordering::Acquire);
        if want_paused && !hardware_paused && can_pause {
            // 不在任意一个非零样本处直接冻结 ALSA。先让写循环补一小段
            // 0 PCM，再等待旧 DMA 队列排空，最后才 pause 硬件。
            if !pause_draining {
                pause_draining = true;
                pause_silence_remaining = (rate as usize / 200).max(1); // 5 ms
            }
            if pause_silence_remaining == 0 {
                let delay = pcm.status().map(|s| s.get_delay()).unwrap_or(0);
                if delay <= 0 {
                    if pcm.pause(true).is_ok() {
                        hardware_paused = true;
                        pause_draining = false;
                    }
                } else {
                    let _ = pcm.wait(Some(WAIT_CEILING_MS));
                    continue;
                }
            }
        } else if !want_paused {
            pause_draining = false;
            pause_silence_remaining = 0;
            if hardware_paused {
                if pcm.pause(false).is_ok() {
                    hardware_paused = false;
                }
            }
        } else if want_paused && !can_pause {
            let _ = pcm.drop();
            hardware_paused = true;
        }
        if hardware_paused {
            // 设备时钟已冻结（原位 pause）：无数据可写，等指令即可
            std::thread::sleep(std::time::Duration::from_millis(WAIT_CEILING_MS as u64));
            continue;
        }

        match pcm.state() {
            State::Running | State::Prepared | State::Paused => {}
            State::XRun => {
                // 修正蓝图示例缺陷 3：XRUN 先 prepare 再重试，不作为错误中断
                xrun_count += 1;
                XRUN_TOTAL.fetch_add(1, Ordering::Release);
                pcm.prepare()?;
                continue;
            }
            State::Suspended => {
                if pcm.resume().is_err() {
                    pcm.prepare()?;
                }
                continue;
            }
            State::Open | State::Setup => {
                pcm.prepare()?;
                continue;
            }
            other => anyhow::bail!("ALSA 设备进入不可恢复状态: {other:?}"),
        }

        let running_now = matches!(pcm.state(), State::Running);
        if diag_last.elapsed() >= std::time::Duration::from_millis(1000)
            && (!running_now || diag_started.elapsed() < std::time::Duration::from_secs(15))
        {
            // Status 无 hw_ptr/appl_ptr 访问器；delay = appl-hw+在途（播放语义），
            // 配合 avail（=buf-filled）足以还原硬件消费进度
            let (delay, avail_st) = pcm
                .status()
                .map(|s| (s.get_delay(), s.get_avail()))
                .unwrap_or((-1, -1));
            info!(
                state = ?pcm.state(),
                delay, avail_st,
                avail_now = pcm.avail_update().unwrap_or(-1),
                diag_writes, diag_frames,
                "ALSA MMAP 写循环轨迹"
            );
            diag_last = std::time::Instant::now();
        }

        let avail = match pcm.avail_update() {
            Ok(avail) => avail as usize,
            Err(err) => {
                if pcm.state() == State::XRun {
                    xrun_count += 1;
                    XRUN_TOTAL.fetch_add(1, Ordering::Release);
                    pcm.prepare()?;
                    continue;
                }
                contiguous_errors += 1;
                if contiguous_errors >= MAX_CONTIGUOUS_ERRORS {
                    on_failure();
                    anyhow::bail!("ALSA 连续错误超限（last: {err}）");
                }
                continue;
            }
        };
        if avail == 0 {
            let _ = pcm.wait(Some(WAIT_CEILING_MS));
            continue;
        }

        // 高水位限速（v9e）：hw 已填水位 = buffer_frames - avail。每轮只补到
        // ~HW_HIGH_WATERMARK_MS 上限，多余供给滞留在 Shared 队列作网络缓冲垫。
        // pacing 只约束"何时写"，不碰样本路径——位纯真不受影响。
        let filled = buffer_frames.saturating_sub(avail as u64);
        let allowed = watermark_frames.saturating_sub(filled) as usize;
        let mut frames = if watermark_ms == 0 {
            avail.min(4096)
        } else {
            avail.min(4096).min(allowed)
        };
        if pause_draining {
            frames = frames.min(pause_silence_remaining);
        }
        if frames == 0 {
            // 已填至高水位：hw 还在按真实时消耗，睡一小段再补。
            // 不能用 pcm.wait——它等的是 avail>=avail_min（周期级空闲），
            // 与水位条件 filled<watermark 不等价：水位满但 hw 仍有空闲时
            // wait 立即返回，会造成忙转烧满一核（2026-09-11 v9e 实测教训）
            let drain_ms = (watermark_ms / 4).clamp(2, 20);
            std::thread::sleep(std::time::Duration::from_millis(drain_ms));
            continue;
        }
        // 格式为运行期值（非 const 可匹配），按相等性分派
        let result = if format == Format::s16() {
            pcm.io_i16()?.mmap(frames, |buf: &mut [i16]| {
                let mut frame = [0.0f32; 2];
                let mut done = 0usize;
                for pair in buf.chunks_exact_mut(2) {
                    fill_frame!(frame);
                    pair[0] = to_i16(frame[0]);
                    pair[1] = to_i16(frame[1]);
                    done += 1;
                }
                done
            })
        } else if format == Format::s24() {
            pcm.io_i32_s24()?.mmap(frames, |buf: &mut [i32]| {
                let mut frame = [0.0f32; 2];
                let mut done = 0usize;
                for pair in buf.chunks_exact_mut(2) {
                    fill_frame!(frame);
                    pair[0] = to_s24_container(frame[0]);
                    pair[1] = to_s24_container(frame[1]);
                    done += 1;
                }
                done
            })
        } else {
            pcm.io_i32()?.mmap(frames, |buf: &mut [i32]| {
                let mut frame = [0.0f32; 2];
                let mut done = 0usize;
                for pair in buf.chunks_exact_mut(2) {
                    fill_frame!(frame);
                    pair[0] = to_i32(frame[0]);
                    pair[1] = to_i32(frame[1]);
                    done += 1;
                }
                done
            })
        };
        match result {
            Ok(n) => {
                contiguous_errors = 0;
                diag_writes += 1;
                diag_frames += n as u64;
                if pause_draining {
                    pause_silence_remaining = pause_silence_remaining.saturating_sub(n);
                }
            }
            Err(err) => {
                if pcm.state() == State::XRun {
                    xrun_count += 1;
                    let _ = pcm.prepare();
                    continue;
                }
                contiguous_errors += 1;
                if contiguous_errors >= MAX_CONTIGUOUS_ERRORS {
                    on_failure();
                    anyhow::bail!("ALSA 连续错误超限（last: {err}）");
                }
            }
        }

        // v9e4：显式启动流。实测 MMAP commit 推进 appl_ptr 后内核并未按
        // start_threshold 自动启动（state 停留 Prepared、hw_ptr 冻结、delay=0），
        // 该路径自上线以来实际从未出声（此前"能播"仅指解码侧 position 前进）。
        // 每轮写成功后若仍处 Prepared（含 XRun prepare / 暂停恢复路径）即显式 start
        if matches!(pcm.state(), State::Prepared) {
            match pcm.start() {
                Ok(()) => {
                    info!(filled = buffer_frames.saturating_sub(avail as u64), "ALSA MMAP 显式 start：hw 自动启动未触发，已手动拉起");
                }
                Err(err) => {
                    if pcm.state() == State::XRun {
                        xrun_count += 1;
                        let _ = pcm.prepare();
                    } else {
                        contiguous_errors += 1;
                        warn!(error = %err, "ALSA MMAP 显式 start 失败");
                    }
                }
            }
        }
    }
}

/// 整数 PCM 写入循环。单位增益时直接把 FFmpeg 的 S32 容器样本映射到
/// ALSA 的 S32/S24/S16 容器；只有用户主动调节音量时才走 f32 增益分支。
#[allow(clippy::too_many_arguments)]
fn write_loop_integer(
    device: &str,
    requested_rate: Option<u32>,
    mut source: IntegerDecoderSource,
    volume: Arc<AtomicU32>,
    stopped: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    on_failure: OutputFailureCallback,
) -> Result<()> {
    let (pcm, format, rate, _channels, can_pause) = open_pcm(device, requested_rate)?;
    let (buffer_frames, _period_frames) = pcm.get_params()?;
    let watermark_ms = std::env::var("SPLAYER_ALSAMMAP_WATERMARK_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(HW_HIGH_WATERMARK_MS);
    let watermark_frames = watermark_ms.saturating_mul(rate as u64) / 1000;
    let mut hardware_paused = false;
    let mut pause_draining = false;
    let mut pause_silence_remaining = 0usize;
    let mut startup_silence_remaining = (rate as usize / 200).max(1);
    let mut contiguous_errors = 0_u32;
    let mut xrun_count = 0_u64;

    macro_rules! next_i32 {
        () => {{
            if startup_silence_remaining > 0 {
                startup_silence_remaining -= 1;
                0
            } else if stopped.load(Ordering::Acquire) {
                0
            } else {
            let sample = source.next().unwrap_or(0);
            let gain = f32::from_bits(volume.load(Ordering::Relaxed));
            if gain.to_bits() == 1.0_f32.to_bits() {
                sample
            } else {
                (sample as f32 * gain).round().clamp(i32::MIN as f32, i32::MAX as f32) as i32
            }
            }
        }};
    }

    loop {
        if stop.load(Ordering::Acquire) {
            drain_before_drop(&pcm, format, hardware_paused, rate);
            let _ = pcm.drop();
            info!(xrun_count, "ALSA MMAP i32 写循环停止（已排空旧 DMA 队列）");
            return Ok(());
        }
        let want_paused = paused.load(Ordering::Acquire);
        if want_paused && !hardware_paused && can_pause {
            if !pause_draining {
                pause_draining = true;
                pause_silence_remaining = (rate as usize / 200).max(1);
            }
            if pause_silence_remaining == 0 {
                let delay = pcm.status().map(|s| s.get_delay()).unwrap_or(0);
                if delay <= 0 {
                    if pcm.pause(true).is_ok() {
                        hardware_paused = true;
                        pause_draining = false;
                    }
                } else {
                    let _ = pcm.wait(Some(WAIT_CEILING_MS));
                    continue;
                }
            }
        } else if !want_paused {
            pause_draining = false;
            pause_silence_remaining = 0;
            if hardware_paused {
                if pcm.pause(false).is_ok() {
                    hardware_paused = false;
                }
            }
        } else if want_paused && !can_pause {
            let _ = pcm.drop();
            hardware_paused = true;
        }
        if hardware_paused {
            std::thread::sleep(std::time::Duration::from_millis(WAIT_CEILING_MS as u64));
            continue;
        }
        match pcm.state() {
            State::Running | State::Prepared | State::Paused => {}
            State::XRun => {
                xrun_count += 1;
                XRUN_TOTAL.fetch_add(1, Ordering::Release);
                pcm.prepare()?;
                continue;
            }
            State::Suspended => {
                if pcm.resume().is_err() {
                    pcm.prepare()?;
                }
                continue;
            }
            State::Open | State::Setup => {
                pcm.prepare()?;
                continue;
            }
            other => anyhow::bail!("ALSA 设备进入不可恢复状态: {other:?}"),
        }
        let avail = match pcm.avail_update() {
            Ok(avail) => avail as usize,
            Err(error) => {
                if pcm.state() == State::XRun {
                    xrun_count += 1;
                    XRUN_TOTAL.fetch_add(1, Ordering::Release);
                    pcm.prepare()?;
                    continue;
                }
                contiguous_errors += 1;
                if contiguous_errors >= MAX_CONTIGUOUS_ERRORS {
                    on_failure();
                    anyhow::bail!("ALSA i32 连续错误超限（last: {error}）");
                }
                continue;
            }
        };
        if avail == 0 {
            let _ = pcm.wait(Some(WAIT_CEILING_MS));
            continue;
        }
        let filled = buffer_frames.saturating_sub(avail as u64);
        let allowed = watermark_frames.saturating_sub(filled) as usize;
        let mut frames = if watermark_ms == 0 {
            avail.min(4096)
        } else {
            avail.min(4096).min(allowed)
        };
        if pause_draining {
            frames = frames.min(pause_silence_remaining);
        }
        if frames == 0 {
            std::thread::sleep(std::time::Duration::from_millis(
                (watermark_ms / 4).clamp(2, 20),
            ));
            continue;
        }
        let result = if format == Format::s16() {
            pcm.io_i16()?.mmap(frames, |buf: &mut [i16]| {
                for pair in buf.chunks_exact_mut(2) {
                    pair[0] = (next_i32!() >> 16) as i16;
                    pair[1] = (next_i32!() >> 16) as i16;
                }
                frames
            })
        } else if format == Format::s24() {
            pcm.io_i32_s24()?.mmap(frames, |buf: &mut [i32]| {
                for pair in buf.chunks_exact_mut(2) {
                    pair[0] = next_i32!() >> 8;
                    pair[1] = next_i32!() >> 8;
                }
                frames
            })
        } else {
            pcm.io_i32()?.mmap(frames, |buf: &mut [i32]| {
                for pair in buf.chunks_exact_mut(2) {
                    pair[0] = next_i32!();
                    pair[1] = next_i32!();
                }
                frames
            })
        };
        match result {
            Ok(_) => {
                contiguous_errors = 0;
                if pause_draining {
                    pause_silence_remaining = pause_silence_remaining.saturating_sub(frames);
                }
            }
            Err(error) => {
                if pcm.state() == State::XRun {
                    xrun_count += 1;
                    let _ = pcm.prepare();
                    continue;
                }
                contiguous_errors += 1;
                if contiguous_errors >= MAX_CONTIGUOUS_ERRORS {
                    on_failure();
                    anyhow::bail!("ALSA i32 连续错误超限（last: {error}）");
                }
            }
        }
        if matches!(pcm.state(), State::Prepared) {
            if let Err(error) = pcm.start() {
                if pcm.state() == State::XRun {
                    xrun_count += 1;
                    let _ = pcm.prepare();
                } else {
                    warn!(error = %error, "ALSA MMAP i32 显式 start 失败");
                }
            }
        }
    }
}

/// 枚举 ALSA hw 直出设备（"hw:X,Y"），供 devices 端点合成 alsammap 条目。
/// 枚举失败（权限/无声卡）返回空列表，best-effort 不构成错误
pub fn list_hw_devices() -> Vec<(String, String)> {
    let Some(iface) = std::ffi::CString::new("pcm").ok() else {
        return Vec::new();
    };
    let Ok(iter) = alsa::device_name::HintIter::new(None, &iface) else {
        return Vec::new();
    };
    iter.filter_map(|hint| {
        if matches!(hint.direction, Some(Direction::Capture)) {
            return None;
        }
        let name = hint.name?;
        if !name.starts_with("hw:") {
            return None;
        }
        let desc = hint
            .desc
            .unwrap_or_else(|| name.clone())
            .lines()
            .next()
            .unwrap_or(&name)
            .to_owned();
        Some((name, desc))
    })
    .collect()
}

// ==================== v10: ALSA 原生 DSD 直出（DSD_U32_BE） ====================

/// DSD_U32_BE 直出的重排缓冲上限（字节）：DSD512 立体声 ~10.6MB/s，128ms 块
const DSD_WIRE_BUF_BYTES: usize = 2 * 1024 * 1024;

/// DSD 线格式协商候选。速率映射：ALSA rate = DSD bit_rate / divisor
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DsdWireFmt {
    U32Be,
    U16Be,
    U8,
}

impl DsdWireFmt {
    fn alsa_format(self) -> alsa::pcm::Format {
        match self {
            DsdWireFmt::U32Be => Format::DSDU32BE,
            DsdWireFmt::U16Be => Format::DSDU16BE,
            DsdWireFmt::U8 => Format::DSDU8,
        }
    }

    /// 每声道每帧容器字节数
    fn bytes_per_ch(self) -> usize {
        match self {
            DsdWireFmt::U32Be => 4,
            DsdWireFmt::U16Be => 2,
            DsdWireFmt::U8 => 1,
        }
    }

    fn rate_divisor(self) -> u32 {
        match self {
            DsdWireFmt::U32Be => 32,
            DsdWireFmt::U16Be => 16,
            DsdWireFmt::U8 => 8,
        }
    }
}

/// 打开 DSD PCM，按 U32_BE(=/32) → U16_BE(=/16) → U8(=/8) 顺序协商，
/// 兼容不同 DAC 暴露的 DSD 容器宽度（XMOS 系多为 U32_BE，部分设备只给 U16/U8）。
/// 设备未暴露任何 DSD 格式（内核未开 DSD_RAW quirk 或硬件不支持）时报清晰错误
fn open_dsd_pcm(
    device: &str,
    dsd_bit_rate: u32,
    channels: u32,
) -> Result<(alsa::pcm::PCM, u32, DsdWireFmt)> {
    let pcm = alsa::pcm::PCM::new(device, Direction::Playback, false)
        .with_context(|| format!("打开 DSD 设备 {device} 失败"))?;
    for (wire, divisor) in [
        (DsdWireFmt::U32Be, 32_u32),
        (DsdWireFmt::U16Be, 16),
        (DsdWireFmt::U8, 8),
    ] {
        let alsa_rate = dsd_bit_rate / divisor;
        let mut h = alsa::pcm::HwParams::any(&pcm)?;
        h.set_access(Access::MMapInterleaved)?;
        h.set_channels(channels).map_err(|_| {
            anyhow!("DSD 设备 {device} 不支持 {channels} 声道（该设备 DSD altset 可能仅立体声，多声道 DSD 请用 Diretta 输出）")
        })?;
        if h.set_format(wire.alsa_format()).is_err() {
            continue; // 该容器宽度未暴露，试下一档
        }
        if h.set_rate(alsa_rate, ValueOr::Nearest).is_err() {
            continue;
        }
        let actual = match h.get_rate() {
            Ok(r) => r,
            Err(_) => continue,
        };
        if actual != alsa_rate {
            continue; // DSD 速率必须精确，此档不接受则试下一档
        }
        h.set_period_size_near(1024, ValueOr::Nearest)?;
        pcm.hw_params(&mut h)?;
        let rate = h.get_rate()?;
        drop(h); // HwParams 持有 pcm 引用，先释放再返回
        set_sw_params(&pcm)?;
        return Ok((pcm, rate, wire));
    }
    anyhow::bail!(
        "设备 {device} 不支持原生 DSD（DSD_U32_BE/U16_BE/U8 均未协商成功；         内核需设 quirk_flags=<VID>:<PID>:8000 并重枚举 USB 后重试，或改用 Diretta 输出）"
    )
}

/// ALSA 原生 DSD 输出流：DirectDsdReader 拉 raw DSD → L4R4 重排 → MMAP 直写。
/// 写循环骨架与 PCM 版一致（水位限速 + 显式 start + XRun 恢复）；
/// DSD 无音量语义（bit-perfect），暂停/停止送 0x69 静音垫
pub struct AlsaDsdStream {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    sample_rate: u32,
}

impl AlsaDsdStream {
    /// 打开 DSD 直出流。`reader` 由调用方打开（DSF/DFF/SACD 均可），
    /// `on_eof` 在流自然结束（read_block 返回 None）时回调（推进下一曲）
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        device: &str,
        mut reader: crate::direct_dsd::DirectDsdReader,
        on_eof: Box<dyn FnOnce() + Send>,
        on_failure: OutputFailureCallback,
    ) -> Result<Self> {
        let fmt = reader.format();
        let bit_rate = fmt.bit_rate;
        let channels = u32::from(fmt.channels);
        // 早期探测：设备 DSD 格式/声道数/速率任一不满足都在此报清晰错误
        let (pcm_probe, probe_rate, _wire) = open_dsd_pcm(device, bit_rate, channels)?;
        drop(pcm_probe);

        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let device_owned = device.to_string();
        let worker_stop = Arc::clone(&stop);
        let worker_paused = Arc::clone(&paused);
        let worker = std::thread::Builder::new()
            .name("alsa-dsd-output".into())
            .spawn(move || {
                boost_current_audio_thread("alsa-dsd-output");
                bind_current_thread_to_performance_cores("alsa-dsd-output");
                if let Err(error) =
                    dsd_write_loop(
                        &device_owned,
                        bit_rate,
                        &mut reader,
                        worker_stop,
                        worker_paused,
                        on_eof,
                        on_failure,
                    )
                {
                    warn!(error = %error, "ALSA DSD 写循环退出");
                }
            })
            .context("启动 ALSA DSD 写循环线程失败")?;

        Ok(Self { stop, paused, worker: Some(worker), sample_rate: probe_rate })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn play(&self) {
        self.paused.store(false, Ordering::Release);
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }
}

impl Drop for AlsaDsdStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// DSD 写循环：read_block 拉 raw DSD（L4R4 单元交织）→ 水位限速写 MMAP。
/// read_block 输出源位序（DSF=LSB-first / DFF=MSB-first），此处统一适配为
/// MSB-first 线序（kernel quirk bitrev=0 → USB 原生 DSD 期望 MSB-first；
/// Diretta 路径的线序由 SDK 协商，与本路径独立）。SPLAYER_ALSADSD_BITREV
/// =on/off 可强制反转/不反转（默认 auto 按源位序决定）。
#[allow(clippy::too_many_arguments)]
fn dsd_write_loop(
    device: &str,
    dsd_bit_rate: u32,
    reader: &mut crate::direct_dsd::DirectDsdReader,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    on_eof: Box<dyn FnOnce() + Send>,
    on_failure: OutputFailureCallback,
) -> Result<()> {
    let channels = usize::from(reader.format().channels);
    let (pcm, rate, wire_fmt) = open_dsd_pcm(device, dsd_bit_rate, channels as u32)?;
    let frame_bytes = channels * wire_fmt.bytes_per_ch();
    // DSD MMAP 的 hw 几何在本次流中固定，避免实时循环反复查询 ALSA 控制层。
    let (buffer_frames, _period_frames) = pcm.get_params()?;
    info!(?wire_fmt, channels, "ALSA DSD 协商完成");
    let mut contiguous_errors: u32 = 0;
    let mut xrun_count: u64 = 0;
    // 源侧缓冲：read_block 输出进 staging，MMAP 写从 staging 消费
    let mut staging: Vec<u8> = Vec::with_capacity(DSD_WIRE_BUF_BYTES);
    // 复用解码读取块，避免 DSD 写循环每次补充 staging 都重新分配 256 KiB。
    // 该线程持续运行整个曲目，循环内分配会造成 allocator 抖动并增加长时间播放的
    // 内存峰值；read_block 本身只写入前 n 字节，因此复用不会携带旧数据。
    let mut read_chunk = vec![0u8; 256 * 1024];
    let mut staging_pos: usize = 0;
    let mut eof_signaled = false;
    // 位序适配：目标线序 MSB-first（kernel quirk bitrev=0）
    let need_bitrev = match std::env::var("SPLAYER_ALSADSD_BITREV").as_deref() {
        Ok("on") | Ok("1") => true,
        Ok("off") | Ok("0") => false,
        _ => reader.format().bit_order == crate::direct_dsd::DirectDsdBitOrder::LsbFirst,
    };
    // 与 PCM 写循环一致，只在启动时读取一次水位配置。
    let watermark_ms = std::env::var("SPLAYER_ALSAMMAP_WATERMARK_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(HW_HIGH_WATERMARK_MS);
    let watermark_frames = watermark_ms.saturating_mul(rate as u64) / 1000;
    info!(need_bitrev, "ALSA DSD 位序适配（目标线序 MSB-first）");

    loop {
        if stop.load(Ordering::Acquire) {
            let _ = pcm.drop();
            info!(xrun_count, "ALSA DSD 写循环停止");
            return Ok(());
        }
        match pcm.state() {
            State::Running | State::Prepared | State::Paused => {}
            State::XRun => {
                xrun_count += 1;
                XRUN_TOTAL.fetch_add(1, Ordering::Release);
                pcm.prepare()?;
                continue;
            }
            State::Suspended => {
                if pcm.resume().is_err() {
                    pcm.prepare()?;
                }
                continue;
            }
            State::Open | State::Setup => {
                pcm.prepare()?;
                continue;
            }
            other => anyhow::bail!("ALSA DSD 设备进入不可恢复状态: {other:?}"),
        }

        let avail = match pcm.avail_update() {
            Ok(avail) => avail as usize,
            Err(err) => {
                if pcm.state() == State::XRun {
                    xrun_count += 1;
                    XRUN_TOTAL.fetch_add(1, Ordering::Release);
                    pcm.prepare()?;
                    continue;
                }
                contiguous_errors += 1;
                if contiguous_errors >= MAX_CONTIGUOUS_ERRORS {
                    on_failure();
                    anyhow::bail!("ALSA DSD 连续错误超限（last: {err}）");
                }
                continue;
            }
        };
        if avail == 0 {
            let _ = pcm.wait(Some(WAIT_CEILING_MS));
            continue;
        }

        // 水位限速（同 PCM 版）：DSD 预填上限 200ms
        let filled = buffer_frames.saturating_sub(avail as u64);
        let allowed = watermark_frames.saturating_sub(filled) as usize;
        let frames = if watermark_ms == 0 {
            avail.min(4096)
        } else {
            avail.min(4096).min(allowed)
        };
        if frames == 0 {
            // 水位满：sleep 等消耗（不能 pcm.wait——条件不等价会忙转，v9e 教训）
            let drain_ms = (watermark_ms / 4).clamp(2, 20);
            std::thread::sleep(std::time::Duration::from_millis(drain_ms));
            continue;
        }

        // 源不足时拉一批（read_block 输出 L4R4 单元流）
        let need_bytes = frames * frame_bytes;
        // 暂停：不读源，持续垫 0x69 静音（0x69 翻转密度最高 ≈ 零电荷，
        // DAC 端等效静音；保持 DMA 供流避免欠载噪声，恢复播放无缝）
        if paused.load(Ordering::Acquire) {
            let gap = need_bytes.saturating_sub(staging.len() - staging_pos);
            staging.extend(std::iter::repeat(0x69).take(gap));
        }
        while staging.len() - staging_pos < need_bytes && !eof_signaled {
            match reader.read_block(&mut read_chunk) {
                Ok(Some(n)) => {
                    if need_bitrev {
                        for byte in &mut read_chunk[..n] {
                            *byte = byte.reverse_bits();
                        }
                    }
                    staging.extend_from_slice(&read_chunk[..n]);
                }
                Ok(None) => {
                    eof_signaled = true;
                    break;
                }
                Err(err) => {
                    warn!(error = %err, "DSD 读取失败，垫静音");
                    staging.extend(std::iter::repeat(0x69).take(64 * 1024));
                    break;
                }
            }
        }
        // staging 前段已消费部分定期压实，防无限增长
        if staging_pos > DSD_WIRE_BUF_BYTES {
            staging.drain(..staging_pos);
            staging_pos = 0;
        }

        let wire = std::cmp::min(frames, (staging.len() - staging_pos) / frame_bytes);
        // alsa-rs 的 io_checked 以「协商格式 == 类型默认格式」严格等值校验，DSD
        // 三档格式均无 IoFormat 映射，io_i32/io_i16/io_u8 一律 unsupported("io_xx")。
        // 官方逃生口 io_bytes()（文档：unusual format 用）：免检、字节粒度 mmap，
        // IO::mmap 按 frames_to_bytes 换算，对 U32/U16/U8 三档统一适用
        let result = pcm.io_bytes().mmap(frames, |buf: &mut [u8]| {
            let src = &staging[staging_pos..];
            let copy = std::cmp::min(buf.len(), src.len() / frame_bytes * frame_bytes);
            buf[..copy].copy_from_slice(&src[..copy]);
            copy / frame_bytes
        });
        staging_pos += (staging.len() - staging_pos).min(wire * frame_bytes);
        match result {
            Ok(n) => {
                contiguous_errors = 0;
                let _ = n;
            }
            Err(err) => {
                if pcm.state() == State::XRun {
                    xrun_count += 1;
                    let _ = pcm.prepare();
                    continue;
                }
                contiguous_errors += 1;
                if contiguous_errors >= MAX_CONTIGUOUS_ERRORS {
                    on_failure();
                    anyhow::bail!("ALSA DSD 连续错误超限（last: {err}）");
                }
            }
        }

        // 显式 start（v9e4 教训：commit 后内核不一定自动启动）
        if matches!(pcm.state(), State::Prepared) {
            match pcm.start() {
                Ok(()) => info!("ALSA DSD 显式 start：已手动拉起"),
                Err(err) => {
                    if pcm.state() == State::XRun {
                        xrun_count += 1;
                        let _ = pcm.prepare();
                    } else {
                        contiguous_errors += 1;
                        warn!(error = %err, "ALSA DSD 显式 start 失败");
                    }
                }
            }
        }

        // 源耗尽且 staging 已消费完：信号 EOF，等待 flush 完成后停止
        if eof_signaled && staging.len() - staging_pos < frame_bytes {
            // 等 hw 消化完剩余在途数据（上限 2s）
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if let Ok(s) = pcm.status() {
                    if s.get_delay() <= 0 {
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            on_eof();
            let _ = pcm.drop();
            info!(xrun_count, "ALSA DSD 曲目播完");
            return Ok(());
        }
    }
}

fn to_i16(v: f32) -> i16 {
    (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

fn to_i32(v: f32) -> i32 {
    (v * 2_147_483_648.0)
        .round()
        .clamp(-2_147_483_648.0, 2_147_483_647.0) as i32
}

/// S24LE：24 有效位居 32 位容器高位
fn to_s24_container(v: f32) -> i32 {
    (((v * 8_388_608.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32) << 8) as i32
}

#[cfg(test)]
mod tests {
    use super::{to_i16, to_i32, to_s24_container};

    #[test]
    fn f32_conversions_are_exact_for_integer_sources_at_unit_gain() {
        // ffmpeg 解码 s16 → f32 = raw / 32768：乘回 32768 后必须逐样本还原
        for raw in [-32768_i16, -1, 0, 1, 16383, 32767] {
            assert_eq!(to_i16(raw as f32 / 32768.0), raw, "raw={raw}");
        }
        for raw in [-8_388_608_i32, -1, 0, 1, 8_388_607] {
            assert_eq!(
                to_s24_container(raw as f32 / 8_388_608.0) >> 8,
                raw,
                "raw={raw}"
            );
        }
        // 正向满幅是 clamp 上界（整数解码不会产生 +1.0）
        assert_eq!(to_i16(1.0), 32767);
        assert_eq!(to_i16(-1.0), -32768);
        assert_eq!(to_i16(2.0), 32767);
        assert_eq!(to_i32(1.0), 2_147_483_647);
        assert_eq!(to_i32(-1.0), -2_147_483_648);
        // 0 映射到 0（数字静音）
        assert_eq!(to_i16(0.0), 0);
        assert_eq!(to_i32(0.0), 0);
    }
}
