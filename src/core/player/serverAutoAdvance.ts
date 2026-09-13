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
import { resolveTrackSource } from "@/services/audioSource";
import { buildStagingSource } from "@/core/player/gapless";
import { pushServerQueueSnapshot } from "./serverQueue";
import { getNextTrackCandidate } from "./candidate";
import type { CandidateResult } from "./candidate";
import * as lyricLoader from "@/services/lyric/loader";
import * as coverLoader from "@/services/coverLoader";
import { extractColorFromUrl } from "@/utils/color";

/** 已注册的下一曲候选（曲终服务端接力后用于前端采纳） */
let registeredNext: { source: string; track: Track; index: number } | null = null;
/** 已注册候选对应的当前曲 source（去重，防 position tick 重复注册） */
let registeredForSource: string | null = null;
/** 正在/最近一次解析直链的候选 track id（防 position tick 重复发起解析） */
let resolvingTrackId: string | null = null;
/** 上次直链解析尝试时间（失败后的重试冷却） */
let lastResolveAttemptAt = 0;
/** 直链解析失败后的重试冷却：瞬时网络故障不至于直接判死本曲自动连播 */
const RESOLVE_RETRY_COOLDOWN_MS = 30_000;

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
    if (registeredForSource !== null || resolvingTrackId !== null) {
      registeredForSource = null;
      registeredNext = null;
      resolvingTrackId = null;
      void playerClient.clearNextCandidate().catch(() => {});
    }
    return;
  }

  // 本曲候选已注册可用音源：不再重算（在线直链解析/预载切换不做反复覆盖）
  if (registeredNext?.track.id === candidate.track.id && registeredForSource !== null) return;

  // 音源解析：优先预载链路的已解析 URL（在线源），回退 track.path。
  // cueAudioPath 是母版路径（展示/库字段），作加载源会从母版 0:00 出声，永远不进候选
  const resolvedSource =
    peekNextTrackPreload(candidate.track)?.source?.source ?? candidate.track.path;
  if (resolvedSource) {
    registerCandidate(candidate, buildStagingSource(candidate.track, resolvedSource));
    return;
  }
  resolveThenRegister(candidate);
};

/** 解析成功/拿到可用音源后统一注册入口：整表推送服务端队列快照。
 * 服务端 preloader 拿到已解析直链后自治 stage（幂等重调度）；
 * 服务端 stage 被拒（跨 wire 格式）时自行登记接力候选，曲终加载 */
const registerCandidate = (candidate: CandidateResult, source: string): void => {
  if (source === registeredForSource) return;
  registeredForSource = source;
  registeredNext = { source, track: candidate.track, index: candidate.index };
  resolvingTrackId = null;
  pushServerQueueSnapshot();
};

/**
 * 预载未命中且无本地路径（在线/流媒体曲）：先解析直链再注册。
 * 失败按冷却窗口由后续 position tick 重试，不阻塞播放
 */
const resolveThenRegister = (candidate: CandidateResult): void => {
  if (
    resolvingTrackId === candidate.track.id &&
    Date.now() - lastResolveAttemptAt < RESOLVE_RETRY_COOLDOWN_MS
  ) {
    return;
  }
  resolvingTrackId = candidate.track.id;
  lastResolveAttemptAt = Date.now();
  void (async () => {
    try {
      const resolved = await resolveTrackSource(candidate.track, { silent: true });
      if (resolved?.source) {
        registerCandidate(candidate, buildStagingSource(candidate.track, resolved.source));
      }
    } catch {
      // 保持占位等冷却后重试；解析彻底不可用时本曲不注册，服务端曲终自然停止
    }
  })();
};

/**
 * 服务端自动接力后采纳新曲：推进 queue/media/歌词（播放已在服务端发生，不重载）。
 * 匹配优先按队列曲目 id（直链每次解析结果不同，串比对先天脆弱），
 * source 串精确相等作为旧服务端/本地直链的回退；
 * 两者都不可用时放弃采纳（UI 保持原曲，避免错位推进）。
 * 采纳后清空注册状态
 */
export const adoptServerAdvancedTrack = (match: {
  source?: string;
  trackId?: string;
}): boolean => {
  if (!registeredNext) return false;
  const byTrackId = !!match.trackId && registeredNext.track.id === match.trackId;
  const bySource = !!match.source && registeredNext.source === match.source;
  if (!byTrackId && !bySource) return false;
  const { track, index, source } = registeredNext;
  registeredNext = null;
  registeredForSource = null;
  resolvingTrackId = null;
  lastResolveAttemptAt = 0;

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

/**
 * 手动点歌待采纳：客户端发起 load 时登记（source/track/预期队列位），
 * WS 状态推送确认到达后对齐 UI。
 * 修手动切歌 UI 显示与实际播放不同步（显示旧曲/队列游标脱节）：
 * adoptServerAdvancedTrack 只服务自动接力（registeredNext 匹配），手动点歌时
 * 该槽已被 resetServerAutoAdvance 清空，服务端推来的正确 current_source /
 * current_track_id 会被直接丢弃，UI 停留在旧曲；下一曲随后按旧游标计算，
 * 造成"跳曲"。本槽位让手动点歌同样按服务端权威状态对齐
 */
let manualPending: { source: string; track: Track; index: number } | null = null;

/** 客户端 load 提交前登记本次点歌的 source/track/预期队列位 */
export const markManualServerLoad = (
  source: string,
  track: Track | null,
  index: number,
): void => {
  if (!playerClient.supportsServerAutoAdvance) return;
  if (!source || !track) return;
  manualPending = { source, track, index };
};

/**
 * WS 状态推送到达时消费 manualPending：source 精确相等（与提交串一致）
 * 或队列曲目 id 相等即采纳，对齐 playIndex/media/currentSource。
 * 与 adoptServerAdvancedTrack 的差异：不要求 registeredNext 存在
 */
export const tryAdoptManualServerLoad = (match: {
  source?: string;
  trackId?: string;
}): boolean => {
  if (!manualPending) return false;
  const bySource = !!match.source && match.source === manualPending.source;
  const byTrackId =
    !!match.trackId && !!manualPending.track.id && match.trackId === manualPending.track.id;
  if (!bySource && !byTrackId) return false;
  const { track, index, source } = manualPending;
  manualPending = null;

  const status = useStatusStore();
  const media = useMediaStore();
  status.playIndex = index;
  status.trackLoading = false;
  media.setTrack(track);
  status.currentSource = source;
  return true;
};

/** 客户端主动加载新曲时清空注册状态（防止服务端接力与手动切歌竞态） */
export const resetServerAutoAdvance = (): void => {
  registeredNext = null;
  registeredForSource = null;
  manualPending = null;
  resolvingTrackId = null;
  lastResolveAttemptAt = 0;
};
