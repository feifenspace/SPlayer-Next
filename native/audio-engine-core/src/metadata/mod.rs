use std::collections::HashMap;
use std::path::Path;

use encoding_rs::{GB18030, GBK};
use ffmpeg_audio::{AudioReader, SourceAudioInfo};

mod cover;
mod editor;
mod folder_cover;
mod lyrics;
mod tag_fields;

pub use cover::{
    cover_cache_needs_refresh, cover_thumb_path, extract_cover_thumbnail,
    extract_cover_thumbnail_with_directory_cover, extract_directory_cover_thumbnail,
    extract_folder_cover_thumbnail, find_directory_cover, make_thumbnail_jpeg, read_attached_pic,
};
pub use folder_cover::find_folder_cover as legacy_find_folder_cover;
pub use editor::{read_tags, write_tags, TagWriteRequest};
pub use lyrics::{extract_embedded_lyric, find_all_external_lyrics, ExternalLyric};

/// 音频元数据（包含封面路径和歌词）
#[derive(Clone, Default)]
pub struct AudioMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// 注释/副标题
    pub comment: Option<String>,
    pub duration_secs: f64,
    /// 播放采样率（重采样后，用于音频输出）
    pub sample_rate: u32,
    /// 音源原始声道数
    pub channels: u16,
    /// 原始采样率（解码前，用于前端显示）
    pub original_sample_rate: u32,
    /// 位深（bits per sample）
    pub bits_per_sample: u32,
    /// 比特率（bps）
    pub bit_rate: i64,
    /// 编码格式名称（如 "flac", "mp3", "aac"）
    pub codec: String,
    /// 内嵌歌词
    pub embedded_lyric: Option<String>,
    /// 同目录所有歌词文件
    pub external_lyrics: Vec<ExternalLyric>,
    /// 封面缩略图缓存路径（用于前端日常显示）
    pub cover: Option<String>,
    /// 原始封面数据（load 时一次性提取，供 SMTC 等使用，避免重复打开文件）
    pub cover_raw: Option<Vec<u8>>,
}

/// 音频流基本参数（scanner 和 decoder 共用）
pub struct StreamInfo {
    pub bit_rate: i64,
    pub sample_rate: u32,
    pub bits_per_sample: u32,
    pub channels: u32,
}

/// 容器级别的 tag 信息
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track: Option<u16>,
    pub comment: Option<String>,
}

/// 把 ffmpeg_audio 的 SourceAudioInfo 转成内部 StreamInfo
pub fn extract_stream_info(info: &SourceAudioInfo) -> StreamInfo {
    StreamInfo {
        bit_rate: info.bit_rate,
        sample_rate: info.sample_rate.max(0) as u32,
        bits_per_sample: info.bits_per_sample.max(0) as u32,
        channels: info.channels.max(0) as u32,
    }
}

/// 从容器 metadata 提取常见 tag
pub fn extract_tags(dict: &HashMap<String, String>) -> Tags {
    let title = dict_get(dict, "title").map(ToString::to_string);
    let artist = dict_get(dict, "artist")
        .or_else(|| dict_get(dict, "album_artist"))
        .map(ToString::to_string);
    let album = dict_get(dict, "album").map(ToString::to_string);
    let track = dict_get(dict, "track").and_then(|s| s.parse().ok());
    let comment = dict_get(dict, "comment").map(ToString::to_string);
    Tags {
        title,
        artist,
        album,
        track,
        comment,
    }
}

/// 统一的高保真标签提取入口（Lofty 原生标签优先 + FFmpeg 容器回退 + tinyLMS 启发式探测与目录推断）
pub fn extract_file_tags(path: &str, reader: &AudioReader) -> Tags {
    // 1. 优先使用 Lofty 读取真实标签（支持 ID3v2、Vorbis Comments、APEv2、RIFF INFO、MP4 等）
    // 特别是对于 WAV 文件，FFmpeg 往往只读取 RIFF INFO chunk 导致 GBK 被破坏为 \u{FFFD}，
    // 而 Lofty 能准确读取末尾的 ID3v2 chunk（包含原生 UTF-16LE / UTF-8 中文标签）
    let lofty_tags = editor::read_tags(path).ok();

    let mut title = lofty_tags.as_ref().and_then(|t| t.title.clone()).and_then(clean_and_repair_tag);
    let mut artist = lofty_tags.as_ref().and_then(|t| t.artist.clone()).and_then(clean_and_repair_tag);
    let mut album = lofty_tags.as_ref().and_then(|t| t.album.clone()).and_then(clean_and_repair_tag);
    let mut track = lofty_tags.as_ref().and_then(|t| t.track_number.map(|n| n as u16));
    let mut comment = None;

    // 2. 若 Lofty 缺失字段，从 FFmpeg 容器 metadata 回退补全
    if title.is_none() || artist.is_none() || album.is_none() || track.is_none() {
        let raw_metadata = reader.metadata();
        let ffmpeg_tags = extract_tags(&raw_metadata);
        if title.is_none() {
            title = ffmpeg_tags.title.and_then(clean_and_repair_tag);
        }
        if artist.is_none() {
            artist = ffmpeg_tags.artist.and_then(clean_and_repair_tag);
        }
        if album.is_none() {
            album = ffmpeg_tags.album.and_then(clean_and_repair_tag);
        }
        if track.is_none() {
            track = ffmpeg_tags.track;
        }
        if comment.is_none() {
            comment = ffmpeg_tags.comment.and_then(clean_and_repair_tag);
        }
    }

    // 3. 【标签兜底与文件名/目录层级推断】（借鉴 tinyLMS-old 经验）
    if title.is_none() {
        title = clean_title_from_filename(path);
    }
    if artist.is_none() {
        artist = infer_artist_from_path(path);
    }
    if album.is_none() {
        album = infer_album_from_path(path);
    }

    Tags {
        title,
        artist,
        album,
        track,
        comment,
    }
}

/// 清洗和修复标签字符串（检测并丢弃 \u{FFFD} 坏字符，修复以 Latin-1 存储的 GBK/GB18030）
pub fn clean_and_repair_tag(text: String) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 包含 Unicode 替换符说明原文本已被有损截断，直接丢弃以触发高质量回退
    if trimmed.contains('\u{fffd}') {
        return None;
    }

    // GBK 双字节被误当作 UTF-8 解码（例如 GBK "色" = C9 AB → U+026B "ɫ"）时，
    // 产物集中在 Latin Extended-B / IPA 区段（U+0180–U+02AF）。
    // 真实音乐标签几乎不会出现这些字符，直接丢弃以触发高质量回退
    if trimmed.chars().any(is_gbk_as_utf8_mojibake_char) {
        return None;
    }

    // 检查是否为 Latin-1 误存的 GBK/GB18030
    if is_pure_latin1(trimmed) {
        if let Some(repaired) = decode_latin1_as_gbk(trimmed) {
            return Some(repaired);
        }
    }

    Some(trimmed.to_string())
}

/// 从文件名提取纯净标题（剥离音轨序号和分隔符前缀，例如 "01._机遇Ⅰ" -> "机遇Ⅰ"）
pub fn clean_title_from_filename(path: &str) -> Option<String> {
    let stem = Path::new(path).file_stem()?.to_str()?;
    let mut s = stem.trim();
    let bytes = s.as_bytes();
    let mut num_end = 0;
    while num_end < bytes.len() && bytes[num_end].is_ascii_digit() {
        num_end += 1;
    }
    if num_end > 0 && num_end <= 3 && num_end < bytes.len() {
        let after_num = &s[num_end..];
        let trimmed_after = after_num
            .trim_start_matches(|c: char| c == '.' || c == '_' || c == '-' || c == ' ' || c == '、');
        if !trimmed_after.is_empty() {
            s = trimmed_after;
        }
    }
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// 从直接父目录提取专辑名（忽略通用根目录）
pub fn infer_album_from_path(path: &str) -> Option<String> {
    let p = Path::new(path);
    let parent = p.parent()?;
    let name = parent.file_name()?.to_str()?.trim();
    if is_generic_dir_name(name) {
        None
    } else {
        Some(name.to_string())
    }
}

/// 从祖父目录提取艺术家（忽略通用根目录，如 /media/music2/蔡琴/机遇/01.wav -> "蔡琴"）
pub fn infer_artist_from_path(path: &str) -> Option<String> {
    let p = Path::new(path);
    let parent = p.parent()?;
    let grandparent = parent.parent()?;
    let name = grandparent.file_name()?.to_str()?.trim();
    if is_generic_dir_name(name) {
        None
    } else {
        Some(name.to_string())
    }
}

fn is_generic_dir_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "" | "." | ".." | "music" | "music2" | "test" | "download" | "downloads" | "audio" | "media" | "root" | "home"
    )
}

fn is_pure_latin1(s: &str) -> bool {
    let mut has_high = false;
    for ch in s.chars() {
        let code = ch as u32;
        if code > 255 {
            return false;
        }
        if code >= 0x80 {
            has_high = true;
        }
    }
    has_high
}

/// 判断字符是否为 GBK 字节被误当作 UTF-8 解码后的典型产物。
///
/// GBK 首字节 C4–CA、次字节 80–BF 的双字节组恰好是合法 UTF-8 序列，
/// 解码结果落在 Latin Extended-B（U+0180–U+024F）与 IPA Extensions
/// （U+0250–U+02AF）区段；真实标签中这些区段的出现概率可以忽略。
fn is_gbk_as_utf8_mojibake_char(ch: char) -> bool {
    let code = ch as u32;
    (0x0180..=0x02AF).contains(&code)
}

/// 修复少量旧中文下载器写出的损坏 ID3 文本。
///
/// 这类文件把 GBK 原始字节逐字节扩成 Latin-1/UTF-16 码点，例如 `ÇôÄñ` 实际应为 `囚鸟`。
/// 为避免误伤正常西文标签，仅当修复后的标题与真实文件名完全一致时，才修复同一文件的标题/歌手/专辑。
pub fn repair_legacy_gbk_tags_for_path(mut tags: Tags, path: &str) -> Tags {
    let Some(file_stem) = Path::new(path).file_stem().and_then(|value| value.to_str()) else {
        return tags;
    };
    let Some(raw_title) = tags.title.as_deref() else {
        return tags;
    };
    let Some(repaired_title) = decode_latin1_as_gbk(raw_title) else {
        return tags;
    };
    if repaired_title != file_stem {
        return tags;
    }

    tags.title = Some(repaired_title);
    tags.artist = tags
        .artist
        .take()
        .map(|value| decode_latin1_as_gbk(&value).unwrap_or(value));
    tags.album = tags
        .album
        .take()
        .map(|value| decode_latin1_as_gbk(&value).unwrap_or(value));
    tags
}

fn decode_latin1_as_gbk(text: &str) -> Option<String> {
    let bytes = text
        .chars()
        .map(|ch| u8::try_from(u32::from(ch)).ok())
        .collect::<Option<Vec<_>>>()?;
    if bytes.iter().filter(|&&byte| byte >= 0x80).count() < 2 {
        return None;
    }

    // 借鉴 tinyLMS-old：高位统计启发式检测 GBK/GB18030
    let mut gbk_score = 0;
    let mut total_high = 0;
    let mut i = 0;
    while i < bytes.len() {
        let c1 = bytes[i];
        if c1 >= 0x80 {
            total_high += 1;
            if i + 1 < bytes.len() {
                let c2 = bytes[i + 1];
                if c1 >= 0x81 && c1 <= 0xfe && ((c2 >= 0x40 && c2 <= 0x7e) || (c2 >= 0x80 && c2 <= 0xfe)) {
                    gbk_score += 1;
                    i += 1;
                    total_high += 1;
                }
            }
        }
        i += 1;
    }

    // 若高位成对比例 >= 0.7，优先使用 GB18030 解码
    if total_high > 0 && (gbk_score * 2 * 10 >= total_high * 7) {
        let (decoded, _, had_errors) = GB18030.decode(&bytes);
        if !had_errors {
            let res = decoded.trim_matches('\0').trim().to_string();
            if !res.is_empty() && res != text {
                return Some(res);
            }
        }
    }

    // 备用：使用 GBK 解码
    let (decoded, _, had_errors) = GBK.decode(&bytes);
    if !had_errors {
        let decoded = decoded.trim_matches('\0').trim().to_string();
        if !decoded.is_empty() && decoded != text {
            return Some(decoded);
        }
    }

    // 再次备用：使用 chardetng 自动探测
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(&bytes, true);
    let guessed = detector.guess(None, true);
    let (decoded, _, had_errors) = guessed.decode(&bytes);
    if !had_errors {
        let res = decoded.trim_matches('\0').trim().to_string();
        if !res.is_empty() && res != text {
            return Some(res);
        }
    }

    None
}

/// 大小写不敏感查找：原 ffmpeg-next 的 Dictionary::get 默认 case-insensitive，
/// 而 ffmpeg_audio 把 dict 转成普通 HashMap 后丢了这个语义，这里补回来
fn dict_get<'a>(dict: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    let target = tag_fields::normalize_tag_key(key);
    dict.iter()
        .find(|(k, _)| tag_fields::normalize_tag_key(k) == target)
        .map(|(_, v)| v.as_str())
}

/// 从容器 metadata 提取 ReplayGain / R128 增益值（dB）
///
/// 按优先级尝试：R128_TRACK_GAIN → replaygain_track_gain → album 版本
pub fn extract_replay_gain(dict: &HashMap<String, String>) -> Option<f32> {
    // EBU R128：值为 1/256 dB 单位的整数
    if let Some(val) =
        dict_get(dict, "R128_TRACK_GAIN").or_else(|| dict_get(dict, "R128_ALBUM_GAIN"))
    {
        if let Ok(raw) = val.trim().parse::<f32>() {
            return Some(raw / 256.0);
        }
    }

    // ReplayGain：格式如 "-6.50 dB"
    if let Some(val) =
        dict_get(dict, "replaygain_track_gain").or_else(|| dict_get(dict, "replaygain_album_gain"))
    {
        let cleaned = val.trim().trim_end_matches(" dB").trim_end_matches("dB");
        if let Ok(db) = cleaned.parse::<f32>() {
            return Some(db);
        }
    }

    None
}

/// 将 dB 增益转换为线性增益因子
pub fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_tags_are_matched_case_insensitively() {
        let dict = HashMap::from([
            ("TITLE".to_string(), "Track".to_string()),
            ("Album_Artist".to_string(), "Artist".to_string()),
            ("TRACK".to_string(), "7".to_string()),
        ]);

        let tags = extract_tags(&dict);
        assert_eq!(tags.title.as_deref(), Some("Track"));
        assert_eq!(tags.artist.as_deref(), Some("Artist"));
        assert_eq!(tags.track, Some(7));
    }

    #[test]
    fn malformed_legacy_gbk_tags_are_repaired_only_when_title_matches_file_name() {
        let tags = Tags {
            title: Some("ÇôÄñ".to_string()),
            artist: Some("µË×ÏÆå".to_string()),
            album: Some("ÔÚÏßÈÈËÑ£¨»ªÓï£©ÏµÁÐ63".to_string()),
            track: None,
            comment: None,
        };
        let repaired = repair_legacy_gbk_tags_for_path(tags, "/music/囚鸟.mp3");

        assert_eq!(repaired.title.as_deref(), Some("囚鸟"));
        assert_eq!(repaired.artist.as_deref(), Some("邓紫棋"));
        assert_eq!(repaired.album.as_deref(), Some("在线热搜（华语）系列63"));
    }

    #[test]
    fn malformed_legacy_gbk_guess_does_not_override_unrelated_file_name() {
        let tags = Tags {
            title: Some("ÇôÄñ".to_string()),
            artist: Some("Beyoncé".to_string()),
            album: Some("Album".to_string()),
            track: None,
            comment: None,
        };
        let repaired = repair_legacy_gbk_tags_for_path(tags, "/music/Other Song.mp3");

        assert_eq!(repaired.title.as_deref(), Some("ÇôÄñ"));
        assert_eq!(repaired.artist.as_deref(), Some("Beyoncé"));
    }

    #[test]
    fn r128_track_gain_has_priority_and_uses_fixed_point_units() {
        let dict = HashMap::from([
            ("R128_TRACK_GAIN".to_string(), "-1536".to_string()),
            ("replaygain_track_gain".to_string(), "-3.00 dB".to_string()),
        ]);

        assert_eq!(extract_replay_gain(&dict), Some(-6.0));
    }

    #[test]
    fn decibels_are_converted_to_linear_gain() {
        assert!((db_to_linear(-6.0) - 0.501_187_2).abs() < 0.000_001);
    }

    #[test]
    fn test_clean_title_and_path_inference() {
        let path = "/media/music2/蔡琴/机遇（绿色版）/01._机遇Ⅰ.wav";
        assert_eq!(clean_title_from_filename(path), Some("机遇Ⅰ".to_string()));
        assert_eq!(infer_album_from_path(path), Some("机遇（绿色版）".to_string()));
        assert_eq!(infer_artist_from_path(path), Some("蔡琴".to_string()));

        let path2 = "/music/张学友/吻别/02. 每天爱你多一些.flac";
        assert_eq!(clean_title_from_filename(path2), Some("每天爱你多一些".to_string()));
        assert_eq!(infer_album_from_path(path2), Some("吻别".to_string()));
        assert_eq!(infer_artist_from_path(path2), Some("张学友".to_string()));
    }

    #[test]
    fn test_clean_and_repair_tag_discards_fffd_and_decodes_latin1_gbk() {
        // 包含 \u{FFFD} 的损坏文本被彻底丢弃
        assert_eq!(clean_and_repair_tag("".to_string()), None);
        assert_eq!(clean_and_repair_tag("(2003) (ɫ)".to_string()), None);
        assert_eq!(clean_and_repair_tag("(2003) (\u{fffd})".to_string()), None);

        // 以 Latin-1 保存的 GBK 文本自动转码
        let raw_caiqin = "²ÌÇÙ".to_string(); // "蔡琴" in Latin-1 representation of GBK
        assert_eq!(clean_and_repair_tag(raw_caiqin), Some("蔡琴".to_string()));

        let raw_jiyu = "»úÓö¢ñ".to_string(); // "机遇Ⅰ" in Latin-1 representation of GBK
        assert_eq!(clean_and_repair_tag(raw_jiyu), Some("机遇Ⅰ".to_string()));
    }
}

