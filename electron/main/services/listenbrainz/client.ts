import type { ListenBrainzTrackSnapshot } from "@shared/types/listenbrainz";

const API_ROOT = "https://api.listenbrainz.org/1";
const REQUEST_TIMEOUT_MS = 15_000;

export interface ValidateTokenResult {
  valid: boolean;
  user_name?: string;
  message?: string;
}

/**
 * 校验 ListenBrainz 用户 Token
 */
export const validateToken = async (token: string): Promise<ValidateTokenResult> => {
  const res = await fetch(`${API_ROOT}/validate-token`, {
    headers: {
      Authorization: `Token ${token}`,
    },
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });

  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(`ListenBrainz 校验失败 (${res.status}): ${text}`);
  }

  const data = (await res.json()) as ValidateTokenResult;
  return data;
};

/**
 * 提交正在播放（Now Playing）状态
 */
export const submitNowPlaying = async (
  token: string,
  track: ListenBrainzTrackSnapshot,
): Promise<void> => {
  const payload = {
    listen_type: "playing_now",
    payload: [
      {
        track_metadata: {
          artist_name: track.artistName,
          track_name: track.trackName,
          release_name: track.releaseName || undefined,
          additional_info: {
            duration_ms: track.durationMs,
            tracknumber: track.trackNumber,
            submission_client: "SPlayer-Next-Headless",
          },
        },
      },
    ],
  };

  const res = await fetch(`${API_ROOT}/submit-listens`, {
    method: "POST",
    headers: {
      Authorization: `Token ${token}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify(payload),
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });

  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(`ListenBrainz 正在播放上报失败 (${res.status}): ${text}`);
  }
};

/**
 * 提交单曲听歌记录（Scrobble）
 */
export const submitListen = async (
  token: string,
  track: ListenBrainzTrackSnapshot,
): Promise<void> => {
  const payload = {
    listen_type: "single",
    payload: [
      {
        listened_at: Math.floor(track.listenedAt / 1000),
        track_metadata: {
          artist_name: track.artistName,
          track_name: track.trackName,
          release_name: track.releaseName || undefined,
          additional_info: {
            duration_ms: track.durationMs,
            tracknumber: track.trackNumber,
            submission_client: "SPlayer-Next-Headless",
          },
        },
      },
    ],
  };

  const res = await fetch(`${API_ROOT}/submit-listens`, {
    method: "POST",
    headers: {
      Authorization: `Token ${token}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify(payload),
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });

  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(`ListenBrainz 打卡上报失败 (${res.status}): ${text}`);
  }
};
