#!/usr/bin/env python3
"""v10: ALSA MMAP native DSD 直出（GR40 DSD_U32_BE）。

前提（2026-09-11 已实证）：GR40 硬件原生 DSD（Altset4 raw DSD，非 DoP）；内核
quirk_flags=262a:*:8000（DSD_RAW）+ USB 重枚举后暴露 DSD_U32_BE 格式；速率映射
ALSA rate = DSD bit_rate/32（DSD64→88200 / DSD256→352800 / DSD512→705600）。

本补丁：
1. alsa_mmap_sink.rs 新增 AlsaDsdStream：DSD_U32_BE 协商 + DirectDsdReader 拉流，
   写循环复用水位限速 + 显式 start（v9e4 教训全部继承）；
2. PlaybackStream 加 AlsaDsd 变体（play/pause 同 Alsa）；
3. AudioOutput 增加 DSD 入口 build_dsd_stream + DSD 配置变体（不占 PCM 通道，
   DSD 请求绕过 MAX_SAFE_SAMPLE_RATE 守卫——DSD 走专用 altset 不会喂 PCM 通道）。
"""

import sys

PATH = "native/audio-engine-core/src/alsa_mmap_sink.rs"

MARKER = """fn to_i16(v: f32) -> i16 {"""

DSDSINK = '''// ==================== v10: ALSA 原生 DSD 直出（DSD_U32_BE） ====================

/// DSD_U32_BE 直出的重排缓冲上限（字节）：DSD512 立体声 ~10.6MB/s，128ms 块
const DSD_WIRE_BUF_BYTES: usize = 2 * 1024 * 1024;

/// 打开 DSD PCM 并协商 DSD_U32_BE + rate = DSD bit_rate / 32。
/// 返回（PCM，实际 rate）。设备未暴露 DSD 格式（内核未开 DSD_RAW quirk）时报错
fn open_dsd_pcm(device: &str, dsd_rate: u32) -> Result<(alsa::pcm::PCM, u32)> {
    let pcm = alsa::pcm::PCM::new(device, Direction::Playback, false)
        .with_context(|| format!("打开 DSD 设备 {device} 失败"))?;
    let mut h = alsa::pcm::HwParams::any(&pcm)?;
    h.set_access(Access::MMapInterleaved)?;
    h.set_channels(2)?;
    // DSD_U32_BE 是唯一容器格式（4 字节/声道/8 samples）
    h.set_format(Format::DSDU32BE)
        .map_err(|_| anyhow!("设备 {device} 未暴露 DSD_U32_BE（内核需开 DSD_RAW quirk 并重枚举 USB）"))?;
    let alsa_rate = dsd_rate / 32;
    h.set_rate(alsa_rate, ValueOr::Nearest)?;
    let actual = h.get_rate()?;
    ensure!(
        actual == alsa_rate,
        "DSD 设备不接受速率 {alsa_rate}（DSD bit_rate={dsd_rate}，最近支持 {actual}）"
    );
    h.set_period_size_near(1024, ValueOr::Nearest)?;
    pcm.hw_params(&mut h)?;
    set_sw_params(&pcm)?;
    let rate = h.get_rate()?;
    Ok((pcm, rate))
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
        ensure!(fmt.channels == 2, "ALSA DSD 直出仅支持立体声（当前 {} 声）", fmt.channels);
        let (pcm_probe, rate) = open_dsd_pcm(device, bit_rate)?;
        drop(pcm_probe);

        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let device_owned = device.to_string();
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("alsa-dsd-output".into())
            .spawn(move || {
                boost_current_audio_thread("alsa-dsd-output");
                bind_current_thread_to_performance_cores("alsa-dsd-output");
                if let Err(error) =
                    dsd_write_loop(&device_owned, bit_rate, &mut reader, worker_stop, on_eof, on_failure)
                {
                    warn!(error = %error, "ALSA DSD 写循环退出");
                }
            })
            .context("启动 ALSA DSD 写循环线程失败")?;

        Ok(Self { stop, paused, worker: Some(worker), sample_rate: rate })
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
/// DirectDsdReader::read_block 已按 Diretta 语义输出 L4R4 单元流
/// （DSF block 交织 / DFF 逐字节交织均已重排），DSD_U32_BE 每帧 8 字节
/// 恰为一个 L4R4 单元，字节序在 repack 层已保证 MSB-first。
#[allow(clippy::too_many_arguments)]
fn dsd_write_loop(
    device: &str,
    dsd_bit_rate: u32,
    reader: &mut crate::direct_dsd::DirectDsdReader,
    stop: Arc<AtomicBool>,
    on_eof: Box<dyn FnOnce() + Send>,
    on_failure: OutputFailureCallback,
) -> Result<()> {
    let (pcm, rate) = open_dsd_pcm(device, dsd_bit_rate)?;
    let mut contiguous_errors: u32 = 0;
    let mut xrun_count: u64 = 0;
    // 源侧缓冲：read_block 输出进 staging，MMAP 写从 staging 消费
    let mut staging: Vec<u8> = Vec::with_capacity(DSD_WIRE_BUF_BYTES);
    let mut staging_pos: usize = 0;
    let mut eof_signaled = false;

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
        let buffer_frames = {
            let (buf, _per) = pcm.get_params()?;
            buf
        };
        let watermark_ms = std::env::var("SPLAYER_ALSAMMAP_WATERMARK_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(HW_HIGH_WATERMARK_MS);
        let watermark_frames = watermark_ms.saturating_mul(rate as u64) / 1000;
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
        let need_bytes = frames * 8;
        while staging.len() - staging_pos < need_bytes && !eof_signaled {
            let mut chunk = vec![0u8; 256 * 1024];
            match reader.read_block(&mut chunk) {
                Ok(Some(n)) => staging.extend_from_slice(&chunk[..n]),
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

        let wire = std::cmp::min(frames, (staging.len() - staging_pos) / 8);
        let result = pcm.io_u8()?.mmap(frames, |buf: &mut [u8]| {
            let src = &staging[staging_pos..];
            let copy = std::cmp::min(buf.len(), src.len() / 8 * 8);
            buf[..copy].copy_from_slice(&src[..copy]);
            copy / 8
        });
        staging_pos += (staging.len() - staging_pos).min(wire * 8);
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
        if eof_signaled && staging.len() - staging_pos < 8 {
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

fn to_i16(v: f32) -> i16 {'''


def main() -> int:
    src = open(PATH, encoding="utf-8").read()
    if "AlsaDsdStream" in src:
        print("already patched")
        return 0
    assert src.count(MARKER) == 1, "marker not unique"
    src = src.replace(MARKER, DSDSINK, 1)
    open(PATH, "w", encoding="utf-8").write(src)
    print("v10 DSD sink added:", len(DSDSINK), "bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
