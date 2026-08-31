/**
 * TIDAL 搜索（tracks / albums / artists / playlists）
 *
 * 调用 window.api.apis.call("tidal", "search", ...) 并转换响应。
 *
 * 响应结构（search 后端统一定义）：
 * {
 *   status: "success",
 *   tracks:    { items: TidalTrack[] },
 *   albums:    { items: TidalAlbum[] },
 *   artists:   { items: TidalArtist[] },
 *   playlists: { items: TidalPlaylist[] }
 * }
 *
 * 注意：TIDAL 搜索 type 是大写（TRACKS / ALBUMS / ARTISTS / PLAYLISTS），
 *      与 Qobuz 小写不同。
 */

import type { Track } from "@shared/types/player";
import type { CoverItem } from "@/types/artist";
import { tidalCall } from "@/apis/tidal";
import {
  songsToTracks,
  albumToCover,
  artistToCover,
  playlistToCover,
  type TidalTrack,
  type TidalAlbum,
  type TidalArtist,
  type TidalPlaylist,
} from "@/utils/format/tidal";
import type { SearchResult } from "./index";

interface TidalSearchResponse {
  tracks?: { items?: TidalTrack[] };
  albums?: { items?: TidalAlbum[] };
  artists?: { items?: TidalArtist[] };
  playlists?: { items?: TidalPlaylist[] };
}

const call = (
  type: "TRACKS" | "ALBUMS" | "ARTISTS" | "PLAYLISTS",
  keyword: string,
  offset: number,
  limit: number,
): Promise<TidalSearchResponse> =>
  tidalCall<TidalSearchResponse>("search", {
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
  const body = await call("TRACKS", keyword, offset, limit);
  const items = songsToTracks(body?.tracks?.items);
  // TIDAL 搜索后端不返回 total，使用 items.length + offset 判断
  const total = items.length + offset;
  return { items, total, hasMore: items.length === limit };
};

export const albums = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<CoverItem>> => {
  const body = await call("ALBUMS", keyword, offset, limit);
  const items = (body?.albums?.items ?? []).map(albumToCover);
  const total = items.length + offset;
  return { items, total, hasMore: items.length === limit };
};

export const artists = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<CoverItem>> => {
  const body = await call("ARTISTS", keyword, offset, limit);
  const items = (body?.artists?.items ?? []).map(artistToCover);
  const total = items.length + offset;
  return { items, total, hasMore: items.length === limit };
};

export const playlists = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<CoverItem>> => {
  const body = await call("PLAYLISTS", keyword, offset, limit);
  const items = (body?.playlists?.items ?? []).map(playlistToCover);
  const total = items.length + offset;
  return { items, total, hasMore: items.length === limit };
};
