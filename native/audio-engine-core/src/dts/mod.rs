//! DTS-WAV / DTS-CD 比特流特征与防爆音探测器
//!
//! 具备以下能力：
//! 1. 扫描 16-bit PCM 音频流中的 DTS 5.1 环绕声同步字；
//! 2. 支持 16-bit Big Endian (`0x7FFE8001`)、16-bit Little Endian (`0xFE7F0180`)；
//! 3. 支持 14-bit 封装模式 (`0x1FFFE800` / `0xFF1F00E8`)；
//! 4. 在播放与扫库阶段识别 DTS CD，防止直通给立体声输出时产生刺耳噪声。

pub const DTS_SYNC_16_BE: u32 = 0x7FFE8001;
pub const DTS_SYNC_16_LE: u32 = 0xFE7F0180;
pub const DTS_SYNC_14_BE: u32 = 0x1FFFE800;
pub const DTS_SYNC_14_LE: u32 = 0xFF1F00E8;

/// DTS 比特流检测器
#[derive(Debug, Clone, Default)]
pub struct DtsDetector {
    detected: bool,
    sync_count: usize,
    sample_window: u32,
}

impl DtsDetector {
    pub fn new() -> Self {
        Self {
            detected: false,
            sync_count: 0,
            sample_window: 0,
        }
    }

    pub fn reset(&mut self) {
        self.detected = false;
        self.sync_count = 0;
        self.sample_window = 0;
    }

    #[inline(always)]
    pub fn is_dts(&self) -> bool {
        self.detected
    }

    /// 扫描字节流中的 DTS 同步字
    pub fn feed_bytes(&mut self, data: &[u8]) -> bool {
        if self.detected {
            return true;
        }

        if data.len() < 4 {
            return false;
        }

        for i in 0..data.len() - 3 {
            let word = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
            if word == DTS_SYNC_16_BE
                || word == DTS_SYNC_16_LE
                || word == DTS_SYNC_14_BE
                || word == DTS_SYNC_14_LE
            {
                self.sync_count += 1;
                if self.sync_count >= 2 {
                    self.detected = true;
                    return true;
                }
            }
        }
        false
    }

    /// 扫描 16-bit PCM 采样序列中的 DTS 特征
    pub fn feed_samples_s16(&mut self, samples: &[i16]) -> bool {
        if self.detected {
            return true;
        }

        for &sample in samples {
            self.sample_window = (self.sample_window << 16) | (sample as u16 as u32);
            if self.sample_window == DTS_SYNC_16_BE
                || self.sample_window == DTS_SYNC_16_LE
                || self.sample_window == DTS_SYNC_14_BE
                || self.sample_window == DTS_SYNC_14_LE
            {
                self.sync_count += 1;
                if self.sync_count >= 2 {
                    self.detected = true;
                    return true;
                }
            }
        }
        false
    }
}
