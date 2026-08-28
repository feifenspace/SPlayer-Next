//! CUE Sheet 解析与虚拟分轨处理模块
//!
//! 提供：
//! 1. 自动字符集推断与 CUE 文件解析；
//! 2. 多策略音频文件智能关联与模糊匹配；
//! 3. 虚拟分轨（Virtual Track）格式编解码与 Seek 区间管理。

pub mod matcher;
pub mod parser;

pub use matcher::{normalize_for_fuzzy_match, resolve_audio_path};
pub use parser::{
    decode_text_auto, frames_to_seconds, parse_timestamp_frames, seconds_to_frames, CueSheet,
    CueTrack,
};

/// 虚拟 CUE 分轨解析信息
#[derive(Debug, Clone, PartialEq)]
pub struct CueVirtualInfo {
    /// 物理音频文件路径
    pub physical_path: String,
    /// 分轨在母版音频中的起始时间（秒）
    pub start_time: f64,
    /// 分轨持续时长（秒，0.0 表示播放至文件末尾）
    pub duration: f64,
    /// 曲目编号
    pub track_num: u16,
}

/// 检查路径是否为 CUE 虚拟分轨路径 (`path|start_sec|duration_sec|track_num`)
pub fn parse_cue_virtual_path(virtual_path: &str) -> Option<CueVirtualInfo> {
    let parts: Vec<&str> = virtual_path.split('|').collect();
    if parts.len() == 4 {
        let physical_path = parts[0].to_string();
        let start_time = parts[1].parse::<f64>().ok()?;
        let duration = parts[2].parse::<f64>().ok()?;
        let track_num = parts[3].parse::<u16>().ok()?;
        return Some(CueVirtualInfo {
            physical_path,
            start_time,
            duration,
            track_num,
        });
    }
    None
}

/// 判定给定文件路径是否为 CUE 文件
pub fn is_cue_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".cue")
}
