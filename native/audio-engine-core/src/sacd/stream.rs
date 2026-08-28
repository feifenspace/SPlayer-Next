//! SACD 音轨流读取器 (SacdTrackReader)
//!
//! 支持从 SACD 扇区中实时抽取或解压 DSD 数据，并输出为立体声交错格式（4 字节 L + 4 字节 R），
//! 直接对接 `Dsd2PcmDecimator` 降采样器或 Diretta Native DSD 直通通道。

use anyhow::{anyhow, Result};
use std::path::Path;

use super::dst::DstDecoder;
use super::iso_reader::{IsoReader, SACD_SECTOR_SIZE};
use super::scarletbook::{SacdDisc, SacdTrack, SACD_SAMPLING_FREQUENCY};
use crate::dsd::{reverse_byte, DsdRate};

/// SACD 单轨流读取器
pub struct SacdTrackReader {
    pub iso_reader: IsoReader,
    pub track: SacdTrack,
    pub sample_rate: u32,
    pub channels: u16,
    pub dsd_rate: DsdRate,
    pub total_frames: u32,
    pub current_frame: u32,
    pub duration_seconds: f64,

    dst_decoder: Option<DstDecoder>,
    current_sector: u32,
    end_sector: u32,
    sector_buf: Vec<u8>,
    residual_buf: Vec<u8>,
    residual_pos: usize,
}

impl SacdTrackReader {
    /// 打开 SACD 镜像并定位到指定音轨
    pub fn open_track<P: AsRef<Path>>(iso_path: P, track_num: u16) -> Result<Self> {
        let mut iso_reader = IsoReader::open(iso_path)?;
        let disc = SacdDisc::parse(&mut iso_reader)?;

        // 优先使用双声道区域，若无则使用多声道区域
        let area = if let Some(stereo_idx) = disc.stereo_area_idx {
            &disc.areas[stereo_idx]
        } else if let Some(multi_idx) = disc.multichannel_area_idx {
            &disc.areas[multi_idx]
        } else {
            return Err(anyhow!("No audio area in SACD ISO"));
        };

        let track = area
            .tracks
            .iter()
            .find(|t| t.track_num == track_num)
            .cloned()
            .ok_or_else(|| anyhow!("Track {} not found in SACD ISO", track_num))?;

        let total_frames = track.duration_frame;
        let duration_seconds = track.duration;
        let channels = track.channels;
        let sample_rate = SACD_SAMPLING_FREQUENCY;
        let dsd_rate = DsdRate::from_sample_rate(sample_rate);

        let dst_decoder = if track.is_dst {
            Some(DstDecoder::new(channels as usize))
        } else {
            None
        };

        let start_sector = track.start_lsn;
        let end_sector = track.start_lsn + track.length_lsn;

        Ok(Self {
            iso_reader,
            track,
            sample_rate,
            channels,
            dsd_rate,
            total_frames,
            current_frame: 0,
            duration_seconds,
            dst_decoder,
            current_sector: start_sector,
            end_sector,
            sector_buf: vec![0u8; SACD_SECTOR_SIZE * 16], // 32KB 扇区缓存
            residual_buf: Vec::with_capacity(65536),
            residual_pos: 0,
        })
    }

    /// 读取解包后的交错 DSD 数据 (Interleaved MSB DSD)
    pub fn read_interleaved_dsd(&mut self, out_buf: &mut [u8]) -> Result<usize> {
        if out_buf.is_empty() {
            return Ok(0);
        }

        let mut written = 0;

        while written < out_buf.len() {
            // 1. 如果 residual_buf 中有剩余可用数据，先复制
            if self.residual_pos < self.residual_buf.len() {
                let avail = self.residual_buf.len() - self.residual_pos;
                let needed = out_buf.len() - written;
                let copy_len = avail.min(needed);
                out_buf[written..written + copy_len].copy_from_slice(
                    &self.residual_buf[self.residual_pos..self.residual_pos + copy_len],
                );
                self.residual_pos += copy_len;
                written += copy_len;
                continue;
            }

            // 2. 检查是否已达到分轨帧数上限或扇区末尾
            if self.current_frame >= self.total_frames || self.current_sector >= self.end_sector {
                break;
            }

            // 3. 读取下一个音频扇区并解包一帧 DSD
            self.residual_buf.clear();
            self.residual_pos = 0;

            let sectors_to_read = (self.end_sector - self.current_sector).min(1);
            if sectors_to_read == 0 {
                break;
            }

            self.iso_reader.read_sectors(
                self.current_sector,
                sectors_to_read,
                &mut self.sector_buf[..sectors_to_read as usize * SACD_SECTOR_SIZE],
            )?;
            self.current_sector += sectors_to_read;

            let sec_data = &self.sector_buf[..SACD_SECTOR_SIZE];
            let header_byte = sec_data[0];
            let dst_encoded = (header_byte & 0x01) != 0;
            let packet_count = (header_byte >> 5) & 0x07;

            // 提取扇区中的音频载荷
            let offset = 1 + (packet_count as usize) * 2;
            if offset < SACD_SECTOR_SIZE {
                let payload_len = SACD_SECTOR_SIZE - offset;
                let mut temp_payload = vec![0u8; payload_len];
                temp_payload.copy_from_slice(&self.sector_buf[offset..SACD_SECTOR_SIZE]);

                if dst_encoded {
                    if let Some(dec) = &mut self.dst_decoder {
                        let mut dsd_frame = vec![0u8; self.channels as usize * 4704];
                        if let Ok(len) = dec.decode_frame(&temp_payload, &mut dsd_frame) {
                            self.interleave_channels(&dsd_frame[..len]);
                        }
                    }
                } else {
                    // 未压缩 DSD 载荷
                    self.interleave_channels(&temp_payload);
                }
            }

            self.current_frame += 1;
        }

        Ok(written)
    }

    /// 将 planar 或原始 DSD 声道数据转换为交错格式 (4-byte blocks) 并进行比特反转
    fn interleave_channels(&mut self, payload: &[u8]) {
        let ch = self.channels as usize;
        if ch == 2 {
            // 立体声交错
            let block_size = 4;
            let half = payload.len() / 2;
            let left = &payload[..half];
            let right = &payload[half..];

            let count = (left.len() / block_size).min(right.len() / block_size);
            for i in 0..count {
                for b in 0..block_size {
                    self.residual_buf
                        .push(reverse_byte(left[i * block_size + b]));
                }
                for b in 0..block_size {
                    self.residual_buf
                        .push(reverse_byte(right[i * block_size + b]));
                }
            }
        } else {
            // 单声道或多声道直通
            for &b in payload {
                self.residual_buf.push(reverse_byte(b));
            }
        }
    }

    /// Seek 到指定的相对时间（秒）
    pub fn seek_seconds(&mut self, target_sec: f64) -> Result<()> {
        let target_frame = (target_sec * 75.0).round() as u32;
        let clamped_frame = target_frame.min(self.total_frames);
        self.current_frame = clamped_frame;

        // 根据帧数粗略估算扇区位置
        if self.total_frames > 0 {
            let progress = clamped_frame as f64 / self.total_frames as f64;
            let total_sectors = self.end_sector - self.track.start_lsn;
            let target_sec_offset = (progress * total_sectors as f64).round() as u32;
            self.current_sector = (self.track.start_lsn + target_sec_offset).min(self.end_sector);
        }

        self.residual_buf.clear();
        self.residual_pos = 0;
        Ok(())
    }
}
