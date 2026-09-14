import { playerClient } from "@/services/client";
import { useMediaStore } from "@/stores/media";

const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * 自动切歌后同步真实音质。服务端切换 now-playing 与前端边界事件不是同一
 * 个时刻，必须等待并校验 track_id，不能把上一曲的格式写到当前曲。
 */
export const syncNowPlayingQuality = async (trackId: string): Promise<void> => {
  const maxAttempts = 8;
  for (let attempt = 0; attempt < maxAttempts; attempt += 1) {
    try {
      const media = useMediaStore();
      if (media.track?.id !== trackId) return;
      const response = await playerClient.getNowPlaying();
      const data = response.success ? (response.data as any) : null;
      const metadata = data?.metadata;
      const reportedTrackId = metadata?.track_id ?? data?.track_id;
      if (!metadata || reportedTrackId !== trackId) {
        if (attempt + 1 < maxAttempts) await sleep(80 + attempt * 40);
        continue;
      }
      const sampleRate = metadata.original_sample_rate || metadata.sample_rate || 0;
      if (!sampleRate && !metadata.codec) {
        if (attempt + 1 < maxAttempts) await sleep(80 + attempt * 40);
        continue;
      }
      if (useMediaStore().track?.id !== trackId) return;
      useMediaStore().setPlaybackQuality({
        sampleRate,
        channels: metadata.channels || 0,
        bitsPerSample: metadata.bits_per_sample || 0,
        bitRate: metadata.bit_rate || 0,
        codec: metadata.codec || "unknown",
      });
      return;
    } catch (error) {
      if (attempt + 1 >= maxAttempts) {
        console.debug("[player] sync now-playing quality failed", error);
        return;
      }
      await sleep(80 + attempt * 40);
    }
  }
};
