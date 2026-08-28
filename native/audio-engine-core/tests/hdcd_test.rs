use audio_engine_core::hdcd::HdcdProcessor;

#[test]
fn test_hdcd_peak_extend_expansion() {
    let mut hdcd = HdcdProcessor::new();

    // 1. 低电平信号（< -9dBFS，即 < 0.3548）保持线性
    let mut low_signal = vec![0.1f32, -0.2f32];
    hdcd.process_interleaved_stereo_f32(&mut low_signal);
    assert_eq!(low_signal, vec![0.1f32, -0.2f32]);

    // 2. 高电平信号（> -9dBFS）经 Peak Extend 动态扩展
    let mut high_signal = vec![0.8f32, -0.8f32];
    hdcd.process_interleaved_stereo_f32(&mut high_signal);

    // 扩展后幅度应该大于原幅度
    assert!(high_signal[0] > 0.8f32);
    assert!(high_signal[1] < -0.8f32);
    assert!(high_signal[0] <= 1.0f32);
}
