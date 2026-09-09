//! DoP v1.1（DSD over PCM）打包器（蓝图 §3.4）。
//!
//! DoP v1.1：每个 24 位 PCM 样本承载一个声道的 2 个 DSD 字节：
//! `[marker][dsd_b0][dsd_b1]`（高位在前）。marker 在 0x05 与 0xFA 之间
//! 每 16 样本翻转一次，跨缓冲连续计数——DAC 以此实现帧对齐与 DSD/PCM 判别。
//! PCM 采样率 = DSD 比特率 / 16（如 DSD64 → 176.4kHz）。
//!
//! 经现有 PCM Direct 连接发往 Target：打包发生在 Rust 侧 producer 填槽阶段，
//! 输出为 S32_LE 容器（24 位样本左对齐至高位）。FormatID 体系无 DoP 概念，
//! Target 是否解码 DoP 取决于固件——选择权在用户（`dsd_transport` 配置），
//! 播放失败直接报错回退，不做静默降级。

/// marker 翻转周期（样本数）：0x05 × 16 → 0xFA × 16 循环
const MARKER_PERIOD: usize = 32;

#[derive(Debug, Default, Clone)]
pub struct DopStreamPacker {
    /// 32 样本周期内的相位计数（跨缓冲连续，保证 DAC 帧对齐）
    phase: usize,
}

impl DopStreamPacker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 重置相位（新曲目起始时调用；流中途不得重置，否则 DAC 失去对齐）
    pub fn reset(&mut self) {
        self.phase = 0;
    }

    /// 将交错 DSD 字节流打包为 DoP 24 位 PCM 帧（i32 容器，24 位左对齐高位）。
    ///
    /// - `dsd`：交错 DSD 字节流（帧粒度 = 每声道 2 字节）
    /// - `channels`：声道数
    /// - `out`：输出样本缓冲，容量决定本次最多写出的帧数
    ///
    /// 返回写出的 PCM 帧数；输入不足一帧时不消费、返回 0
    pub fn pack_interleaved(&mut self, dsd: &[u8], channels: usize, out: &mut [i32]) -> usize {
        assert!(channels > 0, "声道数无效");
        let bytes_per_frame = channels * 2;
        let frames = dsd.len() / bytes_per_frame;
        let frames = frames.min(out.len() / channels);
        for frame_idx in 0..frames {
            let marker = self.marker();
            let src = &dsd[frame_idx * bytes_per_frame..];
            for (ch, slot) in out[frame_idx * channels..][..channels]
                .iter_mut()
                .enumerate()
            {
                let b0 = src[ch * 2] as u32;
                let b1 = src[ch * 2 + 1] as u32;
                let sample24 = ((marker as u32) << 16) | (b0 << 8) | b1;
                // 24 位 DoP 样本左对齐至 S32 容器高位
                *slot = ((sample24 << 8) as u32) as i32;
            }
            self.phase = (self.phase + 1) % MARKER_PERIOD;
        }
        frames
    }

    /// 当前相位对应的 marker 字节（每 16 样本在 0x05/0xFA 间翻转）
    fn marker(&self) -> u8 {
        if (self.phase / 16) % 2 == 0 {
            0x05
        } else {
            0xFA
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DoP v1.1 向量：立体声前 3 帧。首 16 帧 marker=0x05，
    /// 每帧 = [0x05][L 字节][R 字节] 左对齐 S32
    #[test]
    fn packs_stereo_frames_with_0x05_marker_in_first_phase() {
        let dsd = [
            0xA1, 0xB1, 0xA2, 0xB2, 0xA3, 0xB3, 0xA4, 0xB4, 0xA5, 0xB5, 0xA6, 0xB6,
        ];
        let mut packer = DopStreamPacker::new();
        let mut out = [0i32; 6];
        let frames = packer.pack_interleaved(&dsd, 2, &mut out);
        assert_eq!(frames, 3);
        assert_eq!(out[0], (0x05 << 24) | (0xA1 << 16) | (0xB1 << 8));
        assert_eq!(out[1], (0x05 << 24) | (0xA2 << 16) | (0xB2 << 8));
        assert_eq!(out[2], (0x05 << 24) | (0xA3 << 16) | (0xB3 << 8));
        assert_eq!(out[3], (0x05 << 24) | (0xA4 << 16) | (0xB4 << 8));
        assert_eq!(out[4], (0x05 << 24) | (0xA5 << 16) | (0xB5 << 8));
        assert_eq!(out[5], (0x05 << 24) | (0xA6 << 16) | (0xB6 << 8));
    }

    /// marker 相位在第 16 帧翻转（0x05×16 → 0xFA×16），且跨缓冲连续
    #[test]
    fn marker_alternates_every_16_frames_across_buffers() {
        let mut packer = DopStreamPacker::new();
        // 一次喂 40 帧（交错 5 字节序列 × 2 声道）
        let mut dsd = Vec::new();
        for i in 0..40usize {
            // 每帧每声道 2 字节：L=[i, 0x55] R=[i^0xFF, 0x55]
            dsd.push((i & 0xFF) as u8);
            dsd.push(0x55);
            dsd.push((!(i as u8)) & 0xFF);
            dsd.push(0x55);
        }
        let mut out = [0i32; 80];
        let frames = packer.pack_interleaved(&dsd, 2, &mut out);
        assert_eq!(frames, 40);

        let marker_of = |frame: usize| (out[frame * 2] >> 24) as u8 & 0xFF;
        assert_eq!(marker_of(0), 0x05);
        assert_eq!(marker_of(15), 0x05);
        assert_eq!(marker_of(16), 0xFA);
        assert_eq!(marker_of(31), 0xFA);
        assert_eq!(marker_of(32), 0x05);
        assert_eq!(marker_of(39), 0x05);

        // 跨缓冲：相位跨缓冲连续。再喂 8 帧后相位 48 % 32 = 16 → 0xFA
        let eight = vec![0u8; 32];
        let frames_extra = packer.pack_interleaved(&eight, 2, &mut [0i32; 16]);
        assert_eq!(frames_extra, 16 - 8); // 本周期仅剩 8 帧（40..48）
        let mut out2 = [0i32; 2];
        let frames2 = packer.pack_interleaved(&[0x77, 0x88, 0x99, 0xAA], 2, &mut out2);
        assert_eq!(frames2, 1);
        assert_eq!((out2[0] >> 24) as u8, 0xFA);
    }

    /// DSD 数据零不得覆盖 marker（全零 DSD 输入仍保持 0x05/0xFA 可判别）
    #[test]
    fn silence_preserves_marker_discriminability() {
        let mut packer = DopStreamPacker::new();
        let dsd = vec![0u8; 64]; // 16 帧立体声全零
        let mut out = [0i32; 32];
        packer.pack_interleaved(&dsd, 2, &mut out);
        for frame in out.iter() {
            assert_eq!((frame >> 24) as u8, 0x05);
            assert_eq!(frame & 0x00FF_FFFF, 0);
        }
    }

    /// 输入不足一帧时不消费不输出
    #[test]
    fn partial_frames_are_not_consumed() {
        let mut packer = DopStreamPacker::new();
        let mut out = [0i32; 4];
        assert_eq!(packer.pack_interleaved(&[0x11, 0x22, 0x33], 2, &mut out), 0);
        assert_eq!(packer.pack_interleaved(&[], 2, &mut out), 0);
    }
}
