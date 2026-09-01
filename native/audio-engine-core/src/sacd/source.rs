//! SACD 轨道解码源：把 SACD ISO 中的轨道提取为内存 DSDIFF (DFF) 文件。
//!
//! P4 Phase B：解码侧实现。
//!
//! # 背景
//!
//! FFmpeg 8.0.1 的 DSF demuxer (`dsfdec.c`) 拒绝 `format_id=1` (DST)，只支持
//! `format_id=0` (DSD raw)。但 FFmpeg 的 IFF demuxer (`iff.c`) 支持 DSDIFF 格式
//! 下的 DST codec（`AV_CODEC_ID_DST`）和 DSD raw codec（`AV_CODEC_ID_DSD_MSBF`）。
//! 因此本模块把 SACD 轨道重新打包成 DSDIFF 格式，交给 FFmpeg IFF demuxer 解码。
//!
//! # 流程
//!
//! 1. 解析 SACD 虚拟路径（7 字段，第 2 字段以 "Track" 开头）
//! 2. 探测 ISO 获取轨道元数据（frame_format / channel_count / start_lsn / length_lsn）
//! 3. 按 `scarletbook_process_frames` 算法从扇区中组装帧
//! 4. 根据 frame_format 分派：
//!    - `Dst` → 压缩帧列表 → `build_dsdiff`（DST form + DSTF 子 chunk + CMPR=DST）
//!    - `Dsd3In14` / `Dsd3In16` → 原始 DSD 字节流 → `build_dsdiff_raw`
//!      （DSD chunk + 无 CMPR，codec = DSD_MSBF）
//! 5. 包装为 `Cursor<Vec<u8>>` 返回给 `AudioReader::new()`
//!
//! # DSDIFF 格式参考
//!
//! - 所有 chunk size 为 8 字节大端（64-bit DSDIFF）
//! - PROP 属性类型必须是 `SND `（不是 `DSD `）—— FFmpeg iff.c line 722
//! - FS sample_rate 是 DSD 位率（2822400），FFmpeg 会除以 8 得到 PCM 等价采样率
//! - FRTE num_frames 是 4 字节 BE（不是 2 字节）—— FFmpeg iff.c line 409
//! - 奇数长度 chunk 后补 1 字节 padding
//! - DSTF 每个 chunk 是一个 keyframe，`pkt->duration = sample_rate / 75`
//! - DSD chunk (codec=DSD_MSBF) 仅出现在未压缩 DSDIFF 中，由 IFF demuxer 透传给
//!   DSD_MSBF decoder，不再分帧（FFmpeg iff.c read_dsd_frame 路径）

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{anyhow, bail, ensure, Context, Result};
use tempfile::NamedTempFile;
use tracing::{debug, info, warn};

use super::iso_reader::{IsoReader, SACD_LSN_SIZE};
use super::scarletbook::{probe_sacd_iso, FrameFormat, SacdDisc, SACD_FRAME_RATE, SACD_SAMPLING_FREQUENCY};

// ─────────────────────────────────────────────────────────────────
// 常量（与 scarletbook.h 严格对齐）
// ─────────────────────────────────────────────────────────────────

/// DST 帧最大字节数（`MAX_DST_SIZE = 1024 * 64`）
const MAX_DST_SIZE: usize = 65_536;
/// 未压缩 DSD 帧大小（`FRAME_SIZE_64 = 588 * 64 / 8`，仅 DSD 3-in-14/16 用）
const FRAME_SIZE_64: usize = 4_704;
/// 单包最大字节数（`MAX_PACKET_SIZE`）
const MAX_PACKET_SIZE: usize = 2_045;
/// 单扇区最大包数（scarletbook_read.c line 791）
const MAX_PACKETS_PER_SECTOR: u8 = 7;

/// 音频包数据类型：音频数据
const DATA_TYPE_AUDIO: u8 = 2;
/// 音频包数据类型：补充数据（跳过）
const DATA_TYPE_SUPPLEMENTARY: u8 = 3;
/// 音频包数据类型：填充（跳过）
const DATA_TYPE_PADDING: u8 = 7;

/// DSDIFF 版本（1.5.0 = 0x01050000）
const DSDIFF_VERSION: u32 = 0x0105_0000;

// ─────────────────────────────────────────────────────────────────
// SACD 虚拟路径解析
// ─────────────────────────────────────────────────────────────────

/// SACD 虚拟路径解析结果。
///
/// 虚拟路径格式（7 字段，由 `lib.rs` 生成）：
/// ```text
/// iso_path|TrackXX|duration_sec|start_frames|duration_frames|start_lsn|length_lsn
/// ```
/// 第 2 字段是 "TrackXX"（轨道标识符），与 CUE 虚拟路径的 `start_secs` 语义不同，
/// 用于在 `decoder.rs` 中区分 SACD 虚拟路径与 CUE 虚拟路径。
#[derive(Debug, Clone)]
pub struct SacdVirtualPath {
    /// ISO 镜像文件路径
    pub iso_path: String,
    /// 轨道号（1-based，从 "TrackXX" 解析）
    pub track_num: u32,
    /// 轨道时长（秒，浮点）
    pub duration_secs: f64,
    /// 起始帧（75 fps）
    #[allow(dead_code)] // 当前解析路径未消费，保留供未来 DST 帧提取 API
    pub start_frames: u32,
    /// 时长帧（75 fps）
    #[allow(dead_code)] // 当前解析路径未消费，保留供未来 DST 帧提取 API
    pub duration_frames: u32,
    /// 起始 LSN
    #[allow(dead_code)] // 当前解析路径未消费，保留供未来 DST 帧提取 API
    pub start_lsn: u32,
    /// 长度 LSN
    #[allow(dead_code)] // 当前解析路径未消费，保留供未来 DST 帧提取 API
    pub length_lsn: u32,
}

/// 解析 SACD 虚拟路径。
///
/// 与 CUE 虚拟路径（5 字段，第 2 字段是数字秒数）的区分依据：
/// SACD 第 2 字段以 "Track" 开头（如 "Track01"），CUE 第 2 字段是纯数字。
///
/// # 返回
/// - `Some(SacdVirtualPath)`：是 SACD 虚拟路径
/// - `None`：不是 SACD 虚拟路径（可能是普通路径或 CUE 虚拟路径）
pub fn parse_sacd_virtual_path(source: &str) -> Option<SacdVirtualPath> {
    let fields: Vec<&str> = source.split('|').collect();
    if fields.len() != 7 {
        return None;
    }
    // 第 2 字段必须是 "TrackXX" 格式
    let track_field = fields[1];
    if !track_field.starts_with("Track") {
        return None;
    }
    let track_num_str = &track_field["Track".len()..];
    let track_num: u32 = track_num_str.parse().ok()?;

    let iso_path = fields[0].to_string();
    let duration_secs: f64 = fields[2].parse().ok()?;
    let start_frames: u32 = fields[3].parse().ok()?;
    let duration_frames: u32 = fields[4].parse().ok()?;
    let start_lsn: u32 = fields[5].parse().ok()?;
    let length_lsn: u32 = fields[6].parse().ok()?;

    Some(SacdVirtualPath {
        iso_path,
        track_num,
        duration_secs,
        start_frames,
        duration_frames,
        start_lsn,
        length_lsn,
    })
}

// ─────────────────────────────────────────────────────────────────
// DSDIFF 构建辅助
// ─────────────────────────────────────────────────────────────────

#[inline]
fn write_tag(buf: &mut Vec<u8>, tag: &[u8; 4]) {
    buf.extend_from_slice(tag);
}

#[inline]
fn write_be_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_be_bytes());
}

#[inline]
fn write_be_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_be_bytes());
}

#[inline]
fn write_be_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_be_bytes());
}

/// 写入一个 DSDIFF chunk：ID(4B) + size(8B BE) + body + 可选 padding(1B if odd)。
fn write_chunk(buf: &mut Vec<u8>, tag: &[u8; 4], body: &[u8]) {
    write_tag(buf, tag);
    write_be_u64(buf, body.len() as u64);
    buf.extend_from_slice(body);
    // 奇数长度补 1 字节 padding（DSDIFF 规范要求 chunk 对齐到偶数字节）
    if body.len() % 2 == 1 {
        buf.push(0);
    }
}

/// 生成 DSDIFF 通道 ID 列表。
///
/// DSDIFF 规范定义的 4 字节通道标识符：
/// - `SLFT`：Left
/// - `SRGT`：Right
/// - `C   `：Center
/// - `LFE `：Low Frequency Effects
/// - `LS  `：Left Surround
/// - `RS  `：Right Surround
fn channel_ids(channel_count: u8) -> Vec<&'static [u8; 4]> {
    match channel_count {
        2 => vec![b"SLFT", b"SRGT"],
        5 => vec![b"SLFT", b"SRGT", b"C   ", b"LS  ", b"RS  "],
        6 => vec![b"SLFT", b"SRGT", b"C   ", b"LFE ", b"LS  ", b"RS  "],
        // 兜底：按立体声处理（SACD 多声道已下混到 stereo 的常见场景）
        _ => vec![b"SLFT", b"SRGT"],
    }
}

/// 生成完整 DSDIFF 文件字节流。
///
/// # 结构
/// ```text
/// FRM8 + form_size(8B BE) + DSD  (4B)
///   FVER + size=4 + version=0x01050000
///   PROP + prop_size + SND
///     FS   + size=4 + sample_rate (DSD 位率 2822400)
///     CHNL + size + num_channels(2B) + channel_ids
///     CMPR + size + DST  + count=3 + "DST"
///   DST  + dst_size
///     DSTF + size + frame_data [+ pad]   (每帧一个)
///     ...
///     FRTE + size=6 + num_frames(4B) + frame_rate=75(2B)
/// ```
fn build_dsdiff(sample_rate: u32, channel_count: u8, dst_frames: &[Vec<u8>]) -> Vec<u8> {
    // ---- FVER body ----
    let mut fver_body = Vec::with_capacity(4);
    write_be_u32(&mut fver_body, DSDIFF_VERSION);

    // ---- PROP body = SND + FS + CHNL + CMPR ----
    let mut prop_body = Vec::new();
    write_tag(&mut prop_body, b"SND "); // 属性类型必须是 SND

    // FS sub-chunk
    let mut fs_body = Vec::with_capacity(4);
    write_be_u32(&mut fs_body, sample_rate);
    let mut fs_chunk = Vec::new();
    write_chunk(&mut fs_chunk, b"FS  ", &fs_body);
    prop_body.extend_from_slice(&fs_chunk);

    // CHNL sub-chunk
    let mut chnl_body = Vec::new();
    let ids = channel_ids(channel_count);
    write_be_u16(&mut chnl_body, ids.len() as u16);
    for id in &ids {
        chnl_body.extend_from_slice(*id);
    }
    let mut chnl_chunk = Vec::new();
    write_chunk(&mut chnl_chunk, b"CHNL", &chnl_body);
    prop_body.extend_from_slice(&chnl_chunk);

    // CMPR sub-chunk: DST tag + count(1B) + "DST" string
    let mut cmpr_body = Vec::with_capacity(8);
    write_tag(&mut cmpr_body, b"DST ");
    cmpr_body.push(3); // count
    cmpr_body.extend_from_slice(b"DST");
    let mut cmpr_chunk = Vec::new();
    write_chunk(&mut cmpr_chunk, b"CMPR", &cmpr_body);
    prop_body.extend_from_slice(&cmpr_chunk);

    // ---- DST body = DSTF* + FRTE ----
    let mut dst_body = Vec::new();
    for frame in dst_frames {
        write_chunk(&mut dst_body, b"DSTF", frame);
    }
    // FRTE sub-chunk: num_frames(4B BE) + frame_rate(2B BE)
    let mut frte_body = Vec::with_capacity(6);
    write_be_u32(&mut frte_body, dst_frames.len() as u32);
    write_be_u16(&mut frte_body, SACD_FRAME_RATE as u16);
    let mut frte_chunk = Vec::new();
    write_chunk(&mut frte_chunk, b"FRTE", &frte_body);
    dst_body.extend_from_slice(&frte_chunk);

    // ---- 组装 FRM8 form ----
    // form body = "DSD " + FVER chunk + PROP chunk + DST chunk
    let mut form_body = Vec::new();
    write_tag(&mut form_body, b"DSD ");

    let mut fver_chunk = Vec::new();
    write_chunk(&mut fver_chunk, b"FVER", &fver_body);
    form_body.extend_from_slice(&fver_chunk);

    let mut prop_chunk = Vec::new();
    write_chunk(&mut prop_chunk, b"PROP", &prop_body);
    form_body.extend_from_slice(&prop_chunk);

    let mut dst_chunk = Vec::new();
    write_chunk(&mut dst_chunk, b"DST ", &dst_body);
    form_body.extend_from_slice(&dst_chunk);

    // FRM8 header + form_size + body
    let mut out = Vec::with_capacity(12 + form_body.len());
    write_tag(&mut out, b"FRM8");
    write_be_u64(&mut out, form_body.len() as u64);
    out.extend_from_slice(&form_body);
    out
}

/// 生成完整 DSDIFF 文件字节流（未压缩 DSD 3-in-14/16 格式）。
///
/// # 结构
/// ```text
/// FRM8 + form_size(8B BE) + DSD  (4B)
///   FVER + size=4 + version=0x01050000
///   PROP + prop_size + SND
///     FS   + size=4 + sample_rate (DSD 位率 2822400)
///     CHNL + size + num_channels(2B) + channel_ids
///     CMPR + size + DSD  + count=3 + "DSD"
///   DSD  + dsd_size  (raw DSD 字节流，codec=DSD_MSBF)
/// ```
///
/// 与 DST 格式的差异：直接以 `DSD` chunk 包含原始 DSD 样本流（每字节 8 个 DSD 样本，
/// MSB-first），而非 `DST` form + `DSTF` 子 chunk。FFmpeg iff.c 根据 CMPR 子 chunk
/// 的 tag 区分（"DSD " → AV_CODEC_ID_DSD_MSBF，"DST " → AV_CODEC_ID_DST）。
///
/// **重要**：CMPR 子 chunk 是 FFmpeg iff.c 识别 codec 的必需字段，缺失会导致
/// `Invalid data found when processing input` 错误（已通过 ffprobe 验证）。
fn build_dsdiff_raw(sample_rate: u32, channel_count: u8, raw_dsd: &[u8]) -> Vec<u8> {
    // ---- FVER body ----
    let mut fver_body = Vec::with_capacity(4);
    write_be_u32(&mut fver_body, DSDIFF_VERSION);

    // ---- PROP body = SND + FS + CHNL + CMPR ----
    // CMPR 子 chunk 必须存在，否则 FFmpeg iff.c 无法识别 codec
    let mut prop_body = Vec::new();
    write_tag(&mut prop_body, b"SND ");

    let mut fs_body = Vec::with_capacity(4);
    write_be_u32(&mut fs_body, sample_rate);
    let mut fs_chunk = Vec::new();
    write_chunk(&mut fs_chunk, b"FS  ", &fs_body);
    prop_body.extend_from_slice(&fs_chunk);

    let mut chnl_body = Vec::new();
    let ids = channel_ids(channel_count);
    write_be_u16(&mut chnl_body, ids.len() as u16);
    for id in &ids {
        chnl_body.extend_from_slice(*id);
    }
    let mut chnl_chunk = Vec::new();
    write_chunk(&mut chnl_chunk, b"CHNL", &chnl_body);
    prop_body.extend_from_slice(&chnl_chunk);

    // CMPR sub-chunk: DSD tag + count(1B) + "DSD" string
    // 与 build_dsdiff（DST）的 CMPR 结构一致，仅 tag 从 "DST " 改为 "DSD "
    let mut cmpr_body = Vec::with_capacity(8);
    write_tag(&mut cmpr_body, b"DSD ");
    cmpr_body.push(3); // count
    cmpr_body.extend_from_slice(b"DSD");
    let mut cmpr_chunk = Vec::new();
    write_chunk(&mut cmpr_chunk, b"CMPR", &cmpr_body);
    prop_body.extend_from_slice(&cmpr_chunk);

    // ---- 组装 FRM8 form ----
    // form body = "DSD " + FVER chunk + PROP chunk + DSD chunk
    let mut form_body = Vec::new();
    write_tag(&mut form_body, b"DSD ");

    let mut fver_chunk = Vec::new();
    write_chunk(&mut fver_chunk, b"FVER", &fver_body);
    form_body.extend_from_slice(&fver_chunk);

    let mut prop_chunk = Vec::new();
    write_chunk(&mut prop_chunk, b"PROP", &prop_body);
    form_body.extend_from_slice(&prop_chunk);

    // DSD chunk（raw DSD 字节流，codec = DSD_MSBF）
    let mut dsd_chunk = Vec::new();
    write_chunk(&mut dsd_chunk, b"DSD ", raw_dsd);
    form_body.extend_from_slice(&dsd_chunk);

    // FRM8 header + form_size + body
    let mut out = Vec::with_capacity(12 + form_body.len());
    write_tag(&mut out, b"FRM8");
    write_be_u64(&mut out, form_body.len() as u64);
    out.extend_from_slice(&form_body);
    out
}

// ─────────────────────────────────────────────────────────────────
// DST 帧组装器（移植自 scarletbook_read.c::scarletbook_process_frames）
// ─────────────────────────────────────────────────────────────────

/// DST 帧组装器：跨扇区累积同一个 DST 帧的字节，完成时输出。
///
/// 对应 C 代码中的 `handle->frame` 状态机：
/// - `started`：当前是否有正在累积的帧
/// - `data`：帧字节缓冲
/// - `size`：已累积字节数
/// - `dst_encoded`：是否 DST 编码（决定完成判定方式）
/// - `sector_count`：DST 帧剩余扇区数（每追加一个 audio 包减 1）
/// - `channel_count`：声道数（仅非 DST 用作完成判定）
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

    /// 判断当前帧是否已完整。
    ///
    /// DST：`sector_count == 0`（C 代码 line 820）
    /// 非 DST：`size == channel_count * FRAME_SIZE_64`（C 代码 line 821）
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

    /// 开始新帧：重置状态，从 frame_info 读取 sector_count 和 channel_count。
    ///
    /// 对应 C 代码 line 841-848。
    fn start_new(&mut self, dst_encoded: bool, sector_count: u8, channel_count: u8) {
        self.data.clear();
        self.dst_encoded = dst_encoded;
        self.sector_count = sector_count;
        self.channel_count = channel_count;
        self.started = true;
    }

    /// 追加音频包数据。返回 true 表示追加成功，false 表示溢出（帧被丢弃）。
    ///
    /// 对应 C 代码 line 856-873。
    fn append_packet(&mut self, packet_data: &[u8]) -> bool {
        if self.size() + packet_data.len() > MAX_DST_SIZE {
            // 缓冲溢出：丢弃当前帧
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

    /// 取出已完成帧的字节（consume）。
    fn take_frame(&mut self) -> Vec<u8> {
        let frame = std::mem::take(&mut self.data);
        self.started = false;
        // 重新分配容量供下一帧使用
        self.data = Vec::with_capacity(MAX_DST_SIZE);
        frame
    }
}

/// 解析 audio_frame_info_t 的 channel_count（DST frame_info 第 4 字节）。
///
/// C 代码 `get_channel_count` (scarletbook_read.c line 713-727)：
/// - `channel_bit_2 == 1 && channel_bit_3 == 0` → 6 channels
/// - `channel_bit_2 == 0 && channel_bit_3 == 1` → 5 channels
/// - else → 2 channels
///
/// 字节布局（little-endian，bit 0 = LSB）：
/// - bit 0 = channel_bit_3
/// - bit 1 = channel_bit_2
/// - bits 2-6 = sector_count
/// - bit 7 = channel_bit_1
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

/// 解析 audio_frame_info_t 的 sector_count（DST frame_info 第 4 字节）。
///
/// bits 2-6 = sector_count（5 bits）
fn parse_sector_count(frame_info_byte: u8) -> u8 {
    (frame_info_byte >> 2) & 0x1F
}

/// 从 SACD 扇区流中提取帧（DST 压缩帧 / DSD 3-in-14/16 原始帧）。
///
/// 移植自 `scarletbook_read.c::scarletbook_process_frames`（line 741-921）。
///
/// # 参数
/// - `reader`：ISO 扇区读取器
/// - `start_lsn`：轨道起始 LSN
/// - `length_lsn`：轨道 LSN 长度
/// - `disc_channel_count`：光盘实际声道数（来自 Master TOC area 配置）
///   - DST 模式：未使用（frame_info 内嵌的 channel_count 已足够）
///   - DSD 3-in-14/16 模式：必须使用光盘级声道数，因为 frame_info 不含 channel/sector 字节
///
/// # 返回
/// 成功返回帧字节列表（每帧是一个 `Vec<u8>`）。
/// - DST 模式：每帧为压缩的 DST 帧
/// - DSD 3-in-14/16 模式：每帧为 `channel_count * FRAME_SIZE_64` 字节的原始 DSD
pub fn extract_dst_frames(
    reader: &mut IsoReader,
    start_lsn: u32,
    length_lsn: u32,
    disc_channel_count: u8,
) -> Result<Vec<Vec<u8>>> {
    if length_lsn == 0 {
        bail!("SACD 轨道 length_lsn = 0（无效）");
    }

    // 一次性读取整条轨道的所有扇区（SACD 轨道通常几十 MB，可接受）
    // 对应 C 代码中 `sacd_read_block_raw(sacd, start_lsn, length_lsn, read_buffer)`
    let all_sectors = reader
        .read_sectors(start_lsn as u64, length_lsn as u64)
        .with_context(|| {
            format!(
                "读取 SACD 轨道扇区失败 start_lsn={} length_lsn={}",
                start_lsn, length_lsn
            )
        })?;

    let mut frames: Vec<Vec<u8>> = Vec::new();
    let mut assembler = DstFrameAssembler::new();
    let mut sector_bad_reads = false;

    let num_sectors = (all_sectors.len() / SACD_LSN_SIZE) as u32;
    ensure!(num_sectors > 0, "SACD 轨道扇区数据为空");

    let last_sector_idx = num_sectors - 1;

    for j in 0..num_sectors {
        let sector_start = (j as usize) * SACD_LSN_SIZE;
        let sector_end = sector_start + SACD_LSN_SIZE;
        if sector_end > all_sectors.len() {
            break;
        }
        let sector = &all_sectors[sector_start..sector_end];

        // ---- 1. 解析 audio_frame_header_t（1 字节，little-endian bitfield）----
        // dst_encoded = byte & 0x01
        // frame_info_count = (byte >> 2) & 0x07
        // packet_info_count = (byte >> 5) & 0x07
        let header_byte = sector[0];
        let dst_encoded = (header_byte & 0x01) != 0;
        let frame_info_count = ((header_byte >> 2) & 0x07) as usize;
        let packet_info_count = ((header_byte >> 5) & 0x07) as usize;

        let mut cursor = 1usize; // 跳过 header

        // ---- 2. 解析 audio_packet_info_t[packet_info_count]（每个 2 字节，little-endian manual）----
        // frame_start = (byte[0] >> 7) & 1
        // data_type   = (byte[0] >> 3) & 7
        // packet_length = (byte[0] & 7) << 8 | byte[1]
        if cursor + packet_info_count * 2 > SACD_LSN_SIZE {
            bail!("SACD 扇区 {} packet_info 越界", j);
        }
        let mut packets: Vec<(u8, u8, usize)> = Vec::with_capacity(packet_info_count); // (frame_start, data_type, packet_length)
        for _ in 0..packet_info_count {
            let b0 = sector[cursor];
            let b1 = sector[cursor + 1];
            let frame_start = (b0 >> 7) & 0x01;
            let data_type = (b0 >> 3) & 0x07;
            let packet_length = (((b0 & 0x07) as usize) << 8) | (b1 as usize);
            packets.push((frame_start, data_type, packet_length));
            cursor += 2;
        }

        // ---- 3. 解析 audio_frame_info_t[frame_info_count] ----
        // DST：每条 4 字节（timecode[3] + channel/sector byte[1]）
        // 非 DST：每条 3 字节（仅 timecode[3]）
        let frame_info_size = if dst_encoded { 4 } else { 3 };
        if cursor + frame_info_count * frame_info_size > SACD_LSN_SIZE {
            bail!("SACD 扇区 {} frame_info 越界", j);
        }
        // 仅 DST 需要 sector_count 和 channel_count；保留 frame_info 第 4 字节供后续读取
        let mut frame_infos: Vec<u8> = Vec::with_capacity(frame_info_count);
        for i in 0..frame_info_count {
            let base = cursor + i * frame_info_size;
            if dst_encoded {
                // 第 4 字节是 channel/sector 字节
                frame_infos.push(sector[base + 3]);
            } else {
                // 非 DST 无 channel/sector 字节，用占位值
                frame_infos.push(0);
            }
        }
        cursor += frame_info_count * frame_info_size;

        // ---- 4. packet_info_count 上限校验（C 代码 line 791）----
        if packet_info_count as u8 > MAX_PACKETS_PER_SECTOR {
            warn!(
                sector = j,
                packet_info_count,
                "SACD 扇区 packet_info_count > 7，跳过该扇区"
            );
            sector_bad_reads = true;
            assembler.started = false;
            continue;
        }

        // ---- 5. 遍历 packets，组装帧（C 代码 line 800-888）----
        // frame_info_idx 在每个扇区开始时重置（C 代码 line 800-801）
        let mut frame_info_idx = 0usize;

        for (frame_start, data_type, packet_length) in &packets {
            // 包长度上限校验（C 代码 line 805-809）
            if *packet_length > MAX_PACKET_SIZE {
                sector_bad_reads = true;
                // continue 到下一包（注意：read_buffer_ptr 仍需前进，见下方）
                // 但这里不直接 continue，因为还需要推进 cursor
            }

            match *data_type {
                DATA_TYPE_AUDIO => {
                    if *frame_start == 1 {
                        // 帧起始：检查前一帧是否完整，若完整则输出（C 代码 line 818-826）
                        if assembler.started && assembler.size() > 0 && assembler.is_complete() {
                            let frame = assembler.take_frame();
                            if !frame.is_empty() {
                                frames.push(frame);
                            }
                        }

                        // 开始新帧：从 frame_info[frame_info_idx] 读取 sector_count 和 channel_count
                        // （C 代码 line 841-848）
                        if frame_info_idx < frame_infos.len() {
                            let info_byte = frame_infos[frame_info_idx];
                            let sector_count = parse_sector_count(info_byte);
                            // DST 模式：从 frame_info 解析 channel_count（2/5/6）
                            // DSD 3-in-14/16 模式：frame_info 无 channel 字节，
                            //                使用光盘级 disc_channel_count
                            let channel_count = if dst_encoded {
                                parse_channel_count(info_byte)
                            } else {
                                disc_channel_count
                            };
                            assembler.start_new(dst_encoded, sector_count, channel_count);
                            // frame_info_idx 仅在 frame_start=1 时递增（C 代码 line 852）
                            frame_info_idx += 1;
                        } else {
                            // frame_info 不足：跳过该帧
                            warn!(
                                sector = j,
                                frame_info_idx,
                                frame_info_count,
                                "SACD frame_info 索引越界，跳过帧起始"
                            );
                        }
                    }

                    // 追加音频包数据（C 代码 line 854-873）
                    if assembler.started && *packet_length <= MAX_PACKET_SIZE {
                        if cursor + *packet_length > SACD_LSN_SIZE {
                            warn!(
                                sector = j,
                                cursor,
                                packet_length,
                                "SACD 音频包数据越界，跳过"
                            );
                        } else {
                            let packet_data = &sector[cursor..cursor + *packet_length];
                            if !assembler.append_packet(packet_data) {
                                sector_bad_reads = true;
                                warn!(
                                    sector = j,
                                    size = assembler.size(),
                                    packet_length,
                                    "SACD DST 帧缓冲溢出，丢弃当前帧"
                                );
                            }
                        }
                    }
                }
                DATA_TYPE_SUPPLEMENTARY | DATA_TYPE_PADDING => {
                    // 跳过（C 代码 line 876-878）
                }
                _ => {
                    // 未知类型，跳过
                }
            }

            // read_buffer_ptr 对所有包类型前进（C 代码 line 886）
            cursor += *packet_length;
        }

        // ---- 6. 最后一个扇区：输出残留的完整帧（C 代码 line 896-914）----
        if j == last_sector_idx {
            if assembler.started && assembler.size() > 0 && assembler.is_complete() {
                let frame = assembler.take_frame();
                if !frame.is_empty() {
                    frames.push(frame);
                }
            }
        }
    }

    if frames.is_empty() {
        bail!(
            "SACD DST 帧提取完成但无帧输出（sector_bad_reads={}，num_sectors={}）",
            sector_bad_reads,
            num_sectors
        );
    }

    if sector_bad_reads {
        warn!(
            frames_count = frames.len(),
            num_sectors,
            "SACD DST 帧提取完成，但部分扇区存在坏读"
        );
    } else {
        debug!(
            frames_count = frames.len(),
            num_sectors,
            "SACD DST 帧提取完成"
        );
    }

    Ok(frames)
}

// ─────────────────────────────────────────────────────────────────
// 主入口
// ─────────────────────────────────────────────────────────────────

/// 从 SACD 虚拟路径提取轨道，返回内存 DSDIFF + 光盘元数据。
///
/// # 流程
/// 1. 解析虚拟路径
/// 2. 探测 ISO 获取 SacdDisc
/// 3. 根据 track_num 找到对应 SacdTrack
/// 4. 根据 frame_format 分派：
///    - `Dst` → `build_dsdiff`（DST 压缩格式）
///    - `Dsd3In14` / `Dsd3In16` → `build_dsdiff_raw`（未压缩 DSD 格式）
/// 5. 提取帧（DST / 原始 DSD）
/// 6. 生成 DSDIFF
/// 7. 包装为 Cursor
///
/// # 返回
/// `(Cursor<Vec<u8>>, SacdDisc)` —— Cursor 传给 `AudioReader::new()`，
/// SacdDisc 供调用方提取 title/artist/album 元数据（DSDIFF 无 tag）。
#[allow(dead_code)] // 当前解码路径未消费，保留供未来 DST 帧提取 API
pub fn extract_track_to_dsdiff_cursor(source: &str) -> Result<(Cursor<Vec<u8>>, SacdDisc)> {
    // 1. 解析虚拟路径
    let vp = parse_sacd_virtual_path(source).ok_or_else(|| {
        anyhow!("不是 SACD 虚拟路径（期望 7 字段，第 2 字段以 Track 开头）: {}", source)
    })?;

    // 2. 探测 ISO
    let iso_path = Path::new(&vp.iso_path);
    let disc = probe_sacd_iso(iso_path)
        .with_context(|| format!("探测 SACD ISO 失败: {}", vp.iso_path))?;

    // 3. 找到对应轨道
    let track = disc
        .tracks
        .iter()
        .find(|t| t.track_num == vp.track_num)
        .ok_or_else(|| {
            anyhow!(
                "SACD 轨道 {} 未找到（共 {} 轨）",
                vp.track_num,
                disc.track_count
            )
        })?;

    // 4. 提取帧（DST 压缩 / DSD 3-in-14/16 未压缩）
    // SACD 采样率固定 2.8224 MHz（DSD64），声道数取光盘元数据
    //  - DST 模式：frame_info 内嵌 channel_count，disc.channel_count 仅作兜底
    //  - DSD 3-in-14/16 模式：frame_info 无 channel 字节，必须用 disc.channel_count
    let mut reader = IsoReader::open(iso_path)
        .with_context(|| format!("打开 SACD ISO 失败: {}", vp.iso_path))?;
    let start_lsn = track.start_lsn;
    let length_lsn = track.length_lsn;
    let frames = extract_dst_frames(&mut reader, start_lsn, length_lsn, disc.channel_count)
        .with_context(|| format!("提取 SACD 帧失败 track={}", vp.track_num))?;

    // 5. 根据帧格式生成 DSDIFF
    let dsdiff = match disc.frame_format {
        FrameFormat::Dst => build_dsdiff(
            SACD_SAMPLING_FREQUENCY,
            disc.channel_count,
            &frames,
        ),
        FrameFormat::Dsd3In14 | FrameFormat::Dsd3In16 => {
            // 将多帧 Vec<u8> 拼接为单一连续 raw DSD 字节流
            // 每帧长度 = channel_count * FRAME_SIZE_64 = channel_count * 4704
            let mut raw_dsd: Vec<u8> = Vec::with_capacity(frames.iter().map(|f| f.len()).sum());
            for f in &frames {
                raw_dsd.extend_from_slice(f);
            }
            build_dsdiff_raw(SACD_SAMPLING_FREQUENCY, disc.channel_count, &raw_dsd)
        }
    };

    info!(
        iso_path = %vp.iso_path,
        track_num = vp.track_num,
        frame_format = ?disc.frame_format,
        frames = frames.len(),
        channel_count = disc.channel_count,
        dsdiff_size = dsdiff.len(),
        "SACD 轨道已提取为内存 DSDIFF"
    );

    // P4 DEBUG：将生成的 DSDIFF 写入磁盘以便 ffprobe 调试
    // 启用方式：export TINYLMS_DUMP_SACD_DSDIFF=/tmp/sacd-dump
    if let Ok(dir) = std::env::var("TINYLMS_DUMP_SACD_DSDIFF") {
        let filename = format!(
            "{}/track{:02}_{:?}_{}ch.dff",
            dir, vp.track_num, disc.frame_format, disc.channel_count
        );
        match std::fs::write(&filename, &dsdiff) {
            Ok(_) => info!(path = %filename, "DEBUG: DSDIFF 已写入磁盘供 ffprobe 调试"),
            Err(e) => warn!(path = %filename, error = %e, "DEBUG: 写入 DSDIFF 失败"),
        }
    }

    // 6. 包装为 Cursor
    Ok((Cursor::new(dsdiff), disc))
}

/// 从 SACD 虚拟路径提取轨道，写入**临时文件**而非内存 Cursor。
///
/// # 背景（OOM 修复）
///
/// `extract_track_to_dsdiff_cursor` 将整个轨道的 DST 帧提取到内存
/// （`Vec<Vec<u8>>`），再构建完整 DSDIFF（`Vec<u8>`），最后包装为
/// `Cursor<Vec<u8>>` 传给 `AudioReader::new()`。对于长轨道（如 267 秒
/// DSD64 立体声），DST 压缩数据约 50-100MB，加上 DSDIFF 构建过程中的
/// 多个中间 Vec 副本和 FFmpeg 解码缓冲区，导致 node 进程虚拟内存膨胀
/// 到 14GB，触发 Linux OOM Killer。
///
/// 本函数将 DSDIFF 写入 `NamedTempFile`（系统临时目录），返回文件句柄。
/// `NamedTempFile` 实现 `Read + Seek + Send + 'static`，可直接传给
/// `AudioReader::new()`。当 `AudioReader` 被 drop 时，`NamedTempFile`
/// 的 Drop impl 自动删除临时文件，无需手动清理。
///
/// # 内存优化
///
/// 1. `build_dsdiff` / `build_dsdiff_raw` 返回 `Vec<u8>` 后立即写入文件并 drop
/// 2. `frames: Vec<Vec<u8>>` 在 `build_dsdiff` 返回后立即 drop
/// 3. FFmpeg 通过 `File` 的 `lseek()` 随机访问，不需要将整个文件加载到内存
///
/// # 返回
/// `(NamedTempFile, SacdDisc)` —— NamedTempFile 传给 `AudioReader::new()`，
/// SacdDisc 供调用方提取 title/artist/album 元数据。
pub fn extract_track_to_dsdiff_file(source: &str) -> Result<(NamedTempFile, SacdDisc)> {
    // 1. 解析虚拟路径
    let vp = parse_sacd_virtual_path(source).ok_or_else(|| {
        anyhow!("不是 SACD 虚拟路径（期望 7 字段，第 2 字段以 Track 开头）: {}", source)
    })?;

    // 2. 探测 ISO
    let iso_path = Path::new(&vp.iso_path);
    let disc = probe_sacd_iso(iso_path)
        .with_context(|| format!("探测 SACD ISO 失败: {}", vp.iso_path))?;

    // 3. 找到对应轨道
    let track = disc
        .tracks
        .iter()
        .find(|t| t.track_num == vp.track_num)
        .ok_or_else(|| {
            anyhow!(
                "SACD 轨道 {} 未找到（共 {} 轨）",
                vp.track_num,
                disc.track_count
            )
        })?;

    // 4. 提取帧（DST 压缩 / DSD 3-in-14/16 未压缩）
    let mut reader = IsoReader::open(iso_path)
        .with_context(|| format!("打开 SACD ISO 失败: {}", vp.iso_path))?;
    let start_lsn = track.start_lsn;
    let length_lsn = track.length_lsn;
    let frames = extract_dst_frames(&mut reader, start_lsn, length_lsn, disc.channel_count)
        .with_context(|| format!("提取 SACD 帧失败 track={}", vp.track_num))?;

    // 5. 根据帧格式生成 DSDIFF（Vec<u8>，临时存在内存中）
    let dsdiff = match disc.frame_format {
        FrameFormat::Dst => build_dsdiff(
            SACD_SAMPLING_FREQUENCY,
            disc.channel_count,
            &frames,
        ),
        FrameFormat::Dsd3In14 | FrameFormat::Dsd3In16 => {
            let mut raw_dsd: Vec<u8> = Vec::with_capacity(frames.iter().map(|f| f.len()).sum());
            for f in &frames {
                raw_dsd.extend_from_slice(f);
            }
            build_dsdiff_raw(SACD_SAMPLING_FREQUENCY, disc.channel_count, &raw_dsd)
        }
    };

    let dsdiff_size = dsdiff.len();
    let frames_count = frames.len();

    // 6. 立即释放 frames Vec，减少峰值内存（frames + dsdiff 同时在内存中）
    drop(frames);

    // 7. 创建临时文件并写入 DSDIFF
    let mut tmp = tempfile::Builder::new()
        .prefix("tinylms-sacd-")
        .suffix(".dff")
        .tempfile()
        .with_context(|| "创建 SACD 临时文件失败")?;

    tmp.write_all(&dsdiff)
        .with_context(|| "写入 SACD DSDIFF 到临时文件失败")?;
    tmp.as_file().sync_all()
        .with_context(|| "sync SACD 临时文件失败")?;

    // 8. 释放 dsdiff Vec，此后只有临时文件在磁盘上
    drop(dsdiff);

    // 9. seek 回文件开头，供 AudioReader 从头读取
    tmp.seek(SeekFrom::Start(0))
        .with_context(|| "seek SACD 临时文件到开头失败")?;

    info!(
        iso_path = %vp.iso_path,
        track_num = vp.track_num,
        frame_format = ?disc.frame_format,
        frames = frames_count,
        channel_count = disc.channel_count,
        dsdiff_size,
        temp_path = %tmp.path().display(),
        "SACD 轨道已提取为临时文件 DSDIFF（OOM 修复：避免内存 Cursor）",
    );

    // P4 DEBUG：将生成的 DSDIFF 写入磁盘以便 ffprobe 调试
    // 启用方式：export TINYLMS_DUMP_SACD_DSDIFF=/tmp/sacd-dump
    if let Ok(dir) = std::env::var("TINYLMS_DUMP_SACD_DSDIFF") {
        let filename = format!(
            "{}/track{:02}_{:?}_{}ch.dff",
            dir, vp.track_num, disc.frame_format, disc.channel_count
        );
        // 从临时文件复制到调试路径
        match std::fs::copy(tmp.path(), &filename) {
            Ok(_) => info!(path = %filename, "DEBUG: DSDIFF 已复制到磁盘供 ffprobe 调试"),
            Err(e) => warn!(path = %filename, error = %e, "DEBUG: 复制 DSDIFF 失败"),
        }
    }

    Ok((tmp, disc))
}

// ─────────────────────────────────────────────────────────────────
// 测试
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sacd_virtual_path_valid() {
        let source = "/music/sacd.iso|Track01|307.293333|0|23047|1000|25000";
        let vp = parse_sacd_virtual_path(source).expect("应解析成功");
        assert_eq!(vp.iso_path, "/music/sacd.iso");
        assert_eq!(vp.track_num, 1);
        assert!((vp.duration_secs - 307.293333).abs() < 1e-6);
        assert_eq!(vp.start_frames, 0);
        assert_eq!(vp.duration_frames, 23047);
        assert_eq!(vp.start_lsn, 1000);
        assert_eq!(vp.length_lsn, 25000);
    }

    #[test]
    fn test_parse_sacd_virtual_path_invalid_cue() {
        // CUE 虚拟路径：5 字段，第 2 字段是数字秒数（不是 TrackXX）
        let cue_source = "/music/track.flac|10.5|200.5|12345|67890";
        assert!(parse_sacd_virtual_path(cue_source).is_none());
    }

    #[test]
    fn test_parse_sacd_virtual_path_invalid_plain() {
        // 普通路径：无 `|` 分隔
        assert!(parse_sacd_virtual_path("/music/track.flac").is_none());
    }

    #[test]
    fn test_parse_channel_count() {
        // 2 channels：channel_bit_2=0, channel_bit_3=0 → byte=0x00
        assert_eq!(parse_channel_count(0x00), 2);
        // 6 channels：channel_bit_2=1, channel_bit_3=0 → byte=0x02
        assert_eq!(parse_channel_count(0x02), 6);
        // 5 channels：channel_bit_2=0, channel_bit_3=1 → byte=0x01
        assert_eq!(parse_channel_count(0x01), 5);
    }

    #[test]
    fn test_parse_sector_count() {
        // sector_count 在 bits 2-6
        // byte=0x04 → sector_count=1
        assert_eq!(parse_sector_count(0x04), 1);
        // byte=0x08 → sector_count=2
        assert_eq!(parse_sector_count(0x08), 2);
        // byte=0x7C → sector_count=31（最大值）
        assert_eq!(parse_sector_count(0x7C), 31);
    }

    #[test]
    fn test_build_dsdiff_structure() {
        // 构造 2 个 DST 帧的 DSDIFF，验证基本结构
        let frame1 = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let frame2 = vec![0xCA, 0xFE]; // 奇数长度，应补 padding
        let frames = vec![frame1, frame2];

        let dsdiff = build_dsdiff(SACD_SAMPLING_FREQUENCY, 2, &frames);

        // FRM8 header
        assert_eq!(&dsdiff[0..4], b"FRM8");
        // form_size (8B BE)
        let form_size = u64::from_be_bytes(dsdiff[4..12].try_into().unwrap());
        assert_eq!(form_size as usize, dsdiff.len() - 12);
        // DSD  form type
        assert_eq!(&dsdiff[12..16], b"DSD ");

        // 验证包含 FVER / PROP / DST  chunk 标签
        let s = String::from_utf8_lossy(&dsdiff);
        assert!(s.contains("FVER"));
        assert!(s.contains("PROP"));
        assert!(s.contains("SND "));
        assert!(s.contains("FS  "));
        assert!(s.contains("CHNL"));
        assert!(s.contains("CMPR"));
        assert!(s.contains("DST "));
        assert!(s.contains("DSTF"));
        assert!(s.contains("FRTE"));
    }

    #[test]
    fn test_build_dsdiff_frame_count() {
        let frames: Vec<Vec<u8>> = (0..10).map(|i| vec![i as u8; 100]).collect();
        let dsdiff = build_dsdiff(SACD_SAMPLING_FREQUENCY, 2, &frames);

        // 找到 FRTE chunk，验证 num_frames
        // 使用字节搜索，避免 String::from_utf8_lossy 替换无效字节为 U+FFFD
        // 导致字符位置 != 字节位置（曾引发 size=393216 的假阳性失败）
        let frte_pos = dsdiff
            .windows(4)
            .position(|w| w == b"FRTE")
            .expect("应有 FRTE chunk");
        let frte_offset = frte_pos + 4; // 跳过 "FRTE"
        let size = u64::from_be_bytes(
            dsdiff[frte_offset..frte_offset + 8].try_into().unwrap(),
        );
        assert_eq!(size, 6); // num_frames(4) + frame_rate(2)
        let num_frames = u32::from_be_bytes(
            dsdiff[frte_offset + 8..frte_offset + 12]
                .try_into()
                .unwrap(),
        );
        assert_eq!(num_frames, 10);
        let frame_rate = u16::from_be_bytes(
            dsdiff[frte_offset + 12..frte_offset + 14]
                .try_into()
                .unwrap(),
        );
        assert_eq!(frame_rate, SACD_FRAME_RATE as u16);
    }

    #[test]
    fn test_dst_frame_assembler_complete_dst() {
        let mut asm = DstFrameAssembler::new();
        // DST 帧，sector_count=2，channel_count=2
        asm.start_new(true, 2, 2);
        // 追加第一个包：sector_count 减到 1
        assert!(asm.append_packet(&[0u8; 100]));
        assert_eq!(asm.sector_count, 1);
        assert!(!asm.is_complete());
        // 追加第二个包：sector_count 减到 0，帧完整
        assert!(asm.append_packet(&[0u8; 100]));
        assert_eq!(asm.sector_count, 0);
        assert!(asm.is_complete());
        let frame = asm.take_frame();
        assert_eq!(frame.len(), 200);
        assert!(!asm.started);
    }

    #[test]
    fn test_dst_frame_assembler_overflow() {
        let mut asm = DstFrameAssembler::new();
        asm.start_new(true, 1, 2);
        // 追加超过 MAX_DST_SIZE 的包应失败
        let big = vec![0u8; MAX_DST_SIZE + 1];
        assert!(!asm.append_packet(&big));
        assert!(!asm.started);
    }

    #[test]
    fn test_dst_frame_assembler_complete_dsd_raw() {
        // 非 DST（DSD 3-in-14/16）：用 disc_channel_count=2, 帧大小 = 2 * 4704 = 9408
        let mut asm = DstFrameAssembler::new();
        asm.start_new(false, 0, 2);
        // 追加 4704 字节，帧未完成
        assert!(asm.append_packet(&vec![0u8; 4704]));
        assert!(!asm.is_complete());
        // 再追加 4704 字节，帧完成
        assert!(asm.append_packet(&vec![0u8; 4704]));
        assert!(asm.is_complete());
        let frame = asm.take_frame();
        assert_eq!(frame.len(), 9408);
    }

    #[test]
    fn test_build_dsdiff_raw_structure() {
        // 构造 3 帧（每帧 2ch * 4704 = 9408 字节）的 raw DSD
        let frames_total = 3usize;
        let raw_dsd = vec![0xABu8; frames_total * 2 * FRAME_SIZE_64];
        let dsdiff = build_dsdiff_raw(SACD_SAMPLING_FREQUENCY, 2, &raw_dsd);

        // FRM8 header
        assert_eq!(&dsdiff[0..4], b"FRM8");
        // DSD form type
        assert_eq!(&dsdiff[12..16], b"DSD ");

        // 必须包含 FVER / PROP / SND / FS / CHNL / CMPR / DSD
        // CMPR 是 FFmpeg iff.c 识别 codec（AV_CODEC_ID_DSD_MSBF）的必需字段，
        // 缺失会导致 "Invalid data found when processing input" 错误。
        // 不应包含 FRTE / DSTF：
        // - FRTE 只在 build_dsdiff（DST 格式）中生成（num_frames + frame_rate）
        // - DSTF 只在 DST 格式中出现，raw DSD 用单个 DSD chunk 承载字节流
        let s = String::from_utf8_lossy(&dsdiff);
        assert!(s.contains("FVER"), "原始 DSDIFF 应包含 FVER chunk");
        assert!(s.contains("PROP"), "原始 DSDIFF 应包含 PROP chunk");
        assert!(s.contains("SND "), "原始 DSDIFF 应包含 SND chunk");
        assert!(s.contains("FS  "), "原始 DSDIFF 应包含 FS chunk");
        assert!(s.contains("CHNL"), "原始 DSDIFF 应包含 CHNL chunk");
        assert!(s.contains("CMPR"), "原始 DSDIFF 必须包含 CMPR chunk（FFmpeg iff.c 识别 codec 必需）");
        assert!(s.contains("DSD "), "原始 DSDIFF 应包含 DSD chunk");
        assert!(!s.contains("FRTE"), "原始 DSDIFF 不应包含 FRTE chunk（仅 DST 格式生成）");
        assert!(!s.contains("DSTF"), "原始 DSDIFF 不应包含 DSTF chunk");

        // 验证 CMPR chunk 的 body：tag="DSD " + count(1B=3) + "DSD"
        let cmpr_pos = dsdiff
            .windows(4)
            .position(|w| w == b"CMPR")
            .expect("应有 CMPR chunk");
        let cmpr_size_offset = cmpr_pos + 4;
        let cmpr_size = u64::from_be_bytes(
            dsdiff[cmpr_size_offset..cmpr_size_offset + 8].try_into().unwrap(),
        );
        assert_eq!(cmpr_size, 8); // "DSD "(4) + count(1) + "DSD"(3) = 8 字节
        let cmpr_body_offset = cmpr_size_offset + 8;
        assert_eq!(&dsdiff[cmpr_body_offset..cmpr_body_offset + 4], b"DSD ");
        assert_eq!(dsdiff[cmpr_body_offset + 4], 3); // count
        assert_eq!(&dsdiff[cmpr_body_offset + 5..cmpr_body_offset + 8], b"DSD");

        // 验证 DSD chunk（form body 内最后一个 chunk）承载完整的 raw DSD 字节流
        // DSD chunk tag 出现在两处：(1) form type "DSD " 在 offset 12；(2) raw DSD chunk "DSD "
        // 位于 form body 末尾。后者承载 raw_dsd 字节，需定位到该 chunk 并验证 size + body。
        let dsd_chunk_pos = dsdiff
            .windows(4)
            .rposition(|w| w == b"DSD ")
            .expect("应有 DSD chunk（raw DSD 字节流）");
        // rposition 返回最后一次匹配位置，对应 form body 末尾的 DSD chunk
        let dsd_size_offset = dsd_chunk_pos + 4;
        let dsd_size = u64::from_be_bytes(
            dsdiff[dsd_size_offset..dsd_size_offset + 8].try_into().unwrap(),
        );
        assert_eq!(dsd_size as usize, raw_dsd.len());
        let dsd_body_offset = dsd_size_offset + 8;
        assert_eq!(
            &dsdiff[dsd_body_offset..dsd_body_offset + raw_dsd.len()],
            raw_dsd.as_slice(),
            "DSD chunk body 应与输入 raw_dsd 完全一致"
        );
    }
}
