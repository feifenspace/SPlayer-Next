import { store } from "@main/store";
import { listenbrainzLog } from "@main/utils/logger";
import * as client from "./client";
import * as credentials from "./credentials";
import * as scrobbler from "./scrobbler";
import type { ListenBrainzStatus } from "@shared/types/listenbrainz";

let session: credentials.ListenBrainzCredentials | null = null;
let lastError: string | null = null;

const cfg = () => store.get("listenbrainz");

export const init = (): void => {
  session = credentials.load();
  if (session) {
    listenbrainzLog.info(`已载入 ListenBrainz 凭证: ${session.account}`);
  }

  scrobbler.setHandlers({
    onNowPlaying: (track) => {
      const config = cfg();
      if (!config.enabled || !config.sendNowPlaying || !session) return;
      client
        .submitNowPlaying(session.token, track)
        .catch((err) => {
          lastError = String(err);
          listenbrainzLog.warn("ListenBrainz submitNowPlaying 失败:", err);
        });
    },
    onScrobble: (track) => {
      const config = cfg();
      if (!config.enabled || !session) return;
      client
        .submitListen(session.token, track)
        .then(() => {
          lastError = null;
          listenbrainzLog.debug(`ListenBrainz scrobble: ${track.artistName} - ${track.trackName}`);
        })
        .catch((err) => {
          lastError = String(err);
          listenbrainzLog.warn("ListenBrainz scrobble 失败:", err);
        });
    },
  });
};

export const reloadConfig = (): void => {
  if (!cfg().enabled) {
    scrobbler.reset();
  }
};

export const getStatus = (): ListenBrainzStatus => {
  const config = cfg();
  const linked = !!session;
  return {
    enabled: config.enabled,
    sendNowPlaying: config.sendNowPlaying,
    linked,
    account: session?.account ?? null,
    state: !config.enabled
      ? "disabled"
      : !linked
        ? "unconfigured"
        : lastError
          ? "error"
          : "ready",
    pending: 0,
    dead: 0,
    lastError,
    processActive: true,
  };
};

export const link = async (
  token: string,
): Promise<{ ok: boolean; account?: string; error?: string }> => {
  const trimmed = token.trim();
  if (!trimmed) {
    return { ok: false, error: "Token 不能为空" };
  }
  try {
    const res = await client.validateToken(trimmed);
    if (!res.valid || !res.user_name) {
      return { ok: false, error: res.message || "无效的 ListenBrainz Token" };
    }
    session = { account: res.user_name, token: trimmed };
    credentials.save(session);
    lastError = null;
    listenbrainzLog.info(`已连接 ListenBrainz: ${res.user_name}`);
    return { ok: true, account: res.user_name };
  } catch (err: any) {
    lastError = err?.message || String(err);
    listenbrainzLog.error("ListenBrainz 连接失败:", err);
    return { ok: false, error: err?.message || "网络请求失败" };
  }
};

export const unlink = (): void => {
  session = null;
  lastError = null;
  credentials.clear();
  scrobbler.reset();
  listenbrainzLog.info("已断开 ListenBrainz 连接");
};

export const onTrackLoaded = scrobbler.onTrackLoaded;
export const onState = scrobbler.onState;
export const onPosition = scrobbler.onPosition;
export const onEnded = scrobbler.onEnded;
export const reset = scrobbler.reset;
