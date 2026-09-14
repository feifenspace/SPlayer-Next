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

/// v12-4 tinyLMS 切歌模式开关（默认启用）：手动切歌对齐 tinyLMS 久经验证的
/// 处理方式——同格式 Quick Resume（无 fade/无排空垫，块边界数据级硬拼接，
/// 与自动无缝切歌同路径）；跨格式 Hard Reset（SDK 回调层预静音 8 周期 PCM /
/// 0x69 垫 DSD 后再拆连接重连）。SPLAYER_DIRECT_TINYLMS_SWITCH=0/false/off
/// 回退 v12-3 行为（fade-out + 动态排空垫 + fade-in）
pub fn tiny_lms_switch_enabled() -> bool {
    match std::env::var("SPLAYER_DIRECT_TINYLMS_SWITCH") {
        Ok(value) => !matches!(value.as_str(), "0" | "false" | "off" | "OFF"),
        Err(_) => true,
    }
}

/// Direct 排空的静音垫时长时间下限（微秒）：块尺寸随 codec/采样率波动
/// （MP3 ~26ms / FLAC@192k ~21ms/块，固定 4 块的垫时长不可控），
/// 达到该时长即视为设备端缓冲已置换完成，块数谓词保留为兜底
pub const DIRECT_FADE_DRAIN_MIN_MICROS: u64 = 200_000;

/// v12-B 动态排空目标的时长下限（微秒）：Sink 实测 latency ~110ms 时
/// 200ms 旧下限会把垫顶到 200ms（base×1.5=165ms 被下限覆盖），徒增可闻静音。
/// 垫只需 ≥ Target 缓冲深度（latency）即可置换旧音频，故下限收紧到 120ms；
/// SPLAYER_DIRECT_DRAIN_FLOOR_MS 可覆盖（设 200 恢复旧行为）
pub const DIRECT_FADE_DRAIN_FLOOR_MICROS: u64 = 120_000;

/// v12-A 手动 handoff 并行开源：arm 后 ReplaceStaged 等待 stage 预打开
/// 就绪的上限。stage 与淡出排空并行（正常在排空完成前就绪）；超时/失败/
/// generation 不符则回退 ReplaceLocal 同步开源（行为与改动前一致，无回归）
pub const DIRECT_HANDOFF_STAGE_WAIT: Duration = Duration::from_millis(800);

/// Direct 排空的事件等待上限（超时兜底，正常远快于此值）。
/// ring 未注入动态排空目标时使用；已注入时调用方按 drain_target + EXTRA 计算
pub const DIRECT_FADE_DRAIN_TIMEOUT: Duration = Duration::from_millis(600);

/// 动态排空超时的额外余量：timeout = drain_target_micros + EXTRA。
/// 覆盖轮询周期（10ms）与调度抖动，防止排空谓词达成前被超时截断
pub const DIRECT_FADE_DRAIN_EXTRA: Duration = Duration::from_millis(400);

/// Phase3 软暂停开关：暂停前先淡出（PCM）/ 0x69 置零（DSD）到零电平再停发，
/// 消除暂停/恢复的块边界阶跃。默认启用；SPLAYER_DIRECT_SOFT_PAUSE=0/false/off 关闭
pub fn direct_soft_pause_enabled() -> bool {
    match std::env::var("SPLAYER_DIRECT_SOFT_PAUSE") {
        Ok(value) => !matches!(value.as_str(), "0" | "false" | "off" | "OFF"),
        Err(_) => true,
    }
}

/// Diretta full reconnect 后的 Target/DAC 格式稳定窗口。
/// 仅替换现存 DirectPlayback（全量重连）时使用；同格式 staged/handoff 不经过此路径
pub const DIRECT_FULL_RECONNECT_STABILIZATION: Duration = Duration::from_millis(150);
/// DSD stream setup needs a longer DAC/PLL stabilization window after a hard reconnect.
pub const DIRECT_DSD_FULL_RECONNECT_STABILIZATION: Duration = Duration::from_millis(800);

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
                value
                    .position_micros
                    .load(std::sync::atomic::Ordering::Acquire) as f64
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

    /// 高码率判定：PCM >96kHz，或任意 DSD 位流（块周期长）——停滞阈值放宽依据。
    /// Fake 恒为 false
    pub fn is_high_rate(&self) -> bool {
        match self {
            Self::Pcm(value) => value.sample_rate() > 96_000,
            Self::Dsd(_) => true,
            #[cfg(test)]
            Self::Fake(_) => false,
        }
    }

    /// 事件驱动排空等待：淡出/静音垫完成且已交付足量静音（时长时间下限为
    /// 主谓词），或超时。DSD 排空 = 回调侧静音垫置换设备端缓冲；
    /// Fake 无音频流恒 true。
    /// 句柄持有 ring 的 Arc 引用，供调用方在 player 锁外排空
    pub fn wait_fade_drained(&self, min_blocks: u32, timeout: Duration) -> bool {
        match self {
            Self::Pcm(value) => value.wait_fade_drained(min_blocks, timeout),
            Self::Dsd(value) => value.wait_fade_drained(min_blocks, timeout),
            #[cfg(test)]
            Self::Fake(_) => true,
        }
    }

    /// 排空目标时长（µs）：ring 建连后注入的动态值；未注入时为旧常量
    /// 200_000。调用方以此联动计算 wait_fade_drained 超时。Fake 恒为旧常量
    pub fn drain_target_micros(&self) -> u64 {
        match self {
            Self::Pcm(value) => value.drain_target_micros(),
            Self::Dsd(value) => value.drain_target_micros(),
            #[cfg(test)]
            Self::Fake(_) => DIRECT_FADE_DRAIN_MIN_MICROS,
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

    /// staged 候选已接受但尚未装填进 ring：曲终判定（Ended）必须避开此窗口，
    /// 否则 stage 迟到时会触发假 Ended（监视线程死亡 + 看门狗抢跑重放）。
    /// Fake 监视器（仅测试）恒 false
    pub fn staging(&self) -> bool {
        match self {
            Self::Pcm(value) => value.staging(),
            Self::Dsd(value) => value.staging(),
            #[cfg(test)]
            Self::Fake(_) => false,
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
    /// `open_path` 为打开用物理路径（在线源 memfd/磁盘缓存物化产物、本地源
    /// preload memfd），与逻辑 `source` 分离：cue/sacd 解析与 DSD 家族嗅探仍
    /// 基于 source（物化路径无原始扩展名与虚拟轨信息）；`None` = 按 source
    /// 解析出的物理路径打开（原行为）
    pub fn stage_local(
        &self,
        source: &str,
        open_path: Option<&str>,
        duration_secs: f64,
        generation: u64,
    ) -> Result<()> {
        let is_http = source.starts_with("http://") || source.starts_with("https://");
        if is_http && open_path.is_none() {
            bail!("[Direct] 在线 gapless staging 需要已物化的本地 seekable 输入");
        }
        // 在线源由上层完整物化到 memfd，并通过 open_path 传入；逻辑 source 仍
        // 用于保留格式嗅探和元数据，不能据此误判为不可 seek。
        // stop_secs：CUE 分轨有界播放的虚拟 EOF（文件时间轴 start+轨长）；
        // 非 CUE 源为 0（自然 EOF 收尾），SACD/DSD 由解码器按轨界自然结束
        let (path_str, start, cue_dur, stop_secs) =
            if let Some(cue) = crate::cue::parse_cue_virtual_path(source) {
                let dur = if cue.duration > 0.0 {
                    cue.duration
                } else {
                    duration_secs
                };
                (cue.physical_path, cue.start_time, dur, cue.start_time + dur)
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
                value.stage_local(Path::new(open_str), start, cue_dur, stop_secs, generation)
            }
            Self::Dsd(value) => {
                if !is_dsd {
                    bail!("[Direct] Native DSD → PCM 需要重新协商 Diretta connection");
                }
                value.stage_local(Path::new(open_str), start, cue_dur, generation)
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
    /// 停止 SDK 拉流并释放当前回调租约，供 handoff 在清理 ring 前使用。
    fn pause_for_handoff(&mut self) -> Result<()> {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => value.pause(),
            #[cfg(feature = "diretta")]
            Self::Dsd(value) => value.pause(),
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => Ok(()),
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        }
    }

    /// handoff 完成后重新启动同一 Diretta 会话。
    fn play_after_handoff(&mut self) -> Result<()> {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => value.play(),
            #[cfg(feature = "diretta")]
            Self::Dsd(value) => value.play(),
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => Ok(()),
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        }
    }

    /// 换源/关流前的源级静音垫请求：PCM 走淡出（下一交付块 10ms 升余弦渐零，
    /// 随后块为静音）；DSD 位流不可乘增益（无淡出通道），改为请求排空——
    /// 此后每个交付块在回调侧替换为 0x69 静音，持续顶掉设备端缓冲
    fn begin_fade_out(&self) {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => value.begin_fade_out(),
            #[cfg(feature = "diretta")]
            Self::Dsd(value) => value.begin_drain(),
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => {}
            // 空枚举兜底：既无 diretta 也非 test 的构建不存在可构造的传输
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        }
    }

    /// Cross-format hard reset: PCM drains zeros and DSD drains 0x69.
    fn begin_mute_drain(&self) {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => value.begin_mute_drain(),
            #[cfg(feature = "diretta")]
            Self::Dsd(value) => value.begin_drain(),
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => {}
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct transport only exists with diretta/test enabled"),
        }
    }

    /// 淡出/排空是否已生效（后续块均为静音）。DSD 排空完成度由
    /// wait_fade_drained 判定，此处与 PCM 一致返回排空请求是否已置位之后
    /// 的静音态——DSD 无独立淡出态，保持恒 true（等待方以 wait_fade_drained 为准）
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

    /// 事件驱动排空等待：淡出/静音垫完成且达到时长时间（主）或块数（兜底）
    /// 下限，或超时。PCM/DSD 均真实等待——DSD 此前恒 true 导致换源零排空
    fn wait_fade_drained(&self, min_blocks: u32, timeout: Duration) -> bool {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => value.wait_fade_drained(min_blocks, timeout),
            #[cfg(feature = "diretta")]
            Self::Dsd(value) => value.monitor().wait_fade_drained(min_blocks, timeout),
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => true,
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        }
    }

    /// 排空目标时长（µs）：ring 建连后注入的动态值；未注入时为旧常量。
    /// Fake/空枚举兜底返回旧常量
    fn drain_target_micros(&self) -> u64 {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => value.monitor().drain_target_micros(),
            #[cfg(feature = "diretta")]
            Self::Dsd(value) => value.monitor().drain_target_micros(),
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => DIRECT_FADE_DRAIN_MIN_MICROS,
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        }
    }

    /// 软暂停（Phase3）：淡出（PCM）/ 0x69 置零（DSD）到零电平并至少交付
    /// 一个静音块（或超时返回 false——调用方仍会停发，退化为硬停）。
    /// 供 InnerPlayer::pause 在 sync->stop 前调用，消除暂停末块阶跃
    fn begin_soft_pause_and_wait(&self, timeout: Duration) -> bool {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => value.begin_soft_pause_and_wait(timeout),
            #[cfg(feature = "diretta")]
            Self::Dsd(value) => value.begin_soft_pause_and_wait(timeout),
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => true,
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        }
    }

    /// 恢复播放软起：清除软暂停留下的静音/排空态（PCM 另做淡入）。
    /// 不清除则恢复后永久静音（正确性关键）。Fake 无状态
    fn resume_soft(&self) {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => value.resume_soft(),
            #[cfg(feature = "diretta")]
            Self::Dsd(value) => value.resume_soft(),
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => {}
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        }
    }

    /// tinyLMS Quick Resume：清除静音/排空态但不做淡入渐变——新源与旧源
    /// 缓冲尾巴数据级硬拼接（无静音间隙），对齐 tinyLMS SwapToNext；
    /// 淡入反而制造"静音跳回音频"阶跃（可闻咔哒）
    fn resume_handoff_no_fade(&self) {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => value.clear_fade_state(),
            #[cfg(feature = "diretta")]
            Self::Dsd(value) => value.resume_soft(),
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => {}
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        }
    }

    /// tinyLMS Hard Reset 预静音触发：PCM 武装 8 周期回调层静音倒计时
    /// （对齐 tinyLMS TriggerPreMute(8)）；DSD 置 0x69 静音垫并同步短等
    /// （DSD 无回调倒计数通道，直接等排空垫就绪）。返回是否为 PCM
    /// 倒计时模式（true 时调用方需轮询 tinylms_pre_mute_pending 等消耗）
    fn trigger_tinylms_pre_mute(&self) -> bool {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => {
                value.trigger_pre_mute_cycles(8);
                true
            }
            #[cfg(feature = "diretta")]
            Self::Dsd(value) => {
                value.begin_soft_pause_and_wait(std::time::Duration::from_millis(80));
                false
            }
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => false,
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        }
    }

    /// 预静音倒计时是否尚未消耗完（仅 PCM 倒计时模式有意义）
    fn tinylms_pre_mute_pending(&self) -> bool {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => value.forced_mute_pending(),
            #[cfg(feature = "diretta")]
            Self::Dsd(_) => false,
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => false,
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        }
    }

    /// v11-3: 武装下一次换源的跨格式旁路（仅 PCM 传输有意义；
    /// 消费一次自动复位，常规 handoff 不受影响）
    #[cfg(any(feature = "diretta", test))]
    fn arm_cross_format_replace(&self) {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => value.arm_cross_format_replace(),
            #[cfg(feature = "diretta")]
            Self::Dsd(_) => {}
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => {}
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        }
    }

    /// v11-3: 热重配 wire（不拆连接）。仅 PCM 传输支持；
    /// DSD 家族切换不在实验范围（DSD 位率/位序协商复杂度高，回退全量重连）
    #[cfg(any(feature = "diretta", test))]
    fn hot_reconfigure(&self, format: &DirectPcmFormat) -> Result<()> {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => value.hot_reconfigure(format),
            #[cfg(feature = "diretta")]
            Self::Dsd(_) => bail!("[Direct] DSD 传输不支持热重配"),
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => Ok(()),
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        }
    }

    /// v12-A: 手动 handoff 并行开源武装（排空窗口内后台预打开候选源）。
    /// 仅 PCM 传输支持；DSD/Fake 返回 None（走原同步开源路径）
    #[cfg(any(feature = "diretta", test))]
    fn arm_handoff_stage(
        &self,
        path: &Path,
        start_secs: f64,
        stop_secs: f64,
        duration_secs: f64,
    ) -> Option<u64> {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => value.arm_handoff_stage(path, start_secs, stop_secs, duration_secs),
            #[cfg(feature = "diretta")]
            Self::Dsd(_) => None,
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => None,
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        }
    }

    /// v12-A: 装填 staged 预打开候选完成换源（generation 凭据校验，
    /// 未就绪由 producer 短等后回退同步开源）。仅 PCM 传输支持
    #[cfg(any(feature = "diretta", test))]
    fn replace_with_staged_source(
        &mut self,
        open_str: &str,
        start_secs: f64,
        stop_secs: f64,
        cancel: crate::ffmpeg_audio::HttpCancelHandle,
        expected_generation: u64,
    ) -> Result<DirectPcmFormat> {
        match self {
            #[cfg(feature = "diretta")]
            Self::Pcm(value) => value.replace_with_staged_source(
                open_str,
                start_secs,
                stop_secs,
                cancel,
                expected_generation,
            ),
            #[cfg(feature = "diretta")]
            Self::Dsd(_) => bail!("[Direct] DSD 传输不支持 staged handoff"),
            #[cfg(all(test, not(feature = "diretta")))]
            Self::Fake(_) => bail!("[Direct] Fake 传输不支持 staged handoff"),
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
    /// 当前源在物理文件内的起始偏移（CUE 虚拟轨 > 0）：seek 目标与 seek_base 换算基准
    start_offset: f64,
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
        let (path_str, cue_start, cue_dur, cue_stop) =
            if let Some(cue) = crate::cue::parse_cue_virtual_path(source) {
                let dur = if cue.duration > 0.0 {
                    cue.duration
                } else {
                    duration
                };
                (cue.physical_path, cue.start_time, dur, cue.start_time + dur)
            } else if let Some(sacd) = crate::sacd::parse_sacd_virtual_path(source) {
                (
                    source.to_owned(),
                    0.0,
                    if sacd.duration_secs > 0.0 {
                        sacd.duration_secs
                    } else {
                        duration
                    },
                    0.0,
                )
            } else {
                (source.to_owned(), 0.0, duration, 0.0)
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

        let (transport, seek_base) = if is_dsd {
            let (connection, actual_position) = DirettaDirectDsdConnection::open_local_at(
                selector,
                path,
                cue_start + position_secs,
                0.0,
            )?;
            (
                DirectTransport::Dsd(connection),
                (actual_position - cue_start).max(0.0),
            )
        } else {
            let (connection, actual_position) = DirettaDirectConnection::open_local_at(
                selector,
                path,
                cue_start + position_secs,
                cue_stop,
            )?;
            (
                DirectTransport::Pcm(connection),
                (actual_position - cue_start).max(0.0),
            )
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
            start_offset: cue_start,
            selector: selector.to_owned(),
            source: source.to_owned(),
            transport,
        };
        if auto_play {
            // v12-5 首块淡入前置：play() 之前武装 fade-in（零增益 10ms 升余弦
            // 渐入），保证 SDK 第一次拉到的真实音频块就带渐入包络——消除
            // preroll 静音垫耗尽到全电平首块之间的阶跃（此前 resume_soft 在
            // open 返回后调用，wait_for_direct_start 期间消费的首块不受保护）。
            // 首次冷启动（无旧连接）同样无害：10ms 渐入不可闻且防止 DAC
            // 未稳时全电平冲击。DSD 传输 resume_soft 仅清排空态，不动位流
            playback.resume_soft();
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
        Self::open_reader_with_offset(selector, source, reader, duration, 0.0, 0.0, auto_play)
    }

    /// 以 Reader 打开并做启动验证（L2 纯内存播放路径）。
    /// `start_offset_secs` 为源在 Reader 内容内的起点（CUE 轨 = 母版内偏移，
    /// 非 CUE 传 0）：demuxer 级精确定位，且作为后续 seek 的坐标基准。
    /// 消费方需已把整曲物化进 Reader（读侧全量可用，无供数等待）
    #[cfg(feature = "diretta")]
    #[allow(clippy::too_many_arguments)]
    pub fn open_reader_verified(
        selector: &str,
        source: &str,
        reader: Box<dyn crate::direct_pcm::ReadSeek>,
        duration: f64,
        start_offset_secs: f64,
        auto_play: bool,
        load_token: &std::sync::atomic::AtomicU64,
        token: u64,
    ) -> Result<Self> {
        let mut playback = Self::open_reader_with_offset(
            selector,
            source,
            reader,
            duration,
            start_offset_secs,
            start_offset_secs,
            auto_play,
        )?;
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

    /// Reader 打开核心：position_secs 为 demuxer 级定位目标，start_offset 为
    /// 轨内坐标基准（seek_base / 后续 seek 换算使用）
    #[cfg(feature = "diretta")]
    fn open_reader_with_offset(
        selector: &str,
        source: &str,
        reader: Box<dyn crate::direct_pcm::ReadSeek>,
        duration: f64,
        position_secs: f64,
        start_offset_secs: f64,
        auto_play: bool,
    ) -> Result<Self> {
        let (transport, seek_base) = {
            let (connection, actual_position) =
                DirettaDirectConnection::open_reader_at(selector, reader, position_secs, 0.0)?;
            (
                DirectTransport::Pcm(connection),
                (actual_position - start_offset_secs).max(0.0),
            )
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
            start_offset: start_offset_secs,
            selector: selector.to_owned(),
            source: source.to_owned(),
            transport,
        };
        if auto_play {
            // v12-5 首块淡入前置（与 open_local 同语义，见其注释）
            playback.resume_soft();
            playback.play()?;
        }
        Ok(playback)
    }

    /// handoff 换源。`open_path` 为打开用物理路径（在线源的物化产物 memfd/磁盘
    /// 缓存），与逻辑 `source` 分离：cue/sacd 解析与 DSD 家族嗅探仍基于 source
    /// （物化路径无原始扩展名；HTTP DSD 源物化后仍是原始 DSF/DFF，家族判定
    /// 按 URL 才正确），连接与 current_source 记录的也是 source；
    /// `None` = 直接按 source 打开（本地源原行为）。
    /// v12-A: `staged_generation` 为并行开源的装填凭据（arm_handoff_stage 返回值）；
    /// None 走原同步开源路径
    #[cfg(feature = "diretta")]
    pub fn handoff_drained_source(
        &mut self,
        source: &str,
        open_path: Option<&str>,
        duration: f64,
        cancel: crate::ffmpeg_audio::HttpCancelHandle,
        staged_generation: Option<u64>,
    ) -> Result<DirectFormat> {
        let (path_str, cue_start, cue_dur, cue_stop) =
            if let Some(cue) = crate::cue::parse_cue_virtual_path(source) {
                let dur = if cue.duration > 0.0 {
                    cue.duration
                } else {
                    duration
                };
                (cue.physical_path, cue.start_time, dur, cue.start_time + dur)
            } else if let Some(sacd) = crate::sacd::parse_sacd_virtual_path(source) {
                (
                    source.to_owned(),
                    0.0,
                    if sacd.duration_secs > 0.0 {
                        sacd.duration_secs
                    } else {
                        duration
                    },
                    0.0,
                )
            } else {
                (source.to_owned(), 0.0, duration, 0.0)
            };
        let open_str = open_path.unwrap_or(&path_str);
        let path = Path::new(&path_str);
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let is_dsd = matches!(extension.as_str(), "dsf" | "dff" | "dsdiff" | "iso")
            || path_str.contains(".iso|")
            || path_str.contains(".ISO|");

        // SDK 可能在停止拉流后仍持有最后一个回调块。先停止并显式释放
        // lease，再允许 producer 清理旧 ring；否则 handoff 会与 SDK 读旧内存竞态。
        // 同格式 handoff 保持 SDK 拉流线程和 Sync 会话持续运行，只在 producer
        // 的安全块边界原子替换 ring。禁止调用 SDK stop/release/play，避免
        // 连接复用时重新引入尾帧、点击声和线程生命周期竞态。
        // set_duration 必须先于 replace：slot 的 boundary_duration 在 replace 内发布，
        // 后设只改 ring 值会在边界消费时被旧时长覆盖
        let replaced: Result<DirectFormat> = (|| {
            Ok(match &mut self.transport {
            DirectTransport::Pcm(value) => {
                if is_dsd {
                    bail!("[Direct] PCM → Native DSD 需要重新协商 Diretta connection");
                }
                value.set_duration(cue_dur);
                // v12-A：有 staged 凭据则优先装填预打开候选（并行开源），
                // producer 侧短等后回退同步开源，语义与同步路径一致
                let pcm_format = match staged_generation {
                    Some(expected_generation) => value.replace_with_staged_source(
                        open_str,
                        cue_start,
                        cue_stop,
                        cancel,
                        expected_generation,
                    )?,
                    None => {
                        value.replace_drained_local_source(open_str, cue_start, cue_stop, cancel)?
                    }
                };
                DirectFormat::Pcm(pcm_format)
            }
            DirectTransport::Dsd(value) => {
                if !is_dsd {
                    bail!("[Direct] Native DSD → PCM 需要重新协商 Diretta connection");
                }
                let format = value.replace_drained_local_source(open_str, cue_start)?;
                DirectFormat::Dsd(format)
            }
            })
        })();
        let format = match replaced {
            Ok(format) => format,
            Err(error) => return Err(error),
        };
        self.source = source.to_owned();
        self.duration = cue_dur;
        self.start_offset = cue_start;
        self.seek_base = 0.0;
        self.seek_transition_count = self.monitor().transition_count();
        Ok(format)
    }
    /// 轨内 seek（position_secs 为轨内相对位置）：物理定位需加 start_offset，
    /// 返回值与 seek_base 同步换算回轨内坐标
    pub fn seek_while_paused(&mut self, position_secs: f64) -> Result<f64> {
        let physical_target = self.start_offset + position_secs.max(0.0);
        let actual_position: f64 = match &mut self.transport {
            #[cfg(feature = "diretta")]
            DirectTransport::Pcm(value) => value.seek_while_paused(physical_target)?,
            #[cfg(feature = "diretta")]
            DirectTransport::Dsd(value) => value.seek_while_paused(physical_target)?,
            #[cfg(all(test, not(feature = "diretta")))]
            DirectTransport::Fake(value) => {
                value
                    .position_micros
                    .store(0, std::sync::atomic::Ordering::Release);
                physical_target
            }
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        };

        self.seek_base = actual_position - self.start_offset;
        self.seek_transition_count = self.monitor().transition_count();
        Ok(self.seek_base)
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
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
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
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
        }
    }

    /// Cross-format reconnect: drain pure digital silence before disconnect.
    /// PCM sends zero-valued S32 frames; DSD sends its native 0x69 mute pattern.
    /// Already delivered program samples are never altered.
    pub fn begin_mute_drain(&self) {
        self.transport.begin_mute_drain();
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

    /// 排空目标时长（µs）：ring 建连后注入的动态值；未注入时为旧常量
    /// 200_000。调用方以此联动计算 wait_fade_drained 超时
    pub fn drain_target_micros(&self) -> u64 {
        self.transport.drain_target_micros()
    }

    /// 软暂停等待（Phase3）：淡出/0x69 置零到零电平再停发（详见 transport）
    pub fn begin_soft_pause_and_wait(&self, timeout: Duration) -> bool {
        self.transport.begin_soft_pause_and_wait(timeout)
    }

    /// 恢复播放软起：清除静音/排空态（PCM 另做淡入）
    pub fn resume_soft(&self) {
        self.transport.resume_soft()
    }

    /// tinyLMS Quick Resume：清除静音/排空态但不做淡入渐变（PCM 数据级
    /// 硬拼接，对齐 tinyLMS SwapToNext；DSD 无增益通道，clear_drain 即可）
    pub fn resume_handoff_no_fade(&self) {
        self.transport.resume_handoff_no_fade()
    }

    /// tinyLMS Hard Reset 预静音触发（PCM 武装 8 周期回调静音倒计时；
    /// DSD 置 0x69 垫并同步短等）。返回是否为 PCM 倒计时模式
    pub fn trigger_tinylms_pre_mute(&self) -> bool {
        self.transport.trigger_tinylms_pre_mute()
    }

    /// 预静音倒计时是否尚未消耗完（仅 PCM 倒计时模式为真）
    pub fn tinylms_pre_mute_pending(&self) -> bool {
        self.transport.tinylms_pre_mute_pending()
    }

    /// v11-3: 武装下一次换源的跨格式旁路（热重配实验专用，消费一次自动复位）
    #[cfg(any(feature = "diretta", test))]
    pub fn arm_cross_format_replace(&self) {
        self.transport.arm_cross_format_replace();
    }

    /// v11-3: 热重配 wire（不拆连接）：stop → setSinkConfigure →
    /// configTransferAuto → preroll → play。失败由调用方回退全量重连
    #[cfg(any(feature = "diretta", test))]
    pub fn hot_reconfigure(&self, format: &DirectPcmFormat) -> Result<()> {
        self.transport.hot_reconfigure(format)
    }

    /// v12-A: 手动 handoff 并行开源武装——排空窗口内后台预打开候选源，
    /// 返回 generation 凭据供 handoff_drained_source 走 ReplaceStaged 装填。
    /// 仅 PCM 传输支持（DSD/Fake 返回 None）
    #[cfg(any(feature = "diretta", test))]
    pub fn arm_handoff_stage(
        &self,
        open_path: &str,
        start_secs: f64,
        stop_secs: f64,
        duration_secs: f64,
    ) -> Option<u64> {
        self.transport
            .arm_handoff_stage(Path::new(open_path), start_secs, stop_secs, duration_secs)
    }

    /// open 后启动验证：等待首块被设备真正消费。
    /// 首块消费前失败/提前结束/被新 load 取代即报错；超时返回 Ok(false)，
    /// 由调用方决定回退方式。事件驱动等待，单次 100ms 上限保证 load 取消的响应性
    pub fn wait_for_direct_start(
        &self,
        load_token: &std::sync::atomic::AtomicU64,
        token: u64,
    ) -> Result<bool> {
        // 启动验证总超时：Target 时钟锁定通常亚秒级，2s 已覆盖慢启动；
        // 高码率（>96k PCM / 任意 DSD）首块交付前置更长（DSD 400ms 预缓冲 +
        // 慢盘首帧读），放宽到 5s——误判会拆掉本已成功的连接，表现即"切歌失败"
        const DIRECT_START_TIMEOUT: Duration = Duration::from_secs(2);
        const DIRECT_START_TIMEOUT_HIGH_RATE: Duration = Duration::from_secs(5);
        let start_timeout = if self.monitor().is_high_rate() {
            DIRECT_START_TIMEOUT_HIGH_RATE
        } else {
            DIRECT_START_TIMEOUT
        };
        let deadline = std::time::Instant::now() + start_timeout;
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
    #[cfg(feature = "diretta")]
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
        open_path: Option<&str>,
        duration: f64,
        cancel: crate::ffmpeg_audio::HttpCancelHandle,
        staged_generation: Option<u64>,
    ) -> Result<DirectFormat> {
        let _ = (open_path, cancel, staged_generation);
        self.duration = duration;
        self.start_offset = crate::cue::parse_cue_virtual_path(source)
            .map(|cue| cue.start_time)
            .unwrap_or(0.0);
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
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
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
            #[cfg(not(any(feature = "diretta", test)))]
            _ => unreachable!("Direct 传输仅在 diretta/test 配置下可用"),
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
        self.start_offset = crate::cue::parse_cue_virtual_path(source)
            .map(|cue| cue.start_time)
            .unwrap_or(0.0);
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
            start_offset: 0.0,
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
