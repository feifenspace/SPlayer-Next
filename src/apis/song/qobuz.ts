import type { Track } from "@shared/types/player";
import { ErrorCode } from "@shared/types/errors";
import type { QualityLevel } from "@/utils/quality";
import { qobuzCall } from "@/apis/qobuz";

export type QobuzPlayUrlResult =
  | { available: true; url: string; isTrial: boolean }
  | { available: false; errorCode: ErrorCode };

interface QobuzSongUrlResponse {
  code: number;
  data?: {
    url: string;
    format_id: number;
    mime_type: string;
    sampling_rate?: number;
    bit_depth?: number;
  };
}

/**
 * 解析 Qobuz 单曲的播放 URL
 * @param track - 待解析的 Track
 * @param songLevel - 音质偏好
 */
export const resolveQobuzUrl = async (
  track: Track,
  songLevel: QualityLevel,
): Promise<QobuzPlayUrlResult> => {
  try {
    const res = await qobuzCall<QobuzSongUrlResponse>("track_getFileUrl", {
      track_id: track.id,
      level: songLevel,
    });

    const directUrl = res?.data?.url ?? (res as any)?.url;
    if (directUrl) {
      return {
        available: true,
        url: directUrl,
        isTrial: false,
      };
    }

    return {
      available: false,
      errorCode: ErrorCode.URL_RESOLVE_FAILED,
    };
  } catch (err) {
    console.warn("[qobuz] resolve URL failed:", err);
    return {
      available: false,
      errorCode: ErrorCode.URL_RESOLVE_FAILED,
    };
  }
};
