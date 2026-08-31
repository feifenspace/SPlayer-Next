/**
 * TIDAL 歌单详情（含曲目列表）
 *
 * 参照 MemoryPlay L10001-10048：
 * TIDAL playlist_get 只返回元数据，曲目列表需单独调用 playlist_getTracks。
 * 两者并行请求后合并。
 */

import type { Playlist, Track } from "@shared/types/player";
import { tidalCall } from "@/apis/tidal";
import {
  songsToTracks,
  toPlaylist,
  type TidalPlaylist,
  type TidalTrack,
} from "@/utils/format/tidal";

interface TidalPlaylistTracksResponse {
  items?: TidalTrack[];
  totalNumberOfItems?: number;
}

/**
 * 拉取歌单：元数据 + 全部曲目
 * 参照 MemoryPlay: Promise.all([tidalGetPlaylist(id), tidalGetPlaylistTracks(id)])
 * @param playlistId 歌单 id
 */
export const fetchPlaylist = async (
  playlistId: string,
): Promise<{ playlist: Playlist; tracks: Track[] } | null> => {
  const [data, tracksData] = await Promise.all([
    tidalCall<TidalPlaylist>("playlist_get", { playlist_id: playlistId }),
    tidalCall<TidalPlaylistTracksResponse>("playlist_getTracks", { playlist_id: playlistId, limit: 100 }),
  ]);
  if (!data?.uuid) return null;
  return {
    playlist: toPlaylist(data),
    tracks: songsToTracks(tracksData?.items),
  };
};
