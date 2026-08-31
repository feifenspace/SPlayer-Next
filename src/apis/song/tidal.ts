import type { Track } from "@shared/types/player";
import { ErrorCode } from "@shared/types/errors";
import type { QualityLevel } from "@/utils/quality";
import { tidalCall } from "@/apis/tidal";

export type TidalPlayUrlResult =
  | { available: true; url: string; isTrial: boolean }
  | { available: false; errorCode: ErrorCode };

interface TidalSongUrlResponse {
  code: number;
  data?: {
    url: string;
    audioQuality?: string;
    codec?: string;
    bitDepth?: number;
    sampleRate?: number;
  };
}

/**
 * 解析 TIDAL 单曲的播放 URL
 * @param track - 待解析的 Track
 * @param songLevel - 音质偏好
 */
export const resolveTidalUrl = async (
  track: Track,
  songLevel: QualityLevel,
): Promise<TidalPlayUrlResult> => {
  try {
    const res = await tidalCall<TidalSongUrlResponse>("track_getStreamUrl", {
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
    console.warn("[tidal] resolve URL failed:", err);
    return {
      available: false,
      errorCode: ErrorCode.URL_RESOLVE_FAILED,
    };
  }
};
