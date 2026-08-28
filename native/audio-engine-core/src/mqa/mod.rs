//! MQA (Master Quality Authenticated) 纯 Rust 深度比特流探测器
//!
//! 具备以下能力：
//! 1. 基于 MQA_identifier 标准的 36-bit 同步指纹扫描 (`0xBE0498C88`)；
//! 2. 左右声道 `(sL ^ sR)` 异或相位指纹提取，滤除音频伪特征；
//! 3. 覆盖 16-bit 与 24-bit (S32) 常见 LSB 偏移（shift 0, 1, 2, 8）；
//! 4. 跨帧持续状态机，用于快速判定并自动避让 DSP 进行 Bit-Perfect 直通。

/// MQA 36-bit 专属同步指纹
pub const MQA_MAGIC_36: u64 = 0xBE0498C88;
pub const MQA_MASK_36: u64 = 0xFFFFFFFFF;

/// MQA 比特流检测状态机
#[derive(Debug, Clone, Default)]
pub struct MqaDetector {
    regs: [u64; 4],
    detected: bool,
    processed_samples: usize,
}

impl MqaDetector {
    pub fn new() -> Self {
        Self {
            regs: [0; 4],
            detected: false,
            processed_samples: 0,
        }
    }

    /// 重置探测状态机
    pub fn reset(&mut self) {
        self.regs = [0; 4];
        self.detected = false;
        self.processed_samples = 0;
    }

    /// 是否已成功命中 MQA 指纹
    #[inline(always)]
    pub fn is_mqa(&self) -> bool {
        self.detected
    }

    /// 处理交错立体声 16-bit PCM 数据 (`[L0, R0, L1, R1, ...]`)
    pub fn feed_interleaved_s16(&mut self, samples: &[i16]) -> bool {
        if self.detected {
            return true;
        }

        let pairs = samples.len() / 2;
        for i in 0..pairs {
            let s_l = samples[i * 2] as u16 as u32;
            let s_r = samples[i * 2 + 1] as u16 as u32;
            let xored = s_l ^ s_r;

            // 检查 0, 1, 2, 3 位移
            for shift in 0..4 {
                let bit = ((xored >> shift) & 1) as u64;
                self.regs[shift] = ((self.regs[shift] << 1) | bit) & MQA_MASK_36;
                if self.regs[shift] == MQA_MAGIC_36 {
                    self.detected = true;
                    return true;
                }
            }
        }
        self.processed_samples += pairs;
        false
    }

    /// 处理交错立体声 32-bit (含 24-bit 填充) PCM 数据
    pub fn feed_interleaved_s32(&mut self, samples: &[i32]) -> bool {
        if self.detected {
            return true;
        }

        let pairs = samples.len() / 2;
        for i in 0..pairs {
            let s_l = samples[i * 2] as u32;
            let s_r = samples[i * 2 + 1] as u32;
            let xored = s_l ^ s_r;

            // 针对 24-bit (LSB 在 bit 0 或 bit 8) 进行多位置扫描
            let shifts = [0, 1, 2, 8];
            for (idx, &shift) in shifts.iter().enumerate() {
                let bit = ((xored >> shift) & 1) as u64;
                self.regs[idx] = ((self.regs[idx] << 1) | bit) & MQA_MASK_36;
                if self.regs[idx] == MQA_MAGIC_36 {
                    self.detected = true;
                    return true;
                }
            }
        }
        self.processed_samples += pairs;
        false
    }

    /// 处理交错立体声 f32 数据（先量化为 24-bit PCM 后检测）
    pub fn feed_interleaved_f32(&mut self, samples: &[f32]) -> bool {
        if self.detected {
            return true;
        }

        let pairs = samples.len() / 2;
        for i in 0..pairs {
            let l_clamped = samples[i * 2].clamp(-1.0, 1.0);
            let r_clamped = samples[i * 2 + 1].clamp(-1.0, 1.0);

            let s_l = (l_clamped * 8_388_607.0) as i32;
            let s_r = (r_clamped * 8_388_607.0) as i32;

            let xored = (s_l as u32) ^ (s_r as u32);
            for shift in 0..4 {
                let bit = ((xored >> shift) & 1) as u64;
                self.regs[shift] = ((self.regs[shift] << 1) | bit) & MQA_MASK_36;
                if self.regs[shift] == MQA_MAGIC_36 {
                    self.detected = true;
                    return true;
                }
            }
        }
        self.processed_samples += pairs;
        false
    }
}
