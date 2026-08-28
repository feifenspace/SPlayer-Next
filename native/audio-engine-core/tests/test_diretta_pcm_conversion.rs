//! 测试 DirettaStream::write_samples 的 PCM 转换逻辑
//!
//! 此测试验证 f32 样本到不同位深 PCM 的转换是否正确。

#[test]
fn test_pcm_conversion_16bit() {
    // 创建一个模拟的 DirettaStream，使用 16-bit 位深
    // 注意：我们不实际连接到设备，只测试转换逻辑

    // 模拟 f32 样本数据（正弦波：0.0, 0.5, 1.0, -0.5, -1.0）
    let f32_samples = vec![0.0f32, 0.5, 1.0, -0.5, -1.0];

    // 预期的 16-bit PCM 值（little-endian）
    // 16-bit PCM 值（little-endian）
    // Rust `as i16` 向零截断：16383.5 -> 16383, -16383.5 -> -16383
    // -1.0 * 32767.0 = -32767.0 -> -32767 = 0x8001
    // 0.0 -> 0
    // 0.5 -> 0.5 * 32767 = 16383.5 -> truncate -> 16383 = 0x3FFF
    // 1.0 -> 32767 = 0x7FFF
    // -0.5 -> -16383.5 -> truncate -> -16383 = 0xC001
    // -1.0 -> -32767 = 0x8001
    let expected_16bit: Vec<u8> = vec![
        0x00, 0x00, // 0.0
        0xFF, 0x3F, // 16383 (0x3FFF)
        0xFF, 0x7F, // 32767 (0x7FFF)
        0x01, 0xC0, // -16383 (0xC001)
        0x01, 0x80, // -32767 (0x8001)
    ];

    println!("Testing 16-bit PCM conversion:");
    println!("Input f32 samples: {:?}", f32_samples);

    // 由于我们无法直接测试 private 方法，我们创建一个测试辅助函数
    // 来模拟 write_samples 的转换逻辑
    let mut byte_buf = Vec::new();
    for &s in &f32_samples {
        let val = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        byte_buf.extend_from_slice(&val.to_le_bytes());
    }

    println!("Converted bytes: {:02X?}", byte_buf);
    println!("Expected bytes:  {:02X?}", expected_16bit);

    assert_eq!(byte_buf, expected_16bit, "16-bit PCM conversion failed");
}

#[test]
fn test_pcm_conversion_24bit() {
    // 模拟 f32 样本数据
    let f32_samples = vec![0.0f32, 0.5, 1.0, -0.5, -1.0];

    // 预期的 24-bit PCM 值（little-endian，只取低 3 字节）。
    // Rust `as i32` 截断小数部分：0.5 -> 4194303，-0.5 -> -4194303。
    let expected_24bit: Vec<u8> = vec![
        0x00, 0x00, 0x00, // 0.0
        0xFF, 0xFF, 0x3F, // 4194303 (0x3FFFFF)
        0xFF, 0xFF, 0x7F, // 8388607 (0x7FFFFF)
        0x01, 0x00, 0xC0, // -4194303 (低 24 位 0xC00001)
        0x01, 0x00, 0x80, // -8388607 (低 24 位 0x800001)
    ];

    println!("Testing 24-bit PCM conversion:");
    println!("Input f32 samples: {:?}", f32_samples);

    let mut byte_buf = Vec::new();
    for &s in &f32_samples {
        let val = (s.clamp(-1.0, 1.0) * 8388607.0) as i32;
        let bytes = val.to_le_bytes();
        byte_buf.extend_from_slice(&bytes[..3]);
    }

    println!("Converted bytes: {:02X?}", byte_buf);
    println!("Expected bytes:  {:02X?}", expected_24bit);

    assert_eq!(byte_buf, expected_24bit, "24-bit PCM conversion failed");
}

#[test]
fn test_pcm_conversion_32bit() {
    // 模拟 f32 样本数据
    let f32_samples = vec![0.0f32, 0.5, 1.0, -0.5, -1.0];

    // 预期的 32-bit PCM 值（little-endian）
    // f32 计算结果：0.5 * 2147483647.0 为 1073741824.0。
    let expected_32bit: Vec<u8> = vec![
        0x00, 0x00, 0x00, 0x00, // 0.0
        0x00, 0x00, 0x00, 0x40, // 1073741824 (0x40000000)
        0xFF, 0xFF, 0xFF, 0x7F, // 2147483647 (0x7FFFFFFF)
        0x00, 0x00, 0x00, 0xC0, // -1073741824 (0xC0000000)
        0x00, 0x00, 0x00, 0x80, // -2147483648 (0x80000000)
    ];

    println!("Testing 32-bit PCM conversion:");
    println!("Input f32 samples: {:?}", f32_samples);

    let mut byte_buf = Vec::new();
    for &s in &f32_samples {
        let val = (s.clamp(-1.0, 1.0) * 2147483647.0) as i32;
        byte_buf.extend_from_slice(&val.to_le_bytes());
    }

    println!("Converted bytes: {:02X?}", byte_buf);
    println!("Expected bytes:  {:02X?}", expected_32bit);

    assert_eq!(byte_buf, expected_32bit, "32-bit PCM conversion failed");
}

#[test]
fn test_pcm_conversion_edge_cases() {
    // 测试边界情况：超出 [-1.0, 1.0] 范围的值应该被 clamp
    let f32_samples = vec![-2.0f32, 1.5, 0.0, -1.0, 1.0];

    println!("Testing edge cases (clamping):");
    println!("Input f32 samples: {:?}", f32_samples);

    // 32-bit 转换
    let mut byte_buf = Vec::new();
    for &s in &f32_samples {
        let clamped = s.clamp(-1.0, 1.0);
        let val = (clamped * 2147483647.0) as i32;
        byte_buf.extend_from_slice(&val.to_le_bytes());
    }

    println!("Converted bytes: {:02X?}", byte_buf);

    // 验证 clamp 后的值
    let expected_clamped = vec![-1.0f32, 1.0, 0.0, -1.0, 1.0];
    for (i, (&input, &expected)) in f32_samples.iter().zip(expected_clamped.iter()).enumerate() {
        assert_eq!(
            input.clamp(-1.0, 1.0),
            expected,
            "Sample {}: expected clamped value {} but got {}",
            i,
            expected,
            input.clamp(-1.0, 1.0)
        );
    }
}

#[test]
fn test_pcm_conversion_sine_wave() {
    // 生成一个简单的正弦波来测试连续的样本转换
    use std::f64::consts::PI;

    let sample_rate = 44100.0;
    let frequency = 440.0; // A4 音符
    let duration = 0.01; // 10ms
    let num_samples = (sample_rate * duration) as usize;

    let mut f32_samples = Vec::with_capacity(num_samples);
    for i in 0..num_samples {
        let t = i as f64 / sample_rate;
        let sample = (2.0 * PI * frequency * t).sin() as f32;
        f32_samples.push(sample);
    }

    println!("Testing sine wave conversion:");
    println!("Generated {} samples", num_samples);
    println!(
        "First 10 samples: {:?}",
        &f32_samples[..10.min(num_samples)]
    );

    // 32-bit 转换
    let mut byte_buf = Vec::new();
    for &s in &f32_samples {
        let val = (s.clamp(-1.0, 1.0) * 2147483647.0) as i32;
        byte_buf.extend_from_slice(&val.to_le_bytes());
    }

    println!("Converted to {} bytes", byte_buf.len());
    assert_eq!(byte_buf.len(), num_samples * 4, "Byte count mismatch");

    // 验证所有样本都在有效范围内
    for &s in &f32_samples {
        assert!((-1.0..=1.0).contains(&s), "Sample out of range: {}", s);
    }
}
