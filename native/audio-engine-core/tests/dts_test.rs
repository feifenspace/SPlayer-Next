use audio_engine_core::dts::{DtsDetector, DTS_SYNC_14_BE, DTS_SYNC_16_BE};

#[test]
fn test_dts_detector_sync_words() {
    let mut detector = DtsDetector::new();
    assert!(!detector.is_dts());

    // 构造带连续 2 次 DTS_SYNC_16_BE 同步字的数据
    let mut data = Vec::new();
    data.extend_from_slice(&DTS_SYNC_16_BE.to_be_bytes());
    data.extend_from_slice(&[0x00, 0x11, 0x22, 0x33]);
    data.extend_from_slice(&DTS_SYNC_16_BE.to_be_bytes());

    let hit = detector.feed_bytes(&data);
    assert!(hit);
    assert!(detector.is_dts());

    // 测试 14-bit 模式
    let mut detector_14 = DtsDetector::new();
    let mut data_14 = Vec::new();
    data_14.extend_from_slice(&DTS_SYNC_14_BE.to_be_bytes());
    data_14.extend_from_slice(&[0x44, 0x55, 0x66, 0x77]);
    data_14.extend_from_slice(&DTS_SYNC_14_BE.to_be_bytes());

    assert!(detector_14.feed_bytes(&data_14));
}
