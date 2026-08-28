//! DSD (Direct Stream Digital) Native and Decimation Decoder Module
//!
//! 移植并增强自 tinyLMS-old 的高保真 DSD 解码算法：
//! 1. DSF (Sony): 逐块 Planar 解析与 4 字节 L/R 高速交错交织 + 比特翻转 (LSB -> MSB)。
//! 2. DFF (Philips DSDIFF): 块结构与逐点交错比特流解析。
//! 3. Diretta 零拷贝 Native DSD 直通与通用 PCM 抽取降采样 (DSD-to-PCM)。

pub mod dff;
pub mod dsd2pcm;
pub mod dsf;

pub use dff::DffReader;
pub use dsd2pcm::Dsd2PcmDecimator;
pub use dsf::DsfReader;

/// DSD 采样率与规格
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsdRate {
    Dsd64,  // 2.8224 MHz (64 x 44.1 kHz)
    Dsd128, // 5.6448 MHz (128 x 44.1 kHz)
    Dsd256, // 11.2896 MHz (256 x 44.1 kHz)
    Dsd512, // 22.5792 MHz (512 x 44.1 kHz)
    Other(u32),
}

impl DsdRate {
    pub fn from_sample_rate(hz: u32) -> Self {
        match hz {
            2_822_400 => DsdRate::Dsd64,
            5_644_800 => DsdRate::Dsd128,
            11_289_600 => DsdRate::Dsd256,
            22_579_200 => DsdRate::Dsd512,
            other => DsdRate::Other(other),
        }
    }

    pub fn to_hz(&self) -> u32 {
        match self {
            DsdRate::Dsd64 => 2_822_400,
            DsdRate::Dsd128 => 5_644_800,
            DsdRate::Dsd256 => 11_289_600,
            DsdRate::Dsd512 => 22_579_200,
            DsdRate::Other(hz) => *hz,
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            DsdRate::Dsd64 => "DSD64 (2.82 MHz)",
            DsdRate::Dsd128 => "DSD128 (5.64 MHz)",
            DsdRate::Dsd256 => "DSD256 (11.28 MHz)",
            DsdRate::Dsd512 => "DSD512 (22.58 MHz)",
            DsdRate::Other(_) => "DSD (Custom)",
        }
    }
}

/// 基于 DAC 硬件能力决策的 DSD 播放策略
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DsdPlaybackStrategy {
    /// 目标 DAC 支持 Native DSD 直通
    NativeDsd { sample_rate: u32, is_dsd_lsb: bool },
    /// 目标 DAC 不支持 Native DSD，或采样率超出上限，降采样为 PCM 传输
    DsdToPcm { pcm_sample_rate: u32, bit_depth: u8 },
}

impl DsdPlaybackStrategy {
    /// 根据 Diretta 目标 DAC 的 SinkCaps 硬件能力自动决策 DSD 播放策略
    pub fn resolve_diretta_strategy(
        caps: Option<&diretta_sys::DirettaSinkInfo>,
        requested_rate: DsdRate,
    ) -> Self {
        let Some(caps) = caps else {
            // 默认无能力信息时安全降采样
            return Self::DsdToPcm {
                pcm_sample_rate: 176_400,
                bit_depth: 32,
            };
        };

        let req_hz = requested_rate.to_hz();

        // 判定 1: DAC 明确支持 Native DSD，且采样率在 DAC 支持范围内
        if caps.supports_dsd {
            let in_rate_range = (caps.dsd_min_sample_rate == 0
                || req_hz >= caps.dsd_min_sample_rate)
                && (caps.dsd_max_sample_rate == 0 || req_hz <= caps.dsd_max_sample_rate);

            if in_rate_range {
                let is_lsb = caps.supports_dsd_lsb && !caps.supports_dsd_msb;
                return Self::NativeDsd {
                    sample_rate: req_hz,
                    is_dsd_lsb: is_lsb,
                };
            }
        }

        // 判定 2: DAC 不支持 DSD 或采样率超出范围 -> 自动决策最高兼容 PCM 采样率
        let max_pcm = if caps.supports_pcm && caps.pcm_max_sample_rate > 0 {
            caps.pcm_max_sample_rate
        } else {
            192_000
        };

        let pcm_rate = if max_pcm >= 352_800 && req_hz >= 11_289_600 {
            352_800
        } else if max_pcm >= 176_400 {
            176_400
        } else if max_pcm >= 88_200 {
            88_200
        } else {
            44_100
        };

        let bit_depth = if caps.supports_pcm && caps.pcm_max_bits > 0 {
            caps.pcm_max_bits.min(32)
        } else {
            32
        };

        Self::DsdToPcm {
            pcm_sample_rate: pcm_rate,
            bit_depth: bit_depth as u8,
        }
    }
}

/// 检查路径是否为支持的 DSD 音频文件
pub fn is_dsd_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".dsf") || lower.ends_with(".dff")
}

/// 8 位比特反转查找表 (LSB <-> MSB 翻转)
///
/// DSF 格式内部按 LSB-First 存储，DAC 及 Diretta 标准帧要求 MSB-First。
pub const BIT_REVERSE_LUT: [u8; 256] = [
    0x00, 0x80, 0x40, 0xc0, 0x20, 0xa0, 0x60, 0xe0, 0x10, 0x90, 0x50, 0xd0, 0x30, 0xb0, 0x70, 0xf0,
    0x08, 0x88, 0x48, 0xc8, 0x28, 0xa8, 0x68, 0xe8, 0x18, 0x98, 0x58, 0xd8, 0x38, 0xb8, 0x78, 0xf8,
    0x04, 0x84, 0x44, 0xc4, 0x24, 0xa4, 0x64, 0xe4, 0x14, 0x94, 0x54, 0xd4, 0x34, 0xb4, 0x74, 0xf4,
    0x0c, 0x8c, 0x4c, 0xcc, 0x2c, 0xac, 0x6c, 0xec, 0x1c, 0x9c, 0x5c, 0xdc, 0x3c, 0xbc, 0x7c, 0xfc,
    0x02, 0x82, 0x42, 0xc2, 0x22, 0xa2, 0x62, 0xe2, 0x12, 0x92, 0x52, 0xd2, 0x32, 0xb2, 0x72, 0xf2,
    0x0a, 0x8a, 0x4a, 0xca, 0x2a, 0xaa, 0x6a, 0xea, 0x1a, 0x9a, 0x5a, 0xda, 0x3a, 0xba, 0x7a, 0xfa,
    0x06, 0x86, 0x46, 0xc6, 0x26, 0xa6, 0x66, 0xe6, 0x16, 0x96, 0x56, 0xd6, 0x36, 0xb6, 0x76, 0xf6,
    0x0e, 0x8e, 0x4e, 0xce, 0x2e, 0xae, 0x6e, 0xee, 0x1e, 0x9e, 0x5e, 0xde, 0x3e, 0xbe, 0x7e, 0xfe,
    0x01, 0x81, 0x41, 0xc1, 0x21, 0xa1, 0x61, 0xe1, 0x11, 0x91, 0x51, 0xd1, 0x31, 0xb1, 0x71, 0xf1,
    0x09, 0x89, 0x49, 0xc9, 0x29, 0xa9, 0x69, 0xe9, 0x19, 0x99, 0x59, 0xd9, 0x39, 0xb9, 0x79, 0xf9,
    0x05, 0x85, 0x45, 0xc5, 0x25, 0xa5, 0x65, 0xe5, 0x15, 0x95, 0x55, 0xd5, 0x35, 0xb5, 0x75, 0xf5,
    0x0d, 0x8d, 0x4d, 0xcd, 0x2d, 0xad, 0x6d, 0xed, 0x1d, 0x9d, 0x5d, 0xdd, 0x3d, 0xbd, 0x7d, 0xfd,
    0x03, 0x83, 0x43, 0xc3, 0x23, 0xa3, 0x63, 0xe3, 0x13, 0x93, 0x53, 0xd3, 0x33, 0xb3, 0x73, 0xf3,
    0x0b, 0x8b, 0x4b, 0xcb, 0x2b, 0xab, 0x6b, 0xeb, 0x1b, 0x9b, 0x5b, 0xdb, 0x3b, 0xbb, 0x7b, 0xfb,
    0x07, 0x87, 0x47, 0xc7, 0x27, 0xa7, 0x67, 0xe7, 0x17, 0x97, 0x57, 0xd7, 0x37, 0xb7, 0x77, 0xf7,
    0x0f, 0x8f, 0x4f, 0xcf, 0x2f, 0xaf, 0x6f, 0xef, 0x1f, 0x9f, 0x5f, 0xdf, 0x3f, 0xbf, 0x7f, 0xff,
];

#[inline(always)]
pub fn reverse_byte(b: u8) -> u8 {
    BIT_REVERSE_LUT[b as usize]
}
