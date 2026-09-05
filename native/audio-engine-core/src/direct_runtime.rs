use anyhow::{bail, Result};

use std::time::Duration;

use crate::direct_dsd::{DirectDsdFormat, DirectDsdMonitor};
use crate::direct_pcm::{DirectPcmFormat, DirectPcmMonitor};

#[cfg(feature = "diretta")]
use crate::direct_dsd::DirectDsdStageHandle;
#[cfg(feature = "diretta")]
use crate::direct_pcm::DirectPcmStageHandle;

#[cfg(feature = "diretta")]
use std::path::Path;

#[cfg(feature = "diretta")]
use crate::diretta::{DirettaDirectConnection, DirettaDirectDsdConnection};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectFormat {
    Pcm(DirectPcmFormat),
    Dsd(DirectDsdFormat),
}

/// load 被更新的 load/stop 取代：调用方按"让位"处理而非作为故障上报。
/// Display 保留 `[Cancelled]` 前缀——NAPI 错误分类 is_cancelled_napi_error 依赖它
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("[Cancelled] load 已被更新的请求取代")]
pub struct LoadSuperseded;

/// Direct handoff/拆连接前排空：要求至少交付这么多块数字静音（顶掉设备端缓冲中的旧音频）
pub const DIRECT_FADE_DRAIN_MIN_BLOCKS: u32 = 4;

/// Direct 排空的事件等待上限（超时兜底，正常远快于此值）
pub const DIRECT_FADE_DRAIN_TIMEOUT: Duration = Duration::from_millis(600);

/// Diretta full reconnect 后的 Target/DAC 格式稳定窗口。
/// 仅替换现存 DirectPlayback（全量重连）时使用；同格式 staged/handoff 不经过此路径
pub const DIRECT_FULL_RECONNECT_STABILIZATION: Duration = Duration::from_millis(800);

/// 源扩展名判断是否 DSD 原生流（DSF/DFF/SACD ISO）
pub fn is_native_dsd_source(source: &str) -> bool {
    let lowered = source.to_lowercase();
    [".dsf", ".dff"].iter().any(|ext| lowered.contains(ext))
        || source.contains(".iso|")
        || source.contains(".ISO|")
}

/// Direct 载入结果：handoff（连接复用，已在任务内 commit）或全量重连（待 commit）
pub enum DirectLoadOutcome<M> {
    Handoff(M),
    FullReconnect {
        metadata: M,
        playback: DirectPlayback,
        token: u64,
    },
}

#[derive(Clone)]
pub enum DirectMonitor {
    Pcm(DirectPcmMonitor),
    Dsd(DirectDsdMonitor),
    #[cfg(test)]
    Fake(std::sync::Arc<FakeDirectState>),
}

impl DirectMonitor {
    pub fn consumed_position(&self) -> f64 {
        match self {
            Self::Pcm(value) => value.consumed_position(),
            Self::Dsd(value) => value.consumed_position(),
            #[cfg(test)]
            Self::Fake(value) => {
                value.position_micros.load(std::sync::atomic::Ordering::Acquire) as f64
                    / 1_000_000.0
            }
        }
    }

    pub fn failed(&self) -> bool {
        match self {
            Self::Pcm(value) => value.failed(),
            Self::Dsd(value) => value.failed(),
            #[cfg(test)]
            Self::Fake(value) => value.failed.load(std::sync::atomic::Ordering::Acquire),
        }
    }

    /// 等待设备消费的数据块数（READY + IN_FLIGHT）。Fake 监视器无 ring，恒为 0
    pub fn pending_blocks(&self) -> usize {
        match self {
            Self::Pcm(value) => value.pending_blocks(),
            Self::Dsd(value) => value.pending_blocks(),
            #[cfg(test)]
            Self::Fake(_) => 0,
        }
    }

    /// 事件驱动排空等待：淡出完成且已交付 min_blocks 块静音，或超时。
    /// DSD 无淡出通道恒 true；Fake 无音频流恒 true。
    /// 句柄持有 ring 的 Arc 引用，供调用方在 player 锁外排空
    pub fn wait_fade_drained(&self, min_blocks: u32, timeout: Duration) -> bool {
        match self {
            Self::Pcm(value) => value.wait_fade_drained(min_blocks, timeout),
            Self::Dsd(_) => true,
            #[cfg(test)]
            Self::Fake(_) => true,
        }
    }

    pub fn finished(&self) -> bool {
        match self {
            Self::Pcm(value) => value.finished(),
            Self::Dsd(value) => value.finished(),
            #[cfg(test)]
            Self::Fake(value) => value.finished.load(std::sync::atomic::Ordering::Acquire),
        }
    }

    /// 事件驱动首块消费等待：任一音频数据被设备消费、失败或源提前结束即唤醒。
    /// Fake 监视器（仅测试）无事件机制，直接返回 true 由调用方复查状态
    pub fn wait_first_consumed(&self, timeout: Duration) -> bool {
        match self {
            Self::Pcm(value) => value.wait_first_consumed(timeout),
            Self::Dsd(value) => value.wait_first_consumed(timeout),
            #[cfg(test)]
            Self::Fake(_) => true,
        }
    }

    pub fn transition_count(&self) -> u64 {
        match self {
            Self::Pcm(value) => value.transition_count(),
            Self::Dsd(value) => value.transition_count(),
            #[cfg(test)]
            Self::Fake(_) => 0,
        }
    }

    pub fn duration(&self) -> f64 {
        match self {
            Self::Pcm(value) => value.duration(),
            Self::Dsd(value) => value.duration(),
            #[cfg(test)]
            Self::Fake(_) => 0.0,
        }
    }

    pub fn boundary_generation(&self) -> u64 {
        match self {
            Self::Pcm(value) => value.boundary_generation(),
            Self::Dsd(value) => value.boundary_generation(),
            #[cfg(test)]
            Self::Fake(_) => 0,
        }
    }
}

#[cfg(feature = "diretta")]
#[derive(Clone)]
pub enum DirectStageHandle {
    Pcm(DirectPcmStageHandle),
    Dsd(DirectDsdStageHandle),
}

#[cfg(feature = "diretta")]
impl DirectStageHandle {
    pub fn stage_local(&self, source: &str, duration_secs: f64, generation: u64) -> Result<()> {
        if source.starts_with("http://") || source.starts_with("https://") {
            bail!("[Direct] 当前 gapless staging 仅支持本地 seekable 音源");
        }
        let (path_str, _start, cue_dur) =
            if let Some(cue) = crate::cue::parse_cue_virtual_path(source) {
                (
                    cue.physical_path,
                    cue.start_time,
                    if cue.duration > 0.0 {
                        cue.duration
                    } else {
                        duration_secs
                    },
                )
            } else if let Some(sacd) = crate::sacd::parse_sacd_virtual_path(source) {
                (
                    source.to_owned(),
                    0.0,
                    if sacd.duration_secs > 0.0 {
                        sacd.duration_secs
                    } else {
                        duration_secs
                    },
                )

            } else {
                (source.to_owned(), 0.0, duration_secs)
            };
        let path = Path::new(&path_str);
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let is_dsd = matches!(extension.as_str(), "dsf" | "dff" | "dsdiff" | "iso")
            || path_str.contains(".iso|")
            || path_str.contains(".ISO|");

        match self {
            Self::Pcm(value) => {
                if is_dsd {
                    bail!("[Direct] PCM → Native DSD 需要重新协商 Diretta connection");
                }
                value.stage_local(path, cue_dur, generation)
            }
            Self::Dsd(value) => {
                if !is_dsd {
                    bail!("[Direct] Native DSD → PCM 需要重新协商 Diretta connection");
                }
                value.stage_local(path, cue_dur, generation)
            }
        }
    }

    pub fn cancel(&self) {
        match self {
            Self::Pcm(value) => value.cancel(),
            Self::Dsd(value) => value.cancel(),
        }
    }
}

/// Direct 传输层：cfg 只出现在变体声明处（Fake 仅无 SDK 单测存在），
/// 方法体用带 cfg 的单 match 分派——不再需要每方法三套 cfg 与 `unreachable!()`
enum DirectTransport {
    #[cfg(feature = "diretta")]
    Pcm(DirettaDirectConnection),
    #[cfg(feature = "diretta")]
    Dsd(DirettaDirectDsdConnection),
    /// 无 SDK 单测（`all(test, not(feature = "diretta"))`）用的 fake 传输
    #[cfg(all(test, not(feature = "diretta")))]
    Fake(std::sync::Arc<FakeDirectState>),
}

impl DirectTransport {
    /// 换源/关流前的源级淡出：下一交付块 20ms 线性渐零，随后块为数字静音。
    /// DSD 无独立淡出通道（位流在块边界硬切换），保持 no-op。
    fn begin_fade_out(&self) {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => value.begin_fade_out(),
            #[cfg(feature = "diretta")]
            Self::Dsd(_) => {}
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => {}
            // 空枚举兜底：既无 diretta 也非 test 的构建不存在可构造的传输
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        }
    }

    /// 淡出是否已生效（后续块均为数字静音）。DSD/Fake 恒返回 true。
    fn is_faded_out(&self) -> bool {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => value.is_faded_out(),
            #[cfg(feature = "diretta")]
            Self::Dsd(_) => true,
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => true,
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        }
    }

    /// 事件驱动排空等待：淡出完成且已交付 min_blocks 块静音，或超时。
    /// DSD/Fake 无淡出需求，恒返回 true。
    fn wait_fade_drained(&self, min_blocks: u32, timeout: Duration) -> bool {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => value.wait_fade_drained(min_blocks, timeout),
            #[cfg(feature = "diretta")]
            Self::Dsd(_) => true,
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => true,
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        }
    }
}

#[cfg(test)]
#[derive(Default)]
pub struct FakeDirectState {
    position_micros: std::sync::atomic::AtomicU64,
    playing: std::sync::atomic::AtomicBool,
    failed: std::sync::atomic::AtomicBool,
    finished: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl FakeDirectState {
    pub fn set_position(&self, position: f64) {
        self.position_micros.store(
            (position.max(0.0) * 1_000_000.0) as u64,
            std::sync::atomic::Ordering::Release,
        );
    }

    pub fn playing(&self) -> bool {
        self.playing.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn set_failed(&self, failed: bool) {
        self.failed
            .store(failed, std::sync::atomic::Ordering::Release);
    }

    pub fn set_finished(&self, finished: bool) {
        self.finished
            .store(finished, std::sync::atomic::Ordering::Release);
    }
}

pub struct DirectPlayback {
    duration: f64,
    seek_base: f64,
    seek_transition_count: u64,
    #[cfg(feature = "diretta")]
    selector: String,
    #[cfg(feature = "diretta")]
    source: String,
    transport: DirectTransport,
}

impl DirectPlayback {
    #[cfg(feature = "diretta")]
    pub fn open_local(
        selector: &str,
        source: &str,
        duration: f64,
        position_secs: f64,
        auto_play: bool,
    ) -> Result<Self> {
        if source.starts_with("http://") || source.starts_with("https://") {
            bail!("[Direct] 当前 Direct Lifecycle Gate 仅支持本地 seekable 音源");
        }
        let (path_str, cue_start, cue_dur) =
            if let Some(cue) = crate::cue::parse_cue_virtual_path(source) {
                (
                    cue.physical_path,
                    cue.start_time,
                    if cue.duration > 0.0 {
                        cue.duration
                    } else {
                        duration
                    },
                )
            } else if let Some(sacd) = crate::sacd::parse_sacd_virtual_path(source) {
                (
                    source.to_owned(),
                    0.0,
                    if sacd.duration_secs > 0.0 {
                        sacd.duration_secs
                    } else {
                        duration
                    },
                )

            } else {
                (source.to_owned(), 0.0, duration)
            };
        let path = Path::new(&path_str);
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();

        let is_dsd = matches!(extension.as_str(), "dsf" | "dff" | "dsdiff" | "iso")
            || path_str.contains(".iso|")
            || path_str.contains(".ISO|");

        let (transport, seek_base) =
            if is_dsd {
                let (connection, actual_position) =
                    DirettaDirectDsdConnection::open_local_at(selector, path, cue_start + position_secs)?;
                (DirectTransport::Dsd(connection), (actual_position - cue_start).max(0.0))
            } else {
            let (connection, actual_position) =
                DirettaDirectConnection::open_local_at(selector, path, cue_start + position_secs)?;
            (DirectTransport::Pcm(connection), (actual_position - cue_start).max(0.0))
        };

        let final_duration = match &transport {
            DirectTransport::Pcm(value) => {
                value.set_duration(cue_dur);
                cue_dur
            }
            DirectTransport::Dsd(value) => {
                let dsd_dur = value.monitor().duration();
                if cue_dur > 0.0 {
                    cue_dur
                } else if dsd_dur > 0.0 {
                    dsd_dur
                } else {
                    cue_dur
                }
            }
        };
        let mut playback = Self {
            duration: final_duration,
            seek_base,
            seek_transition_count: 0,
            selector: selector.to_owned(),
            source: source.to_owned(),
            transport,
        };
        if auto_play {
            playback.play()?;
        }
        Ok(playback)
    }

    /// 以流式 Reader 打开（在线音源 stream 模式）：边下边播，不做全量预下载。
    /// 仅支持 PCM 可解码源（FLAC/MP3/AAC/WAV 等）；DSD 流请使用 preload 落盘/memfd 路径。
    #[cfg(feature = "diretta")]
    pub fn open_stream(
        selector: &str,
        source: &str,
        reader: Box<dyn crate::direct_pcm::ReadSeek>,
        duration: f64,
        auto_play: bool,
    ) -> Result<Self> {
        let (transport, seek_base) = {
            let (connection, actual_position) =
                DirettaDirectConnection::open_reader_at(selector, reader, 0.0)?;
            (DirectTransport::Pcm(connection), actual_position.max(0.0))
        };
        let final_duration = match &transport {
            DirectTransport::Pcm(value) => {
                value.set_duration(duration);
                duration
            }
            DirectTransport::Dsd(_) => duration,
        };
        let mut playback = Self {
            duration: final_duration,
            seek_base,
            seek_transition_count: 0,
            selector: selector.to_owned(),
            source: source.to_owned(),
            transport,
        };
        if auto_play {
            playback.play()?;
        }
        Ok(playback)
    }

    #[cfg(feature = "diretta")]
    pub fn handoff_drained_source(
        &mut self,
        source: &str,
        duration: f64,
        cancel: crate::ffmpeg_audio::HttpCancelHandle,
    ) -> Result<DirectFormat> {
        let (path_str, _cue_start, cue_dur) =
            if let Some(cue) = crate::cue::parse_cue_virtual_path(source) {
                (
                    cue.physical_path,
                    cue.start_time,
                    if cue.duration > 0.0 {
                        cue.duration
                    } else {
                        duration
                    },
                )
            } else if let Some(sacd) = crate::sacd::parse_sacd_virtual_path(source) {
                (
                    source.to_owned(),
                    0.0,
                    if sacd.duration_secs > 0.0 {
                        sacd.duration_secs
                    } else {
                        duration
                    },
                )

            } else {
                (source.to_owned(), 0.0, duration)
            };
        let path = Path::new(&path_str);
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let is_dsd = matches!(extension.as_str(), "dsf" | "dff" | "dsdiff" | "iso")
            || path_str.contains(".iso|")
            || path_str.contains(".ISO|");

        let format = match &mut self.transport {
            DirectTransport::Pcm(value) => {
                if is_dsd {
                    bail!("[Direct] PCM → Native DSD 需要重新协商 Diretta connection");
                }
                let format = value.replace_drained_local_source(&path_str, cancel)?;
                value.set_duration(cue_dur);
                DirectFormat::Pcm(format)
            }
            DirectTransport::Dsd(value) => {
                if !is_dsd {
                    bail!("[Direct] Native DSD → PCM 需要重新协商 Diretta connection");
                }
                let format = value.replace_drained_local_source(&path_str)?;
                DirectFormat::Dsd(format)
            }
        };
        self.source = source.to_owned();
        self.duration = cue_dur;
        self.seek_base = 0.0;
        self.seek_transition_count = self.monitor().transition_count();
        Ok(format)
    }
    pub fn seek_while_paused(&mut self, position_secs: f64) -> Result<f64> {
        let actual_position = match &mut self.transport {
            #[cfg(feature = "diretta")]
            DirectTransport::Pcm(value) => value.seek_while_paused(position_secs)?,
            #[cfg(feature = "diretta")]
            DirectTransport::Dsd(value) => value.seek_while_paused(position_secs)?,
            #[cfg(all(test, not(feature = "diretta")))]
            DirectTransport::Fake(value) => {
                value
                    .position_micros
                    .store(0, std::sync::atomic::Ordering::Release);
                position_secs
            }
        };

        self.seek_base = actual_position;
        self.seek_transition_count = self.monitor().transition_count();
        Ok(actual_position)
    }

    pub fn play(&mut self) -> Result<()> {
        match &mut self.transport {
            #[cfg(feature = "diretta")]
            DirectTransport::Pcm(value) => value.play(),
            #[cfg(feature = "diretta")]
            DirectTransport::Dsd(value) => value.play(),
            #[cfg(all(test, not(feature = "diretta")))]
            DirectTransport::Fake(value) => {
                value
                    .playing
                    .store(true, std::sync::atomic::Ordering::Release);
                Ok(())
            }
        }
    }

    pub fn pause(&mut self) -> Result<()> {
        match &mut self.transport {
            #[cfg(feature = "diretta")]
            DirectTransport::Pcm(value) => value.pause(),
            #[cfg(feature = "diretta")]
            DirectTransport::Dsd(value) => value.pause(),
            #[cfg(all(test, not(feature = "diretta")))]
            DirectTransport::Fake(value) => {
                value
                    .playing
                    .store(false, std::sync::atomic::Ordering::Release);
                Ok(())
            }
        }
    }

    /// 换源/关流前的源级淡出（详见 DirectTransport::begin_fade_out）
    pub fn begin_fade_out(&self) {
        self.transport.begin_fade_out();
    }

    /// 淡出是否已生效（后续块均为数字静音）
    pub fn is_faded_out(&self) -> bool {
        self.transport.is_faded_out()
    }

    /// 事件驱动排空等待：淡出完成且已交付 min_blocks 块静音，或超时返回 false
    pub fn wait_fade_drained(&self, min_blocks: u32, timeout: Duration) -> bool {
        self.transport.wait_fade_drained(min_blocks, timeout)
    }

    /// open 后启动验证：等待首块被设备真正消费。
    /// 首块消费前失败/提前结束/被新 load 取代即报错；超时返回 Ok(false)，
    /// 由调用方决定回退方式。事件驱动等待，单次 100ms 上限保证 load 取消的响应性
    pub fn wait_for_direct_start(
        &self,
        load_token: &std::sync::atomic::AtomicU64,
        token: u64,
    ) -> Result<bool> {
        /// 启动验证总超时：Target 时钟锁定通常亚秒级，2s 已覆盖慢启动
        const DIRECT_START_TIMEOUT: Duration = Duration::from_secs(2);
        let deadline = std::time::Instant::now() + DIRECT_START_TIMEOUT;
        loop {
            if load_token.load(std::sync::atomic::Ordering::Acquire) != token {
                return Err(LoadSuperseded.into());
            }
            if self.failed() {
                bail!("[Direct] Diretta 音源在首块消费前失败");
            }
            if self.position() > self.seek_base() {
                return Ok(true);
            }
            if self.finished() {
                bail!("[Direct] Diretta 音源在首块消费前结束");
            }
            if std::time::Instant::now() >= deadline {
                return Ok(false);
            }
            // 事件等待：首块消费 / 失败 / 提前结束任一发生即唤醒
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let _ = self.wait_first_consumed(remaining.min(Duration::from_millis(100)));
        }
    }

    /// open_local + 启动验证：auto_play 时等待首块被消费，失败/超时先优雅暂停
    /// 再报错（连接随本值 drop 关闭）。非播放加载（auto_play=false）跳过验证
    pub fn open_verified_local(
        selector: &str,
        source: &str,
        duration_secs: f64,
        auto_play: bool,
        load_token: &std::sync::atomic::AtomicU64,
        token: u64,
    ) -> Result<Self> {
        let mut playback = Self::open_local(selector, source, duration_secs, 0.0, auto_play)?;
        if !auto_play {
            return Ok(playback);
        }
        match playback.wait_for_direct_start(load_token, token) {
            Ok(true) => Ok(playback),
            Ok(false) => {
                let _ = playback.pause();
                bail!("[Device] Diretta 连接未开始消费音频");
            }
            Err(error) => {
                let _ = playback.pause();
                Err(error)
            }
        }
    }

    /// fake 传输的换源：仅更新时长，返回 fake PCM 格式（供 player 层 handoff 单测使用）
    #[cfg(all(test, not(feature = "diretta")))]
    pub fn handoff_drained_source(
        &mut self,
        source: &str,
        duration: f64,
        cancel: crate::ffmpeg_audio::HttpCancelHandle,
    ) -> Result<DirectFormat> {
        let _ = (source, cancel);
        self.duration = duration;
        self.seek_base = 0.0;
        Ok(DirectFormat::Pcm(DirectPcmFormat {
            sample_rate: 44_100,
            channels: 2,
            valid_bits: 16,
            storage_bits: 16,
            sample_format: crate::direct_pcm::DirectPcmSampleFormat::Signed16,
            memory_path: crate::direct_pcm::DirectPcmMemoryPath::ZeroCopyPacked,
        }))
    }

    pub fn format(&self) -> DirectFormat {
        match &self.transport {
            #[cfg(feature = "diretta")]
            DirectTransport::Pcm(value) => DirectFormat::Pcm(value.format()),
            #[cfg(feature = "diretta")]
            DirectTransport::Dsd(value) => DirectFormat::Dsd(value.format()),
            #[cfg(all(test, not(feature = "diretta")))]
            DirectTransport::Fake(_) => DirectFormat::Pcm(DirectPcmFormat {
                sample_rate: 44_100,
                channels: 2,
                valid_bits: 16,
                storage_bits: 16,
                sample_format: crate::direct_pcm::DirectPcmSampleFormat::Signed16,
                memory_path: crate::direct_pcm::DirectPcmMemoryPath::ZeroCopyPacked,
            }),
        }
    }

    pub fn monitor(&self) -> DirectMonitor {
        match &self.transport {
            #[cfg(feature = "diretta")]
            DirectTransport::Pcm(value) => DirectMonitor::Pcm(value.monitor()),
            #[cfg(feature = "diretta")]
            DirectTransport::Dsd(value) => DirectMonitor::Dsd(value.monitor()),
            #[cfg(all(test, not(feature = "diretta")))]
            DirectTransport::Fake(value) => DirectMonitor::Fake(std::sync::Arc::clone(value)),
        }
    }

    pub fn position(&self) -> f64 {
        let monitor = self.monitor();
        if monitor.transition_count() == self.seek_transition_count {
            self.seek_base + monitor.consumed_position()
        } else {
            monitor.consumed_position()
        }
    }

    pub fn duration(&self) -> f64 {
        let direct_duration = self.monitor().duration();
        if direct_duration > 0.0 {
            direct_duration
        } else {
            self.duration
        }
    }

    pub fn seek_base(&self) -> f64 {
        self.seek_base
    }

    pub fn transition_count(&self) -> u64 {
        self.monitor().transition_count()
    }

    #[cfg(feature = "diretta")]
    pub fn stage_handle(&self) -> DirectStageHandle {
        match &self.transport {
            DirectTransport::Pcm(value) => DirectStageHandle::Pcm(value.stage_handle()),
            DirectTransport::Dsd(value) => DirectStageHandle::Dsd(value.stage_handle()),
        }
    }

    #[cfg(feature = "diretta")]
    pub fn commit_gapless_boundary(&mut self, source: &str, duration: f64) {
        self.source = source.to_owned();
        self.duration = duration;
        self.seek_base = 0.0;
        self.seek_transition_count = self.monitor().transition_count();
    }

    pub fn failed(&self) -> bool {
        self.monitor().failed()
    }

    pub fn finished(&self) -> bool {
        self.monitor().finished()
    }

    /// 事件驱动首块消费等待：用于 Direct 启动校验，取代轮询 sleep
    pub fn wait_first_consumed(&self, timeout: Duration) -> bool {
        self.monitor().wait_first_consumed(timeout)
    }

    #[cfg(test)]
    pub fn fake(duration: f64, auto_play: bool) -> Self {
        let state = std::sync::Arc::new(FakeDirectState::default());
        state
            .playing
            .store(auto_play, std::sync::atomic::Ordering::Release);
        #[cfg(feature = "diretta")]
        {
            let _ = (duration, auto_play, state);
            panic!("fake DirectPlayback is only used in no-SDK unit tests");
        }
        #[cfg(not(feature = "diretta"))]
        Self {
            duration,
            seek_base: 0.0,
            seek_transition_count: 0,
            transport: DirectTransport::Fake(state),
        }
    }

    #[cfg(all(test, not(feature = "diretta")))]
    pub fn fake_state(&self) -> std::sync::Arc<FakeDirectState> {
        match &self.transport {
            DirectTransport::Fake(value) => std::sync::Arc::clone(value),
        }
    }
}
