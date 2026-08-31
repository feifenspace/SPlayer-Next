/**
 * TIDAL API 渲染端封装
 *
 * 用 Proxy 把所有接口代理到主进程：`tidalRaw("search", {query})` 实际等于
 * `window.api.apis.call("tidal", "search", {query})`
 *
 * 调用约定：成功 → 返回 body；失败 → 抛 Error
 *
 * 参照 src/apis/qobuz.ts 的封装模式。TIDAL 后端模块注册表见
 * electron/main/apis/tidal/modules/index.ts。
 */

import type { ApiCallResponse } from "@shared/types/apis";

/**
 * 调用 TIDAL API，返回原始响应
 * @param name 接口名（对应 electron/main/apis/tidal/modules/index.ts 中的 key）
 * @param params 接口参数
 * @returns 原始响应（含 status + body）
 */
export const tidalRaw = async (
  name: string,
  params?: Record<string, unknown>,
): Promise<{ status: number; body: unknown }> => {
  const res: ApiCallResponse = await window.api.apis.call("tidal", name, params);
  if (!res.ok) throw new Error(res.error);
  return { status: res.status ?? 200, body: res.body ?? res.data };
};

/**
 * 调用 TIDAL API，只返回 body
 * @param name 接口名
 * @param params 接口参数
 */
export const tidalCall = async <T = any>(
  name: string,
  params?: Record<string, unknown>,
): Promise<T> => {
  const res = await tidalRaw(name, params);
  return res.body as T;
};

// ============ 认证 ============

/** 设备码授权响应（auth_device_authorization） */
export interface TidalDeviceAuthorizationResponse {
  status: string;
  deviceCode: string;
  userCode: string;
  verificationUri: string;
  verificationUriComplete: string;
  interval: number;
  expiresIn: number;
}

/**
 * 第一步：发起设备码授权，获取 userCode + verificationUri
 * @param clientId 可选（用户自定义 Client ID；未传则用后端默认值）
 * @param clientSecret 可选（用户自定义 Client Secret）
 */
export const authDeviceAuthorization = (clientId?: string, clientSecret?: string) =>
  tidalCall<TidalDeviceAuthorizationResponse>("auth_device_authorization", {
    client_id: clientId,
    client_secret: clientSecret,
  });

/** 设备码轮询响应（auth_token_poll） */
export interface TidalTokenPollResponse {
  status: "success" | "pending" | "expired" | "denied" | "timeout" | "error";
  username?: string;
  countryCode?: string;
  expiresAt?: number;
  message?: string;
}

/**
 * 第二步：轮询设备码 token
 * @param deviceCode auth_device_authorization 返回的 deviceCode
 * @param clientId 可选
 * @param clientSecret 可选
 */
export const authTokenPoll = (deviceCode: string, clientId?: string, clientSecret?: string) =>
  tidalCall<TidalTokenPollResponse>("auth_token_poll", {
    device_code: deviceCode,
    client_id: clientId,
    client_secret: clientSecret,
  });

/** 刷新 token */
export const authTokenRefresh = () => tidalCall("auth_token_refresh");

/** 登出 TIDAL */
export const logout = () => tidalCall("auth_logout");

/** 查询 TIDAL 登录状态 */
export const authStatus = () =>
  tidalCall<{
    status: string;
    loggedIn: boolean;
    userId?: string;
    username?: string;
    countryCode?: string;
    clientId?: string;
    expiresAt?: number;
  }>("auth_status");


/** 授权码登录响应（auth_authorize） */
export interface TidalAuthorizeResponse {
  status: string;
  url: string;
  state: string;
  redirectUri: string;
}

/**
 * 发起授权码登录（官方移动客户端，支持标准 LOSSLESS）
 * @param redirectBase 回调基础地址（前端传 window.location.origin）
 * @param clientId 可选（覆盖默认官方 client）
 */
export const authAuthorize = (redirectBase?: string, clientId?: string) =>
  tidalCall<TidalAuthorizeResponse>("auth_authorize", {
    redirect_base: redirectBase,
    client_id: clientId,
  });

/**
 * 授权码交换（手动回填回调 URL 中的 code+state 完成 token 交换）
 * @param code 回调 URL 中的 code（必填）
 * @param state 回调 URL 中的 state（可选，与发起授权时服务端 pending 会话对应）
 */
export const authExchange = (code: string, state?: string) =>
  tidalCall("auth_exchange", { code, state });

// ============ 搜索 ============

/**
 * 搜索（tracks / albums / artists / playlists）
 * @param query 关键词
 * @param type 搜索类型（注意：TIDAL 大写，与 Qobuz 略不同）
 * @param limit 返回数量（最大 50）
 * @param offset 偏移量
 */
export const search = <T = any>(
  query: string,
  type: "TRACKS" | "ALBUMS" | "ARTISTS" | "PLAYLISTS",
  limit = 20,
  offset = 0,
) =>
  tidalCall<T>("search", { query, type, limit, offset });

// ============ 专辑 ============

/** 专辑详情 */
export const albumGet = (albumId: string) =>
  tidalCall("album_get", { album_id: albumId });

/** 专辑曲目列表 */
export const albumGetTracks = (albumId: string, limit = 50, offset = 0) =>
  tidalCall("album_getTracks", { album_id: albumId, limit, offset });

// ============ 歌单 ============

/** 歌单详情 */
export const playlistGet = (playlistId: string) =>
  tidalCall("playlist_get", { playlist_id: playlistId });

// ============ 艺术家 ============

/** 艺术家详情 */
export const artistGet = (artistId: string) =>
  tidalCall("artist_get", { artist_id: artistId });

/** 艺术家专辑列表 */
export const artistGetAlbums = (artistId: string, limit = 50, offset = 0) =>
  tidalCall("artist_getAlbums", { artist_id: artistId, limit, offset });

/** 艺术家热门曲目 */
export const artistGetTopTracks = (artistId: string, limit = 50, offset = 0) =>
  tidalCall("artist_getTopTracks", { artist_id: artistId, limit, offset });

// ============ 曲目 / 流 URL ============

/** 曲目详情 */
export const trackGet = (trackId: string) =>
  tidalCall("track_get", { track_id: trackId });

/** 获取曲目流 URL（含 DASH manifest） */
export const trackGetStreamUrl = (trackId: string, quality?: string) =>
  tidalCall("track_getStreamUrl", { track_id: trackId, quality });

// ============ 收藏 ============

/** 用户收藏 */
export const userGetFavorites = (
  type: "tracks" | "albums" | "artists" | "playlists",
  limit = 50,
  offset = 0,
) => tidalCall("user_getFavorites", { type, limit, offset });
