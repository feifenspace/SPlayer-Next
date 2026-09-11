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
use crate::source::DecoderSource;

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

/// 直出采样格式优先级（容器精度从高到低）
const FORMAT_PRIORITY: [Format; 3] = [Format::s32(), Format::s24(), Format::s16()];

/// ALSA MMAP 直出采样率安全上限。正常 PCM 最高 768k；DSD 解码 PCM 最低
/// DSD64 = 2.8224 MHz。高于 1 MHz 的请求只可能来自 DSD 源解码——直接拒绝，
/// 防止 USB isoc 超高带宽请求拖垮 snd-usb-audio/xHCI 导致整机死机
/// （2026-09-11 播 DSD 经 ALSA MMAP 整机 hang 的防护）。
const MAX_SAFE_SAMPLE_RATE: u32 = 1_000_000;

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
    let (pcm, format, _rate, _channels, can_pause) = open_pcm(device, requested_rate)?;
    let mut hardware_paused = false;
    let mut contiguous_errors: u32 = 0;
    let mut xrun_count: u64 = 0;

    // 暂停/停止时送数字静音：f32 静音帧（写入时按格式转换）
    macro_rules! fill_frame {
        ($frame:expr) => {{
            let gain = f32::from_bits(volume.load(Ordering::Relaxed));
            if stopped.load(Ordering::Acquire) || paused.load(Ordering::Acquire) {
                $frame[0] = 0.0;
                $frame[1] = 0.0;
            } else {
                $frame[0] = source.next().map(|v| v * gain).unwrap_or(0.0);
                $frame[1] = source.next().map(|v| v * gain).unwrap_or(0.0);
            }
        }};
    }

    loop {
        if stop.load(Ordering::Acquire) {
            let _ = pcm.drop();
            info!(xrun_count, "ALSA MMAP 写循环停止");
            return Ok(());
        }

        let want_paused = paused.load(Ordering::Acquire);
        if want_paused != hardware_paused {
            if can_pause {
                if pcm.pause(want_paused).is_ok() {
                    hardware_paused = want_paused;
                }
            } else if want_paused {
                let _ = pcm.drop();
                hardware_paused = true;
            } else {
                pcm.prepare()?;
                hardware_paused = false;
            }
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

        let frames = avail.min(4096);
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
            Ok(_) => contiguous_errors = 0,
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
    }
}

/// 整数源解码的 f32 归一化是除以 2^(n-1)，回写必须乘同系数：
/// 乘 32767/8388607 会引入 0.003% 失真，破坏位纯真（§六.1 回录哈希不过）
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
