//! ScarletBook 规范解析器。
//!
//! 移植自 `tinyLMS-old/src/core/library/sacd/libsacd/scarletbook_read.c`，
//! 仅保留元数据提取所需的最小路径：
//! - Master TOC（LSN 510，10 扇区）
//! - Area TOC（TWOCHTOC / MULCHTOC）
//! - SACDTRL1（轨道 LSN 偏移表）
//! - SACDTRL2（轨道时间表，用于时长计算）
//! - SACDText（专辑/艺术家文本）
//! - SACDTTxt（轨道级文本）
//!
//! 解码所需 DST / DSD 帧读取逻辑不在本模块，
//! 后续在 `decoder.rs` 集成阶段通过 FFmpeg DST codec + IsoReader 直接读扇区实现。
//!
//! 所有整数字段按大端序解析（ScarletBook 规范定义，与 `scarletbook_read.c` 中
//! `bswap` 系列调用对齐）。

use anyhow::{anyhow, bail, ensure, Context, Result};
use std::path::Path;

use super::iso_reader::{IsoReader, SACD_LSN_SIZE};

// ─────────────────────────────────────────────────────────────────
// 智能文本解码（修复专辑名乱码）
// ─────────────────────────────────────────────────────────────────
// ScarletBook 规范定义 SACDText 字符集通过 locale_table.charset_code 指定，
// 但实际镜像可能不严格遵守。常见编码：
//   - UTF-8（规范允许的 ASCII 子集）
//   - UTF-16BE（ ScarletBook 默认 Unicode 编码）
//   - Shift-JIS（日本 SACD 常见）
//   - Latin-1 / Windows-1252（欧美 SACD）
//
// 解码策略：
//   1. 检测 UTF-16 BOM (0xFEFF / 0xFFFE) → 对应 UTF-16BE/LE
//   2. 尝试 UTF-8（严格模式，无 replacement char）
//   3. 检测 UTF-16BE 模式（偶数长度 + 奇数位置字节多为 0x00）
//   4. 尝试 Shift-JIS
//   5. Fallback 到 Latin-1（不会失败，至少不乱码）
pub fn decode_sacd_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    // 1. BOM 检测
    if bytes.len() >= 2 {
        if bytes[0] == 0xFE && bytes[1] == 0xFF {
            // UTF-16BE BOM
            return decode_utf16_be(&bytes[2..]);
        }
        if bytes[0] == 0xFF && bytes[1] == 0xFE {
            // UTF-16LE BOM
            return decode_utf16_le(&bytes[2..]);
        }
    }

    // 2. 尝试 UTF-8（严格）
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_owned();
    }

    // 3. 启发式检测 UTF-16（无 BOM）
    //    ScarletBook 规范默认 UTF-16BE，但实际镜像可能用 LE。
    //    UTF-16BE ASCII 字符：[0x00, XX]（零字节在前）
    //    UTF-16LE ASCII 字符：[XX, 0x00]（零字节在后）
    //    在 UTF-8 / Shift-JIS / Latin-1 中 0x00 不会出现在文本中间，
    //    所以只要看到 [0x00, printable] 模式即可判定为 UTF-16。
    if bytes.len() % 2 == 0 && bytes.len() >= 2 {
        let total_pairs = bytes.len() / 2;
        let be_pattern: usize = bytes
            .chunks_exact(2)
            .filter(|c| c[0] == 0 && c[1].is_ascii() && c[1] != 0)
            .count();
        let le_pattern: usize = bytes
            .chunks_exact(2)
            .filter(|c| c[1] == 0 && c[0].is_ascii() && c[0] != 0)
            .count();
        // CJK 字符在 UTF-16 中两字节都非零，不匹配上述任一模式，
        // 故阈值降低到 20%（混合 CJK + ASCII 文本也能命中）
        if be_pattern > 0 && be_pattern >= le_pattern && be_pattern * 5 >= total_pairs {
            return decode_utf16_be(bytes);
        }
        if le_pattern > 0 && le_pattern > be_pattern && le_pattern * 5 >= total_pairs {
            return decode_utf16_le(bytes);
        }
    }

    // 4. 启发式区分 Shift-JIS / Big5 / GBK
    //    Shift-JIS 半角片假名占 0xA1-0xDF 单字节；Big5/GBK 不使用此范围作为单字节。
    //    若文本含较多 0xA1-0xDF 单字节字符，优先 Shift-JIS；否则优先 Big5/GBK。
    let sjis_kana_count = bytes
        .iter()
        .filter(|&&b| (0xA1..=0xDF).contains(&b))
        .count();
    let prefer_sjis = sjis_kana_count > 0 && sjis_kana_count * 4 >= bytes.len();

    if prefer_sjis {
        let (sjis_s, _, sjis_had_errors) = encoding_rs::SHIFT_JIS.decode(bytes);
        if !sjis_had_errors {
            return sjis_s.into_owned();
        }
        // Shift-JIS 失败则继续尝试 Big5/GBK
    }

    // 5. 尝试 Big5（繁体中文，港台 SACD 常见）
    let (big5_s, _, big5_had_errors) = encoding_rs::BIG5.decode(bytes);
    if !big5_had_errors {
        return big5_s.into_owned();
    }

    // 6. 尝试 GBK（简体中文，GB2312 超集）
    let (gbk_s, _, gbk_had_errors) = encoding_rs::GBK.decode(bytes);
    if !gbk_had_errors {
        return gbk_s.into_owned();
    }

    // 7. 若前面偏好 Shift-JIS 失败，此处再尝试（fallback）
    if !prefer_sjis {
        let (sjis_s, _, sjis_had_errors) = encoding_rs::SHIFT_JIS.decode(bytes);
        if !sjis_had_errors {
            return sjis_s.into_owned();
        }
    }

    // 8. Fallback：Windows-1252（encoding_rs 中 ISO-8859-1 的等价编码，不会失败）
    //    每字节直接映射到 Unicode 码点 0x00-0xFF
    let (latin1_s, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
    latin1_s.into_owned()
}

fn decode_utf16_be(bytes: &[u8]) -> String {
    let (s, _, _) = encoding_rs::UTF_16BE.decode(bytes);
    s.trim_start_matches('\u{FEFF}').to_owned()
}

fn decode_utf16_le(bytes: &[u8]) -> String {
    let (s, _, _) = encoding_rs::UTF_16LE.decode(bytes);
    s.trim_start_matches('\u{FEFF}').to_owned()
}

// ─────────────────────────────────────────────────────────────────
// 常量（与 scarletbook.h 严格对齐）
// ─────────────────────────────────────────────────────────────────

/// Master TOC 起始 LSN
pub const START_OF_MASTER_TOC: u64 = 510;
/// Master TOC 占用扇区数
pub const MASTER_TOC_LEN: u64 = 10;
/// SACD 采样率（DSD64 = 2.8224 MHz）
pub const SACD_SAMPLING_FREQUENCY: u32 = 2_822_400;
/// SACD 帧率（帧/秒）
pub const SACD_FRAME_RATE: u32 = 75;
/// Master TOC 中的 locale 表最大语言数
#[allow(unused)]
const MAX_LANGUAGE_COUNT: usize = 8;
/// Area TOC 中的 locale 表最大语言数
#[allow(unused)]
const AREA_MAX_LANGUAGES: usize = 10;
/// 轨道表最大轨道数（SACDTRL1/2 固定 255 槽位）
const MAX_TRACKS: usize = 255;

// ─────────────────────────────────────────────────────────────────
// 帧格式枚举
// ─────────────────────────────────────────────────────────────────

/// ScarletBook 帧格式（area_toc.frame_format 字段）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameFormat {
    /// DST 压缩帧
    Dst = 0,
    /// DSD 3-in-14（未压缩）
    Dsd3In14 = 2,
    /// DSD 3-in-16（未压缩）
    Dsd3In16 = 3,
}

impl FrameFormat {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Dst),
            2 => Some(Self::Dsd3In14),
            3 => Some(Self::Dsd3In16),
            _ => None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// 高层 SACD 元数据
// ─────────────────────────────────────────────────────────────────

/// 单个 SACD 轨道的元数据（对应 `SacdParser.cpp` 提取的字段集合）。
#[derive(Debug, Clone)]
pub struct SacdTrack {
    /// 轨道号（1-based）
    pub track_num: u32,
    /// 标题（SACDTTxt track_type_title）
    pub title: Option<String>,
    /// 艺术家（SACDTTxt track_type_performer）
    pub artist: Option<String>,
    /// ISRC（SACD_IGL 中对应槽位，可能为空字符串）
    pub isrc: Option<String>,
    /// 起始帧（TIME_FRAMECOUNT(start)，75 fps）
    pub start_frames: u32,
    /// 时长帧（TIME_FRAMECOUNT(duration)，75 fps）
    pub duration_frames: u32,
    /// 时长（秒）= duration_frames / 75
    pub duration_secs: f64,
    /// 起始 LSN（来自 SACDTRL1 track_start_lsn）
    pub start_lsn: u32,
    /// 长度 LSN（来自 SACDTRL1：下一轨 start_lsn - 当前 start_lsn，末轨用 track_end）
    pub length_lsn: u32,
}

/// SACD 光盘元数据（专辑级 + 轨道列表）。
#[derive(Debug, Clone)]
pub struct SacdDisc {
    /// 专辑标题（master_text.album_title）
    pub album_title: Option<String>,
    /// 专辑艺术家（master_text.album_artist）
    pub album_artist: Option<String>,
    /// 专辑发行商
    pub album_publisher: Option<String>,
    /// 专辑版权
    pub album_copyright: Option<String>,
    /// 光盘目录号
    pub disc_catalog_number: String,
    /// 帧格式（DST / DSD 3-in-14 / DSD 3-in-16）
    pub frame_format: FrameFormat,
    /// 声道数（2 = 立体声，5/6 = 多声道）
    pub channel_count: u8,
    /// 采样率（始终 2_822_400）
    pub sample_rate: u32,
    /// 总轨道数
    pub track_count: u8,
    /// 选定区域（"twoch" 或 "mulch"）
    pub area_type: String,
    /// 轨道列表
    pub tracks: Vec<SacdTrack>,
}

// ─────────────────────────────────────────────────────────────────
// 字节序读取辅助
// ─────────────────────────────────────────────────────────────────

#[inline]
fn read_be_u16(buf: &[u8], offset: usize) -> Result<u16> {
    ensure!(
        offset + 2 <= buf.len(),
        "read_be_u16 越界 offset={} len={}",
        offset,
        buf.len()
    );
    Ok(u16::from_be_bytes([buf[offset], buf[offset + 1]]))
}

#[inline]
fn read_be_u32(buf: &[u8], offset: usize) -> Result<u32> {
    ensure!(
        offset + 4 <= buf.len(),
        "read_be_u32 越界 offset={} len={}",
        offset,
        buf.len()
    );
    Ok(u32::from_be_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ]))
}

/// 读取以空格或 NUL 填充的固定长度字符串。
#[allow(dead_code)] // 当前解析器未消费，保留供 ScarletBook 文本字段读取
fn read_fixed_string(buf: &[u8], offset: usize, len: usize) -> Result<String> {
    ensure!(
        offset + len <= buf.len(),
        "read_fixed_string 越界 offset={} len={}",
        offset,
        buf.len()
    );
    let slice = &buf[offset..offset + len];
    // 去除尾部 0x00 / 0x20
    let end = slice
        .iter()
        .position(|&b| b == 0 || b == 0x20)
        .unwrap_or(len);
    Ok(decode_sacd_text(&slice[..end]))
}

// ─────────────────────────────────────────────────────────────────
// Master TOC 解析
// ─────────────────────────────────────────────────────────────────

/// Master TOC 关键字段（仅提取元数据探测所需）。
struct MasterToc {
    /// 区域 1（两声道）TOC 起始 LSN
    area_1_toc_1_start: u32,
    /// 区域 1 TOC 大小（扇区数）
    area_1_toc_size: u16,
    /// 区域 2（多声道）TOC 起始 LSN
    area_2_toc_1_start: u32,
    /// 区域 2 TOC 大小（扇区数）
    area_2_toc_size: u16,
}

/// 解析 Master TOC（位于 LSN 510，10 扇区）。
fn parse_master_toc(reader: &mut IsoReader) -> Result<MasterToc> {
    let buf = reader
        .read_sectors(START_OF_MASTER_TOC, MASTER_TOC_LEN)
        .context("读取 Master TOC 失败")?;
    // 校验 id == "SACDMTOC"
    if buf.len() < 8 || &buf[0..8] != b"SACDMTOC" {
        bail!("Master TOC id 校验失败，不是合法 SACD 镜像");
    }
    // master_toc_t 字段偏移（按 scarletbook.h 结构体定义累加）：
    //   id[8]                    offset 0
    //   version{major,minor}     offset 8 (2 bytes)
    //   reserved01[6]            offset 10
    //   album_set_size(u16)      offset 16
    //   album_sequence_number    offset 18
    //   reserved02[4]            offset 20
    //   album_catalog_number[16] offset 24
    //   album_genre[4]           offset 40 (genre_table_t = 4 bytes each = 16)
    //   reserved03[8]            offset 56
    //   area_1_toc_1_start(u32)  offset 64
    //   area_1_toc_2_start(u32)  offset 68
    //   area_2_toc_1_start(u32)  offset 72
    //   area_2_toc_2_start(u32)  offset 76
    //   disc_type_flags(u8)      offset 80
    //   reserved04[3]            offset 81
    //   area_1_toc_size(u16)     offset 84
    //   area_2_toc_size(u16)     offset 86
    let area_1_toc_1_start = read_be_u32(&buf, 64)?;
    let area_2_toc_1_start = read_be_u32(&buf, 72)?;
    let area_1_toc_size = read_be_u16(&buf, 84)?;
    let area_2_toc_size = read_be_u16(&buf, 86)?;
    Ok(MasterToc {
        area_1_toc_1_start,
        area_1_toc_size,
        area_2_toc_1_start,
        area_2_toc_size,
    })
}

// ─────────────────────────────────────────────────────────────────
// Master SACDText 解析
// ─────────────────────────────────────────────────────────────────

/// 从 Master TOC 数据块中提取 SACDText 块（专辑/艺术家文本）。
///
/// Master TOC 区域共 10 扇区。第 1 扇区是 `master_toc_t` 本体；
/// 后续扇区是若干个标识块，其中 `SACDText` 块包含专辑/艺术家文本指针。
///
/// 参考 `scarletbook_read.c::scarletbook_read_master_toc` 的块遍历逻辑。
fn parse_master_text(
    reader: &mut IsoReader,
    _master_toc: &MasterToc,
) -> Result<(Option<String>, Option<String>, Option<String>, Option<String>)> {
    let buf = reader
        .read_sectors(START_OF_MASTER_TOC, MASTER_TOC_LEN)
        .context("重读 Master TOC 区域以提取 SACDText 失败")?;

    // master_toc_t 之后开始遍历：从第 1 扇区起（offset = SACD_LSN_SIZE）
    let mut p = SACD_LSN_SIZE;
    let end = MASTER_TOC_LEN as usize * SACD_LSN_SIZE;

    // 先找到 SACDText 块；记录 album_title/artist 位置
    let mut text_block: Option<(usize, usize)> = None; // (block_offset, data_offset_in_block)
    while p + SACD_LSN_SIZE <= end {
        if &buf[p..p + 8] == b"SACDText" {
            // master_sacd_text_t:
            //   id[8]                         offset 0
            //   reserved[8]                   offset 8
            //   album_title_position(u16)     offset 16
            //   album_artist_position(u16)    offset 18
            //   album_publisher_position(u16) offset 20
            //   album_copyright_position(u16) offset 22
            //   ... 8 个 phonetic/disc 位置 ...
            //   data[2000]                    offset 48
            text_block = Some((p, p + 48));
            break;
        }
        p += SACD_LSN_SIZE;
    }

    let Some((block_off, _data_off)) = text_block else {
        // 无 SACDText 块：返回全 None（合法但少见）
        return Ok((None, None, None, None));
    };

    let album_title_pos = read_be_u16(&buf, block_off + 16)? as usize;
    let album_artist_pos = read_be_u16(&buf, block_off + 18)? as usize;
    let album_publisher_pos = read_be_u16(&buf, block_off + 20)? as usize;
    let album_copyright_pos = read_be_u16(&buf, block_off + 22)? as usize;

    // position 是相对于 master_sacd_text_t 结构体起始的偏移（与 scarletbook_read.c 一致：
    // `(char *) master_text + position`），不是相对于 data[] 字段。
    // data[] 起始位于 offset 48，故有效 position 值应 >= 48（或为 0 表示无此字段）。
    // position 为 0 表示该字段不存在。
    let read_text = |pos: usize| -> Option<String> {
        if pos == 0 || pos < 48 {
            return None;
        }
        let abs = block_off + pos;
        if abs >= buf.len() {
            return None;
        }
        // 文本以 NUL 结尾；最多读到块末
        let max_len = buf.len() - abs;
        let end = buf[abs..abs + max_len]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(max_len);
        let s = decode_sacd_text(&buf[abs..abs + end]);
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    };

    Ok((
        read_text(album_title_pos),
        read_text(album_artist_pos),
        read_text(album_publisher_pos),
        read_text(album_copyright_pos),
    ))
}

// ─────────────────────────────────────────────────────────────────
// Area TOC 解析
// ─────────────────────────────────────────────────────────────────

/// Area TOC 关键字段（仅元数据探测所需）。
struct AreaToc {
    /// 帧格式（DST / DSD 3-in-14 / DSD 3-in-16）
    frame_format: FrameFormat,
    /// 声道数
    channel_count: u8,
    /// 轨道数
    track_count: u8,
    /// 区域音频数据起始 LSN（area_toc.track_start）
    track_start: u32,
    /// 区域音频数据结束 LSN（area_toc.track_end）
    track_end: u32,
}

/// Area TOC 解析后提取的轨道表信息。
struct AreaTracklist {
    /// track_start_lsn[255]：每条轨道的起始 LSN
    start_lsn: Vec<u32>,
    /// track_length_lsn[255]：每条轨道的 LSN 长度（ScarletBook 规范定义，
    /// 但实测多数镜像此字段为 0，故优先用相邻 start_lsn 差值计算）
    length_lsn: Vec<u32>,
    /// start[255] 时间结构体：每条轨道的起始时间（75 fps）
    start_frames: Vec<u32>,
    /// duration[255] 时间结构体：每条轨道的时长（75 fps）
    duration_frames: Vec<u32>,
}

/// 解析 Area TOC 区域（TWOCHTOC 或 MULCHTOC）。
///
/// 返回 (AreaToc, AreaTracklist, area_text_block_offset)。
/// area_text_block_offset 是 SACDTTxt 块在 area 数据中的字节偏移（None 表示无文本块）。
fn parse_area_toc(
    reader: &mut IsoReader,
    area_start_lsn: u32,
    area_size_sectors: u16,
) -> Result<(AreaToc, AreaTracklist, Option<usize>)> {
    let area_size = if area_size_sectors == 0 {
        // 部分镜像 area_size 字段为 0，兜底读取一个合理上限（area_toc + 后续数据块通常 ≤ 64 扇区）
        64u16
    } else {
        area_size_sectors
    };
    let buf = reader
        .read_sectors(area_start_lsn as u64, area_size as u64)
        .with_context(|| format!("读取 Area TOC 失败 start_lsn={}", area_start_lsn))?;
    if buf.len() < SACD_LSN_SIZE {
        bail!("Area TOC 数据过短: {} 字节", buf.len());
    }

    // 校验首扇区 id
    let id = &buf[0..8];
    if id != b"TWOCHTOC" && id != b"MULCHTOC" {
        bail!("Area TOC id 校验失败: {:?}（期望 TWOCHTOC 或 MULCHTOC）", id);
    }

    // area_toc_t 字段偏移（按 scarletbook.h 结构体定义累加）：
    //   id[8]                         offset 0
    //   version{major,minor}          offset 8 (2 bytes)
    //   size(u16)                     offset 10
    //   reserved01[4]                 offset 12
    //   max_byte_rate(u32)            offset 16
    //   sample_frequency(u8)          offset 20
    //   frame_format:4|reserved02:4   offset 21
    //   reserved03[10]                offset 22
    //   channel_count(u8)             offset 32
    //   extra_settings:3|loudspeaker_config:5  offset 33
    //   max_available_channels(u8)    offset 34
    //   area_mute_flags(u8)           offset 35
    //   reserved04[12]                offset 36
    //   track_attribute:4|reserved05:4 offset 48
    //   reserved06[15]                offset 49
    //   total_playtime{m,s,f}         offset 64 (3 bytes)
    //   reserved07(u8)                offset 67
    //   track_offset(u8)              offset 68
    //   track_count(u8)               offset 69
    //   reserved08[2]                 offset 70
    //   track_start(u32)              offset 72
    //   track_end(u32)                offset 76
    //   text_area_count(u8)           offset 80
    //   reserved09[7]                 offset 81
    //   locale_table_t languages[10]  offset 88 (4 bytes each = 40 bytes)
    //   track_text_offset(u16)        offset 128
    //   index_list_offset(u16)        offset 130
    //   access_list_offset(u16)       offset 132
    //   reserved10[10]                offset 134
    //   area_description_offset(u16)  offset 144
    //   copyright_offset(u16)         offset 146
    //   ... phonetic/copyright_phonetic
    //   data[1896]                    offset 152
    let frame_format_raw = buf[21] & 0x0F;
    let frame_format = FrameFormat::from_u8(frame_format_raw).ok_or_else(|| {
        anyhow!(
            "未知的 frame_format 值: {}（仅支持 0=DST / 2=DSD3-in-14 / 3=DSD3-in-16）",
            frame_format_raw
        )
    })?;
    let channel_count = buf[32];
    let track_count = buf[69];
    let track_start = read_be_u32(&buf, 72)?;
    let track_end = read_be_u32(&buf, 76)?;

    let area_toc = AreaToc {
        frame_format,
        channel_count,
        track_count,
        track_start,
        track_end,
    };

    // 遍历 area 数据块查找 SACDTRL1 / SACDTRL2 / SACDTTxt。
    // 起始指针跳过 area_toc_t 本体（第 1 扇区）。
    let mut p = SACD_LSN_SIZE;
    let end = buf.len();

    let mut tracklist = AreaTracklist {
        start_lsn: vec![0u32; MAX_TRACKS],
        length_lsn: vec![0u32; MAX_TRACKS],
        start_frames: vec![0u32; MAX_TRACKS],
        duration_frames: vec![0u32; MAX_TRACKS],
    };
    let mut text_block_off: Option<usize> = None;

    while p + 8 <= end {
        let block_id = &buf[p..p + 8];
        if block_id == b"SACDTRL1" {
            // area_tracklist_offset_t:
            //   id[8]                     offset 0
            //   track_start_lsn[255]      offset 8 (u32 * 255 = 1020 bytes)
            //   track_length_lsn[255]     offset 1028 (u32 * 255 = 1020 bytes)
            //   总长度: 8 + 2040 = 2048 = 1 扇区
            let base = p + 8;
            for i in 0..MAX_TRACKS {
                let off = base + i * 4;
                if off + 4 > end {
                    break;
                }
                tracklist.start_lsn[i] = read_be_u32(&buf, off)?;
            }
            for i in 0..MAX_TRACKS {
                let off = base + MAX_TRACKS * 4 + i * 4;
                if off + 4 > end {
                    break;
                }
                tracklist.length_lsn[i] = read_be_u32(&buf, off)?;
            }
            p += SACD_LSN_SIZE;
        } else if block_id == b"SACDTRL2" {
            // area_tracklist_t:
            //   id[8]                    offset 0
            //   start[255]                offset 8 (4 bytes each = 1020 bytes)
            //   duration[255]             offset 1028 (4 bytes each = 1020 bytes)
            // 总长度: 8 + 2040 = 2048 = 1 扇区
            // 每个时间结构: minutes(1) + seconds(1) + frames(1) + flags(1) = 4 bytes
            let base = p + 8;
            for i in 0..MAX_TRACKS {
                let off = base + i * 4;
                if off + 3 >= end {
                    break;
                }
                tracklist.start_frames[i] = time_framecount(&buf[off..off + 3]);
            }
            for i in 0..MAX_TRACKS {
                let off = base + MAX_TRACKS * 4 + i * 4;
                if off + 3 >= end {
                    break;
                }
                tracklist.duration_frames[i] = time_framecount(&buf[off..off + 3]);
            }
            p += SACD_LSN_SIZE;
        } else if block_id == b"SACDTTxt" {
            // area_text_t:
            //   id[8]                       offset 0
            //   track_text_position[...]    offset 8 (u16 数组，每元素指向 data 区的文本偏移)
            // 占 1 扇区，track_text_position 数量 = track_count + 1（首元素是区域级描述）
            text_block_off = Some(p);
            p += SACD_LSN_SIZE;
        } else if block_id == b"SACD_IGL" {
            // area_isrc_genre_t: id[8] + isrc[255] (12 bytes each) + reserved(4) + track_genre[255] (4 bytes each)
            // = 8 + 3060 + 4 + 1020 = 4092 → 占 2 扇区
            p += SACD_LSN_SIZE * 2;
        } else if block_id == b"SACD_ACC" {
            // area_access_list_t: id[8] + entry_count(2) + main_step_size(1) + reserved01[5] +
            // main_access_list[6550][5] + reserved02[2] + detailed_access_list[32768]
            // 总长度约 65536 = 32 扇区
            p += SACD_LSN_SIZE * 32;
        } else {
            // 未知块或块遍历结束
            break;
        }
    }

    Ok((area_toc, tracklist, text_block_off))
}

/// TIME_FRAMECOUNT 宏的 Rust 等价实现：
/// `(minutes * 60 * 75) + (seconds * 75) + frames`
fn time_framecount(t: &[u8]) -> u32 {
    // t.len() >= 3 已由调用方保证
    let minutes = t[0] as u32;
    let seconds = t[1] as u32;
    let frames = t[2] as u32;
    minutes * 60 * SACD_FRAME_RATE + seconds * SACD_FRAME_RATE + frames
}

// ─────────────────────────────────────────────────────────────────
// 轨道文本解析
// ─────────────────────────────────────────────────────────────────

/// 从 SACDTTxt 块提取每条轨道的 title / performer。
///
/// 块结构（area_text_t）：
/// - id[8] = "SACDTTxt"
/// - track_text_position[]：u16 数组，每元素是相对 data 区（即块内 offset 8 之后）
///   的文本偏移。`track_text_position[0]` 通常是区域级描述，`[1]..[track_count]` 对应
///   每条轨道。文本区由若干子块组成，每个子块结构：
///   - track_type(u8)：0x01=title / 0x02=performer / 0x03=songwriter / ...
///   - null-terminated string
///   - 后续 0x00 表示该轨道文本结束，进入下一轨道。
///
/// 参考 `scarletbook_read.c::scarletbook_read_area_toc` 中 SACDTTxt 分支。
fn parse_area_track_text(
    area_buf: &[u8],
    text_block_off: usize,
    track_count: u8,
) -> Result<Vec<(Option<String>, Option<String>)>> {
    let mut out: Vec<(Option<String>, Option<String>)> = Vec::with_capacity(track_count as usize);
    if text_block_off == 0 || text_block_off + 8 > area_buf.len() {
        // 无文本块：返回全 None
        for _ in 0..track_count {
            out.push((None, None));
        }
        return Ok(out);
    }

    // 块内布局：id[8] | track_text_position[u16; N]
    // N = track_count + 1（区域级 + 每轨道）
    let positions_base = text_block_off + 8;
    let positions_end = positions_base + (track_count as usize + 1) * 2;
    if positions_end > area_buf.len() {
        // 数据不完整：填充 None 返回
        for _ in 0..track_count {
            out.push((None, None));
        }
        return Ok(out);
    }

    // data 区结尾：块末（每块 1 扇区 = 2048 字节）
    let data_end = (text_block_off + SACD_LSN_SIZE).min(area_buf.len());

    // 读取每条轨道的 position 偏移
    for i in 0..track_count as usize {
        // track_text_position[i+1] 是第 i 条轨道的文本偏移（[0] 是区域级）
        let pos_off = positions_base + (i + 1) * 2;
        let pos = read_be_u16(area_buf, pos_off)? as usize;
        if pos == 0 {
            out.push((None, None));
            continue;
        }
        let abs = text_block_off + pos;
        if abs >= data_end {
            out.push((None, None));
            continue;
        }

        // 从 abs 开始解析若干 (track_type, string) 子块，直到遇到 0x00 或块末
        let mut title: Option<String> = None;
        let mut performer: Option<String> = None;
        let mut q = abs;
        while q < data_end {
            let track_type = area_buf[q];
            q += 1;
            if track_type == 0 {
                // 该轨道文本结束
                break;
            }
            // 读取 NUL 结尾字符串
            let str_end = area_buf[q..data_end]
                .iter()
                .position(|&b| b == 0)
                .map(|p| q + p)
                .unwrap_or(data_end);
            let s = decode_sacd_text(&area_buf[q..str_end]);
            // 跳过字符串 + NUL
            q = if str_end < data_end {
                str_end + 1
            } else {
                data_end
            };
            // 仅保留 title (0x01) 与 performer (0x02)，其他类型暂不提取
            match track_type {
                0x01 => title = Some(s),
                0x02 => performer = Some(s),
                _ => {}
            }
        }
        out.push((title, performer));
    }

    Ok(out)
}

// ─────────────────────────────────────────────────────────────────
// 公开 API
// ─────────────────────────────────────────────────────────────────

/// 打开 SACD ISO 镜像并提取完整元数据。
///
/// 区域选择策略（与 `SacdParser.cpp` 对齐）：
/// 1. 优先使用 TWOCH 区域（两声道）
/// 2. 若无 TWOCH，回落使用 MULCH 区域
/// 3. 两者皆无则报错
///
/// # 参数
/// - `iso_path`: SACD ISO 镜像文件路径
///
/// # 返回
/// 成功返回 `SacdDisc`（包含专辑级文本 + 轨道列表）。
pub fn probe_sacd_iso<P: AsRef<Path>>(iso_path: P) -> Result<SacdDisc> {
    let iso_path_ref = iso_path.as_ref();
    let mut reader = IsoReader::open(iso_path_ref)?;

    // 1. 解析 Master TOC
    let master_toc = parse_master_toc(&mut reader)?;

    // 2. 提取专辑级文本
    let (album_title, album_artist, album_publisher, album_copyright) =
        parse_master_text(&mut reader, &master_toc)?;

    // 3. 选择区域：优先 TWOCH，回落 MULCH
    let (area_start, area_size, area_type) = if master_toc.area_1_toc_size > 0 {
        (
            master_toc.area_1_toc_1_start,
            master_toc.area_1_toc_size,
            "twoch",
        )
    } else if master_toc.area_2_toc_size > 0 {
        (
            master_toc.area_2_toc_1_start,
            master_toc.area_2_toc_size,
            "mulch",
        )
    } else {
        bail!(
            "Master TOC 中 area_1_toc_size 与 area_2_toc_size 均为 0，无可用区域: {}",
            iso_path_ref.display()
        );
    };

    if area_start == 0 {
        bail!(
            "区域 {} 的 TOC 起始 LSN 为 0（无效镜像）: {}",
            area_type,
            iso_path_ref.display()
        );
    }

    // 4. 解析 Area TOC + 轨道表 + 文本块偏移
    let (area_toc, tracklist, text_block_off) =
        parse_area_toc(&mut reader, area_start, area_size)?;

    if area_toc.track_count == 0 {
        bail!(
            "Area TOC track_count = 0（无轨道）: {}",
            iso_path_ref.display()
        );
    }

    let track_count = area_toc.track_count;
    let _track_start = area_toc.track_start;
    let track_end = area_toc.track_end;

    // 5. 重新读取 area 数据用于 SACDTTxt 解析
    // （parse_area_toc 已读过一次但未保留 buffer）
    let area_buf = reader
        .read_sectors(area_start as u64, area_size as u64)
        .context("重读 Area TOC 区域以提取 SACDTTxt 失败")?;
    let track_texts = parse_area_track_text(&area_buf, text_block_off.unwrap_or(0), track_count)?;

    // 6. 组装轨道列表
    let mut tracks: Vec<SacdTrack> = Vec::with_capacity(track_count as usize);
    for i in 0..track_count as usize {
        let start_lsn = tracklist.start_lsn[i];
        // length_lsn：优先用 SACDTRL1 中的 track_length_lsn[i]；
        // 若为 0（多数镜像），用相邻轨道 start_lsn 差值；末轨用 track_end
        let length_lsn = if tracklist.length_lsn[i] != 0 {
            tracklist.length_lsn[i]
        } else if i + 1 < track_count as usize {
            tracklist.start_lsn[i + 1].saturating_sub(start_lsn)
        } else {
            track_end.saturating_sub(start_lsn) + 1
        };

        let start_frames = tracklist.start_frames[i];
        let duration_frames = tracklist.duration_frames[i];
        let duration_secs = duration_frames as f64 / SACD_FRAME_RATE as f64;

        let (title, artist) = track_texts
            .get(i)
            .cloned()
            .unwrap_or((None, None));

        tracks.push(SacdTrack {
            track_num: (i + 1) as u32,
            title,
            artist,
            isrc: None, // ISRC 在 SACD_IGL 块，本版本暂不提取
            start_frames,
            duration_frames,
            duration_secs,
            start_lsn,
            length_lsn,
        });
    }

    // 7. 构造 SacdDisc
    Ok(SacdDisc {
        album_title,
        album_artist,
        album_publisher,
        album_copyright,
        disc_catalog_number: String::new(), // 暂不提取
        frame_format: area_toc.frame_format,
        channel_count: area_toc.channel_count,
        sample_rate: SACD_SAMPLING_FREQUENCY,
        track_count,
        area_type: area_type.to_string(),
        tracks,
    })
}
