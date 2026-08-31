/**
 * Qobuz API 渲染端封装
 *
 * 用 Proxy 把所有接口代理到主进程：`qobuz.catalog_search({query})` 实际等于
 * `window.api.apis.call("qobuz", "catalog_search", {query})`
 *
 * 调用约定：成功 → 返回 body；失败 → 抛 Error
 *
 * 参照 src/apis/netease.ts 的封装模式。
 */

import type { ApiCallResponse } from "@shared/types/apis";

/**
 * 调用 Qobuz API，返回原始响应
 * @param name 接口名（对应 electron/main/apis/qobuz/modules/index.ts 中的 key）
 * @param params 接口参数
 * @returns 原始响应（含 status + body）
 */
export const qobuzRaw = async (
  name: string,
  params?: Record<string, unknown>,
): Promise<{ status: number; body: unknown }> => {
  const res: ApiCallResponse = await window.api.apis.call("qobuz", name, params);
  if (!res.ok) throw new Error(res.error);
  return { status: res.status ?? 200, body: res.body ?? res.data };
};

/**
 * 调用 Qobuz API，只返回 body
 * @param name 接口名
 * @param params 接口参数
 */
export const qobuzCall = async <T = any>(
  name: string,
  params?: Record<string, unknown>,
): Promise<T> => {
  const res = await qobuzRaw(name, params);
  return res.body as T;
};

/**
 * 登录 Qobuz（user_id + user_auth_token）
 * @param userId 用户 ID
 * @param userAuthToken 用户认证 token
 */
export const login = (userId: string, userAuthToken: string) =>
  qobuzCall<{ status: string; user?: { id: string; login: string } }>("auth_login", {
    user_id: userId,
    user_auth_token: userAuthToken,
  });

/** 登出 Qobuz */
export const logout = () => qobuzCall("auth_logout");

/** 查询 Qobuz 登录状态 */
export const authStatus = () =>
  qobuzCall<{ status: string; loggedIn: boolean; username?: string; userId?: string }>(
    "auth_status",
  );

/**
 * 搜索（tracks / albums / artists / playlists）
 * @param query 关键词
 * @param type 搜索类型
 * @param limit 返回数量
 * @param offset 偏移量
 */
export const catalogSearch = <T = any>(
  query: string,
  type: "tracks" | "albums" | "artists" | "playlists",
  limit = 20,
  offset = 0,
) =>
  qobuzRaw("catalog_search", { query, type, limit, offset }).then(
    (r) => r.body as T,
  );

/** 专辑详情（含曲目） */
export const albumGet = (albumId: string) =>
  qobuzCall("album_get", { album_id: albumId });

/** 歌单详情（含曲目） */
export const playlistGet = (playlistId: string) =>
  qobuzCall("playlist_get", { playlist_id: playlistId });

/** 歌手详情（含专辑列表，一次请求） */
export const artistGet = (artistId: string, extra = "albums", limit = 100, offset = 0) =>
  qobuzCall("artist_get", { artist_id: artistId, extra, limit, offset });

/** 歌手专辑列表（分页） */
export const artistGetReleasesList = (
  artistId: string,
  limit = 50,
  offset = 0,
  type: "albums" | "tracks" | "playlists" = "albums",
) =>
  qobuzCall("artist_getReleasesList", {
    artist_id: artistId,
    type,
    limit,
    offset,
  });

/** 用户收藏 */
export const userGetFavorites = (type: "tracks" | "albums" | "artists", limit = 50, offset = 0) =>
  qobuzCall("user_getFavorites", { type, limit, offset });

/** 添加收藏 */
export const favoriteCreate = (id: string, type: "track" | "album" | "artist") =>
  qobuzCall("favorite_create", { id, type });

/** 取消收藏 */
export const favoriteDelete = (id: string, type: "track" | "album" | "artist") =>
  qobuzCall("favorite_delete", { id, type });
