import type { Track } from "@shared/types/player";
import { ErrorCode } from "@shared/types/errors";
import type { QualityLevel } from "@/utils/quality";
import { qqmusicCall } from "@/apis/qqmusic";

export type QQMusicPlayUrlResult =
  | {
      available: true;
      url: string;
      isTrial: boolean;
      /** 实际命中的音质档位（主进程 song_url 返回的 level） */
      actualLevel?: string;
      /** 实际档位低于请求档位时为 true（QQ 静默降级） */
      isFallback?: boolean;
    }
  | { available: false; errorCode: ErrorCode };

interface SongUrlData {
  id: string;
  url: string;
  level?: string;
  format?: string;
  isFallback?: boolean;
}

interface SongUrlResponse {
  code: number;
  message?: string;
  data?: SongUrlData[];
}

/**
 * 解析 QM 单曲的播放 URL
 * @param track - 待解析的 Track（track.id 为 songmid）
 * @param songLevel - 音质偏好
 */
export const resolveQQMusicUrl = async (
  track: Track,
  songLevel: QualityLevel,
): Promise<QQMusicPlayUrlResult> => {
  try {
    const body = await qqmusicCall<SongUrlResponse>("song_url", {
      mid: track.id,
      mediaMid: track.mediaId,
      level: songLevel,
    });

    const item = body?.data?.[0];
    if (item?.url) {
      if (item.isFallback) {
        // QQ vkey 对首选档位返回空 purl 时会静默滑落到低档文件。
        // 此处显式记录，供调用方决定是否缓存以及 UI 提示。
        console.warn(
          `[qqmusic] 音质降级: ${track.id} 请求 ${songLevel}，实际命中 ${item.level ?? "unknown"} (${item.format ?? "?"})`,
        );
      }
      return {
        available: true,
        url: item.url,
        isTrial: false,
        actualLevel: item.level,
        isFallback: item.isFallback === true,
      };
    }

    return {
      available: false,
      errorCode: ErrorCode.URL_RESOLVE_FAILED,
    };
  } catch (err) {
    console.warn("[qqmusic] resolve URL failed:", err);
    return {
      available: false,
      errorCode: ErrorCode.URL_RESOLVE_FAILED,
    };
  }
};
