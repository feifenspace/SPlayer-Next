/**
 * Qobuz 专辑详情（含曲目列表）
 *
 * 参照 src/apis/album/netease.ts 的模式。
 *
 * [FIX] Qobuz album/get 返回的 track 中 album 字段为空对象 {}，
 *       没有 image 封面。必须把专辑的 image 注入到每个 track.album 中，
 *       否则曲目列表无法显示封面。
 */

import type { Album, Track } from "@shared/types/player";
import { qobuzCall } from "@/apis/qobuz";
import {
  songsToTracks,
  toAlbum,
  type QobuzAlbum,
  type QobuzTrack,
  type QobuzImage,
} from "@/utils/format/qobuz";

/** album/get 响应（QobuzAlbum 已含 tracks.items 字段） */
type QobuzAlbumResponse = QobuzAlbum;

/**
 * 拉取专辑：元数据 + 全部曲目
 * @param albumId 专辑 id
 */
export const fetchAlbum = async (
  albumId: string,
): Promise<{ album: Album; tracks: Track[]; description?: string } | null> => {
  const data = await qobuzCall<QobuzAlbumResponse>("album_get", { album_id: albumId });
  if (!data?.id) return null;

  // [FIX] 将专辑级 image 注入到每个 track.album 中
  // Qobuz album/get 返回的 track.album 字段是空 {}，没有封面信息
  const rawTracks = (data?.tracks?.items ?? []) as QobuzTrack[];
  const albumImage: QobuzImage | undefined = data.image;
  const enrichedTracks: QobuzTrack[] = rawTracks.map((t) => ({
    ...t,
    album: {
      id: data.id,
      title: data.title,
      image: albumImage,
      artist: data.artist,
    },
  }));
  return {
    album: toAlbum(data),
    tracks: songsToTracks(enrichedTracks),
    description: data.description,
  };
};

/**
 * 收藏 / 取消收藏专辑
 * @param id 专辑 id
 * @param subscribe true 收藏 / false 取消
 */
export const subscribeAlbum = async (id: string, subscribe: boolean): Promise<void> => {
  if (subscribe) {
    await qobuzCall("favorite_create", { id, type: "album" });
  } else {
    await qobuzCall("favorite_delete", { id, type: "album" });
  }
};
