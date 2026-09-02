use std::collections::HashMap;
use std::path::Path;

use encoding_rs::GBK;
use ffmpeg_audio::SourceAudioInfo;

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
    let (decoded, _, had_errors) = GBK.decode(&bytes);
    if had_errors {
        return None;
    }
    let decoded = decoded.trim_matches('\0').trim().to_string();
    (!decoded.is_empty() && decoded != text).then_some(decoded)
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
}
