//! SACD（Super Audio CD）ISO 镜像解析与解码支持模块。

pub mod dst_ffi;
pub mod iso_reader;
pub mod native_source;
pub mod scarletbook;
pub mod source;

pub use iso_reader::{IsoReader, SACD_LSN_SIZE};
pub use native_source::SacdNativeSource;
pub use scarletbook::{
    decode_sacd_text, probe_sacd_iso, FrameFormat, SacdDisc, SacdTrack,
    SACD_FRAME_RATE, SACD_SAMPLING_FREQUENCY, START_OF_MASTER_TOC,
};
pub use source::{extract_track_to_dsdiff_file, parse_sacd_virtual_path, SacdVirtualPath};

/// 检查文件路径是否为 SACD ISO 文件
pub fn is_sacd_iso_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".iso")
}
