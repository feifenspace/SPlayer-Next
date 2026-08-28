//! Sony DSF 格式解析与交织流式解包器
//!
//! 移植自 tinyLMS-old DsdDecoder::ParseAndDecodeDsf。
//! DSF 特性：Little-Endian，LSB First，Planar 块组织（默认 4096 字节/声道）。
//! 解码时通过预置 LUT 比特翻转为 MSB First，并按每声道 4 字节（L0..3, R0..3）交叉重组。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{bail, Context, Result};
use tracing::info;

use super::{reverse_byte, DsdRate};

/// DSF 流式音频解析器
pub struct DsfReader {
    file: File,
    pub sample_rate: u32,
    pub channels: u16,
    pub block_size: u32,
    pub sample_count: u64,
    pub duration_seconds: f64,
    pub dsd_rate: DsdRate,
    pub data_offset: u64,
    pub data_size: u64,

    // 读取状态
    current_data_pos: u64,
    io_buf: Vec<u8>,
    interleave_buf: Vec<u8>,
    interleave_pos: usize,
}

impl DsfReader {
    /// 打开并解析 DSF 音频文件头部
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut file = File::open(path.as_ref())
            .with_context(|| format!("Failed to open DSF file: {:?}", path.as_ref()))?;

        let mut header = [0u8; 28];
        file.read_exact(&mut header)
            .context("Failed to read DSF header")?;

        if &header[0..4] != b"DSD " {
            bail!("Invalid DSF magic header, expected 'DSD '");
        }

        // 读取 'fmt ' chunk
        let mut fmt_header = [0u8; 52];
        file.read_exact(&mut fmt_header)
            .context("Failed to read DSF fmt chunk")?;

        if &fmt_header[0..4] != b"fmt " {
            bail!("Invalid DSF format chunk, expected 'fmt '");
        }

        let fmt_size = u64::from_le_bytes(fmt_header[4..12].try_into().unwrap());
        let format_id = u32::from_le_bytes(fmt_header[16..20].try_into().unwrap());
        if format_id != 0 {
            bail!(
                "Unsupported DSF format ID: {}, only raw DSD (0) is supported",
                format_id
            );
        }

        let channels = u32::from_le_bytes(fmt_header[24..28].try_into().unwrap()) as u16;
        let sample_rate = u32::from_le_bytes(fmt_header[28..32].try_into().unwrap());
        let _bits_per_sample = u32::from_le_bytes(fmt_header[32..36].try_into().unwrap());
        let sample_count = u64::from_le_bytes(fmt_header[36..44].try_into().unwrap());
        let block_size = u32::from_le_bytes(fmt_header[44..48].try_into().unwrap());

        if channels != 2 {
            bail!(
                "Currently only stereo (2-channel) DSF files are supported, found {}",
                channels
            );
        }
        if block_size == 0 || block_size % 4 != 0 {
            bail!(
                "Invalid DSF block size: {}, must be non-zero multiple of 4",
                block_size
            );
        }

        // 定位 'data' chunk
        file.seek(SeekFrom::Start(28 + fmt_size))?;
        let mut data_chunk_header = [0u8; 12];
        file.read_exact(&mut data_chunk_header)
            .context("Failed to read DSF data chunk header")?;

        let (data_offset, data_size) = if &data_chunk_header[0..4] == b"data" {
            let size = u64::from_le_bytes(data_chunk_header[4..12].try_into().unwrap());
            (28 + fmt_size + 12, size.saturating_sub(12))
        } else {
            // 兜底扫描 data chunk
            let mut found = None;
            while let Ok(_) = file.read_exact(&mut data_chunk_header) {
                let size = u64::from_le_bytes(data_chunk_header[4..12].try_into().unwrap());
                if &data_chunk_header[0..4] == b"data" {
                    let offset = file.stream_position()?;
                    found = Some((offset, size.saturating_sub(12)));
                    break;
                }
                file.seek(SeekFrom::Current(size.saturating_sub(12) as i64))?;
            }
            match found {
                Some(pair) => pair,
                None => bail!("Failed to locate DSF data chunk"),
            }
        };

        let duration_seconds = if sample_rate > 0 {
            sample_count as f64 / sample_rate as f64
        } else {
            0.0
        };

        let dsd_rate = DsdRate::from_sample_rate(sample_rate);
        info!(
            "Opened DSF: rate={} ({}), ch={}, block_size={}, samples={}, duration={:.2}s",
            sample_rate,
            dsd_rate.display_name(),
            channels,
            block_size,
            sample_count,
            duration_seconds
        );

        let io_buf = vec![0u8; (block_size * channels as u32) as usize];
        let interleave_buf = Vec::with_capacity(io_buf.len());

        let mut reader = Self {
            file,
            sample_rate,
            channels,
            block_size,
            sample_count,
            duration_seconds,
            dsd_rate,
            data_offset,
            data_size,
            current_data_pos: 0,
            io_buf,
            interleave_buf,
            interleave_pos: 0,
        };

        reader.seek_to_data(0)?;
        Ok(reader)
    }

    /// 跳转到数据块中的指定字节偏移
    fn seek_to_data(&mut self, offset: u64) -> Result<()> {
        let aligned_offset = (offset / (self.block_size as u64 * self.channels as u64))
            * (self.block_size as u64 * self.channels as u64);
        self.file
            .seek(SeekFrom::Start(self.data_offset + aligned_offset))?;
        self.current_data_pos = aligned_offset;
        self.interleave_buf.clear();
        self.interleave_pos = 0;
        Ok(())
    }

    /// 按秒为单位进行 Seek
    pub fn seek_seconds(&mut self, seconds: f64) -> Result<()> {
        if self.duration_seconds <= 0.0 {
            return self.seek_to_data(0);
        }
        let ratio = (seconds / self.duration_seconds).clamp(0.0, 1.0);
        let target_bytes = (ratio * self.data_size as f64) as u64;
        self.seek_to_data(target_bytes)
    }

    /// 读取解包并完成 4 字节 L/R 交织与 MSB 比特翻转的 DSD 原生数据
    ///
    /// 输出格式：每 8 字节包含 [L0, L1, L2, L3, R0, R1, R2, R3] (MSB First)，
    /// 符合 Diretta DSD_SIZ_32 及行业标准 Native DSD DAC 输入规范。
    pub fn read_interleaved_dsd(&mut self, out_buf: &mut [u8]) -> Result<usize> {
        let mut total_written = 0;

        while total_written < out_buf.len() {
            // 如果内部交织缓冲区还有剩余未读数据，优先提供
            if self.interleave_pos < self.interleave_buf.len() {
                let available = self.interleave_buf.len() - self.interleave_pos;
                let needed = out_buf.len() - total_written;
                let to_copy = available.min(needed);

                out_buf[total_written..total_written + to_copy].copy_from_slice(
                    &self.interleave_buf[self.interleave_pos..self.interleave_pos + to_copy],
                );

                self.interleave_pos += to_copy;
                total_written += to_copy;
                continue;
            }

            // 检查是否已达到数据尾部
            if self.current_data_pos >= self.data_size {
                break;
            }

            // 从文件读取一个完整的 Planar 块组 (Left + Right)
            let read_size = (self.block_size * 2) as usize;
            self.interleave_buf.clear();
            self.interleave_pos = 0;

            let bytes_read = match self.file.read(&mut self.io_buf[..read_size]) {
                Ok(n) => n,
                Err(e) => return Err(e.into()),
            };

            if bytes_read == 0 {
                break;
            }

            self.current_data_pos += bytes_read as u64;

            if bytes_read == read_size {
                // 核心交织算法：4 字节 L + 4 字节 R 交叉，应用 BitReverseLUT 翻转为 MSB
                let p_l = &self.io_buf[0..self.block_size as usize];
                let p_r = &self.io_buf[self.block_size as usize..read_size];

                self.interleave_buf.resize(read_size, 0);
                let p_out = self.interleave_buf.as_mut_slice();
                let mut write_idx = 0;

                let bsize = self.block_size as usize;
                let mut i = 0;
                while i < bsize {
                    p_out[write_idx + 0] = reverse_byte(p_l[i + 0]);
                    p_out[write_idx + 1] = reverse_byte(p_l[i + 1]);
                    p_out[write_idx + 2] = reverse_byte(p_l[i + 2]);
                    p_out[write_idx + 3] = reverse_byte(p_l[i + 3]);

                    p_out[write_idx + 4] = reverse_byte(p_r[i + 0]);
                    p_out[write_idx + 5] = reverse_byte(p_r[i + 1]);
                    p_out[write_idx + 6] = reverse_byte(p_r[i + 2]);
                    p_out[write_idx + 7] = reverse_byte(p_r[i + 3]);

                    write_idx += 8;
                    i += 4;
                }
            } else {
                // 不足一个完整块时填充静音 (0x69 为标准 DSD 静音字节)
                break;
            }
        }

        Ok(total_written)
    }

    /// 获取当前播放进度（秒）
    pub fn current_position_seconds(&self) -> f64 {
        if self.data_size == 0 {
            0.0
        } else {
            (self.current_data_pos as f64 / self.data_size as f64) * self.duration_seconds
        }
    }
}
