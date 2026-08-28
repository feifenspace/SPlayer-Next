use audio_engine_core::mqa::{MqaDetector, MQA_MAGIC_36};

#[test]
fn test_mqa_detector_s16_synthetic() {
    let mut detector = MqaDetector::new();
    assert!(!detector.is_mqa());

    // 构造包含 36-bit MQA 同步指纹的 16-bit 左右声道序列
    // MQA_MAGIC_36: 0xBE0498C88 (36 bits)
    let mut samples: Vec<i16> = Vec::new();

    // 填充 36 个采样对，使得 (sL ^ sR) & 1 正好产生 MQA_MAGIC_36 的比特序列
    for i in (0..36).rev() {
        let bit = ((MQA_MAGIC_36 >> i) & 1) as i16;
        let s_l = 1000i16;
        let s_r = 1000i16 ^ bit; // 产生目标 LSB 异或值
        samples.push(s_l);
        samples.push(s_r);
    }

    let detected = detector.feed_interleaved_s16(&samples);
    assert!(detected, "MQA detector must hit 36-bit magic pattern");
    assert!(detector.is_mqa());
}

#[test]
fn test_mqa_detector_s32_synthetic() {
    let mut detector = MqaDetector::new();

    let mut samples: Vec<i32> = Vec::new();
    for i in (0..36).rev() {
        let bit = ((MQA_MAGIC_36 >> i) & 1) as i32;
        let s_l = 100000i32;
        let s_r = 100000i32 ^ (bit << 8); // 24-bit 偏移
        samples.push(s_l);
        samples.push(s_r);
    }

    let detected = detector.feed_interleaved_s32(&samples);
    assert!(detected, "MQA detector must hit 24-bit shift-8 pattern");
}
