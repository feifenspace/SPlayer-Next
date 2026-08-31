/**
 * Qobuz 歌单详情（含曲目列表）
 *
 * 参照 src/apis/playlist/netease.ts 的模式，但 Qobuz playlist/get 一次返回全量曲目，
 * 无需分批 song/detail 补尾。
 */

import type { Playlist, Track } from "@shared/types/player";
import { qobuzCall } from "@/apis/qobuz";
import { songsToTracks, toPlaylist, type QobuzPlaylist, type QobuzTrack, type QobuzImage } from "@/utils/format/qobuz";

interface QobuzPlaylistResponse extends QobuzPlaylist {}

/**
 * 拉取歌单：元数据 + 全部曲目
 * @param playlistId 歌单 id
 */
export const fetchPlaylist = async (
  playlistId: string,
): Promise<{ playlist: Playlist; tracks: Track[] } | null> => {
  const data = await qobuzCall<QobuzPlaylistResponse>("playlist_get", {
    playlist_id: playlistId,
  });
  if (!data?.id) return null;
  // [FIX] Qobuz playlist/get 返回的 track.album 可能是空对象 {},
  // 需要注入歌单级封面到每个 track.album 中（与 album/qobuz.ts 同理）
  const rawTracks = (data?.tracks?.items ?? []) as QobuzTrack[];
  const playlistCover: QobuzImage | undefined = data.image;
  const enrichedTracks: QobuzTrack[] = rawTracks.map((t) => {
    if (t.album?.image?.large) return t;
    return {
      ...t,
      album: {
        id: t.album?.id ?? data.id,
        title: t.album?.title ?? data.name ?? data.title,
        image: playlistCover ?? t.album?.image,
        artist: t.album?.artist,
      },
    };
  });
  return {
    playlist: toPlaylist(data),
    tracks: songsToTracks(enrichedTracks),
  };
};
