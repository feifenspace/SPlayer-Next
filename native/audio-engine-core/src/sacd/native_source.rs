//! SACD 原生 DSD 数据源：从 ISO 流式读取 + DST 解码 + Block32 interleave。
//!
//! 阶段2：绕过 FFmpeg，直接通过 FFI 调用 libdstdec 进行 DST 解码。
//!
//! # 背景
//!
//! 旧路径（`source.rs::extract_track_to_dsdiff_*`）把整条 SACD 轨道提取为内存/临时
//! DSDIFF 文件，再交给 FFmpeg IFF demuxer + DST decoder。问题：
//! - FFmpeg DST decoder 输出 `fltp`（PCM float）而非 U8 DSD 字节流，导致杂音
//! - 一次性提取整条轨道到内存，长轨道 OOM 崩溃
//!
//! 新路径（本模块）：
//! - 流式读取 ISO 扇区（一次一个扇区，避免 OOM）
//! - DST 帧通过 `dst_ffi::DstDecoder`（FFI 调用 libdstdec）解码为原始 DSD 字节
//! - DSD 3-in-14/16 帧已是原始 DSD 字节，无需解码
//! - 通过 `interleaved_1byte_to_block32_in_place` 转为 Diretta 要求的 Block32 布局
//!
//! # 输出契约
//!
//! `next_block()` 返回的 `Vec<u8>` 与 `NativeDsdSource` 一致：
//! - 布局：InterleavedBlock32（`[L:4B][R:4B]…`）
//! - 字节序：MSB-first
//! - 长度：单帧 4704×channels 字节（stereo = 9408B），末帧可能 padding 到 8 的倍数

use std::collections::VecDeque;
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use tracing::{debug, info, warn};

use super::dst_ffi::DstDecoder;
use super::iso_reader::{IsoReader, SACD_LSN_SIZE};
use super::scarletbook::{probe_sacd_iso, FrameFormat, SACD_SAMPLING_FREQUENCY};
use super::source::parse_sacd_virtual_path;

use crate::dsd::interleaved_1byte_to_block32_in_place;


// ─────────────────────────────────────────────────────────────────
// 常量（与 source.rs 严格对齐）
// ─────────────────────────────────────────────────────────────────

/// DST 帧最大字节数
const MAX_DST_SIZE: usize = 65_536;
/// 未压缩 DSD 帧大小（每声道，= 588 × 64 / 8）
const FRAME_SIZE_64: usize = 4_704;
/// 单包最大字节数
const MAX_PACKET_SIZE: usize = 2_045;
/// 单扇区最大包数
const MAX_PACKETS_PER_SECTOR: u8 = 7;

/// 音频包数据类型：音频数据
const DATA_TYPE_AUDIO: u8 = 2;
/// 音频包数据类型：补充数据（跳过）
const DATA_TYPE_SUPPLEMENTARY: u8 = 3;
/// 音频包数据类型：填充（跳过）
const DATA_TYPE_PADDING: u8 = 7;

/// DSD 静音电平（PDM 物理静音值），用于末帧 padding
const DSD_SILENCE_BYTE: u8 = 0x69;

/// stereo 声道布局掩码（FL + FR）
const STEREO_CHANNEL_MASK: u32 = 0x3;

// ─────────────────────────────────────────────────────────────────
// DST 帧组装器（移植自 source.rs::DstFrameAssembler）
// ─────────────────────────────────────────────────────────────────
//
// 为避免暴露 source.rs 的私有结构，此处复制实现。逻辑与原版完全一致，
// 后续若重构可改为 pub(crate) 共享。

struct DstFrameAssembler {
    data: Vec<u8>,
    dst_encoded: bool,
    sector_count: u8,
    channel_count: u8,
    started: bool,
}

impl DstFrameAssembler {
    fn new() -> Self {
        Self {
            data: Vec::with_capacity(MAX_DST_SIZE),
            dst_encoded: false,
            sector_count: 0,
            channel_count: 2,
            started: false,
        }
    }

    fn is_complete(&self) -> bool {
        if self.dst_encoded {
            self.sector_count == 0
        } else {
            self.size() == self.channel_count as usize * FRAME_SIZE_64
        }
    }

    #[inline]
    fn size(&self) -> usize {
        self.data.len()
    }

    /// 当前是否有正在累积的帧。
    #[inline]
    fn is_started(&self) -> bool {
        self.started
    }

    fn start_new(&mut self, dst_encoded: bool, sector_count: u8, channel_count: u8) {
        self.data.clear();
        self.dst_encoded = dst_encoded;
        self.sector_count = sector_count;
        self.channel_count = channel_count;
        self.started = true;
    }

    fn append_packet(&mut self, packet_data: &[u8]) -> bool {
        if self.size() + packet_data.len() > MAX_DST_SIZE {
            self.started = false;
            false
        } else {
            self.data.extend_from_slice(packet_data);
            if self.dst_encoded {
                self.sector_count = self.sector_count.saturating_sub(1);
            }
            true
        }
    }

    fn take_frame(&mut self) -> Vec<u8> {
        let frame = std::mem::take(&mut self.data);
        self.started = false;
        self.data = Vec::with_capacity(MAX_DST_SIZE);
        frame
    }
}

#[inline]
fn parse_channel_count(frame_info_byte: u8) -> u8 {
    let channel_bit_3 = (frame_info_byte >> 0) & 0x01;
    let channel_bit_2 = (frame_info_byte >> 1) & 0x01;
    if channel_bit_2 == 1 && channel_bit_3 == 0 {
        6
    } else if channel_bit_2 == 0 && channel_bit_3 == 1 {
        5
    } else {
        2
    }
}

#[inline]
fn parse_sector_count(frame_info_byte: u8) -> u8 {
    (frame_info_byte >> 2) & 0x1F
}

// ─────────────────────────────────────────────────────────────────
// SacdNativeSource
// ─────────────────────────────────────────────────────────────────

/// SACD 原生 DSD 数据源。
///
/// 流式读取 SACD ISO 中的轨道扇区，按 ScarletBook 规范组装 DST 帧 / 原始 DSD 帧，
/// 通过 libdstdec FFI 解码 DST 帧为原始 DSD 字节，最后转换为 Diretta 要求的
/// InterleavedBlock32 + MSB-first 格式。
///
/// # 支持的帧格式
///
/// - `FrameFormat::Dst`：DST 压缩帧，通过 `DstDecoder` 解码
/// - `FrameFormat::Dsd3In14` / `Dsd3In16`：原始 DSD 字节，直接输出
///
/// # 输出契约
///
/// `next_block()` 返回的 `Vec<u8>` 与 `NativeDsdSource` 一致：
/// - 布局：InterleavedBlock32
/// - 字节序：MSB-first
/// - 长度：单帧 `4704 × channels` 字节（stereo = 9408B）
pub struct SacdNativeSource {
    /// ISO 扇区读取器
    iso_reader: IsoReader,
    /// 轨道起始 LSN
    start_lsn: u32,
    /// 轨道结束 LSN（exclusive）
    end_lsn: u32,
    /// 当前读取位置 LSN
    current_lsn: u32,
    /// 帧格式（Dst / Dsd3In14 / Dsd3In16）
    frame_format: FrameFormat,
    /// 光盘级声道数（仅 DSD 3-in-14/16 用作 channel_count 兜底）
    disc_channel_count: u8,

    /// 帧组装器（跨扇区累积同一帧的字节）
    frame_assembler: DstFrameAssembler,

    /// 当前正在解析的扇区缓冲（2048 字节）
    sector_buf: Vec<u8>,
    /// 当前扇区是否已解析完毕（true=需要读取下一扇区）
    sector_consumed: bool,

    /// DST 解码器（仅 Dst 格式创建；其他格式为 None）
    dst_decoder: Option<DstDecoder>,

    /// 已就绪的 DSD 帧队列（1-byte 交错 + MSB-first，待 Block32 转换）
    decoded_queue: VecDeque<Vec<u8>>,

    /// 流结束标志
    eof: bool,

    /// DSD 采样率（固定 2,822,400 Hz for DSD64）
    pub sample_rate: u32,
    /// 声道数
    pub channels: u16,
    /// 轨道时长（秒）
    pub duration_secs: f64,
}

impl std::fmt::Debug for SacdNativeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SacdNativeSource")
            .field("frame_format", &self.frame_format)
            .field("disc_channel_count", &self.disc_channel_count)
            .field("channels", &self.channels)
            .field("sample_rate", &self.sample_rate)
            .field("duration_secs", &self.duration_secs)
            .field("current_lsn", &self.current_lsn)
            .field("end_lsn", &self.end_lsn)
            .field("eof", &self.eof)
            .field("decoded_queue_len", &self.decoded_queue.len())
            .finish_non_exhaustive()
    }
}

impl SacdNativeSource {
    /// 从 SACD ISO + 虚拟路径打开原生 DSD 数据源。
    ///
    /// # 参数
    ///
    /// - `iso_path`：SACD ISO 镜像文件路径
    /// - `virtual_path`：SACD 7 字段虚拟路径
    ///   `iso_path|TrackXX|duration_sec|start_frames|duration_frames|start_lsn|length_lsn`
    ///
    /// # 错误
    ///
    /// - 虚拟路径格式无效
    /// - ISO 探测失败
    /// - DST 解码器创建失败（仅 Dst 格式）
    pub fn open<P: AsRef<Path>>(iso_path: P, virtual_path: &str) -> Result<Self> {
        let vp = parse_sacd_virtual_path(virtual_path)
            .ok_or_else(|| anyhow::anyhow!("无效的 SACD 虚拟路径: {}", virtual_path))?;

        let iso_path_ref = iso_path.as_ref();
        let disc = probe_sacd_iso(iso_path_ref)
            .with_context(|| format!("探测 SACD ISO 失败: {}", iso_path_ref.display()))?;

        let frame_format = disc.frame_format;
        let disc_channel_count = disc.channel_count;
        let sample_rate = SACD_SAMPLING_FREQUENCY;
        let channels = disc_channel_count as u16;
        let duration_secs = vp.duration_secs;

        // 仅支持 stereo（interleaved_1byte_to_block32_in_place 当前仅支持 2 声道）
        ensure!(
            channels == 2,
            "SACD 原生路径当前仅支持 stereo（2 声道），实际声道数: {}",
            channels
        );

        let iso_reader = IsoReader::open(iso_path_ref)
            .with_context(|| format!("打开 SACD ISO 失败: {}", iso_path_ref.display()))?;

        let dst_decoder = if frame_format == FrameFormat::Dst {
            Some(DstDecoder::new(disc_channel_count).context("创建 DST 解码器失败")?)
        } else {
            None
        };

        info!(
            target: "audio::decoder::dsd::sacd",
            path = %iso_path_ref.display(),
            track = vp.track_num,
            format = ?frame_format,
            channels,
            sample_rate,
            duration_secs = format!("{:.3}", duration_secs),
            start_lsn = vp.start_lsn,
            length_lsn = vp.length_lsn,
            "原生 SACD 数据源已打开"
        );

        Ok(Self {
            iso_reader,
            start_lsn: vp.start_lsn,
            end_lsn: vp.start_lsn.saturating_add(vp.length_lsn),
            current_lsn: vp.start_lsn,
            frame_format,
            disc_channel_count,
            frame_assembler: DstFrameAssembler::new(),
            sector_buf: Vec::with_capacity(SACD_LSN_SIZE),
            sector_consumed: true,
            dst_decoder,
            decoded_queue: VecDeque::new(),
            eof: false,
            sample_rate,
            channels,
            duration_secs,
        })
    }


    /// 解析当前扇区，把音频包喂给 frame_assembler，输出完整帧列表。
    ///
    /// 逻辑移植自 `source.rs::extract_dst_frames` 的单扇区处理段（line 524-688）。
    fn parse_current_sector(&mut self) -> Result<Vec<Vec<u8>>> {
        let mut frames_out: Vec<Vec<u8>> = Vec::new();
        let sector = self.sector_buf.as_slice();
        ensure!(sector.len() == SACD_LSN_SIZE, "扇区长度异常: {}", sector.len());

        // ---- 1. 解析 audio_frame_header_t（1 字节 LE bitfield）----
        let header_byte = sector[0];
        let dst_encoded = (header_byte & 0x01) != 0;
        let frame_info_count = ((header_byte >> 2) & 0x07) as usize;
        let packet_info_count = ((header_byte >> 5) & 0x07) as usize;

        let mut cursor = 1usize;

        // ---- 2. 解析 packet_info[]（每条 2 字节）----
        if cursor + packet_info_count * 2 > SACD_LSN_SIZE {
            bail!("SACD 扇区 packet_info 越界");
        }
        let mut packets: Vec<(u8, u8, usize)> = Vec::with_capacity(packet_info_count);
        for _ in 0..packet_info_count {
            let b0 = sector[cursor];
            let b1 = sector[cursor + 1];
            let frame_start = (b0 >> 7) & 0x01;
            let data_type = (b0 >> 3) & 0x07;
            let packet_length = (((b0 & 0x07) as usize) << 8) | (b1 as usize);
            packets.push((frame_start, data_type, packet_length));
            cursor += 2;
        }

        // ---- 3. 解析 frame_info[]（DST: 4 字节 / 非 DST: 3 字节）----
        let frame_info_size = if dst_encoded { 4 } else { 3 };
        if cursor + frame_info_count * frame_info_size > SACD_LSN_SIZE {
            bail!("SACD 扇区 frame_info 越界");
        }
        let mut frame_infos: Vec<u8> = Vec::with_capacity(frame_info_count);
        for i in 0..frame_info_count {
            let base = cursor + i * frame_info_size;
            if dst_encoded {
                frame_infos.push(sector[base + 3]);
            } else {
                frame_infos.push(0);
            }
        }
        cursor += frame_info_count * frame_info_size;

        // ---- 4. packet_info_count 上限校验 ----
        if packet_info_count as u8 > MAX_PACKETS_PER_SECTOR {
            warn!(
                packet_info_count,
                "SACD 扇区 packet_info_count > 7，跳过该扇区"
            );
            self.frame_assembler.started = false;
            return Ok(frames_out);
        }

        // ---- 5. 遍历 packets，组装帧 ----
        let mut frame_info_idx = 0usize;
        for (frame_start, data_type, packet_length) in &packets {
            if *packet_length > MAX_PACKET_SIZE {
                // 坏包：仍推进 cursor（见末尾 cursor += packet_length）
                warn!(packet_length, "SACD 包长度超限，跳过包内容");
            }

            if *data_type == DATA_TYPE_AUDIO {
                if *frame_start == 1 {
                    // 帧起始：检查前一帧是否完整，若完整则输出
                    if self.frame_assembler.started
                        && self.frame_assembler.size() > 0
                        && self.frame_assembler.is_complete()
                    {
                        let frame = self.frame_assembler.take_frame();
                        if !frame.is_empty() {
                            frames_out.push(frame);
                        }
                    }

                    // 开始新帧
                    if frame_info_idx < frame_infos.len() {
                        let info_byte = frame_infos[frame_info_idx];
                        let sector_count = parse_sector_count(info_byte);
                        let channel_count = if dst_encoded {
                            parse_channel_count(info_byte)
                        } else {
                            self.disc_channel_count
                        };
                        self.frame_assembler
                            .start_new(dst_encoded, sector_count, channel_count);
                        frame_info_idx += 1;
                    } else {
                        warn!(
                            frame_info_idx,
                            frame_info_count, "SACD frame_info 索引越界，跳过帧起始"
                        );
                    }
                }

                // 追加音频包数据
                if self.frame_assembler.started && *packet_length <= MAX_PACKET_SIZE {
                    if cursor + *packet_length > SACD_LSN_SIZE {
                        warn!(cursor, packet_length, "SACD 音频包数据越界，跳过");
                    } else {
                        let packet_data = &sector[cursor..cursor + *packet_length];
                        if !self.frame_assembler.append_packet(packet_data) {
                            warn!(
                                size = self.frame_assembler.size(),
                                packet_length,
                                "SACD DST 帧缓冲溢出，丢弃当前帧"
                            );
                        }
                    }
                }
            }
            // DATA_TYPE_SUPPLEMENTARY / DATA_TYPE_PADDING / unknown：跳过

            cursor += *packet_length;
        }

        // ---- 6. DST 帧跨扇区完成检测 ----
        // DST：sector_count 在 append_packet 中递减，归零表示完整
        // DSD 3-in-14/16：size 达到 channel_count * FRAME_SIZE_64 表示完整
        if self.frame_assembler.started
            && self.frame_assembler.size() > 0
            && self.frame_assembler.is_complete()
        {
            let frame = self.frame_assembler.take_frame();
            if !frame.is_empty() {
                frames_out.push(frame);
            }
        }

        Ok(frames_out)
    }

    /// 处理一帧（DST 压缩帧或原始 DSD 帧）。
    ///
    /// - DST：submit 到解码器，poll 已就绪的解码帧到 `decoded_queue`
    /// - DSD 3-in-14/16：直接入队 `decoded_queue`
    fn process_frame(&mut self, frame: Vec<u8>) -> Result<()> {
        match self.frame_format {
            FrameFormat::Dst => {
                if let Some(decoder) = &mut self.dst_decoder {
                    decoder.submit(&frame);
                    // 立即 poll 已就绪的解码帧（避免队列无限膨胀）
                    while let Some(dsd_frame) = decoder.next_decoded() {
                        self.decoded_queue.push_back(dsd_frame);
                    }
                } else {
                    warn!("DST 帧收到但解码器未初始化，丢弃");
                }
            }
            FrameFormat::Dsd3In14 | FrameFormat::Dsd3In16 => {
                // 原始 DSD 字节，无需解码
                self.decoded_queue.push_back(frame);
            }
        }
        Ok(())
    }

    /// 读取下一个扇区到 `sector_buf`。
    ///
    /// # 返回
    /// - `Ok(true)`：成功读取
    /// - `Ok(false)`：已到轨道末尾（EOF）
    fn read_next_sector(&mut self) -> Result<bool> {
        if self.current_lsn >= self.end_lsn {
            self.eof = true;
            return Ok(false);
        }
        self.sector_buf = self
            .iso_reader
            .read_sector(self.current_lsn as u64)
            .with_context(|| format!("读取 SACD 扇区失败 lsn={}", self.current_lsn))?;
        self.current_lsn = self.current_lsn.saturating_add(1);
        self.sector_consumed = false;
        Ok(true)
    }

    /// 把 1-byte 交错 + MSB-first 的 DSD 帧转为 InterleavedBlock32 格式。
    ///
    /// - 输入：`[L0 R0 L1 R1 …]`（DST 解码或 DSD 3-in-14/16 原始输出）
    /// - 输出：`[L0 L1 L2 L3 R0 R1 R2 R3 …]`（Diretta Block32）
    ///
    /// 末块若不足 8 字节倍数，用 `0x69`（DSD 静音）padding。
    fn convert_to_block32(&self, mut dsd_frame: Vec<u8>) -> Result<Vec<u8>> {
        let rem = dsd_frame.len() % 8;
        if rem != 0 {
            let pad = 8 - rem;
            dsd_frame.extend(std::iter::repeat(DSD_SILENCE_BYTE).take(pad));
        }
        interleaved_1byte_to_block32_in_place(&mut dsd_frame, self.channels)?;
        Ok(dsd_frame)
    }

    /// 读取下一个 DSD 数据块（已转换为 InterleavedBlock32 + MSB-first）。
    ///
    /// # 返回
    /// - `Ok(Some(bytes))`：Diretta-ready 的 DSD 字节流
    /// - `Ok(None)`：EOF
    pub fn next_block(&mut self) -> Result<Option<Vec<u8>>> {
        // 1. 优先消费已就绪的解码帧
        if let Some(dsd_frame) = self.decoded_queue.pop_front() {
            return Ok(Some(self.convert_to_block32(dsd_frame)?));
        }

        // 2. 持续读取扇区 + 解析 + 解码，直到有帧就绪或 EOF
        while !self.eof {
            if self.sector_consumed {
                if !self.read_next_sector()? {
                    break;
                }
            }

            let frames = self.parse_current_sector()?;
            self.sector_consumed = true;

            for frame in frames {
                self.process_frame(frame)?;
            }

            // 检查是否有解码帧就绪
            if let Some(dsd_frame) = self.decoded_queue.pop_front() {
                return Ok(Some(self.convert_to_block32(dsd_frame)?));
            }
        }

        // 3. EOF：flush DST 解码器，取出剩余解码帧
        if let Some(decoder) = &mut self.dst_decoder {
            decoder.flush();
            while let Some(dsd_frame) = decoder.next_decoded() {
                self.decoded_queue.push_back(dsd_frame);
            }
        }

        if let Some(dsd_frame) = self.decoded_queue.pop_front() {
            return Ok(Some(self.convert_to_block32(dsd_frame)?));
        }

        Ok(None)
    }

    /// 按秒数 seek（近似实现：按时间比例换算 LSN 偏移）。
    ///
    /// # 行为
    ///
    /// 1. 清空所有缓冲状态（frame_assembler / sector_buf / decoded_queue）
    /// 2. 重建 DST 解码器（旧解码器已 flush，不可复用）
    /// 3. 按时间比例计算目标 LSN
    ///
    /// # 注意
    ///
    /// seek 精度受 SACD 帧边界（1/75 秒）限制，且可能落在帧中间导致首帧解码失败
    /// （frame_assembler 会丢弃不完整帧，下一帧自动恢复）。
    pub fn seek_secs(&mut self, secs: f64) -> Result<()> {
        // 重置所有状态
        self.frame_assembler = DstFrameAssembler::new();
        self.sector_buf.clear();
        self.sector_consumed = true;
        self.decoded_queue.clear();
        self.eof = false;

        // 按时间比例计算目标 LSN
        let ratio = if self.duration_secs > 0.0 {
            (secs / self.duration_secs).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let track_lsn_len = self.end_lsn.saturating_sub(self.start_lsn);
        let lsn_offset = (track_lsn_len as f64 * ratio) as u32;
        self.current_lsn = self.start_lsn.saturating_add(lsn_offset);

        // 重建 DST 解码器（旧解码器 flush 后已销毁，无法复用）
        if self.frame_format == FrameFormat::Dst {
            self.dst_decoder = Some(
                DstDecoder::new(self.disc_channel_count).context("重建 DST 解码器失败")?,
            );
        }

        debug!(
            target: "audio::decoder::dsd::sacd",
            secs, current_lsn = self.current_lsn, "SACD seek 完成"
        );
        Ok(())
    }

    /// 文件时长（秒）。
    #[must_use]
    pub fn duration_secs(&self) -> f64 {
        self.duration_secs
    }

    /// 是否为原生 DSD 数据源（恒为 true）。
    #[must_use]
    pub fn is_native_dsd(&self) -> bool {
        true
    }
}

// ─────────────────────────────────────────────────────────────────
// 单元测试
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_channel_count() {
        // 2 channels: bits[1:0] = 00 or 11
        assert_eq!(parse_channel_count(0b000_00_00), 2); // 0x00
        assert_eq!(parse_channel_count(0b000_11_11), 2); // 0x0F
        // 5 channels: bit0=1, bit1=0
        assert_eq!(parse_channel_count(0b000_00_01), 5); // 0x01
        // 6 channels: bit0=0, bit1=1
        assert_eq!(parse_channel_count(0b000_00_10), 6); // 0x02
    }

    #[test]
    fn test_parse_sector_count() {
        // bits 2-6 = sector_count
        assert_eq!(parse_sector_count(0b00000_00), 0);
        assert_eq!(parse_sector_count(0b00001_00), 1); // bit2=1
        assert_eq!(parse_sector_count(0b11111_00), 31); // 全 1
        assert_eq!(parse_sector_count(0b11111_10), 31); // bit1=1 不影响
    }

    #[test]
    fn test_frame_assembler_dst_complete() {
        let mut asm = DstFrameAssembler::new();
        // DST 帧：sector_count=2，每追加一个包减 1
        asm.start_new(true, 2, 2);
        assert!(!asm.is_complete());
        asm.append_packet(&[0u8; 100]);
        assert!(!asm.is_complete()); // sector_count=1
        asm.append_packet(&[0u8; 100]);
        assert!(asm.is_complete()); // sector_count=0
        let frame = asm.take_frame();
        assert_eq!(frame.len(), 200);
        assert!(!asm.is_started());
    }

    #[test]
    fn test_frame_assembler_dsd_complete() {
        let mut asm = DstFrameAssembler::new();
        // DSD 3-in-14: 需要 channel_count * FRAME_SIZE_64 字节
        asm.start_new(false, 0, 2);
        let target = 2 * FRAME_SIZE_64; // 9408
        asm.append_packet(&vec![0u8; target / 2]);
        assert!(!asm.is_complete());
        asm.append_packet(&vec![0u8; target / 2]);
        assert!(asm.is_complete());
    }
}
