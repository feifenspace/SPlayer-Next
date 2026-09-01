use audio_engine_core::dsd::{
    is_dsd_path, reverse_byte, Dsd2PcmDecimator, DsdRate, DsfReader,
};
use std::io::Write;

#[test]
fn test_bit_reverse_lut() {
    // 1. 测试反转表对称性
    for i in 0..=255u8 {
        let rev = reverse_byte(i);
        assert_eq!(reverse_byte(rev), i, "Bit reverse must be an involution");
    }

    // 2. 测试典型位模式
    assert_eq!(reverse_byte(0x80), 0x01);
    assert_eq!(reverse_byte(0x01), 0x80);
    assert_eq!(reverse_byte(0xAA), 0x55); // 10101010 -> 01010101
    assert_eq!(reverse_byte(0xF0), 0x0F); // 11110000 -> 00001111
    assert_eq!(reverse_byte(0x00), 0x00);
    assert_eq!(reverse_byte(0xFF), 0xFF);
}

#[test]
fn test_dsd_rate_conversion() {
    assert_eq!(DsdRate::from_sample_rate(2_822_400), DsdRate::Dsd64);
    assert_eq!(DsdRate::from_sample_rate(5_644_800), DsdRate::Dsd128);
    assert_eq!(DsdRate::from_sample_rate(11_289_600), DsdRate::Dsd256);
    assert_eq!(DsdRate::from_sample_rate(22_579_200), DsdRate::Dsd512);

    assert_eq!(DsdRate::Dsd64.to_hz(), 2_822_400);
    assert_eq!(DsdRate::Dsd128.to_hz(), 5_644_800);
    assert_eq!(DsdRate::Dsd256.to_hz(), 11_289_600);
    assert_eq!(DsdRate::Dsd512.to_hz(), 22_579_200);

    assert!(is_dsd_path("track.dsf"));
    assert!(is_dsd_path("ALBUM/SONG.DFF"));
    assert!(!is_dsd_path("song.flac"));
}

#[test]
fn test_dsf_reader_synthetic() {
    let temp_dir = tempfile::tempdir().unwrap();
    let dsf_path = temp_dir.path().join("test.dsf");

    // 构造合法的 Synthetic DSF 文件
    let mut file = std::fs::File::create(&dsf_path).unwrap();

    // 1. DSD chunk (28 bytes)
    file.write_all(b"DSD ").unwrap();
    file.write_all(&28u64.to_le_bytes()).unwrap(); // chunk size
    file.write_all(&0u64.to_le_bytes()).unwrap(); // file size
    file.write_all(&0u64.to_le_bytes()).unwrap(); // metadata ptr

    // 2. fmt chunk (52 bytes)
    file.write_all(b"fmt ").unwrap();
    file.write_all(&52u64.to_le_bytes()).unwrap(); // fmt size
    file.write_all(&1u32.to_le_bytes()).unwrap(); // format version
    file.write_all(&0u32.to_le_bytes()).unwrap(); // format id (0 = raw DSD)
    file.write_all(&2u32.to_le_bytes()).unwrap(); // channel type (2 = stereo)
    file.write_all(&2u32.to_le_bytes()).unwrap(); // channel num
    file.write_all(&2_822_400u32.to_le_bytes()).unwrap(); // sample rate (DSD64)
    file.write_all(&1u32.to_le_bytes()).unwrap(); // bits per sample
    file.write_all(&(2_822_400u64 * 2).to_le_bytes()).unwrap(); // sample count (2 seconds)
    file.write_all(&4096u32.to_le_bytes()).unwrap(); // block size per channel (4096)
    file.write_all(&0u32.to_le_bytes()).unwrap(); // reserved

    // 3. data chunk (12 bytes header + 8192 bytes test payload = 2 x 4096 blocks)
    let payload_size = 8192u64;
    file.write_all(b"data").unwrap();
    file.write_all(&(payload_size + 12).to_le_bytes()).unwrap();

    // 填充 4096 字节 Left (全部 0x80 = 10000000) 和 4096 字节 Right (全部 0x01 = 00000001)
    let left_block = vec![0x80u8; 4096];
    let right_block = vec![0x01u8; 4096];
    file.write_all(&left_block).unwrap();
    file.write_all(&right_block).unwrap();
    file.flush().unwrap();

    // 使用 DsfReader 读取并校验交织与比特反转
    let mut reader = DsfReader::open(&dsf_path).expect("Failed to open synthetic DSF");
    assert_eq!(reader.sample_rate, 2_822_400);
    assert_eq!(reader.channels, 2);
    assert_eq!(reader.dsd_rate, DsdRate::Dsd64);
    assert_eq!(reader.duration_seconds, 2.0);

    let mut out_buf = vec![0u8; 8192];
    let read_bytes = reader.read_interleaved_dsd(&mut out_buf).unwrap();
    assert_eq!(read_bytes, 8192);

    // 校验交织结果：每 8 字节为 [L0, L1, L2, L3, R0, R1, R2, R3]
    // 原 Left = 0x80 -> 反转后 = 0x01
    // 原 Right = 0x01 -> 反转后 = 0x80
    for chunk in out_buf.chunks_exact(8) {
        assert_eq!(&chunk[0..4], &[0x01, 0x01, 0x01, 0x01]);
        assert_eq!(&chunk[4..8], &[0x80, 0x80, 0x80, 0x80]);
    }
}

#[test]
fn test_dsd2pcm_decimator() {
    let mut decimator = Dsd2PcmDecimator::new(2_822_400);
    assert_eq!(decimator.target_sample_rate, 88_200);

    // 构造 64 字节交错 DSD 数据 (8 个 8-byte 周期)
    let mut dsd_bytes = Vec::new();
    for _ in 0..8 {
        // L=0x69 (标准静音), R=0x96 (标准静音)
        dsd_bytes.extend_from_slice(&[0x69, 0x69, 0x69, 0x69, 0x96, 0x96, 0x96, 0x96]);
    }

    let mut pcm_samples = Vec::new();
    decimator.convert(&dsd_bytes, &mut pcm_samples);

    // 64 字节 DSD (每 4 字节=32 bit 抽取为 2 个样本) -> 输出 16 个 stereo 样本 (32 floats)
    assert_eq!(pcm_samples.len(), 32);

    // 验证所有 PCM 样本均处于正常音频范围内 [-1.0, 1.0]
    for sample in pcm_samples {
        assert!(sample >= -1.0 && sample <= 1.0);
    }
}


