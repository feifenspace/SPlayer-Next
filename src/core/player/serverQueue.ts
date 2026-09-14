/**
 * 服务端队列快照同步（配合 headless 服务端自治无缝预载）
 *
 * 把播放队列 + 当前索引 + repeat 整表推送 PUT /api/v1/player/queue 后，
 * 服务端 direct_preloader 自治完成"下一曲 stage → boundary 簿记/队列推进 →
 * 再预载"闭环：同 wire 格式零间隙无缝、跨格式曲终接力，浏览器离场照常工作。
 *
 * 前端职责收缩为：
 *  - 队列/索引/模式变化与下一曲直链解析完成后推送快照（在线曲目 URL 由
 *    预载链路解析，解析落地即重推，服务端拿到可 stage 的 source）
 *  - UI 推进（directTrackBoundary 事件处理保持不变）
 * 快照注册后前端自身 stage 编排（maybeStageDirectNext）停用，避免与服务端
 * staging 的 generation 竞争。
 */
import { toRaw, watch } from "vue";
import type { Track } from "@shared/types/player";
import { useMediaStore } from "@/stores/media";
import { useStatusStore } from "@/stores/status";
import { useSettingsStore } from "@/stores/settings";
import * as queue from "@/stores/queue";
import { playerClient } from "@/services/client";
import { peekNextTrackPreload } from "@/services/nextTrackPreloader";
import { buildStagingSource } from "./gapless";

/** 服务端队列已注册：前端自身 stage 编排停用（服务端自治接管） */
let serverQueueActive = false;

export const isServerQueueActive = (): boolean => serverQueueActive;

/** 是否处于队列恢复/权威对齐期间，防止 watch immediate 或初次加载时将陈旧的前端状态推回服务端 */
let isRestoringQueue = true;

export const setRestoringQueue = (restoring: boolean): void => {
  isRestoringQueue = restoring;
};

export const isQueueRestoring = (): boolean => isRestoringQueue;

/** 各曲目最近一次落定的可加载 source（track.id 键）：快照推送与直链解析是
 * 异步竞态——watch 触发的推送常早于解析落定，空串占位会覆盖服务端已有的
 * 直链，让预载/接力断链（浏览器离场后无重推机会，直接断到曲终停播）。
 * 解析未落定的条目回退此缓存 */
const lastResolvedSources = new Map<string, string>();

/** 单曲的快照 source：与候选/接力注册同构（CUE 转管道格式，在线为已解析直链；
 * 无可用 source 回退最近一次落定值，再无则空串占位——保持 items 与队列 1:1，
 * 索引不错位） */
const snapshotSourceFor = (track: Track): string => {
  const resolvedSource = peekNextTrackPreload(track)?.source?.source ?? track.path;
  if (!resolvedSource) return lastResolvedSources.get(track.id) ?? "";
  const stagingSource = buildStagingSource(track, resolvedSource);
  lastResolvedSources.set(track.id, stagingSource);
  return stagingSource;
};

let lastSignature = "";
let pushTimer: ReturnType<typeof setTimeout> | null = null;
// 快照请求串行发送，避免快速插入/重排时旧快照晚到覆盖新快照。
let pushChain: Promise<unknown> = Promise.resolve();

const computePayload = () => {
  const status = useStatusStore();
  const tracks = queue.queue.value;
  const items = tracks.map((track) => ({
    source: snapshotSourceFor(track),
    duration_ms: track.duration && track.duration > 0 ? track.duration : null,
    title: track.title ?? null,
    artist: track.artists.map((artist) => artist.name).join(" / ") || null,
    album: track.album?.name ?? null,
    cover: track.cover ?? null,
    // 完整曲目快照（服务端透传）：浏览器存储被清空后重开页面时，可恢复
    // 平台身份（id/source/extId/serverId/CUE 分段/音质等），而不仅是展示字段
    track: { ...toRaw(track), headlessQuality: useSettingsStore().player.songLevel },
  }));
  // 缓存随队列瘦身：已移出队列的曲目不再保留旧直链
  const liveIds = new Set(tracks.map((track) => track.id));
  for (const id of lastResolvedSources.keys()) {
    if (!liveIds.has(id)) lastResolvedSources.delete(id);
  }
  return {
    items,
    index: Math.max(0, Math.min(status.playIndex, Math.max(tracks.length - 1, 0))),
    repeat: status.repeatMode === "list" ? "all" : status.repeatMode,
    shuffle: false,
  };
};

/**
 * 标记本地队列与服务端已权威同步（记录最新签名并激活 serverQueueActive），
 * 防止权威对齐后再次触发向服务端的反向覆盖
 */
export const markServerQueueSynchronized = (): void => {
  const payload = computePayload();
  lastSignature = JSON.stringify(payload);
  serverQueueActive = true;
};

/**
 * 推送队列快照（签名去重；FM 模式/桌面端跳过；恢复期间阻断）。
 * 服务端收到即重新调度无缝预载（invalidate + 重 stage），幂等安全
 */
export const pushServerQueueSnapshot = (): void => {
  if (!playerClient.supportsServerAutoAdvance) return;
  if (isRestoringQueue) return;
  const status = useStatusStore();
  if (status.fmMode) return;

  const payload = computePayload();

  // 签名去重：position tick 驱动的解析落定可能频繁触发，内容未变不重推
  const signature = JSON.stringify(payload);
  if (signature === lastSignature) {
    serverQueueActive = true;
    return;
  }

  pushChain = pushChain
    .catch(() => undefined)
    .then(() => playerClient.pushQueueSnapshot(payload))
    .then((result) => {
      if (result.success) {
        serverQueueActive = true;
        lastSignature = signature;
      }
    })
    .catch(() => {
      // 推送失败保持 inactive：前端自身 stage 编排继续兜底
    });
};

/** 防抖推送（队列批量操作/连续切歌时合并） */
const schedulePush = (): void => {
  if (isRestoringQueue) return;
  if (pushTimer != null) return;
  pushTimer = setTimeout(() => {
    pushTimer = null;
    pushServerQueueSnapshot();
  }, 300);
};

/** 安装队列快照同步监听（initPlayer 调用一次） */
export const installServerQueueSync = (): void => {
  const status = useStatusStore();
  const media = useMediaStore();
  watch(
    () => [
      status.playIndex,
      status.repeatMode,
      status.shuffleMode,
      useSettingsStore().player.songLevel,
      media.track?.id ?? "",
      queue.queue.value.map((track) => track.id).join(","),
    ],
    () => schedulePush(),
    { immediate: true },
  );
};
