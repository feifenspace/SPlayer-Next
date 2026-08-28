//! SACD ScarletBook 结构与元数据解析器
//!
//! 支持解析：
//! 1. Master TOC（扇区 510，主目录区与光盘信息）；
//! 2. Area TOC（TWOCHTOC 双声道区 / MULCHTOC 多声道区）；
//! 3. SACDTRL1 (LSN 扇区索引) & SACDTRL2 (75fps 时间索引)；
//! 4. SACDTTxt（多语言曲目名称、表演者、作曲者文本）；
//! 5. 自动多国文字编码转 UTF-8。

use std::path::Path;

use chardetng::EncodingDetector;
use encoding_rs::{BIG5, EUC_KR, GBK, SHIFT_JIS, WINDOWS_1252};

use super::iso_reader::{IsoReader, SACD_SECTOR_SIZE};

pub const START_OF_MASTER_TOC: u32 = 510;
pub const MASTER_TOC_LEN: u32 = 10;
pub const SACD_SAMPLING_FREQUENCY: u32 = 2_822_400; // 64 x 44.1 kHz
pub const SACD_FRAME_RATE: u32 = 75; // 75 fps

/// SACD 区域类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AreaType {
    Stereo,
    MultiChannel,
}

/// SACD 单个分轨元数据
#[derive(Debug, Clone, PartialEq)]
pub struct SacdTrack {
    pub track_num: u16,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub channels: u16,
    pub sample_rate: u32,
    pub bit_rate: u64,
    pub start_lsn: u32,
    pub length_lsn: u32,
    pub start_frame: u32,
    pub duration_frame: u32,
    pub start_time: f64,
    pub duration: f64,
    pub is_dst: bool,
    /// 虚拟路径：`iso_path|TrackXX|duration_sec|start_frame|duration_frame`
    pub virtual_path: String,
}

/// SACD 区域结构
#[derive(Debug, Clone)]
pub struct SacdArea {
    pub area_type: AreaType,
    pub channel_count: u8,
    pub frame_format: u8,
    pub is_dst: bool,
    pub track_count: u8,
    pub tracks: Vec<SacdTrack>,
}

/// SACD 完整光盘元数据
#[derive(Debug, Clone)]
pub struct SacdDisc {
    pub album_title: Option<String>,
    pub album_artist: Option<String>,
    pub disc_title: Option<String>,
    pub disc_artist: Option<String>,
    pub disc_num: u16,
    pub areas: Vec<SacdArea>,
    pub stereo_area_idx: Option<usize>,
    pub multichannel_area_idx: Option<usize>,
}

/// 根据 ScarletBook 编码代号解码文本
pub fn decode_scarletbook_charset(bytes: &[u8], charset_code: u8) -> String {
    let clean_bytes = if let Some(pos) = bytes.iter().position(|&b| b == 0) {
        &bytes[..pos]
    } else {
        bytes
    };

    if clean_bytes.is_empty() {
        return String::new();
    }

    if let Ok(utf8_str) = std::str::from_utf8(clean_bytes) {
        return utf8_str.to_string();
    }

    let encoding = match charset_code & 0x07 {
        1 | 2 | 7 => WINDOWS_1252, // ISO-646 / ISO-8859-1
        3 => SHIFT_JIS,            // MusicShiftJIS / RIS-506
        4 => EUC_KR,               // KSC5601
        5 => GBK,                  // GB2312
        6 => BIG5,                 // Big5
        _ => {
            let mut detector = EncodingDetector::new();
            detector.feed(clean_bytes, true);
            detector.guess(None, true)
        }
    };

    let (decoded, _, _) = encoding.decode(clean_bytes);
    decoded.trim().to_string()
}

/// 提取 ISO 文件名中的 Disc 编号（例如 "Album Disc 1.iso" -> 1）
pub fn extract_disc_num_from_filename(path: &Path) -> u16 {
    let stem = match path.file_stem() {
        Some(s) => s.to_string_lossy().to_lowercase(),
        None => return 1,
    };

    if let Some(pos) = stem.rfind("disc") {
        let after = &stem[pos + 4..];
        let num_str: String = after.chars().filter(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = num_str.parse::<u16>() {
            if n > 0 {
                return n;
            }
        }
    }
    1
}

impl SacdDisc {
    /// 从 ISO 文件解析完整 SACD ScarletBook 信息
    pub fn parse(iso_reader: &mut IsoReader) -> anyhow::Result<Self> {
        let disc_num = extract_disc_num_from_filename(&iso_reader.path);

        // 1. 读取 Master TOC (扇区 510，共 10 个扇区 = 20480 字节)
        let mut master_data = vec![0u8; MASTER_TOC_LEN as usize * SACD_SECTOR_SIZE];
        iso_reader.read_sectors(START_OF_MASTER_TOC, MASTER_TOC_LEN, &mut master_data)?;

        if &master_data[0..8] != b"SACDMTOC" {
            anyhow::bail!("Not a valid ScarletBook SACD ISO: Master TOC magic missing");
        }

        let area1_toc1_start = u32::from_be_bytes(master_data[32..36].try_into()?);
        let area1_toc_size = u16::from_be_bytes(master_data[46..48].try_into()?);

        let area2_toc1_start = u32::from_be_bytes(master_data[40..44].try_into()?);
        let area2_toc_size = u16::from_be_bytes(master_data[48..50].try_into()?);

        // 解析 Master Text (扇区 511 ~ 518)
        let mut album_title: Option<String> = None;
        let mut album_artist: Option<String> = None;
        let mut disc_title: Option<String> = None;
        let mut disc_artist: Option<String> = None;

        for text_sec_idx in 0..8 {
            let offset = (1 + text_sec_idx) * SACD_SECTOR_SIZE;
            if offset + 2048 <= master_data.len() && &master_data[offset..offset + 8] == b"SACDText"
            {
                let text_chunk = &master_data[offset..offset + 2048];
                let title_pos = u16::from_be_bytes(text_chunk[16..18].try_into()?) as usize;
                let artist_pos = u16::from_be_bytes(text_chunk[18..20].try_into()?) as usize;
                let disc_title_pos = u16::from_be_bytes(text_chunk[24..26].try_into()?) as usize;
                let disc_artist_pos = u16::from_be_bytes(text_chunk[26..28].try_into()?) as usize;

                if title_pos > 0 && title_pos < 2048 && album_title.is_none() {
                    let s = decode_scarletbook_charset(&text_chunk[title_pos..], 2);
                    if !s.is_empty() {
                        album_title = Some(s);
                    }
                }
                if artist_pos > 0 && artist_pos < 2048 && album_artist.is_none() {
                    let s = decode_scarletbook_charset(&text_chunk[artist_pos..], 2);
                    if !s.is_empty() {
                        album_artist = Some(s);
                    }
                }
                if disc_title_pos > 0 && disc_title_pos < 2048 && disc_title.is_none() {
                    let s = decode_scarletbook_charset(&text_chunk[disc_title_pos..], 2);
                    if !s.is_empty() {
                        disc_title = Some(s);
                    }
                }
                if disc_artist_pos > 0 && disc_artist_pos < 2048 && disc_artist.is_none() {
                    let s = decode_scarletbook_charset(&text_chunk[disc_artist_pos..], 2);
                    if !s.is_empty() {
                        disc_artist = Some(s);
                    }
                }
            }
        }

        let mut areas: Vec<SacdArea> = Vec::new();
        let mut stereo_area_idx: Option<usize> = None;
        let mut multichannel_area_idx: Option<usize> = None;

        // 2. 解析 Area 1 (TWOCHTOC 2-Channel Stereo)
        if area1_toc1_start > 0 && area1_toc_size > 0 {
            if let Ok(area) = Self::parse_area(
                iso_reader,
                area1_toc1_start,
                area1_toc_size as u32,
                AreaType::Stereo,
                album_title.as_deref(),
                album_artist.as_deref(),
                disc_num,
            ) {
                stereo_area_idx = Some(areas.len());
                areas.push(area);
            }
        }

        // 3. 解析 Area 2 (MULCHTOC Multi-channel 5.1)
        if area2_toc1_start > 0 && area2_toc_size > 0 {
            if let Ok(area) = Self::parse_area(
                iso_reader,
                area2_toc1_start,
                area2_toc_size as u32,
                AreaType::MultiChannel,
                album_title.as_deref(),
                album_artist.as_deref(),
                disc_num,
            ) {
                multichannel_area_idx = Some(areas.len());
                areas.push(area);
            }
        }

        if areas.is_empty() {
            anyhow::bail!("No valid audio area (Stereo or Multi-channel) found in SACD ISO");
        }

        Ok(Self {
            album_title,
            album_artist,
            disc_title,
            disc_artist,
            disc_num,
            areas,
            stereo_area_idx,
            multichannel_area_idx,
        })
    }

    /// 解析指定 Area TOC 扇区块
    fn parse_area(
        iso_reader: &mut IsoReader,
        start_lsn: u32,
        size_lsn: u32,
        area_type: AreaType,
        album_title: Option<&str>,
        album_artist: Option<&str>,
        _disc_num: u16,
    ) -> anyhow::Result<SacdArea> {
        let total_bytes = size_lsn as usize * SACD_SECTOR_SIZE;
        let mut area_data = vec![0u8; total_bytes];
        iso_reader.read_sectors(start_lsn, size_lsn, &mut area_data)?;

        let magic = &area_data[0..8];
        if magic != b"TWOCHTOC" && magic != b"MULCHTOC" {
            anyhow::bail!("Invalid Area TOC magic");
        }

        let channel_count = area_data[24];
        let frame_format = area_data[13] & 0x0F;
        let is_dst = frame_format == 0; // 0 = DST compressed, 2/3 = uncompressed DSD

        let track_count = area_data[61];

        // 遍历查找 SACDTRL1, SACDTRL2, SACDTTxt
        let mut track_start_lsn: Vec<u32> = Vec::new();
        let mut track_len_lsn: Vec<u32> = Vec::new();
        let mut track_start_frames: Vec<u32> = Vec::new();
        let mut track_dur_frames: Vec<u32> = Vec::new();
        let mut track_titles: Vec<Option<String>> = vec![None; track_count as usize];
        let mut track_artists: Vec<Option<String>> = vec![None; track_count as usize];

        let mut offset = SACD_SECTOR_SIZE;
        while offset + SACD_SECTOR_SIZE <= area_data.len() {
            let sector_chunk = &area_data[offset..offset + SACD_SECTOR_SIZE];
            let sec_magic = &sector_chunk[0..8];

            if sec_magic == b"SACDTRL1" {
                // 读取各分轨的 LSN 偏移与长度
                for i in 0..track_count as usize {
                    let entry_off = 8 + i * 8;
                    if entry_off + 8 <= sector_chunk.len() {
                        let s_lsn =
                            u32::from_be_bytes(sector_chunk[entry_off..entry_off + 4].try_into()?);
                        let l_lsn = u32::from_be_bytes(
                            sector_chunk[entry_off + 4..entry_off + 8].try_into()?,
                        );
                        track_start_lsn.push(s_lsn);
                        track_len_lsn.push(l_lsn);
                    }
                }
            } else if sec_magic == b"SACDTRL2" {
                // 读取各分轨的时间戳 (minutes, seconds, frames -> 75 fps)
                let start_base = 8;
                let dur_base = 8 + 255 * 4;
                for i in 0..track_count as usize {
                    let s_off = start_base + i * 4;
                    let d_off = dur_base + i * 4;
                    if d_off + 3 <= sector_chunk.len() {
                        let s_min = sector_chunk[s_off] as u32;
                        let s_sec = sector_chunk[s_off + 1] as u32;
                        let s_fr = sector_chunk[s_off + 2] as u32;
                        let start_f = (s_min * 60 + s_sec) * SACD_FRAME_RATE + s_fr;

                        let d_min = sector_chunk[d_off] as u32;
                        let d_sec = sector_chunk[d_off + 1] as u32;
                        let d_fr = sector_chunk[d_off + 2] as u32;
                        let dur_f = (d_min * 60 + d_sec) * SACD_FRAME_RATE + d_fr;

                        track_start_frames.push(start_f);
                        track_dur_frames.push(dur_f);
                    }
                }
            } else if sec_magic == b"SACDTTxt" {
                // 解析分轨标题与艺术家
                for i in 0..track_count as usize {
                    let pos_off = 8 + i * 2;
                    if pos_off + 2 <= sector_chunk.len() {
                        let text_pos =
                            u16::from_be_bytes(sector_chunk[pos_off..pos_off + 2].try_into()?)
                                as usize;
                        if text_pos > 0 && text_pos < SACD_SECTOR_SIZE {
                            let text_ptr = &sector_chunk[text_pos..];
                            let track_amount = text_ptr[0];
                            let mut p = 4;
                            for _ in 0..track_amount {
                                if p >= text_ptr.len() {
                                    break;
                                }
                                let track_type = text_ptr[p];
                                p += 2; // skip type and padding
                                let start_p = p;
                                while p < text_ptr.len() && text_ptr[p] != 0 {
                                    p += 1;
                                }
                                let item_str = decode_scarletbook_charset(&text_ptr[start_p..p], 2);
                                if track_type == 0x01
                                    && track_titles[i].is_none()
                                    && !item_str.is_empty()
                                {
                                    track_titles[i] = Some(item_str);
                                } else if track_type == 0x02
                                    && track_artists[i].is_none()
                                    && !item_str.is_empty()
                                {
                                    track_artists[i] = Some(item_str);
                                }
                                p += 1; // skip null terminator
                            }
                        }
                    }
                }
            }
            offset += SACD_SECTOR_SIZE;
        }

        let mut tracks: Vec<SacdTrack> = Vec::with_capacity(track_count as usize);

        for i in 0..track_count as usize {
            let track_num = (i + 1) as u16;
            let start_lsn = track_start_lsn.get(i).copied().unwrap_or(0);
            let length_lsn = track_len_lsn.get(i).copied().unwrap_or(0);
            let start_frame = track_start_frames.get(i).copied().unwrap_or(0);
            let duration_frame = track_dur_frames.get(i).copied().unwrap_or(0);

            let duration = duration_frame as f64 / SACD_FRAME_RATE as f64;
            let start_time = start_frame as f64 / SACD_FRAME_RATE as f64;

            let title = track_titles[i]
                .clone()
                .unwrap_or_else(|| format!("Track {:02}", track_num));
            let artist = track_artists[i]
                .clone()
                .or_else(|| album_artist.map(|s| s.to_string()));

            let virtual_path = format!(
                "{}|Track{:02}|{:.3}|{}|{}",
                iso_reader.path.to_string_lossy(),
                track_num,
                duration,
                start_frame,
                duration_frame
            );

            tracks.push(SacdTrack {
                track_num,
                title: Some(title),
                artist,
                album: album_title.map(|s| s.to_string()),
                channels: channel_count as u16,
                sample_rate: SACD_SAMPLING_FREQUENCY,
                bit_rate: channel_count as u64 * SACD_SAMPLING_FREQUENCY as u64,
                start_lsn,
                length_lsn,
                start_frame,
                duration_frame,
                start_time,
                duration,
                is_dst,
                virtual_path,
            });
        }

        Ok(SacdArea {
            area_type,
            channel_count,
            frame_format,
            is_dst,
            track_count,
            tracks,
        })
    }
}
