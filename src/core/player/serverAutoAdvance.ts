/**
 * headless 自动连播前端接线（§八 B 层）：
 * 浏览器降级为遥控器——曲中把下一曲候选注册到服务端，曲终由服务端接力加载；
 * 浏览器关闭不影响接续。桌面模式（IPC 队列由渲染进程驱动）全部为 no-op。
 */
import type { Track } from "@shared/types/player";
import { useMediaStore } from "@/stores/media";
import { useStatusStore } from "@/stores/status";
import { useSettingsStore } from "@/stores/settings";
import * as queue from "@/stores/queue";
import { playerClient } from "@/services/client";
import { peekNextTrackPreload } from "@/services/nextTrackPreloader";
import { buildStagingSource } from "@/core/player/gapless";
import { getNextTrackCandidate } from "./candidate";
import * as lyricLoader from "@/services/lyric/loader";
import * as coverLoader from "@/services/coverLoader";
import { extractColorFromUrl } from "@/utils/color";

/** 已注册的下一曲候选（曲终服务端接力后用于前端采纳） */
let registeredNext: { source: string; track: Track; index: number } | null = null;
/** 已注册候选对应的当前曲 source（去重，防 position tick 重复注册） */
let registeredForSource: string | null = null;

/**
 * 位置事件驱动：把下一曲候选注册到服务端（内部自节流；桌面模式 no-op）。
 * 单曲循环本地 seek(0) 续播、FM 实时解析：均不走服务端接力，不注册
 */
export const maybeRegisterNextCandidate = (): void => {
  if (!playerClient.supportsServerAutoAdvance) return;
  const status = useStatusStore();
  const settings = useSettingsStore();
  if (status.repeatMode === "one" || status.fmMode) return;
  if (status.state !== "playing") return;

  const candidate = getNextTrackCandidate({
    playIndex: status.playIndex,
    queue: queue.queue.value,
    fmMode: status.fmMode,
    fuckDjMode: settings.preset.fuckDjMode,
    shuffleMode: status.shuffleMode,
  });
  if (!candidate) {
    if (registeredForSource !== null) {
      registeredForSource = null;
      registeredNext = null;
      void playerClient.clearNextCandidate().catch(() => {});
    }
    return;
  }

  // 音源解析：优先预载链路的已解析 URL（在线源），回退本地/管道格式
  const peeked = peekNextTrackPreload(candidate.track);
  const resolved =
    peeked?.source?.source ??
    candidate.track.cueAudioPath ??
    candidate.track.path ??
    candidate.track.id;
  const source = buildStagingSource(candidate.track, resolved);
  if (source === registeredForSource) return;
  registeredForSource = source;
  registeredNext = { source, track: candidate.track, index: candidate.index };
  const durationHintSecs = candidate.track.duration > 0 ? candidate.track.duration / 1000 : 0;
  void playerClient.registerNextCandidate(source, durationHintSecs).catch(() => {});
};

/**
 * 服务端自动接力后采纳新曲：推进 queue/media/歌词（播放已在服务端发生，不重载）。
 * 仅当 source 与已注册候选一致时生效；采纳后清空注册状态
 */
export const adoptServerAdvancedTrack = (source: string): boolean => {
  if (!registeredNext || registeredNext.source !== source) return false;
  const { track, index } = registeredNext;
  registeredNext = null;
  registeredForSource = null;

  const status = useStatusStore();
  const media = useMediaStore();
  status.playIndex = index;
  status.trackLoading = false;
  media.setTrack(track);
  status.currentSource = source;
  // 候选曲 detail 未探测：先走在线歌词/封面，本地嵌入歌词等下次完整 load 恢复
  lyricLoader.beginLoad();
  void lyricLoader.loadForTrack(null);
  void coverLoader.loadCoverForTrack(track);
  extractColorFromUrl(track.cover ?? track.coverOriginal ?? null);
  return true;
};

/** 客户端主动加载新曲时清空注册状态（防止服务端接力与手动切歌竞态） */
export const resetServerAutoAdvance = (): void => {
  registeredNext = null;
  registeredForSource = null;
};
