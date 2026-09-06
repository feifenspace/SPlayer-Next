//! DSD → DoP WAV 物化转换（B6.2）。
//!
//! 把 DSF/DFF/SACD-ISO 源整曲转成 DoP v1.1 的 32-bit PCM WAV（内存内），
//! 使仅支持 PCM 的链路（含 Diretta PCM Direct、alsammap）得以承载 DSD。
//! 适用场景：原生 DSD 通道不可用（如 v1 级 CPU 无法执行 SDK v2 库指令）。

use std::path::Path;

use anyhow::{ensure, Context, Result};

use super::dop_pack::DopStreamPacker;
use crate::direct_dsd::{DirectDsdBitOrder, DirectDsdReader};
use crate::dsd::reverse_byte;
use crate::ram_buffer::RamTrackBuffer;

/// 每次打包的批大小（帧）
const PACK_BATCH_FRAMES: usize = 65536;

/// 把 DSD 源整曲转换为 DoP WAV 并物化进 RAM 缓冲。
/// PCM 采样率 = DSD 比特率 / 16；DoP marker 相位跨整曲连续。
pub fn convert_dsd_to_dop_ram(source: &str, max_bytes: usize) -> Result<RamTrackBuffer> {
    let mut reader = DirectDsdReader::open_local(Path::new(source))
        .with_context(|| format!("打开 DSD 源失败: {source}"))?;
    let format = reader.format();
    let channels = usize::from(format.channels);
    ensure!(
        channels == 2,
        "DoP 物化当前仅支持立体声（实际 {channels} 声道）"
    );
    ensure!(
        format.bit_rate % 16 == 0,
        "DSD 比特率 {} 无法对齐 DoP（需为 16 的倍数）",
        format.bit_rate
    );
    let pcm_rate = format.bit_rate / 16;

    let duration_secs = reader.duration_secs();
    ensure!(duration_secs > 0.0, "DSD 源时长无效");
    let data_size = (duration_secs * pcm_rate as f64 * channels as f64 * 4.0).ceil() as usize;
    ensure!(
        data_size + 44 <= max_bytes,
        "DoP 物化需要 {data_size} 字节，超出上限 {max_bytes}（曲目过长）"
    );

    let msb_source = format.bit_order == DirectDsdBitOrder::MsbFirst;
    let mut buf = RamTrackBuffer::with_capacity(data_size + 44, max_bytes);
    write_wav_header(&mut buf, pcm_rate, channels, data_size)?;

    let mut packer = DopStreamPacker::new();
    let mut in_buf = vec![0u8; reader.max_output_len()];
    let mut carry = [0u8; 2];
    let mut has_carry = false;
    let mut paired: Vec<u8> = Vec::with_capacity(262144);
    let mut out = vec![0i32; PACK_BATCH_FRAMES * channels];
    let mut written = 44usize;

    loop {
        match reader.read_block(&mut in_buf)? {
            Some(n) => {
                if !msb_source {
                    // DSF 文件位序 LSB-first：DoP 要求 MSB-first
                    for byte in &mut in_buf[..n] {
                        *byte = reverse_byte(*byte);
                    }
                }
                let mut src = &in_buf[..n];
                if has_carry {
                    ensure!(src.len() >= 2, "DSD 块在字节时间中间截断");
                    paired.extend_from_slice(&carry);
                    paired.extend_from_slice(&src[..2]);
                    src = &src[2..];
                    has_carry = false;
                }
                let whole = src.len() / 4 * 4;
                // 4 字节组 [c0_t0, c1_t0, c0_t1, c1_t1] → [c0_t0, c0_t1, c1_t0, c1_t1]
                for g in src[..whole].chunks_exact(4) {
                    paired.extend_from_slice(&[g[0], g[2], g[1], g[3]]);
                }
                if src.len() - whole >= 2 {
                    carry.copy_from_slice(&src[whole..whole + 2]);
                    has_carry = true;
                }
                if paired.len() >= PACK_BATCH_FRAMES * channels * 2 {
                    flush(
                        &mut packer,
                        &mut paired,
                        channels,
                        &mut out,
                        &mut buf,
                        &mut written,
                    )?;
                }
            }
            None => break,
        }
    }
    flush(
        &mut packer,
        &mut paired,
        channels,
        &mut out,
        &mut buf,
        &mut written,
    )?;
    // 补零到 header 声明的 data_size（≤4 字节取整误差）
    while written < data_size + 44 {
        written += buf.append(&[0u8; 4]);
    }
    buf.mark_fully_loaded();
    // mlock 失败（EPERM）降级为普通内存，lock_memory 内部已带日志
    let _ = buf.lock_memory();
    tracing::info!(
        source = %source,
        pcm_rate,
        channels,
        data_size,
        "DSD 已转换为 DoP WAV 并物化进 RAM"
    );
    Ok(buf)
}

fn flush(
    packer: &mut DopStreamPacker,
    paired: &mut Vec<u8>,
    channels: usize,
    out: &mut [i32],
    buf: &mut RamTrackBuffer,
    written: &mut usize,
) -> Result<()> {
    let frame_bytes = channels * 2;
    let frames = paired.len() / frame_bytes;
    let mut done = 0usize;
    while done < frames {
        let batch = (frames - done).min(out.len() / channels);
        packer.pack_interleaved(
            &paired[done * frame_bytes..(done + batch) * frame_bytes],
            channels,
            &mut out[..batch * channels],
        );
        let mut bytes = Vec::with_capacity(batch * channels * 4);
        for sample in &out[..batch * channels] {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let n = buf.append(&bytes);
        ensure!(n == bytes.len(), "DoP WAV 写入截断");
        *written += n;
        done += batch;
    }
    paired.clear();
    Ok(())
}

fn write_wav_header(
    buf: &mut RamTrackBuffer,
    rate: u32,
    channels: usize,
    data_size: usize,
) -> Result<()> {
    let mut hdr = Vec::with_capacity(44);
    hdr.extend_from_slice(b"RIFF");
    hdr.extend_from_slice(&(36 + data_size as u32).to_le_bytes());
    hdr.extend_from_slice(b"WAVEfmt ");
    hdr.extend_from_slice(&16u32.to_le_bytes());
    hdr.extend_from_slice(&1u16.to_le_bytes());
    hdr.extend_from_slice(&(channels as u16).to_le_bytes());
    hdr.extend_from_slice(&(rate as u32).to_le_bytes());
    hdr.extend_from_slice(&(rate * channels as u32 * 4).to_le_bytes());
    hdr.extend_from_slice(&((channels * 4) as u16).to_le_bytes());
    hdr.extend_from_slice(&32u16.to_le_bytes());
    hdr.extend_from_slice(b"data");
    hdr.extend_from_slice(&(data_size as u32).to_le_bytes());
    let n = buf.append(&hdr);
    ensure!(n == hdr.len(), "写入 DoP WAV 头失败");
    Ok(())
}
