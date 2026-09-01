//! 垫片（shim）：仅保留 NAPI 绑定入口，实现整体委托给 audio-engine-core。
//!
//! 通过把 core 的模块 re-export 到 crate 根，bindings 代码中的 `crate::`
//! 路径无需修改即可解析到 core 实现，从而与上游 audio-engine 的绑定层
//! 保持逐字一致，便于后续同步。

mod bindings;

pub use audio_engine_core::audio_output;
pub use audio_engine_core::decoder;
pub use audio_engine_core::device_watcher;
pub use audio_engine_core::direct_dsd;
pub use audio_engine_core::direct_pcm;
pub use audio_engine_core::direct_runtime;
pub use audio_engine_core::diretta;
pub use audio_engine_core::equalizer;
pub use audio_engine_core::error;
pub use audio_engine_core::fft;
pub use audio_engine_core::logger;
pub use audio_engine_core::loudness;
pub use audio_engine_core::metadata;
pub use audio_engine_core::playback;
pub use audio_engine_core::player;
pub use audio_engine_core::priority;
pub use audio_engine_core::scanner;
pub use audio_engine_core::shared;
pub use audio_engine_core::source;
pub use audio_engine_core::tempo;

pub use bindings::*;
