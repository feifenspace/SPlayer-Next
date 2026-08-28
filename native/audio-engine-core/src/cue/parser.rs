//! CUE Sheet 结构与时码解析器
//!
//! 具备以下核心特性：
//! 1. 自动编码识别与转换（GBK / Shift-JIS / Big5 / UTF-8 等）；
//! 2. 75fps 标准 CD 帧时码转换（`MM:SS:FF` <-> 帧数 / 秒数）；
//! 3. 多 FILE 与单 FILE CUE Sheet 规范解析；
//! 4. 虚拟分轨路径生成与物理文件关联。

use std::fs;
use std::path::{Path, PathBuf};

use chardetng::EncodingDetector;

use super::matcher::resolve_audio_path;

/// 单条 CUE 分轨信息
#[derive(Debug, Clone, PartialEq)]
pub struct CueTrack {
    /// 轨道编号（1, 2, ...）
    pub track_num: u16,
    /// 标题
    pub title: Option<String>,
    /// 艺术家 / 表演者
    pub artist: Option<String>,
    /// 专辑名称
    pub album: Option<String>,
    /// 对应音频母版的物理绝对路径
    pub physical_path: PathBuf,
    /// INDEX 00 前隙起始帧（如有）
    pub index0_frames: Option<u32>,
    /// INDEX 01 音乐正式起始帧（75 fps）
    pub cue_start_frames: u32,
    /// 起始秒数
    pub start_time: f64,
    /// 分轨持续帧数
    pub cue_duration_frames: Option<u32>,
    /// 分轨持续秒数
    pub duration: Option<f64>,
    /// 虚拟分轨路径：`physical_path|start_time|duration|track_num`
    pub virtual_path: String,
}

/// 解析后的完整 CUE Sheet
#[derive(Debug, Clone, PartialEq)]
pub struct CueSheet {
    pub global_title: Option<String>,
    pub global_performer: Option<String>,
    pub tracks: Vec<CueTrack>,
}

/// 自动检测字节流文本编码并解码为 UTF-8 字符串
pub fn decode_text_auto(bytes: &[u8]) -> String {
    // 1. 优先尝试直接作为 UTF-8 解码
    if let Ok(utf8_str) = std::str::from_utf8(bytes) {
        // 过滤 UTF-8 BOM
        return utf8_str
            .strip_prefix('\u{feff}')
            .unwrap_or(utf8_str)
            .to_string();
    }

    // 2. 使用 chardetng 自动探测编码
    let mut detector = EncodingDetector::new();
    detector.feed(bytes, true);
    let guessed_encoding = detector.guess(None, true);

    let (decoded, _, _) = guessed_encoding.decode(bytes);
    let s = decoded.into_owned();
    s.strip_prefix('\u{feff}').unwrap_or(&s).to_string()
}

/// 解析 `MM:SS:FF` 格式时码为 CD 帧数（1 秒 = 75 帧）
pub fn parse_timestamp_frames(ts: &str) -> Option<u32> {
    let parts: Vec<&str> = ts.split(':').collect();
    if parts.len() != 3 {
        return None;
    }

    let minutes: u32 = parts[0].trim().parse().ok()?;
    let seconds: u32 = parts[1].trim().parse().ok()?;
    let frames: u32 = parts[2].trim().parse().ok()?;

    Some((minutes * 60 + seconds) * 75 + frames)
}

/// 将 CD 帧数转换为秒数
#[inline]
pub fn frames_to_seconds(frames: u32) -> f64 {
    frames as f64 / 75.0
}

/// 将秒数转换为 CD 帧数
#[inline]
pub fn seconds_to_frames(sec: f64) -> u32 {
    (sec * 75.0).round() as u32
}

/// 去除字符串头尾空白及引号
fn trim_quotes(s: &str) -> &str {
    s.trim().trim_matches('"').trim_matches('\'').trim()
}

impl CueSheet {
    /// 从文件读取并解析 CUE Sheet
    pub fn parse_file<P: AsRef<Path>>(cue_path: P) -> anyhow::Result<Self> {
        let cue_path = cue_path.as_ref();
        let bytes = fs::read(cue_path)?;
        let content = decode_text_auto(&bytes);
        Self::parse_str(&content, cue_path.parent().unwrap_or(Path::new(".")))
    }

    /// 解析 CUE 文本字符串
    pub fn parse_str(content: &str, base_dir: &Path) -> anyhow::Result<Self> {
        let mut global_title: Option<String> = None;
        let mut global_performer: Option<String> = None;
        let mut current_file: Option<PathBuf> = None;

        struct RawTrack {
            track_num: u16,
            title: Option<String>,
            artist: Option<String>,
            physical_path: PathBuf,
            index0_frames: Option<u32>,
            index1_frames: Option<u32>,
        }

        let mut raw_tracks: Vec<RawTrack> = Vec::new();

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("REM") {
                continue;
            }

            let first_space = match line.find(char::is_whitespace) {
                Some(pos) => pos,
                None => continue,
            };

            let (cmd_part, rest) = line.split_at(first_space);
            let cmd = cmd_part.to_ascii_uppercase();
            let val = rest.trim();

            match cmd.as_str() {
                "PERFORMER" => {
                    let text = trim_quotes(val).to_string();
                    if let Some(last) = raw_tracks.last_mut() {
                        last.artist = Some(text);
                    } else {
                        global_performer = Some(text);
                    }
                }
                "TITLE" => {
                    let text = trim_quotes(val).to_string();
                    if let Some(last) = raw_tracks.last_mut() {
                        last.title = Some(text);
                    } else {
                        global_title = Some(text);
                    }
                }
                "FILE" => {
                    // FILE "path/to/audio.flac" WAVE 或 FILE audio.flac WAVE
                    let raw_filename = if let Some(start_q) = val.find('"') {
                        if let Some(end_q) = val[start_q + 1..].find('"') {
                            &val[start_q + 1..start_q + 1 + end_q]
                        } else {
                            val
                        }
                    } else if let Some(last_space) = val.rfind(char::is_whitespace) {
                        &val[..last_space]
                    } else {
                        val
                    };

                    let resolved = resolve_audio_path(base_dir, raw_filename);
                    current_file = Some(resolved);
                }
                "TRACK" => {
                    // TRACK 01 AUDIO
                    let track_tokens: Vec<&str> = val.split_whitespace().collect();
                    let num = track_tokens
                        .first()
                        .and_then(|s| s.parse::<u16>().ok())
                        .unwrap_or_else(|| (raw_tracks.len() + 1) as u16);

                    let physical = current_file
                        .clone()
                        .unwrap_or_else(|| base_dir.join("unknown.audio"));

                    raw_tracks.push(RawTrack {
                        track_num: num,
                        title: None,
                        artist: None,
                        physical_path: physical,
                        index0_frames: None,
                        index1_frames: None,
                    });
                }
                "INDEX" => {
                    let idx_tokens: Vec<&str> = val.split_whitespace().collect();
                    if idx_tokens.len() >= 2 {
                        let idx_type = idx_tokens[0];
                        let ts_str = idx_tokens[1];
                        if let Some(frames) = parse_timestamp_frames(ts_str) {
                            if let Some(last) = raw_tracks.last_mut() {
                                if idx_type == "00" {
                                    last.index0_frames = Some(frames);
                                } else if idx_type == "01" {
                                    last.index1_frames = Some(frames);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // 计算各分轨的时长与持续帧数
        let mut final_tracks: Vec<CueTrack> = Vec::with_capacity(raw_tracks.len());

        for i in 0..raw_tracks.len() {
            let cur = &raw_tracks[i];
            let start_frames = cur.index1_frames.unwrap_or(0);
            let start_sec = frames_to_seconds(start_frames);

            let (duration_frames, duration_sec) = if i + 1 < raw_tracks.len()
                && raw_tracks[i + 1].physical_path == cur.physical_path
            {
                // 下一条轨属于同一物理文件
                let next_start = raw_tracks[i + 1].index1_frames.unwrap_or(start_frames);
                let dur_frames = next_start.saturating_sub(start_frames);
                (Some(dur_frames), Some(frames_to_seconds(dur_frames)))
            } else {
                // 属于文件的最后一轨（或多 FILE 边界），由外部注入或通过 FFmpeg probe 补齐
                (None, None)
            };

            let virtual_path = format!(
                "{}|{:.3}|{:.3}|{}",
                cur.physical_path.to_string_lossy(),
                start_sec,
                duration_sec.unwrap_or(0.0),
                cur.track_num
            );

            final_tracks.push(CueTrack {
                track_num: cur.track_num,
                title: cur.title.clone(),
                artist: cur.artist.clone().or_else(|| global_performer.clone()),
                album: global_title.clone(),
                physical_path: cur.physical_path.clone(),
                index0_frames: cur.index0_frames,
                cue_start_frames: start_frames,
                start_time: start_sec,
                cue_duration_frames: duration_frames,
                duration: duration_sec,
                virtual_path,
            });
        }

        Ok(Self {
            global_title,
            global_performer,
            tracks: final_tracks,
        })
    }
}
