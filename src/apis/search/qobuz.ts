/**
 * Qobuz 搜索（tracks / albums / artists / playlists）
 *
 * 调用 window.api.apis.call("qobuz", "catalog_search", ...) 并转换响应。
 *
 * 响应结构（catalog/search）：
 * {
 *   tracks:   { items: QobuzTrack[], total: number },
 *   albums:   { items: QobuzAlbum[], total: number },
 *   artists:  { items: QobuzArtist[], total: number },
 *   playlists:{ items: QobuzPlaylist[], total: number }
 * }
 */

import type { Track } from "@shared/types/player";
import type { CoverItem } from "@/types/artist";
import { qobuzCall } from "@/apis/qobuz";
import {
  songsToTracks,
  albumToCover,
  artistToCover,
  playlistToCover,
  type QobuzTrack,
  type QobuzAlbum,
  type QobuzArtist,
  type QobuzPlaylist,
} from "@/utils/format/qobuz";
import type { SearchResult } from "./index";

interface QobuzSearchResponse {
  tracks?: { items?: QobuzTrack[]; total?: number };
  albums?: { items?: QobuzAlbum[]; total?: number };
  artists?: { items?: QobuzArtist[]; total?: number };
  playlists?: { items?: QobuzPlaylist[]; total?: number };
}

const call = (
  type: "tracks" | "albums" | "artists" | "playlists",
  keyword: string,
  offset: number,
  limit: number,
): Promise<QobuzSearchResponse> =>
  qobuzCall<QobuzSearchResponse>("catalog_search", {
    query: keyword,
    type,
    limit,
    offset,
  });

export const songs = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<Track>> => {
  const body = await call("tracks", keyword, offset, limit);
  const items = songsToTracks(body?.tracks?.items);
  const total = body?.tracks?.total ?? items.length;
  return { items, total, hasMore: offset + items.length < total };
};

export const albums = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<CoverItem>> => {
  const body = await call("albums", keyword, offset, limit);
  const items = (body?.albums?.items ?? []).map(albumToCover);
  const total = body?.albums?.total ?? items.length;
  return { items, total, hasMore: offset + items.length < total };
};

export const artists = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<CoverItem>> => {
  const body = await call("artists", keyword, offset, limit);
  const items = (body?.artists?.items ?? []).map(artistToCover);
  const total = body?.artists?.total ?? items.length;
  return { items, total, hasMore: offset + items.length < total };
};

export const playlists = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<CoverItem>> => {
  const body = await call("playlists", keyword, offset, limit);
  const items = (body?.playlists?.items ?? []).map(playlistToCover);
  const total = body?.playlists?.total ?? items.length;
  return { items, total, hasMore: offset + items.length < total };
};
