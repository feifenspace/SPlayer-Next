/**
 * Diretta Source Direct 无缝切曲控制
 *
 * 依赖引擎侧已就绪的三段式机制：
 * 1. stage：剩余时间不足阈值时把下一曲（本地可 seek 音源）预载进 Diretta 连接的二级槽
 * 2. boundary：当前曲自然播完，引擎在音频回调内原子切换到已预载音源（Diretta 连接全程保持），
 *    并推送 directTrackBoundary 事件
 * 3. commit：renderer 推进 queue/media 状态后回调引擎确认
 *
 * staging 复用 nextTrackPreloader 已解析的下一曲音源；由 preloadNextTrack 设置统一控制。
 */

import type { Track } from "@shared/types/player";
import type { ResolvedTrackSource } from "@/services/audioSource";
import { useMediaStore } from "@/stores/media";
import { useStatusStore } from "@/stores/status";
import { useSettingsStore } from "@/stores/settings";
import * as queue from "@/stores/queue";
import { getNextTrackCandidate } from "@/core/player/candidate";
import { peekNextTrackPreload, invalidateNextTrackPreload, scheduleNextTrackPreload } from "@/services/nextTrackPreloader";
import { useHistoryStore } from "@/stores/history";
import * as lyricLoader from "@/services/lyric/loader";
import * as coverLoader from "@/services/coverLoader";
import { extractColorFromUrl } from "@/utils/color";
import * as playback from "@/services/playback";
import * as playStats from "./stats";

/** 剩余时间低于该阈值（秒）即触发 stage 预载 */
const STAGE_THRESHOLD_SECS = 30;

/** stage 尝试去重计数器（与引擎 boundary generation 对应） */
let stageGeneration = 0;

/** 当前已 stage 的目标 track id；空串表示无 */
let stagedTrackId = "";
/** 本次播放内 stage 已不可用（非 Direct runtime / 在线源 in desktop / 上次尝试失败） */
let stageUnavailable = false;
/** 上次 stage 检查时的 track id，用于检测切曲并复位状态 */
let lastSeenTrackId = "";

/**
 * CUE 虚拟路径转换为引擎 stage 支持的物理分段格式 `path|start|dur|track`。
 * cue:// 路径只有 DB/load 通道能解析，stage_local 只认管道格式
 */
const buildStagingSource = (track: Track, resolvedSource: string): string => {
  if (!resolvedSource.startsWith("cue://")) return resolvedSource;
  if (!track.cueAudioPath || track.cueStartMs == null) return resolvedSource;
  const startSec = track.cueStartMs / 1000;
  const durSec = Math.max(((track.cueEndMs ?? 0) - track.cueStartMs) / 1000, 0);
  return `${track.cueAudioPath}|${startSec.toFixed(3)}|${durSec.toFixed(3)}|${track.track ?? 1}`;
};

/** 检测切曲：复位 per-track 状态 */
const resetIfTrackChanged = (): void => {
  const track = useMediaStore().track;
  const id = track?.id ?? "";
  if (id !== lastSeenTrackId) {
    lastSeenTrackId = id;
    stagedTrackId = "";
    stageUnavailable = false;
  }
};

/**
 * 位置推送驱动的 stage 预载检查（在 position 事件中调用，内部自节流）
 */
export const maybeStageDirectNext = (): void => {
  resetIfTrackChanged();
  if (stageUnavailable || stagedTrackId) return;

  const settings = useSettingsStore();
  const status = useStatusStore();
  // 与 preloadNextTrack 同一开关控制整条无缝链路
  if (!settings.player.preloadNextTrack) return;
  // 单曲循环引擎内无下一曲概念；FM 的下一曲必须实时解析
  if (status.repeatMode === "one" || status.fmMode) return;
  if (status.state !== "playing") return;

  const durationSecs = status.duration / 1000;
  if (durationSecs <= 0) return;
  const remainingSecs = durationSecs - status.position / 1000;
  if (remainingSecs > STAGE_THRESHOLD_SECS) return;

  const candidate = getNextTrackCandidate({
    playIndex: status.playIndex,
    queue: queue.queue.value,
    fmMode: status.fmMode,
    fuckDjMode: settings.preset.fuckDjMode,
    shuffleMode: status.shuffleMode,
  });
  if (!candidate) return;

  // 复用预载链路已解析的音源；尚未解析完成时等下一个 position tick 再试
  const peeked = peekNextTrackPreload(candidate.track);
  if (!peeked?.source) return;

  const source = buildStagingSource(candidate.track, peeked.source.source);
  const duration = candidate.track.duration > 0 ? candidate.track.duration / 1000 : durationSecs;
  const generation = ++stageGeneration;
  stagedTrackId = candidate.track.id;

  void (async () => {
    try {
      const result = await window.api.player.stageDirectNext(source, duration, generation);
      if (!result.success || !result.data) {
        // 非 Direct runtime 或引擎拒绝（如 PCM→DSD 需重协商）：本曲内不再尝试
        stageUnavailable = true;
        stagedTrackId = "";
        return;
      }
      console.info("[gapless] staged next track:", candidate.track.title);
    } catch (err) {
      console.warn("[gapless] stage next failed silently:", err);
      stageUnavailable = true;
      stagedTrackId = "";
    }
  })();
};

/**
 * 用户主动跳曲/换曲时作废未消费的 staged 音源
 * （引擎侧随旧 DirectPlayback drop 也会清理，此处双保险）
 */
export const cancelStagedDirectNext = (): void => {
  if (!stagedTrackId) return;
  stagedTrackId = "";
  void window.api.player.cancelDirectNext().catch(() => {});
};

/** 当前是否已有已 stage 的下一曲（用于抑制引擎边界前的前端切曲逻辑） */
export const hasStagedDirectNext = (): boolean => stagedTrackId !== "";

/**
 * 处理引擎 directTrackBoundary 事件：音频已零间隙切入下一曲，
 * renderer 侧推进 queue/media/歌词等 UI 状态并向引擎 commit
 * @returns 是否完成了无缝推进（false = 引擎状态与前端不一致，回退常规切曲流程）
 */
export const advanceGaplessBoundary = async (
  durationMs: number,
  _generation: number,
): Promise<boolean> => {
  const status = useStatusStore();
  const settings = useSettingsStore();
  const media = useMediaStore();

  const candidate = getNextTrackCandidate({
    playIndex: status.playIndex,
    queue: queue.queue.value,
    fmMode: status.fmMode,
    fuckDjMode: settings.preset.fuckDjMode,
    shuffleMode: status.shuffleMode,
  });
  // 引擎已切入下一曲但前端无法定位候选：只能提交时长，保持引擎侧状态正确
  if (!candidate) {
    await window.api.player
      .commitDirectBoundary(media.track?.path ?? media.track?.id ?? "", durationMs / 1000)
      .catch(() => {});
    return false;
  }

  const track = candidate.track;

  // 结算上一曲（gapless 无 repeat-one，直接计 scrobble）
  playStats.onTrackEnded(true);
  useHistoryStore().record(track);

  // 队列与 UI 状态推进（音频切换已在引擎内发生，不走 load）
  status.playIndex = candidate.index;
  status.trackLoading = false;
  status.currentSource = null;
  lyricLoader.beginLoad();
  media.setTrack(track);
  media.setPlaybackContext(status.currentPlaybackContext);
  status.position = 0;
  status.duration = durationMs > 0 ? durationMs : (track.duration ?? 0);
  playback.setCurrentTime(0, { force: true });
  playback.setDuration(status.duration);
  playback.setPlaying(true);
  // 歌词：候选曲 detail 未探测，先走在线歌词；本地嵌入歌词等下次完整 load 恢复
  void lyricLoader.loadForTrack(null);
  void coverLoader.loadCoverForTrack(track);
  extractColorFromUrl(track.cover ?? track.coverOriginal ?? null);

  // 向引擎 commit；source 需与 stage 时一致，优先读预载缓存中已解析音源
  const peeked = peekNextTrackPreload(track);
  const commitSource = peeked?.source
    ? buildStagingSource(track, peeked.source.source)
    : (track.cueAudioPath ?? track.path ?? track.id);
  await window.api.player.commitDirectBoundary(commitSource, status.duration / 1000).catch(() => {});

  // 预载缓存已消费，为新的当前曲调度下一下一曲预载
  invalidateNextTrackPreload();
  scheduleNextTrackPreload();
  return true;
};

/** 供外部判断指定 resolved source 是否适合直接 stage（纯本地/虚拟路径） */
export const isDirectStageableSource = (resolved: ResolvedTrackSource | null): boolean => {
  if (!resolved) return false;
  return !resolved.source.startsWith("http://") && !resolved.source.startsWith("https://");
};
