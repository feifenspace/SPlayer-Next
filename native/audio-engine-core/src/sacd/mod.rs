//! SACD ISO (Super Audio CD) 结构解析与 DSD/DST 解码模块
//!
//! 具备以下能力：
//! 1. 2048 字节光盘扇区读取器 (`IsoReader`)；
//! 2. ScarletBook Master TOC & Area TOC & Text 元数据解析 (`SacdDisc`, `SacdTrack`)；
//! 3. 纯 Rust MPEG-4 DST 算术无损解压算法 (`dst::DstDecoder`)；
//! 4. 实时音频帧提取与 DSD2PCM / Diretta 直通流读取器 (`SacdTrackReader`)。

pub mod dst;
pub mod iso_reader;
pub mod scarletbook;
pub mod stream;

pub use dst::{ArithmeticDecoder, DstDecoder};
pub use iso_reader::{IsoReader, SACD_SECTOR_SIZE};
pub use scarletbook::{
    decode_scarletbook_charset, AreaType, SacdArea, SacdDisc, SacdTrack, SACD_FRAME_RATE,
    SACD_SAMPLING_FREQUENCY, START_OF_MASTER_TOC,
};
pub use stream::SacdTrackReader;

/// SACD 虚拟分轨解析信息
#[derive(Debug, Clone, PartialEq)]
pub struct SacdVirtualInfo {
    /// 物理 ISO 镜像路径
    pub iso_path: String,
    /// 轨道编号（1, 2, ...）
    pub track_num: u16,
    /// 音轨时长（秒）
    pub duration: f64,
    /// 起始 CD 帧（75 fps）
    pub start_frame: u32,
    /// 持续 CD 帧
    pub duration_frame: u32,
}

/// 解析 SACD 虚拟分轨路径 (`iso_path|TrackXX|duration|start_frame|duration_frame`)
pub fn parse_sacd_virtual_path(virtual_path: &str) -> Option<SacdVirtualInfo> {
    let parts: Vec<&str> = virtual_path.split('|').collect();
    if parts.len() == 5 {
        let iso_path = parts[0].to_string();
        let track_str = parts[1].strip_prefix("Track")?;
        let track_num = track_str.parse::<u16>().ok()?;
        let duration = parts[2].parse::<f64>().ok()?;
        let start_frame = parts[3].parse::<u32>().ok()?;
        let duration_frame = parts[4].parse::<u32>().ok()?;

        return Some(SacdVirtualInfo {
            iso_path,
            track_num,
            duration,
            start_frame,
            duration_frame,
        });
    }
    None
}

/// 检查文件路径是否为 SACD ISO 文件
pub fn is_sacd_iso_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".iso")
}
