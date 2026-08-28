//! Philips DSDIFF (DFF) 格式解析与流式读取器
//!
//! 移植自 tinyLMS-old DsdDecoder::ParseAndDecodeDff。
//! DFF 特性：Big-Endian，MSB First，Chunk 树状组织。数据区本身即为逐点/逐字节交错。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{bail, Context, Result};
use tracing::info;

use super::DsdRate;

pub struct DffReader {
    file: File,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_count: u64,
    pub duration_seconds: f64,
    pub dsd_rate: DsdRate,
    pub data_offset: u64,
    pub data_size: u64,

    current_data_pos: u64,
}

impl DffReader {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut file = File::open(path.as_ref())
            .with_context(|| format!("Failed to open DFF file: {:?}", path.as_ref()))?;

        let mut magic = [0u8; 12];
        file.read_exact(&mut magic)
            .context("Failed to read DFF magic")?;

        if &magic[0..4] != b"FRM8" || &magic[8..12] != b"DSD " {
            bail!("Invalid DFF header, expected 'FRM8...DSD '");
        }

        let mut sample_rate = 2_822_400; // 默认 DSD64
        let mut channels = 2u16;
        let mut data_offset = 0u64;
        let mut data_size = 0u64;

        // 遍历所有 Chunk
        let mut chunk_head = [0u8; 12];
        while file.read_exact(&mut chunk_head).is_ok() {
            let chunk_id = &chunk_head[0..4];
            let chunk_size = u64::from_be_bytes(chunk_head[4..12].try_into().unwrap());
            let current_pos = file.stream_position()?;

            if chunk_id == b"PROP" {
                // PROP 内部包含 SND 属性子 Chunk
                let mut prop_type = [0u8; 4];
                file.read_exact(&mut prop_type)?;
                if &prop_type == b"SND " {
                    let mut sub_head = [0u8; 12];
                    let prop_end = current_pos + chunk_size;
                    while file.stream_position()? + 12 <= prop_end
                        && file.read_exact(&mut sub_head).is_ok()
                    {
                        let sub_id = &sub_head[0..4];
                        let sub_size = u64::from_be_bytes(sub_head[4..12].try_into().unwrap());
                        let sub_pos = file.stream_position()?;

                        if sub_id == b"FS  " {
                            let mut fs_bytes = [0u8; 4];
                            file.read_exact(&mut fs_bytes)?;
                            sample_rate = u32::from_be_bytes(fs_bytes);
                        } else if sub_id == b"CHNL" {
                            let mut ch_bytes = [0u8; 2];
                            file.read_exact(&mut ch_bytes)?;
                            channels = u16::from_be_bytes(ch_bytes);
                        }

                        file.seek(SeekFrom::Start(sub_pos + sub_size))?;
                    }
                }
            } else if chunk_id == b"DSD " {
                data_offset = current_pos;
                data_size = chunk_size;
                break;
            }

            file.seek(SeekFrom::Start(current_pos + chunk_size))?;
        }

        if data_offset == 0 {
            bail!("Failed to find DSD sound chunk in DFF file");
        }

        let bytes_per_sec = (sample_rate / 8) * channels as u32;
        let duration_seconds = if bytes_per_sec > 0 {
            data_size as f64 / bytes_per_sec as f64
        } else {
            0.0
        };
        let sample_count = (duration_seconds * sample_rate as f64) as u64;
        let dsd_rate = DsdRate::from_sample_rate(sample_rate);

        info!(
            "Opened DFF: rate={} ({}), ch={}, size={}MB, duration={:.2}s",
            sample_rate,
            dsd_rate.display_name(),
            channels,
            data_size / 1024 / 1024,
            duration_seconds
        );

        file.seek(SeekFrom::Start(data_offset))?;

        Ok(Self {
            file,
            sample_rate,
            channels,
            sample_count,
            duration_seconds,
            dsd_rate,
            data_offset,
            data_size,
            current_data_pos: 0,
        })
    }

    pub fn seek_seconds(&mut self, seconds: f64) -> Result<()> {
        if self.duration_seconds <= 0.0 {
            self.file.seek(SeekFrom::Start(self.data_offset))?;
            self.current_data_pos = 0;
            return Ok(());
        }
        let ratio = (seconds / self.duration_seconds).clamp(0.0, 1.0);
        let target_bytes = ((ratio * self.data_size as f64) as u64 / 8) * 8; // 对齐
        self.file
            .seek(SeekFrom::Start(self.data_offset + target_bytes))?;
        self.current_data_pos = target_bytes;
        Ok(())
    }

    pub fn read_interleaved_dsd(&mut self, out_buf: &mut [u8]) -> Result<usize> {
        let remaining = self.data_size.saturating_sub(self.current_data_pos) as usize;
        let to_read = out_buf.len().min(remaining);
        if to_read == 0 {
            return Ok(0);
        }

        let bytes_read = self.file.read(&mut out_buf[..to_read])?;
        self.current_data_pos += bytes_read as u64;
        Ok(bytes_read)
    }

    pub fn current_position_seconds(&self) -> f64 {
        if self.data_size == 0 {
            0.0
        } else {
            (self.current_data_pos as f64 / self.data_size as f64) * self.duration_seconds
        }
    }
}
