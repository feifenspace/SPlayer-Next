import type { PlayerEvent } from "@shared/types/player";
import { useMediaStore } from "@/stores/media";
import { useStatusStore } from "@/stores/status";
import { useFavorite } from "@/composables/useFavorite";
import * as playback from "@/services/playback";
import * as autoClose from "@/services/autoClose";
import * as abLoop from "@/services/abLoop";
import * as cacheScheduler from "@/services/cacheScheduler";
import * as playStats from "./stats";
import { playerClient } from "@/services/client";
import { adoptServerAdvancedTrack, maybeRegisterNextCandidate } from "./serverAutoAdvance";
import {
  hasReachedSeekTarget,
  insertManyToQueue,
  isSeeking,
  markSeek,
  nextTrack,
  pause,
  play,
  playNow,
  prevTrack,
  recoverFromSourceFailure,
  refreshDevices,
  seek,
  setRepeatMode,
  setShuffleMode,
} from "./index";
import {
  advanceGaplessBoundary,
  hasStagedDirectNext,
  maybeStageDirectNext,
} from "./gapless";

/** 防止 ended 事件重入 */
let endedGuard = false;

/** 按当前播放模式结算并进入下一步 */
const finishCurrentTrack = async (): Promise<void> => {
  const status = useStatusStore();
  if (endedGuard) return;
  endedGuard = true;
  try {
    const stopByTimer = autoClose.onTrackEnded();
    const repeatOne = status.repeatMode === "one" && !status.fmMode;
    // headless 自动连播：候选已在曲中注册，曲终由服务端接力加载下一曲；
    // 本地仅结算统计/定时关闭/单曲循环，避免与服务端接力双重加载。
    // FM 候选未注册（需实时解析），回退本地推进
    if (playerClient.supportsServerAutoAdvance && !status.fmMode) {
      playStats.onTrackEnded(repeatOne && !stopByTimer);
      if (stopByTimer) return;
      if (repeatOne) {
        await seek(0);
        await play();
      }
      return;
    }
    // 结算播放统计
    playStats.onTrackEnded(repeatOne && !stopByTimer);
    // 定时关闭"等本曲结束"模式
    if (stopByTimer) return;
    // 单曲循环：seek 回开头继续播放
    if (repeatOne) {
      await seek(0);
      await play();
    } else {
      await nextTrack();
    }
  } finally {
    endedGuard = false;
  }
};

/**
 * 处理主进程推送的播放事件
 * @param event - 播放事件
 */
export const handleEvent = async (event: PlayerEvent): Promise<void> => {
  const status = useStatusStore();
  switch (event.type) {
    case "status":
      // 歌曲加载中或 loading 事件不更新 UI，保持当前封面/进度/播放状态平滑过渡
      if (event.data.state === "loading" || status.trackLoading) break;
      status.state = event.data.state;
      // Web Control 不接收高频 position event，只轮询 status snapshot；
      // 因此 status 也必须能确认 seek 已到目标，否则 seekTarget 会永久卡住。
      if (!isSeeking() || hasReachedSeekTarget(event.data.position)) {
        status.position = playback.setCurrentTime(event.data.position);
      }
      status.duration = event.data.duration;
      status.volume = event.data.volume;
      if (event.data.speed != null) {
        status.speed = event.data.speed;
        playback.setSpeed(event.data.speed);
      }
      playback.setDuration(event.data.duration);
      playback.setPlaying(event.data.state === "playing");
      // headless 自动连播：服务端接力切曲后，按 current_source 采纳队列曲目
      if (event.data.currentSource && playerClient.supportsServerAutoAdvance) {
        adoptServerAdvancedTrack(event.data.currentSource);
      }
      break;
    case "seek":
      markSeek(event.data.position);
      break;
    case "position": {
      // 歌曲加载中不更新进度
      if (status.trackLoading) break;
      // seek 后丢弃旧位置，直到后端推送的位置到达 seek 目标附近
      if (!hasReachedSeekTarget(event.data.position)) break;
      const adjusted = playback.setCurrentTime(event.data.position);
      status.position = adjusted;
      if (event.data.duration > 0) {
        status.duration = event.data.duration;
        playback.setDuration(event.data.duration);
      }
      // 歌词索引叠加用户设置的偏移；进度条仍走 adjusted 不受影响
      useMediaStore().updateLyricIndex(adjusted + status.lyricOffsetMs);
      // AB 循环：到达 B 点 seek 回 A
      abLoop.checkLoop(adjusted);
      // 推进延时缓存调度
      cacheScheduler.tick(adjusted);
      // Diretta Source Direct 无缝 stage 检查（内部自节流）
      maybeStageDirectNext();
      // headless 自动连播：注册下一曲候选（内部自节流）
      maybeRegisterNextCandidate();
      const track = useMediaStore().track;
      // 已 stage 无缝下一曲时交给引擎 boundary 事件推进，避免此处提前完整重载
      if (
        track?.cueEndMs != null &&
        status.isPlaying &&
        status.duration > 0 &&
        !hasStagedDirectNext()
      ) {
        if (adjusted >= status.duration - 250) await finishCurrentTrack();
      }
      break;
    }
    case "fftData":
      playback.setFftFrame(event.data.ldata, event.data.rdata);
      break;
    case "ended": {
      await finishCurrentTrack();
      break;
    }
    case "directTrackBoundary": {
      // 引擎已在音频回调内零间隙切入下一曲，前端推进 queue/media 并 commit
      await advanceGaplessBoundary(event.data.duration, event.data.generation);
      break;
    }
    case "sourceError":
      // 音源失效（网络中断 / URL 过期）
      await recoverFromSourceFailure();
      break;
    case "serverAutoAdvanceFailed":
      // 服务端曲终接力失败（候选 URL 失效等）：浏览器在场时由前端接管推进
      if (playerClient.supportsServerAutoAdvance && !status.fmMode) {
        console.warn(
          "[player] 服务端自动连播失败，前端接管切歌",
          event.data.source,
          event.data.error,
        );
        await nextTrack();
      }
      break;
    case "play":
      await play();
      break;
    case "playTrack":
      await playNow(event.data.track);
      break;
    case "pause":
      await pause();
      break;
    case "next":
      await nextTrack();
      break;
    case "prev":
      await prevTrack();
      break;
    case "setShuffle":
      setShuffleMode(event.data.mode);
      break;
    case "setRepeat":
      setRepeatMode(event.data.mode);
      break;
    case "addToQueue":
      insertManyToQueue(event.data.tracks, event.data.position);
      break;
    case "toggleLike":
      await useFavorite().toggle(useMediaStore().track);
      break;
    case "deviceChanged": {
      refreshDevices();
      break;
    }
  }
};
