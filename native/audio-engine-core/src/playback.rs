use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cpal::traits::StreamTrait;
use tracing::warn;

use crate::audio_output::AudioOutput;
use crate::error::{AudioErrorKind, AudioResultExt};
use crate::source::{DecoderSource, IntegerDecoderSource};

/// 播放流（B1.1 单变体枚举扩展点）：新输出后端加变体，句柄层零改动
pub enum PlaybackStream {
    Cpal(cpal::Stream),
    #[cfg(target_os = "linux")]
    Alsa(crate::alsa_mmap_sink::AlsaMmapStream),
    #[cfg(target_os = "linux")]
    AlsaDsd(crate::alsa_mmap_sink::AlsaDsdStream),
}

impl PlaybackStream {
    pub fn play(&self) -> anyhow::Result<()> {
        match self {
            PlaybackStream::Cpal(stream) => stream.play().map_err(Into::into),
            #[cfg(target_os = "linux")]
            PlaybackStream::Alsa(stream) => {
                stream.play();
                Ok(())
            }
            #[cfg(target_os = "linux")]
            PlaybackStream::AlsaDsd(stream) => {
                stream.play();
                Ok(())
            }
        }
    }

    pub fn pause(&self) -> anyhow::Result<()> {
        match self {
            PlaybackStream::Cpal(stream) => stream.pause().map_err(Into::into),
            #[cfg(target_os = "linux")]
            PlaybackStream::Alsa(stream) => {
                stream.pause();
                Ok(())
            }
            #[cfg(target_os = "linux")]
            PlaybackStream::AlsaDsd(stream) => {
                stream.pause();
                Ok(())
            }
        }
    }
}

/// 平台统一的播放控制句柄：持有一条独立的输出流。
/// 每次加载/seek 由 `attach` 创建，播放期间音量与停止通过原子标志与实时回调通信。
pub struct PlaybackHandle {
    stream: PlaybackStream,
    volume: Arc<AtomicU32>,
    stopped: Arc<AtomicBool>,
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
        let stream = output.build_stream(source, Arc::clone(&volume), Arc::clone(&stopped))?;
        if !paused {
            stream
                .play()
                .context("启动音频输出失败")
                .with_audio_kind(AudioErrorKind::Device)?;
        }
        Ok(Self {
            stream,
            volume,
            stopped,
        })
    }

    /// 接入整数 PCM 输出。仅 ALSA MMAP 支持，其他后端直接返回错误。
    #[cfg(target_os = "linux")]
    pub fn attach_integer(
        output: &AudioOutput,
        source: IntegerDecoderSource,
        volume: f32,
        paused: bool,
    ) -> Result<Self> {
        let volume = Arc::new(AtomicU32::new(volume.to_bits()));
        let stopped = Arc::new(AtomicBool::new(false));
        let stream = output.build_integer_stream(
            source,
            Arc::clone(&volume),
            Arc::clone(&stopped),
        )?;
        if !paused {
            stream
                .play()
                .context("启动整数 ALSA 音频输出失败")
                .with_audio_kind(AudioErrorKind::Device)?;
        }
        Ok(Self {
            stream,
            volume,
            stopped,
        })
    }

    pub fn play(&self) {
        if let Err(error) = self.stream.play() {
            warn!(%error, "恢复音频输出失败");
        }
    }

    pub fn pause(&self) {
        if let Err(error) = self.stream.pause() {
            warn!(%error, "暂停音频输出失败");
        }
    }

    /// 停止播放：实时回调转入静音填充，随句柄销毁释放输出流
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);

        // CPAL 的回调只有在下一次被 ALSA/PipeWire 唤醒时才会看到 stopped。
        // 如果此处立即销毁 Stream，最后一个 DMA period 仍可能保留旧曲目
        // 的尾部样本，切歌时会形成很短的点击声。给静音回调一个稳定窗口，
        // 不改变正常播放的数据路径；MMAP 后端由自己的 worker 负责精确排空。
        if matches!(&self.stream, PlaybackStream::Cpal(_)) {
            std::thread::sleep(Duration::from_millis(120));
        }
    }

    pub fn set_volume(&self, volume: f32) {
        self.volume.store(volume.to_bits(), Ordering::Relaxed);
    }
}
