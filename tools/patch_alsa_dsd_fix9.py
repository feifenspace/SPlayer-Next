"""v10 fix9: ALSA DSD 兼容层 —— 格式三档自适应 + 声道通用化 + enumerate bug 修复.

在 gaoda 仓库根目录执行: python3 tools/patch_alsa_dsd_fix9.py

1. open_dsd_pcm: U32_BE(=/32) -> U16_BE(=/16) -> U8(=/8) 逐档协商,
   任一档成功即用; 全部失败报清晰错误(quirk 指引 + 建议 Diretta).
2. 声道数不再写死 2: 按 DirectDsdFormat.channels 协商, frame_bytes=channels*容器宽.
3. mmap 写循环按协商结果走 io_i32/io_i16/io_u8 + 对应字节宽.
4. 修 fix8 的 (slot, w) enumerate 参数顺序颠倒.
"""
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SINK = "native/audio-engine-core/src/alsa_mmap_sink.rs"

def patch(old: str, new: str, label: str) -> None:
    p = ROOT / SINK
    text = p.read_text(encoding="utf-8")
    if new in text:
        print(f"[SKIP] {label}: 已应用")
        return
    if old not in text:
        print(f"[FAIL] {label}: old 片段未找到")
        sys.exit(1)
    if text.count(old) != 1:
        print(f"[FAIL] {label}: old 片段出现 {text.count(old)} 次，需唯一")
        sys.exit(1)
    p.write_text(text.replace(old, new, 1), encoding="utf-8")
    print(f"[OK] {label}")

# 1) open_dsd_pcm 整体重写为三档协商
patch(
    """/// 打开 DSD PCM 并协商 DSD_U32_BE + rate = DSD bit_rate / 32。
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
    let rate = h.get_rate()?;
    drop(h); // HwParams 持有 pcm 引用，先释放再返回
    set_sw_params(&pcm)?;
    Ok((pcm, rate))
}""",
    """/// DSD 线格式协商候选。速率映射：ALSA rate = DSD bit_rate / divisor
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
    bail!(
        "设备 {device} 不支持原生 DSD（DSD_U32_BE/U16_BE/U8 均未协商成功；\
         内核需设 quirk_flags=<VID>:<PID>:8000 并重枚举 USB 后重试，或改用 Diretta 输出）"
    )
}""",
    "open_dsd_pcm 三档协商重写",
)

# 2) open(): 声道通用化 + 早期探测
patch(
    """        let fmt = reader.format();
        let bit_rate = fmt.bit_rate;
        ensure!(fmt.channels == 2, "ALSA DSD 直出仅支持立体声（当前 {} 声）", fmt.channels);
        let (pcm_probe, rate) = open_dsd_pcm(device, bit_rate)?;
        drop(pcm_probe);""",
    """        let fmt = reader.format();
        let bit_rate = fmt.bit_rate;
        let channels = u32::from(fmt.channels);
        // 早期探测：设备 DSD 格式/声道数/速率任一不满足都在此报清晰错误
        let (pcm_probe, _rate, _wire) = open_dsd_pcm(device, bit_rate, channels)?;
        drop(pcm_probe);""",
    "open() 声道通用化 + 早期探测",
)

# 3) dsd_write_loop 开头: 协商 + frame_bytes
patch(
    """    let (pcm, rate) = open_dsd_pcm(device, dsd_bit_rate)?;
    let mut contiguous_errors: u32 = 0;""",
    """    let channels = usize::from(reader.format().channels);
    let (pcm, rate, wire_fmt) = open_dsd_pcm(device, dsd_bit_rate, channels as u32)?;
    let frame_bytes = channels * wire_fmt.bytes_per_ch();
    info!(?wire_fmt, channels, "ALSA DSD 协商完成");
    let mut contiguous_errors: u32 = 0;""",
    "dsd_write_loop 协商 + frame_bytes",
)

# 4) need_bytes 通用化
patch(
    "        let need_bytes = frames * 8;",
    "        let need_bytes = frames * frame_bytes;",
    "need_bytes 按 frame_bytes",
)

# 5) mmap 块: 修 enumerate 顺序 + 三档 IO
patch(
    """        let wire = std::cmp::min(frames, (staging.len() - staging_pos) / 8);
        // DSD_U32_BE 仅暴露 32-bit IO（io_u8 会 EOPNOTSUPP）；x86 LE 上
        // from_le_bytes 落内存即保持字节流顺序 = DMA 线序（BE 格式内存布局）
        let result = pcm.io_i32()?.mmap(frames, |buf: &mut [i32]| {
            let src = &staging[staging_pos..];
            let words = std::cmp::min(buf.len(), src.len() / 4);
            for (slot, w) in buf.iter_mut().enumerate().take(words) {
                let off = w * 4;
                *slot = i32::from_le_bytes([src[off], src[off + 1], src[off + 2], src[off + 3]]);
            }
            words / 2
        });
        staging_pos += (staging.len() - staging_pos).min(wire * 8);""",
    """        let wire = std::cmp::min(frames, (staging.len() - staging_pos) / frame_bytes);
        // 各容器宽度只暴露对应宽度的 IO（U32_BE 用 io_u8 会 EOPNOTSUPP）；
        // x86 LE 上 from_le_bytes 落内存即保持字节流顺序 = DMA 线序（BE 格式内存布局）
        let result = match wire_fmt {
            DsdWireFmt::U32Be => pcm.io_i32()?.mmap(frames, |buf: &mut [i32]| {
                let src = &staging[staging_pos..];
                let words = std::cmp::min(buf.len(), src.len() / 4);
                for (w, slot) in buf.iter_mut().enumerate().take(words) {
                    let off = w * 4;
                    *slot = i32::from_le_bytes([src[off], src[off + 1], src[off + 2], src[off + 3]]);
                }
                words / channels
            }),
            DsdWireFmt::U16Be => pcm.io_i16()?.mmap(frames, |buf: &mut [i16]| {
                let src = &staging[staging_pos..];
                let units = std::cmp::min(buf.len(), src.len() / 2);
                for (w, slot) in buf.iter_mut().enumerate().take(units) {
                    let off = w * 2;
                    *slot = i16::from_le_bytes([src[off], src[off + 1]]);
                }
                units / channels
            }),
            DsdWireFmt::U8 => pcm.io_u8()?.mmap(frames, |buf: &mut [u8]| {
                let src = &staging[staging_pos..];
                let copy = std::cmp::min(buf.len(), src.len());
                buf[..copy].copy_from_slice(&src[..copy]);
                copy / channels
            }),
        };
        staging_pos += (staging.len() - staging_pos).min(wire * frame_bytes);""",
    "mmap 三档 IO + enumerate 修复",
)

# 6) EOF 剩余检查通用化
patch(
    "        if eof_signaled && staging.len() - staging_pos < 8 {",
    "        if eof_signaled && staging.len() - staging_pos < frame_bytes {",
    "EOF 剩余检查按 frame_bytes",
)

print("patch_alsa_dsd_fix9: all done")
