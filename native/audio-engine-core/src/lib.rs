//! FFmpeg 音频解码 + CPAL 播放 + Diretta Source Direct (Pull 模式)。
//!
//! 在 Headless 环境下直接使用，或通过 NAPI 包装供 Node.js 绑定。
//! FFT 频谱分析（fft feature）和 MIDI 合成器（midi feature）为可选，
//! headless-server 不开启这两个 feature 以减小二进制体积。

#![allow(dead_code)]

pub mod audio_output;
pub mod cue;
pub mod decoder;
pub mod device_watcher;
pub mod direct_dsd;
pub mod direct_pcm;
pub mod direct_runtime;
pub mod diretta;
pub mod dsd;
pub mod dts;
pub mod equalizer;
pub mod error;
pub mod fft;
pub mod hdcd;
pub mod logger;
pub mod loudness;
pub mod metadata;
#[cfg(feature = "midi")]
pub mod midi;
pub mod mqa;
pub mod playback;
pub mod player;
pub mod priority;
pub mod ram_buffer;
pub mod sacd;
pub mod scanner;
pub mod shared;
pub mod source;
pub mod tempo;

pub use error::AudioEngineError;
pub use ffmpeg_audio::{self, HttpCancelHandle};
pub use fft::FftAnalyzer;
pub use metadata::{
    cover_thumb_path, db_to_linear, extract_embedded_lyric, extract_replay_gain,
    find_all_external_lyrics, make_thumbnail_jpeg, AudioMetadata, StreamInfo,
};
pub use player::{EventEmitter, InnerPlayer, PlayerEvent, PlayerState};
pub use shared::{AudioChunk, PopResult, Shared};
