use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use cpal::traits::StreamTrait;
use tracing::warn;

use crate::audio_output::{AudioOutput, PlaybackStream};
use crate::error::{AudioErrorKind, AudioResultExt};
use crate::source::DecoderSource;

/// 平台统一的播放控制句柄：持有一条独立的输出流。
/// 每次加载/seek 由 `attach` 创建，播放期间音量与停止通过原子标志与实时回调通信。
pub struct PlaybackHandle {
    stream: PlaybackStream,
    volume: Arc<AtomicU32>,
    stopped: Arc<AtomicBool>,
    /// Diretta 后端的暂停标志：暂停时推静音保持 Target 时钟在线
    paused: Arc<AtomicBool>,
}

impl PlaybackHandle {
    /// 按 `output` 的配置创建输出流并接入 `source`。
    /// 传入 `volume` 为初始音量，`paused` 为 true 时保持停止（恢复时由 `play` 启动）。
    pub fn attach(
        output: &AudioOutput,
        source: DecoderSource,
        volume: f32,
        paused: bool,
    ) -> Result<Self> {
        let volume = Arc::new(AtomicU32::new(volume.to_bits()));
        let stopped = Arc::new(AtomicBool::new(false));
        let paused_flag = Arc::new(AtomicBool::new(paused));
        let stream = output.build_stream(
            source,
            Arc::clone(&volume),
            Arc::clone(&stopped),
            Arc::clone(&paused_flag),
        )?;
        if let PlaybackStream::Cpal(stream) = &stream {
            if !paused {
                stream
                    .play()
                    .context("启动音频输出失败")
                    .with_audio_kind(AudioErrorKind::Device)?;
            }
        }
        Ok(Self {
            stream,
            volume,
            stopped,
            paused: paused_flag,
        })
    }

    pub fn play(&self) {
        match &self.stream {
            PlaybackStream::Cpal(stream) => {
                if let Err(error) = stream.play() {
                    warn!(%error, "恢复音频输出失败");
                }
            }
            PlaybackStream::Diretta => self.paused.store(false, Ordering::Release),
        }
    }

    pub fn pause(&self) {
        match &self.stream {
            PlaybackStream::Cpal(stream) => {
                if let Err(error) = stream.pause() {
                    warn!(%error, "暂停音频输出失败");
                }
            }
            PlaybackStream::Diretta => self.paused.store(true, Ordering::Release),
        }
    }

    /// 停止播放：实时回调转入静音填充，随句柄销毁释放输出流
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    pub fn set_volume(&self, volume: f32) {
        self.volume.store(volume.to_bits(), Ordering::Relaxed);
    }
}
