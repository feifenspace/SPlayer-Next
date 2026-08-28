use std::io::ErrorKind;

use anyhow::Error;
use audio_engine_core::{
    AudioChunk, AudioEngineError, FftAnalyzer, PlayerState, PopResult, Shared,
};

#[test]
fn shared_round_trips_chunks_and_tracks_position() {
    let shared = Shared::new(48_000, 2);

    assert_eq!(shared.sample_rate(), 48_000);
    assert!(matches!(shared.try_pop(), PopResult::Pending));

    shared.push(AudioChunk {
        player_samples: vec![0.1, -0.1],
        fft_samples: vec![0.2, -0.2],
        source_sample_count: 1,
    });
    let decoded = shared
        .pop_decoded()
        .expect("decoded chunk should be available");
    assert_eq!(decoded.source_sample_count, 1);
    assert_eq!(decoded.player_samples, vec![0.1, -0.1]);

    shared.push_output(AudioChunk {
        player_samples: vec![0.3, -0.3],
        fft_samples: vec![0.4, -0.4],
        source_sample_count: 1,
    });
    assert!(!shared.is_buffer_empty());
    assert!(matches!(shared.try_pop(), PopResult::Chunk(_)));

    shared.advance_consumed(96_000);
    assert!((shared.consumed_position() - 1.0).abs() < f64::EPSILON);
}

#[test]
fn shared_stop_finishes_waiters_without_marking_playback_complete() {
    let shared = Shared::new(48_000, 2);

    shared.stop();

    assert!(shared.is_stopping());
    assert!(!shared.is_all_consumed());
    assert!(matches!(shared.try_pop(), PopResult::Finished));
    assert!(shared.pop_decoded().is_none());
}

#[test]
fn shared_normalization_state_is_updated_atomically() {
    let shared = Shared::new(44_100, 2);

    assert!(!shared.is_normalization_enabled());
    assert_eq!(shared.normalization_gain(), 1.0);

    shared.set_normalization_enabled(true);
    shared.set_normalization_gain(0.5);

    assert!(shared.is_normalization_enabled());
    assert!((shared.normalization_gain() - 0.5).abs() < f32::EPSILON);
}

#[test]
fn fft_analyzer_returns_fixed_sized_silent_spectrum() {
    let analyzer = FftAnalyzer::new();
    analyzer.set_enabled(true);
    analyzer.push_interleaved_samples(&vec![0.0; 2048 * 2]);

    let (left, right) = analyzer.analyze();

    assert!(analyzer.is_enabled());
    assert_eq!(left.len(), 128);
    assert_eq!(right.len(), 128);
    assert!(left.iter().all(|value| *value == 0.0));
    assert!(right.iter().all(|value| *value == 0.0));
}

#[test]
fn audio_errors_use_stable_codes() {
    let error = Error::new(std::io::Error::from(ErrorKind::NotFound));
    let classified = AudioEngineError::classify(&error);

    assert_eq!(classified.code(), "SourceNotFound");
}

#[test]
fn player_states_are_copyable_and_comparable() {
    let state = PlayerState::Playing;
    let copied = state;

    assert_eq!(state, copied);
    assert_ne!(state, PlayerState::Paused);
}

#[cfg(feature = "napi")]
#[test]
fn napi_feature_exposes_metadata_and_scanner_modules() {
    use std::collections::HashMap;

    let tags = audio_engine_core::metadata::extract_tags(&HashMap::from([
        ("TITLE".to_string(), "Track".to_string()),
        ("ALBUM_ARTIST".to_string(), "Artist".to_string()),
    ]));

    assert_eq!(tags.title.as_deref(), Some("Track"));
    assert_eq!(tags.artist.as_deref(), Some("Artist"));
    assert!(audio_engine_core::scanner::file_stat(std::path::Path::new("/no/such/file")).is_none());
}

#[cfg(feature = "napi")]
#[test]
fn napi_feature_exposes_tempo_processor() {
    use audio_engine_core::tempo::StretchProcessor;

    let mut processor = StretchProcessor::new(2, 48_000);
    assert!(processor.is_bypass());

    processor.set_speed(3.0);
    processor.set_pitch(24);
    assert_eq!(processor.speed(), 2.0);
    assert_eq!(processor.pitch(), 12);
    assert!(!processor.is_bypass());

    processor.set_speed(f32::NAN);
    assert_eq!(processor.speed(), 1.0);
}
