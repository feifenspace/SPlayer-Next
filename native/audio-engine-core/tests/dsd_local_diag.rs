//! 临时诊断测试：完整复刻 regular_load_worker 流程（真 AudioOutput + 解码 + 消费计数），
//! 定位本地 DSD 播放 position 停滞的环节（跑完即删）
//! 用法: cargo test -p audio-engine-core --test dsd_local_diag -- --nocapture

use std::sync::Arc;
use std::time::Duration;

#[test]
fn dsf_local_full_pipeline_diagnosis() {
    let path = "/home/songlian/dsd_test.dsf";
    let handle = audio_engine_core::HttpCancelHandle::default();
    let prepared = audio_engine_core::decoder::prepare_decode(path, None, handle)
        .expect("prepare_decode 失败");
    println!("prepare_decode: original_rate={}", prepared.original_sample_rate());

    // 完全复刻 regular_load_worker：指定 alsa:null + 原始采样率
    let noop: audio_engine_core::audio_output::OutputFailureCallback = Arc::new(|| {});
    let out = audio_engine_core::audio_output::AudioOutput::new(
        Some("alsa:null"),
        Some(prepared.original_sample_rate()),
        0,
        noop,
    )
    .expect("AudioOutput 打开失败");
    println!(
        "AudioOutput: device_rate={} channels={}",
        out.sample_rate(),
        out.channels()
    );

    let shared = audio_engine_core::shared::Shared::new(out.sample_rate(), out.channels());
    let eq = Arc::new(parking_lot::Mutex::new(
        audio_engine_core::equalizer::Equalizer::new(out.sample_rate(), out.channels()),
    ));
    let tempo = Arc::new(parking_lot::Mutex::new(
        audio_engine_core::tempo::StretchProcessor::new(out.channels(), out.sample_rate()),
    ));
    let (meta, _decode_handle, _cancel) =
        audio_engine_core::decoder::start_prepared_decode(prepared, shared.clone(), eq, tempo)
            .expect("start_prepared_decode 失败");
    println!(
        "decode: codec={} rate={}",
        meta.codec, meta.sample_rate
    );

    for i in 0..12 {
        std::thread::sleep(Duration::from_millis(500));
        println!(
            "t={:>5}ms buffer_empty={} decode_failed={} consumed={:.3}s consumed_samples={}",
            (i + 1) * 500,
            shared.is_buffer_empty(),
            shared.is_decode_failed(),
            shared.consumed_position(),
            shared.samples_consumed_count(),
        );
    }
    std::process::exit(0);
}
