import type { Track } from "@shared/types/player";
import type { CoverItem } from "@/types/artist";
import type { SearchPlatform } from "@shared/types/platform";
import * as local from "./local";
import * as streaming from "./streaming";
import * as netease from "./netease";
import * as qqmusic from "./qqmusic";
import * as kugou from "./kugou";
import * as qobuz from "./qobuz";
import * as tidal from "./tidal";

/** 搜索结果通用 */
export interface SearchResult<T> {
  items: T[];
  total: number;
  hasMore: boolean;
}

const unsupported = (platform: SearchPlatform, category: string): never => {
  throw new Error(`Search not yet supported: ${platform}.${category}`);
};

/** 搜索单曲 */
export const searchSongs = (
  platform: SearchPlatform,
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<Track>> => {
  if (platform === "local") return local.songs(keyword, offset, limit);
  if (platform === "streaming") return streaming.songs(keyword, offset, limit);
  if (platform === "netease") return netease.songs(keyword, offset, limit);
  if (platform === "qqmusic") return qqmusic.songs(keyword, offset, limit);
  if (platform === "kugou") return kugou.songs(keyword, offset, limit);
  if (platform === "qobuz") return qobuz.songs(keyword, offset, limit);
  if (platform === "tidal") return tidal.songs(keyword, offset, limit);
  return unsupported(platform, "songs");
};

/** 搜索专辑 */
export const searchAlbums = (
  platform: SearchPlatform,
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<CoverItem>> => {
  if (platform === "local") return local.albums(keyword, offset, limit);
  if (platform === "streaming") return streaming.albums(keyword, offset, limit);
  if (platform === "netease") return netease.albums(keyword, offset, limit);
  if (platform === "qqmusic") return qqmusic.albums(keyword, offset, limit);
  if (platform === "kugou") return kugou.albums(keyword, offset, limit);
  if (platform === "qobuz") return qobuz.albums(keyword, offset, limit);
  if (platform === "tidal") return tidal.albums(keyword, offset, limit);
  return unsupported(platform, "albums");
};

/** 搜索歌手 */
export const searchArtists = (
  platform: SearchPlatform,
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<CoverItem>> => {
  if (platform === "local") return local.artists(keyword, offset, limit);
  if (platform === "streaming") return streaming.artists(keyword, offset, limit);
  if (platform === "netease") return netease.artists(keyword, offset, limit);
  if (platform === "qqmusic") return qqmusic.artists(keyword, offset, limit);
  if (platform === "kugou") return kugou.artists(keyword, offset, limit);
  if (platform === "qobuz") return qobuz.artists(keyword, offset, limit);
  if (platform === "tidal") return tidal.artists(keyword, offset, limit);
  return unsupported(platform, "artists");
};

/** 搜索歌单 */
export const searchPlaylists = (
  platform: SearchPlatform,
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<CoverItem>> => {
  if (platform === "local") return local.playlists(keyword, offset, limit);
  if (platform === "streaming") return streaming.playlists(keyword, offset, limit);
  if (platform === "netease") return netease.playlists(keyword, offset, limit);
  if (platform === "qqmusic") return qqmusic.playlists(keyword, offset, limit);
  if (platform === "kugou") return kugou.playlists(keyword, offset, limit);
  if (platform === "qobuz") return qobuz.playlists(keyword, offset, limit);
  if (platform === "tidal") return tidal.playlists(keyword, offset, limit);
  return unsupported(platform, "playlists");
};
