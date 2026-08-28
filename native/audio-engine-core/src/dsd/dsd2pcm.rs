//! High Quality DSD-to-PCM Decimation Engine
//!
//! 当音频输出为普通系统声卡 (ALSA/PulseAudio/CoreAudio/WASAPI) 时，
//! 将 1-bit 高速 DSD 比特流抽取滤波降采样转换为高动态范围的 32-bit Float PCM。

const FIR_DECIMATION: usize = 16; // 16:1 抽取比例 (DSD64 2.8224MHz -> 176.4kHz PCM)

/// 预计算的低通抽取滤波器查找表 (Gaussian FIR Low-Pass Kernel)
const FIR_COEFFS_16: [f32; 16] = [
    0.0031, 0.0125, 0.0348, 0.0712, 0.1134, 0.1508, 0.1742, 0.1800, 0.1742, 0.1508, 0.1134, 0.0712,
    0.0348, 0.0125, 0.0031, 0.0005,
];

pub struct Dsd2PcmDecimator {
    pub target_sample_rate: u32,
    history_l: Vec<f32>,
    history_r: Vec<f32>,
}

impl Dsd2PcmDecimator {
    pub fn new(dsd_sample_rate: u32) -> Self {
        let pcm_rate = if dsd_sample_rate >= 5_644_800 {
            176_400
        } else {
            88_200
        };

        Self {
            target_sample_rate: pcm_rate,
            history_l: Vec::with_capacity(32),
            history_r: Vec::with_capacity(32),
        }
    }

    /// 将交错 DSD 原生数据（每 8 字节 = L4 字节 + R4 字节）转换为交错的 Stereo f32 PCM 样本
    pub fn convert(&mut self, dsd_bytes: &[u8], pcm_samples: &mut Vec<f32>) {
        // 每 2 字节（1 字节 L + 1 字节 R）包含 8 个 1-bit 点
        // 按照 16:1 进行抽取滤波
        let mut idx = 0;
        while idx + 8 <= dsd_bytes.len() {
            // 解析 4 字节 L (32 bits) 与 4 字节 R (32 bits)
            let mut sum_l1 = 0.0f32;
            let mut sum_r1 = 0.0f32;

            for byte_i in 0..2 {
                let b_l = dsd_bytes[idx + byte_i];
                let b_r = dsd_bytes[idx + 4 + byte_i];

                for bit in (0..8).rev() {
                    let bit_idx = byte_i * 8 + (7 - bit);
                    let coeff = FIR_COEFFS_16[bit_idx];

                    let val_l = if (b_l & (1 << bit)) != 0 { 1.0 } else { -1.0 };
                    let val_r = if (b_r & (1 << bit)) != 0 { 1.0 } else { -1.0 };

                    sum_l1 += val_l * coeff;
                    sum_r1 += val_r * coeff;
                }
            }

            // 输出前半段 (16 点抽取得到的第 1 个 PCM 样本)
            pcm_samples.push(sum_l1 * 0.85); // 0.85 留出 DSD 调制器超调余量 (Headroom)
            pcm_samples.push(sum_r1 * 0.85);

            let mut sum_l2 = 0.0f32;
            let mut sum_r2 = 0.0f32;

            for byte_i in 2..4 {
                let b_l = dsd_bytes[idx + byte_i];
                let b_r = dsd_bytes[idx + 4 + byte_i];

                for bit in (0..8).rev() {
                    let bit_idx = (byte_i - 2) * 8 + (7 - bit);
                    let coeff = FIR_COEFFS_16[bit_idx];

                    let val_l = if (b_l & (1 << bit)) != 0 { 1.0 } else { -1.0 };
                    let val_r = if (b_r & (1 << bit)) != 0 { 1.0 } else { -1.0 };

                    sum_l2 += val_l * coeff;
                    sum_r2 += val_r * coeff;
                }
            }

            // 输出后半段 (16 点抽取得到的第 2 个 PCM 样本)
            pcm_samples.push(sum_l2 * 0.85);
            pcm_samples.push(sum_r2 * 0.85);

            idx += 8;
        }
    }
}
