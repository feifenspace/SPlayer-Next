use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::audio_output::AudioOutput;
use crate::decoder;
#[cfg(any(feature = "diretta", test))]
use crate::direct_runtime::{DirectFormat, DirectMonitor, DirectPlayback};
use crate::equalizer::Equalizer;
use crate::metadata::AudioMetadata;
use crate::playback::PlaybackHandle;
use crate::shared::Shared;
use crate::source::DecoderSource;
use crate::tempo::StretchProcessor;
use anyhow::Result;
use ffmpeg_audio::HttpCancelHandle;
use parking_lot::Mutex;
use tracing::{debug, info};

use super::{InnerPlayer, PlayerEvent, PlayerState};

/// v12-A: 手动 handoff 并行开源开关。默认启用；SPLAYER_DIRECT_PARALLEL_OPEN=
/// 0/false/off 关闭（回退同步开源路径，行为等价改动前）
fn direct_parallel_open_enabled() -> bool {
    match std::env::var("SPLAYER_DIRECT_PARALLEL_OPEN") {
        Ok(value) => !matches!(value.as_str(), "0" | "false" | "off" | "OFF"),
        Err(_) => true,
    }
}

// v12-3 点播命中预载缓存：单槽一次性直通代数。手动 load 命中预载曲时由
// server 层注册（Some），本次 load 的 try_direct_handoff 无条件消费——
// 排空窗口内不再重新开源，直接 ReplaceStaged 预载好的候选（~省 170ms）。
// 未命中 load 注册 None 重置，防上次残留串台；ReplaceStaged 的
// expected_generation 校验仍兜底（槽过期时回退同步开源，行为安全）
static PRESTAGED_HANDOFF_GENERATION: std::sync::Mutex<Option<u64>> =
    std::sync::Mutex::new(None);

/// 注册/重置本次 load 的预载直通代数（load 入口调用，None=重置）
pub fn register_prestaged_handoff(generation: Option<u64>) {
    *PRESTAGED_HANDOFF_GENERATION.lock().unwrap_or_else(|p| p.into_inner()) = generation;
}

fn take_prestaged_handoff() -> Option<u64> {
    PRESTAGED_HANDOFF_GENERATION
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
}

/// 切换/seek 时要 join 的旧线程集合，全部挪到 spawn_blocking 工作线程 join，
/// 主线程持锁阶段只 take handle，避免最坏 200ms+ 的卡顿
pub struct OldThreads {
    pub decoder_thread: Option<JoinHandle<decoder::DecoderData>>,
    pub position_timer: Option<JoinHandle<()>>,
    pub fft_timer: Option<JoinHandle<()>>,
    pub fade_handle: Option<JoinHandle<()>>,
    #[cfg(any(feature = "diretta", test))]
    pub direct_playback: Option<DirectPlayback>,
    /// stop 触发的 Diretta 排空+关流后台线程：join 后才 drop direct_playback /
    /// 打开新连接，保证旧连接彻底关闭（消除 stop+load 组合的双会话重叠）
    #[cfg(any(feature = "diretta", test))]
    pub direct_close: Option<JoinHandle<()>>,
}

impl OldThreads {
    /// 在工作线程上 join 所有旧 timer/fade，返回旧解码线程 handle 供调用方继续使用
    /// 忽略 join 错误：辅助线程 panic 不阻止新加载，主播放路径不依赖它们
    pub fn join_aux(self) -> Option<JoinHandle<decoder::DecoderData>> {
        #[cfg(any(feature = "diretta", test))]
        {
            // Phase2：先等 stop 触发的排空+关流线程收尾（通常已结束，瞬时返回），
            // 再 drop 旧连接——顺序保证同一 Target 上不会新旧会话重叠
            if let Some(h) = self.direct_close {
                let _ = h.join();
            }
            drop(self.direct_playback);
        }
        for h in [self.position_timer, self.fft_timer, self.fade_handle]
            .into_iter()
            .flatten()
        {
            let _ = h.join();
        }
        self.decoder_thread
    }
}

/// async seek 阶段 1 的输出：带到工作线程做 join + ffmpeg seek + 重启解码
#[cfg(any(feature = "diretta", test))]
pub struct DirectSeekTake {
    pub playback: DirectPlayback,
    pub token: u64,
}

pub struct SeekTake {
    /// 所有旧线程 handle（工作线程 join）
    pub old_threads: OldThreads,
    /// 归一化开关（继承到新 Shared）
    pub normalization_enabled: bool,
    /// 归一化增益（继承到新 Shared）
    pub normalization_gain: f32,
    /// 当前音频源（seek 失败时 fallback 到 load）
    pub current_source: Option<String>,
    /// seek 前是否在播放（fallback 到 load 时保留状态）
    pub was_playing: bool,
    /// 当前音频源原始采样率
    pub original_sample_rate: u32,
    /// 当前输出设备采样率（新 Shared 沿用，与复用的重采样器目标一致）
    pub output_sample_rate: u32,
    /// 当前输出设备声道数
    pub output_channels: u16,
    /// 本次 seek 的 token，commit_seeked 时比对最新值，不一致说明已被新 load/seek/stop 取代
    pub token: u64,
    /// 解码侧 DSP 共享实例
    pub equalizer: Arc<Mutex<Equalizer>>,
    pub tempo: Arc<Mutex<StretchProcessor>>,
}

/// 完成音源准备后一次性提交给播放器的资源
pub struct LoadedPlayback {
    pub metadata: AudioMetadata,
    pub decode_handle: JoinHandle<decoder::DecoderData>,
    pub shared: Arc<Shared>,
    pub output: AudioOutput,
    pub cancel: Option<HttpCancelHandle>,
}

impl InnerPlayer {
    /// 给 NAPI 绑定层 async load 用：原子地发出停止信号 + take 所有旧线程 handle
    /// 调用方负责在工作线程 join 这些 handle，主线程持锁阶段不阻塞
    /// 返回旧线程集合与本次 load 的 token（token 用于校验本次 load 是否已被取代）
    pub fn take_for_async_load(&mut self, handle: HttpCancelHandle) -> (OldThreads, u64) {
        // 自增 token：本次 load 的标识；任何并发的更早 commit_loaded 比较时会发现不匹配
        let token = self.load_token.fetch_add(1, Ordering::AcqRel) + 1;
        if let Some(previous) = self.pending_load_handle.replace(handle) {
            previous.cancel();
        }
        let old_threads = self.teardown_for_async_load();
        (old_threads, token)
    }

    /// handoff 预留后的二次回收：direct_active 路径下 reserve_direct_handoff_token
    /// 已把本请求的 handle 注册进 pending_load_handle；probe/格式预检失败回退
    /// 全量重连时会再次到达这里，此时 pending_load_handle 里存的正是本请求
    /// 自己的 handle——绝不能执行 previous.cancel()（自取消会让重连阶段的
    /// HttpAudioSource 打开立即以 "Operation cancelled by user" 失败，整个
    /// load 报错、连接已拆、播放器停在 Stopped，用户必须再点一次播放）。
    /// 其余语义（token 推进 + 全量拆线回收）与 take_for_async_load 完全一致
    pub fn retake_for_async_load_after_handoff_reserve(
        &mut self,
        handle: HttpCancelHandle,
    ) -> (OldThreads, u64) {
        let token = self.load_token.fetch_add(1, Ordering::AcqRel) + 1;
        self.pending_load_handle = Some(handle);
        let old_threads = self.teardown_for_async_load();
        (old_threads, token)
    }

    /// take/retake 共用的全量拆线：停信号、停播放、回收线程句柄
    fn teardown_for_async_load(&mut self) -> OldThreads {
        // 发停止信号（原子写，纳秒级）
        if let Some(flag) = self.fade_cancel.take() {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(flag) = self.position_timer_stop.take() {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(flag) = self.fft_timer_stop.take() {
            flag.store(true, Ordering::Relaxed);
        }

        if let Some(ref shared) = self.shared {
            shared.stop();
        }
        if let Some(playback) = self.playback.take() {
            playback.stop();
        }
        if let Some(ref shared) = self.shared {
            shared.drain_buffer();
        }
        self.shared = None;
        self.cover_raw = None;
        self.seek_base = 0.0;
        self.fft.reset();
        self.equalizer.lock().reset_state();
        self.tempo.lock().reset();

        OldThreads {
            decoder_thread: self.decoder_thread.take(),
            position_timer: self.position_timer_handle.take(),
            fft_timer: self.fft_timer_handle.take(),
            fade_handle: self.fade_handle.take(),
            #[cfg(any(feature = "diretta", test))]
            direct_playback: self.direct_playback.take(),
            #[cfg(any(feature = "diretta", test))]
            direct_close: self.direct_close_thread.take(),
        }
    }

    /// token 是否仍是最新值（seek 失败回退到 load 前校验，避免复活已被取代的旧源）
    pub fn is_load_token_current(&self, token: u64) -> bool {
        token == self.load_token.load(Ordering::Acquire)
    }

    /// 获取加载代次，用于工作线程在创建输出流前快速放弃已过期任务
    pub fn load_token_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.load_token)
    }

    /// 获取解码侧共享均衡器
    pub fn equalizer_handle(&self) -> Arc<Mutex<Equalizer>> {
        Arc::clone(&self.equalizer)
    }

    /// 获取解码侧共享变速处理器
    pub fn tempo_handle(&self) -> Arc<Mutex<StretchProcessor>> {
        Arc::clone(&self.tempo)
    }

    /// 清理仍属于指定 load 的网络中断句柄
    pub fn clear_pending_load(&mut self, token: u64) {
        if self.is_load_token_current(token) {
            self.pending_load_handle = None;
        }
    }

    #[cfg(any(feature = "diretta", test))]
    pub fn take_for_async_direct_seek(&mut self) -> Result<Option<DirectSeekTake>> {
        if self.direct_playback.is_none() {
            return Ok(None);
        }
        if self.state == PlayerState::Playing {
            if let Some(playback) = self.direct_playback.as_mut() {
                playback.pause()?;
            }
        }
        self.stop_position_timer();
        self.stop_fft_timer();
        let token = self.load_token.fetch_add(1, Ordering::AcqRel) + 1;
        let playback = self
            .direct_playback
            .take()
            .ok_or_else(|| anyhow::anyhow!("Direct seek 运行态已丢失"))?;
        self.seek_base = playback.position();
        Ok(Some(DirectSeekTake { playback, token }))
    }

    /// 给 NAPI 绑定层的异步 seek 使用：原子发出停止信号并取出所有旧线程句柄（不 join）
    ///
    /// 返回 None 表示当前没有解码线程（空闲 / 已停止 / 正在异步加载被 load 取走），
    /// 此时不做任何副作用——尤其不能 bump token，否则会误杀在途的 load
    pub fn take_for_async_seek(&mut self) -> Option<SeekTake> {
        self.decoder_thread.as_ref()?;

        // 与 load 共用同一 token 序列：commit_seeked 时比对，防止 seek 期间发生的
        // load/stop 完成后被本次 seek 的 commit 覆盖（旧曲复活 + 新解码线程泄漏）
        let token = self.load_token.fetch_add(1, Ordering::AcqRel) + 1;

        if let Some(flag) = self.fade_cancel.take() {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(flag) = self.position_timer_stop.take() {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(flag) = self.fft_timer_stop.take() {
            flag.store(true, Ordering::Relaxed);
        }

        if let Some(ref shared) = self.shared {
            shared.stop();
        }
        if let Some(playback) = self.playback.take() {
            playback.stop();
        }

        let old_threads = OldThreads {
            decoder_thread: self.decoder_thread.take(),
            position_timer: self.position_timer_handle.take(),
            fft_timer: self.fft_timer_handle.take(),
            fade_handle: self.fade_handle.take(),
            #[cfg(any(feature = "diretta", test))]
            direct_playback: None,
            #[cfg(any(feature = "diretta", test))]
            direct_close: self.direct_close_thread.take(),
        };

        let (norm_enabled, norm_gain) = match self.shared.take() {
            Some(s) => {
                s.drain_buffer();
                (s.is_normalization_enabled(), s.normalization_gain())
            }
            None => (self.normalization_enabled, 0.0),
        };

        self.fft.reset();

        Some(SeekTake {
            old_threads,
            normalization_enabled: norm_enabled,
            normalization_gain: norm_gain,
            current_source: self.current_source.clone(),
            was_playing: self.state == PlayerState::Playing,
            original_sample_rate: self.original_sample_rate,
            output_sample_rate: self.output_sample_rate(),
            output_channels: self.output_channels(),
            token,
            equalizer: Arc::clone(&self.equalizer),
            tempo: Arc::clone(&self.tempo),
        })
    }

    /// seek 三段式的最后一段：主线程持锁，attach 新 sink + 新解码线程
    ///
    /// `output` 为输出重建（`reinit_output`）时新建的输出，seek 本身传 `None` 沿用现有输出。
    /// 返回 false 表示本次 seek 已被更新的 load/seek/stop 取代，结果被丢弃
    pub fn commit_seeked(
        &mut self,
        token: u64,
        position_secs: f64,
        shared: Arc<Shared>,
        handle: JoinHandle<decoder::DecoderData>,
        output: Option<AudioOutput>,
    ) -> Result<bool> {
        // 抢占检查：与 commit_loaded 同款，不一致则丢弃本次 seek 结果
        if token != self.load_token.load(Ordering::Acquire) {
            shared.stop();
            // 解码线程读到 stop 信号后自行退出，故意不 join 避免阻塞主线程持锁阶段
            drop(handle);
            return Ok(false);
        }

        if let Some(out) = output {
            self.output = Some(out);
        }
        let reader = DecoderSource::new(Arc::clone(&shared), Arc::clone(&self.fft));
        let was_paused = self.state == PlayerState::Paused;
        let volume = self.target_volume;
        let playback = {
            let output = self.ensure_output(None)?;
            Arc::new(PlaybackHandle::attach(output, reader, volume, was_paused)?)
        };

        self.playback = Some(playback);
        self.shared = Some(shared);
        self.decoder_thread = Some(handle);
        self.seek_base = position_secs;

        if was_paused {
            self.state = PlayerState::Paused;
            self.emit(PlayerEvent::StateChanged {
                state: PlayerState::Paused,
            });
        } else {
            self.state = PlayerState::Playing;
            self.emit(PlayerEvent::StateChanged {
                state: PlayerState::Playing,
            });
            self.start_position_timer();
            self.start_fft_timer();
        }
        // seek 确认事件：客户端不必等下一个 Position 事件兜底
        self.emit(PlayerEvent::Seeked {
            position: position_secs,
        });

        Ok(true)
    }

    /// load 的下半部分：NAPI 绑定层完成异步 IO 后由主线程持锁调用
    ///
    /// `token` 为 take_for_async_load 时拿到的标识。本函数比对当前最新 token：
    /// - 不一致 → 本次 load 已被更新的 load 抢占，丢弃 sink/shared，stop 解码线程后返回 None
    /// - 一致 → 正常 attach 新资源
    pub fn commit_loaded(
        &mut self,
        token: u64,
        source: &str,
        auto_play: bool,
        loaded: LoadedPlayback,
    ) -> Result<Option<AudioMetadata>> {
        let LoadedPlayback {
            mut metadata,
            decode_handle,
            shared,
            output,
            cancel,
        } = loaded;
        // 抢占检查：比对最新 token，不等说明已有更新的 load 在路上 / 已 commit
        if token != self.load_token.load(Ordering::Acquire) {
            if let Some(h) = cancel {
                h.cancel();
            }
            // 停止新解码线程（它会写入 shared 但没人消费），让 join 能尽快返回
            shared.stop();
            // shared / sink / decode_handle 在此函数返回时 drop；解码线程读到 stop 信号后退出
            // decode_handle 故意不 join，避免阻塞主线程持锁阶段（让解码线程在后台自然结束）
            drop(decode_handle);
            return Ok(None);
        }

        self.pending_load_handle = cancel;
        self.output = Some(output);

        let reader = DecoderSource::new(Arc::clone(&shared), Arc::clone(&self.fft));
        let volume = self.target_volume;
        let playback = {
            let output = self.ensure_output(None)?;
            Arc::new(PlaybackHandle::attach(output, reader, volume, !auto_play)?)
        };

        self.playback = Some(playback);
        self.shared = Some(shared);
        self.decoder_thread = Some(decode_handle);
        self.seek_base = 0.0;
        self.current_source = Some(source.to_string());

        self.audio_duration = metadata.duration_secs;
        self.original_sample_rate = metadata.original_sample_rate;
        self.cover_raw = metadata.cover_raw.take();

        if auto_play {
            self.state = PlayerState::Playing;
            self.emit(PlayerEvent::StateChanged {
                state: PlayerState::Playing,
            });
            self.start_position_timer();
            self.start_fft_timer();
        } else {
            self.state = PlayerState::Paused;
            self.emit(PlayerEvent::StateChanged {
                state: PlayerState::Paused,
            });
        }

        Ok(Some(metadata))
    }

    #[cfg(any(feature = "diretta", test))]
    pub fn commit_direct_seeked(
        &mut self,
        token: u64,
        mut playback: DirectPlayback,
    ) -> Result<bool> {
        if token != self.load_token.load(Ordering::Acquire) {
            drop(playback);
            return Ok(false);
        }
        self.output = None;
        self.playback = None;
        self.shared = None;
        self.decoder_thread = None;
        self.seek_base = playback.seek_base();
        let should_play = self.state == PlayerState::Playing;
        if should_play {
            playback.play()?;
        }
        self.direct_playback = Some(playback);
        if should_play {
            self.start_position_timer();
        }
        // seek 确认事件：position 取 Direct 回放的实际 seek 落点
        self.emit(PlayerEvent::Seeked {
            position: self.seek_base,
        });
        Ok(true)
    }

    #[cfg(any(feature = "diretta", test))]
    pub fn commit_direct_loaded(
        &mut self,
        token: u64,
        source: &str,
        auto_play: bool,
        mut metadata: AudioMetadata,
        playback: DirectPlayback,
    ) -> Result<Option<AudioMetadata>> {
        if token != self.load_token.load(Ordering::Acquire) {
            drop(playback);
            return Ok(None);
        }

        self.pending_load_handle = None;
        self.output = None;
        self.playback = None;
        self.shared = None;
        self.decoder_thread = None;
        self.direct_playback = Some(playback);
        self.seek_base = 0.0;
        self.current_source = Some(source.to_owned());
        self.audio_duration = metadata.duration_secs;
        self.cover_raw = metadata.cover_raw.take();
        self.fft.reset();

        if auto_play {
            self.state = PlayerState::Playing;
            self.emit(PlayerEvent::StateChanged {
                state: PlayerState::Playing,
            });
            self.start_position_timer();
        } else {
            self.state = PlayerState::Paused;
            self.emit(PlayerEvent::StateChanged {
                state: PlayerState::Paused,
            });
        }

        Ok(Some(metadata))
    }

    /// Direct handoff 前置登记：只推进 load token 并取消在途加载，
    /// 不触碰 Direct 连接 / 播放线程 —— 旧曲目在 probe / 淡出期间继续出声。
    /// probe 或格式预检失败后，调用方再用 take_for_async_load 做全量回收。
    #[cfg(any(feature = "diretta", test))]
    pub fn reserve_direct_handoff_token(&mut self, handle: HttpCancelHandle) -> u64 {
        let token = self.load_token.fetch_add(1, Ordering::AcqRel) + 1;
        if let Some(previous) = self.pending_load_handle.replace(handle) {
            previous.cancel();
        }
        token
    }

    /// 当前 Direct 连接的 wire 格式（无连接时 None）
    #[cfg(any(feature = "diretta", test))]
    pub fn direct_format(&self) -> Option<DirectFormat> {
        self.direct_playback.as_ref().map(DirectPlayback::format)
    }

    /// 播放中启动 Direct 源级淡出/排空（PCM=淡出、DSD=静音垫请求）。
    ///
    /// 生效条件是"连接活跃且设备正在/即将消费"：
    /// - 播放中：常规手动切歌排空；
    /// - 已 finished（曲末自然结束后的接力）：设备仍在拉静音块，静音垫
    ///   仍需置换设备端缓冲——此分支不依赖 state：曲末自然结束后引擎
    ///   state 残留 Playing 而快照是 Stopped，接力排空曾隐式依赖该残留值，
    ///   finished() 兜底后即使未来修正曲末状态此处也不退化；
    /// - 暂停态：SDK 未在消费、Target 缓冲已自然播空，排空是纯等待 → 跳过
    #[cfg(any(feature = "diretta", test))]
    pub fn begin_direct_fade_out(&mut self) -> Result<()> {
        let playback = match self.direct_playback.as_ref() {
            Some(playback) => playback,
            None => return Ok(()),
        };
        if self.state != PlayerState::Playing && !playback.finished() {
            return Ok(());
        }
        playback.begin_fade_out();
        Ok(())
    }

    /// 淡出是否已完全生效（无连接 / 非播放态视为已静音）
    #[cfg(any(feature = "diretta", test))]
    pub fn direct_faded_out(&self) -> bool {
        match &self.direct_playback {
            None => true,
            Some(playback) => self.state != PlayerState::Playing || playback.is_faded_out(),
        }
    }

    /// 取 Direct 排空等待句柄：None = 无需排空。生效条件与
    /// begin_direct_fade_out 一致（播放中，或曲末接力时的 finished 兜底——
    /// 不依赖曲末 state 残留值；暂停态 SDK 未在消费、缓冲已播空，跳过）。
    /// 句柄持有 ring 的 Arc 引用——排空最长可等 600ms，
    /// 调用方必须在释放 player 锁之后用它等待
    #[cfg(any(feature = "diretta", test))]
    pub fn direct_drain_handle(&self) -> Option<DirectMonitor> {
        let playback = self.direct_playback.as_ref()?;
        if self.state != PlayerState::Playing && !playback.finished() {
            return None;
        }
        Some(playback.monitor())
    }

    /// v12-4 tinyLMS Hard Reset 预静音触发：SDK 回调层连续交付静音周期
    /// （PCM 8 周期 / DSD 0x69 垫）。仅触发不等待，调用方在锁外轮询
    /// direct_pre_mute_pending 等消耗完成。无连接返回 false
    #[cfg(any(feature = "diretta", test))]
    pub fn begin_direct_pre_mute(&self) -> bool {
        match self.direct_playback.as_ref() {
            Some(playback) => playback.trigger_tinylms_pre_mute(),
            None => false,
        }
    }

    /// 预静音倒计时是否尚未消耗完（无连接视为已完成）
    #[cfg(any(feature = "diretta", test))]
    pub fn direct_pre_mute_pending(&self) -> bool {
        match self.direct_playback.as_ref() {
            Some(playback) => playback.tinylms_pre_mute_pending(),
            None => false,
        }
    }

    /// v12-A: 手动 handoff 并行开源武装——格式预检通过后、淡出排空前调用，
    /// 用排空窗口并行完成候选源的后台预打开（消除原先串行在排空之后的
    /// 开源耗时）。返回 generation 凭据供 commit_direct_handoff 走
    /// ReplaceStaged 装填；None = 不适用（非本地路径/DSD/无连接，走同步开源）
    #[cfg(any(feature = "diretta", test))]
    pub fn arm_direct_handoff_stage(
        &self,
        source: &str,
        open_path: Option<&str>,
        duration_secs: f64,
    ) -> Option<u64> {
        let playback = self.direct_playback.as_ref()?;
        // 打开参数解析与 DirectPlayback::handoff_drained_source 保持一致：
        // cue/sacd 虚拟路径 → 物理路径 + 有界播放区间。同一换源的 arm 与
        // commit 必须用完全相同的 open 参数，generation 凭据才有意义
        let (path_str, cue_start, cue_dur, cue_stop) =
            if let Some(cue) = crate::cue::parse_cue_virtual_path(source) {
                let dur = if cue.duration > 0.0 {
                    cue.duration
                } else {
                    duration_secs
                };
                (
                    cue.physical_path,
                    cue.start_time,
                    dur,
                    cue.start_time + dur,
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
                    0.0,
                )
            } else {
                (source.to_owned(), 0.0, duration_secs, 0.0)
            };
        let open_str = open_path.unwrap_or(&path_str);
        playback.arm_handoff_stage(open_str, cue_start, cue_stop, cue_dur)
    }

    /// Direct 载入编排（headless 与 NAPI 两侧调用方共用）：
    /// 淡出旧源 → 锁外排空 → 块边界原子换源。
    ///
    /// `open_path`：打开用物理路径（在线源 preload 物化产物 memfd/磁盘缓存），
    /// None = 按 source 打开（本地源原行为）。
    ///
    /// 返回：
    /// - `Ok(Some(format))`：已切到新源，连接复用成功
    /// - `Ok(None)`：token 被更新的 load/stop 抢占（LoadSuperseded）
    /// - `Err`：换源失败（典型 wire 格式不一致），连接保持原状（可能已淡出静音），
    ///   调用方应回退 take_for_async_load 全量重连
    ///
    /// 淡出与提交分段持锁；最长 600ms 的排空等待在 player 锁外进行
    #[cfg(any(feature = "diretta", test))]
    #[allow(clippy::too_many_arguments)]
    pub fn try_direct_handoff(
        player: &Mutex<InnerPlayer>,
        token: u64,
        source: &str,
        open_path: Option<&str>,
        duration_secs: f64,
        auto_play: bool,
        current_format: DirectFormat,
        metadata: &AudioMetadata,
        is_dsd: bool,
    ) -> Result<Option<DirectFormat>> {
        // 格式预检：家族（PCM/DSD）+ 采样率 + 声道一致才值得做淡出换源。
        // stream 模式元数据为占位 0 值：跳过对应粗检，由换源时的权威校验兜底
        let family_matches = match current_format {
            DirectFormat::Pcm(cur) => {
                !is_dsd
                    && (metadata.original_sample_rate == 0
                        || cur.sample_rate == metadata.original_sample_rate)
                    && (metadata.channels == 0 || cur.channels == metadata.channels)
            }
            DirectFormat::Dsd(cur) => {
                is_dsd && (metadata.channels == 0 || cur.channels == metadata.channels)
            }
        };
        anyhow::ensure!(
            family_matches,
            "[Direct] handoff 格式预检不通过，回退全量重连"
        );

        // 0) v12-A 并行开源：淡出排空窗口（~140ms+）内后台预打开候选源，
        //    消除原先串行在排空之后的开源耗时。默认启用；
        //    SPLAYER_DIRECT_PARALLEL_OPEN=0/false/off 关闭（回退同步开源）。
        //    凭据为 None 时 commit 走原 ReplaceLocal 路径，行为等价改动前
        let staged_generation = if let Some(g) = take_prestaged_handoff() {
            info!(
                target: "diretta_handoff",
                phase = "handoff_prestaged_hit",
                generation = %g,
                "点播命中预载缓存，跳过并行开源直接接力"
            );
            Some(g)
        } else if direct_parallel_open_enabled() {
            player.lock().arm_direct_handoff_stage(source, open_path, duration_secs)
        } else {
            None
        };

        // 1) tinyLMS Quick Resume（默认）：跳过淡出与排空垫——同格式手动切歌
        //    数据级硬拼接（对齐 tinyLMS SwapToNext / splayer 自动无缝切歌，
        //    后者实测无杂音）；旧曲缓冲尾巴自然播完后接新曲，无静音间隙。
        //    legacy 模式：源级淡出（暂停态为无害 no-op）；短锁取排空句柄
        if !crate::direct_runtime::tiny_lms_switch_enabled() {
            let handle = {
                let mut player = player.lock();
                let _ = player.begin_direct_fade_out();
                player.direct_drain_handle()
            };
            // 2) 锁外事件驱动排空：渐零完成 + 交付足量数字静音块（顶掉设备端
            //    缓冲里的旧音频尾巴）；暂停态/无连接时句柄为 None 瞬时通过
            if let Some(monitor) = &handle {
                // 超时与动态排空目标联动：drain_target + EXTRA，ring 未注入时
                // drain_target 退回旧常量 200ms（行为等价改动前）
                if !monitor.wait_fade_drained(
                    crate::direct_runtime::DIRECT_FADE_DRAIN_MIN_BLOCKS,
                    std::time::Duration::from_micros(monitor.drain_target_micros())
                        + crate::direct_runtime::DIRECT_FADE_DRAIN_EXTRA,
                ) {
                    debug!(
                        target: "diretta_handoff",
                        phase = "handoff_fade_drain_timeout",
                        "淡出排空等待超时，仍尝试 commit（块边界校验兜底）"
                    );
                }
            }
        }
        // 3) 块边界原子换源（格式不一致时 Err，旧连接保持静音原状）
        let mut player = player.lock();
        player.commit_direct_handoff(
            token,
            source,
            open_path,
            duration_secs,
            auto_play,
            staged_generation,
        )
    }

    /// 同格式 Direct handoff 提交：保留 Diretta 连接，生产者线程在块边界原子换源。
    ///
    /// 返回：
    /// - `Ok(Some(format))`：已切到新源，old 连接复用成功
    /// - `Ok(None)`：token 已被更新的 load/stop 抢占，放弃本次 handoff
    /// - `Err`：换源失败（典型为 wire 格式不一致），连接保持原状（可能已淡出静音），
    ///   调用方应回退 take_for_async_load 全量重连
    #[cfg(any(feature = "diretta", test))]
    pub fn commit_direct_handoff(
        &mut self,
        token: u64,
        source: &str,
        open_path: Option<&str>,
        duration: f64,
        auto_play: bool,
        staged_generation: Option<u64>,
    ) -> Result<Option<DirectFormat>> {
        if token != self.load_token.load(Ordering::Acquire) {
            return Ok(None);
        }
        let playback = self
            .direct_playback
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("[Direct] 无活跃 Direct 连接可复用"))?;
        // handoff 前置必然是 take_threads_only(handle)：pending_load_handle 即本次
        // load 的取消句柄——被更新的 load supersede 时 cancel 它，即可即时掐断
        // producer 正在打开的 HTTP 连接（否则 join 最坏等满 256s 网络退避）
        let cancel = self
            .pending_load_handle
            .clone()
            .ok_or_else(|| anyhow::anyhow!("[Direct] handoff 前缺少 load 取消句柄"))?;
        let format = playback.handoff_drained_source(
            source,
            open_path,
            duration,
            cancel,
            staged_generation,
        )?;
        // tinyLMS 同格式 Quick Resume 保持数据级连续拼接；若在这里淡入，
        // 会凭空插入零电平阶跃并重新引入手动切歌咔哒。legacy 排空路径才淡入。
        if crate::direct_runtime::tiny_lms_switch_enabled() {
            playback.resume_handoff_no_fade();
        } else {
            playback.resume_soft();
        }
        debug!(
            target: "diretta_handoff",
            phase = "handoff_resume",
            source = %source,
            "handoff 新源起播已提交"
        );
        self.current_source = Some(source.to_owned());
        self.audio_duration = if duration > 0.0 {
            duration
        } else {
            playback.duration()
        };
        // staged gapless boundary 同款处理：旧封面不再属于当前 source
        self.cover_raw = None;
        self.fft.reset();
        if auto_play {
            // 曲终后引擎侧 state 仍停留在 Playing（Ended/StateChanged 只更新服务端
            // 快照，不回写 Transition），但位置定时器线程已随 Ended break 死亡。
            // 旧守卫 `state != Playing` 会跳过 play/定时器重启，导致：
            // ① 服务端快照冻结在曲终值（position=上一曲 EOF、state=Stopped、
            //    is_finished=true），UI 位置不动；
            // ② 看门狗 still_ended 判定恒为真 → 曲终接力按队列快照逐次级联跳曲
            //    （表现为手动切歌后曲目自己接连跳到下一曲/下下曲）。
            // 因此 play、状态翻转与定时器重启必须无条件执行（play 对活跃
            // session 幂等；start_position_timer 内部先 stop 旧的，幂等）。
            let resumed_position = playback.position();
            let resumed_finished = playback.finished();
            playback.play()?;
            self.state = PlayerState::Playing;
            self.emit(PlayerEvent::StateChanged {
                state: PlayerState::Playing,
            });
            self.start_position_timer();
            debug!(
                target: "diretta_handoff",
                phase = "handoff_commit_state",
                position_secs = %resumed_position,
                finished = %resumed_finished,
                "handoff 提交后播放与位置监视已恢复"
            );
        }
        Ok(Some(format))
    }

    /// v11-3 热重配提交：不拆 Diretta 连接，跨采样率手动切歌在既有 Sync
    /// 会话上完成。顺序（消除新旧速率数据/wire 失配窗口）：
    /// ① FFI 热重配 wire 到目标格式（stop → setSinkConfigure →
    ///    configTransferAuto → preroll 静音武装 → play，DAC 重锁相由
    ///    preroll 覆盖）；此刻 SDK 从旧（已排空）环拉静音，无失配风险
    /// ② armed 旁路 + producer 块边界原子换新格式源
    /// ③ 解码格式与目标格式校验（容器/CUE 等场景 metadata 可能失真）
    /// ④ 新源淡入（与 handoff 同语义：raised-cosine 消除零电平阶跃）
    ///
    /// 失败语义：①失败连接保持原格式（完美回退）；②③失败可能已换源/
    /// 重配——调用方回退全量重连本就会按新格式重开，无需回滚
    #[cfg(any(feature = "diretta", test))]
    pub fn commit_direct_hot_reconfigure(
        &mut self,
        token: u64,
        source: &str,
        open_path: Option<&str>,
        duration: f64,
        auto_play: bool,
        wire_format_hint: &crate::direct_pcm::DirectPcmFormat,
    ) -> Result<Option<DirectFormat>> {
        if token != self.load_token.load(Ordering::Acquire) {
            return Ok(None);
        }
        let playback = self
            .direct_playback
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("[Direct] 无活跃 Direct 连接可热重配"))?;
        let cancel = self
            .pending_load_handle
            .clone()
            .ok_or_else(|| anyhow::anyhow!("[Direct] 热重配前缺少 load 取消句柄"))?;
        // ① 先重配 wire（bridge 内部 stop → set → cycle → preroll → play）
        if let Err(error) = playback.hot_reconfigure(wire_format_hint) {
            tracing::error!(
                target: "diretta_handoff",
                phase = "hot_reconfigure_failed",
                error = %error,
                "热重配失败，连接保持原格式，回退全量重连"
            );
            return Err(error);
        }
        // ② armed 旁路换源（SDK 此刻从旧环拉排空静音，块边界原子换新源）
        // v12-A: 热重配路径不走 staged 并行开源——跨格式候选会被 stage 预检
        // 拒绝，且 arm 的 CancelStaged 可能与 armed 旗标时序交叉，保守传 None
        playback.arm_cross_format_replace();
        let format =
            playback.handoff_drained_source(source, open_path, duration, cancel, None)?;
        let DirectFormat::Pcm(pcm_format) = &format else {
            anyhow::bail!("[Direct] 热重配不支持 DSD 家族");
        };
        // ③ 解码格式校验：与 wire 重配目标不一致时立即停发（防止新速率数据
        // 流入错误速率 wire），回退全量重连
        if pcm_format.sample_rate != wire_format_hint.sample_rate
            || pcm_format.channels != wire_format_hint.channels
        {
            let _ = playback.pause();
            anyhow::bail!(
                "[Direct] 实际解码格式（{}Hz/{}ch）与热重配目标（{}Hz/{}ch）不一致，回退全量重连",
                pcm_format.sample_rate,
                pcm_format.channels,
                wire_format_hint.sample_rate,
                wire_format_hint.channels
            );
        }
        // ④ 新源淡入 + 状态簿记（与 commit_direct_handoff 完全同构）
        playback.resume_soft();
        tracing::info!(
            target: "diretta_handoff",
            phase = "hot_reconfigure_commit",
            source = %source,
            sample_rate = %pcm_format.sample_rate,
            "热重配提交成功，Diretta 连接未拆除"
        );
        self.current_source = Some(source.to_owned());
        self.audio_duration = if duration > 0.0 {
            duration
        } else {
            playback.duration()
        };
        self.cover_raw = None;
        self.fft.reset();
        if auto_play {
            let resumed_position = playback.position();
            playback.play()?;
            self.state = PlayerState::Playing;
            self.emit(PlayerEvent::StateChanged {
                state: PlayerState::Playing,
            });
            self.start_position_timer();
            tracing::debug!(
                target: "diretta_handoff",
                phase = "hot_reconfigure_state",
                position_secs = %resumed_position,
                "热重配后播放与位置监视已恢复"
            );
        }
        Ok(Some(format))
    }

    /// v11-3 热重配编排入口：仅 SPLAYER_DIRECT_HOT_RECONFIG=1 且 PCM→PCM
    /// （同声道、采样率可不同）时尝试；其余一律 Err 回退常规路径。
    /// 由 player.rs 在 handoff 预检失败后、全量重连前调用
    #[cfg(any(feature = "diretta", test))]
    #[allow(clippy::too_many_arguments)]
    pub fn try_direct_hot_reconfigure(
        player: &Mutex<InnerPlayer>,
        token: u64,
        source: &str,
        open_path: Option<&str>,
        duration_secs: f64,
        auto_play: bool,
        current_format: DirectFormat,
        metadata: &AudioMetadata,
        is_dsd: bool,
    ) -> Result<Option<DirectFormat>> {
        // 实验开关默认关闭：行为与 v10 完全一致
        let hot_enabled = std::env::var("SPLAYER_DIRECT_HOT_RECONFIG")
            .map(|value| value != "0")
            .unwrap_or(false);
        anyhow::ensure!(
            hot_enabled,
            "[Direct] 热重配实验开关未开启（SPLAYER_DIRECT_HOT_RECONFIG）"
        );
        // 家族限制：PCM → PCM 且声道一致（位深已由 32-bit 容器归一化统一）
        let DirectFormat::Pcm(cur) = current_format else {
            anyhow::bail!("[Direct] 热重配仅支持 PCM 家族");
        };
        anyhow::ensure!(
            !is_dsd,
            "[Direct] 热重配不支持 PCM → Native DSD"
        );
        anyhow::ensure!(
            metadata.channels == 0 || cur.channels == metadata.channels,
            "[Direct] 热重配声道数不一致（{} → {}）",
            cur.channels,
            metadata.channels
        );
        // v12-2: 收紧热重配——对齐 tinyLMS SetFormat 的实测结论（其源码注释）：
        // Diretta SDK 缺少向 Target 外发 SinkConfigure 的支持，在线 setSinkConfigure
        // 无法让 Target 侧 DAC 时钟跟随，跨采样率重配必然错乱失真（真机 59 实测
        // 44.1/192 → 176.4k 走速 0.42x、听感失真，残留状态还污染下一曲排空）。
        // tinyLMS 因此只允许"采样率+声道完全相同"的 Quick Resume，跨采样率一律
        // Hard Reset。这里同样收紧：采样率不同即回退全量重连（v12 手动切歌已有
        // 并行开源 + 动态排空垫，全量重连间隔 ~272ms，可接受）。
        anyhow::ensure!(
            metadata.original_sample_rate == cur.sample_rate,
            "[Direct] 热重配仅限同采样率（SDK 无法让 Target 时钟跟随，跨采样率会失真）：{} → {}，回退全量重连",
            cur.sample_rate,
            metadata.original_sample_rate
        );
        tracing::info!(
            target: "diretta_handoff",
            phase = "hot_reconfigure_attempt",
            old_rate = %cur.sample_rate,
            new_rate = %metadata.original_sample_rate,
            "尝试热重配（不拆连接跨采样率切换）"
        );
        // wire 重配目标：采样率必须已知（stream 模式 metadata 占位 0 时不支持，
        // 回退全量重连）；位深恒为 32 容器（v11-1 归一化）
        anyhow::ensure!(
            metadata.original_sample_rate > 0,
            "[Direct] 热重配需要已知目标采样率（stream 占位 0 不支持）"
        );
        let wire_format_hint = crate::direct_pcm::DirectPcmFormat {
            sample_rate: metadata.original_sample_rate,
            channels: if metadata.channels > 0 {
                metadata.channels
            } else {
                cur.channels
            },
            valid_bits: 32,
            storage_bits: 32,
            sample_format: crate::direct_pcm::DirectPcmSampleFormat::Signed32,
            memory_path: crate::direct_pcm::DirectPcmMemoryPath::ZeroCopyPacked,
        };

        // 1) 源级淡出 + 排空（与 try_direct_handoff 步骤 1-2 同构）
        let drain = {
            let mut player = player.lock();
            let _ = player.begin_direct_fade_out();
            player.direct_drain_handle()
        };
        if let Some(monitor) = &drain {
            if !monitor.wait_fade_drained(
                crate::direct_runtime::DIRECT_FADE_DRAIN_MIN_BLOCKS,
                std::time::Duration::from_micros(monitor.drain_target_micros())
                    + crate::direct_runtime::DIRECT_FADE_DRAIN_EXTRA,
            ) {
                tracing::warn!(
                    target: "diretta_handoff",
                    phase = "hot_reconfigure_fade_drain_timeout",
                    "热重配前排空等待超时，仍尝试提交"
                );
            }
        }
        // 2) 提交：FFI 重配 → armed 换源 → 校验 → 淡入
        let mut player = player.lock();
        player.commit_direct_hot_reconfigure(
            token,
            source,
            open_path,
            duration_secs,
            auto_play,
            &wire_format_hint,
        )
    }
}
