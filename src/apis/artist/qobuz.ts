/**
 * Qobuz 歌手详情（基本资料 + 专辑列表）
 *
 * [FIX] Qobuz API 不存在 artist/getAlbums 和 artist/getTracks 端点。
 *       正确做法：
 *       - artist/get + extra=albums 一次拿到歌手资料 + 专辑列表（含封面）
 *       - 热门曲目无专用端点，用 catalog/search 按歌手名搜 tracks 获取
 *       - 如需更多专辑，用 artist/getReleasesList 分页（extra=albums 最多 100 条已够）
 */

import type { Album, Artist, Track } from "@shared/types/player";
import type { CoverItem } from "@/types/artist";
import { qobuzCall } from "@/apis/qobuz";
import {
  songsToTracks,
  toAlbum,
  toArtist,
  playlistToCover,
  type QobuzAlbum,
  type QobuzArtist,
  type QobuzImage,
  type QobuzPlaylist,
  type QobuzTrack,
} from "@/utils/format/qobuz";

interface QobuzArtistResponse extends QobuzArtist {
  /** artist/get + extra=albums 时返回的专辑列表 */
  albums?: { items?: QobuzAlbum[]; total?: number };
  /** artist/get + extra=playlists 时返回的歌单列表（Qobuz 直接返回数组，非 {items}） */
  playlists?: QobuzPlaylist[];
}

interface QobuzSearchResponse {
  tracks?: { items?: QobuzTrack[]; total?: number };
}

/**
 * 用专辑列表的封面补全 track.album 的封面
 *
 * Qobuz catalog/search 返回的 track.album 可能是空 {} 或缺少 image，
 * 导致全屏播放器无法显示封面。
 *
 * 补全策略：
 * 1. track.album 有 id → 按 id 从 albums 列表查找
 * 2. track.album 无 id（空 {}）→ 按 track.performer.name 从 albums 中
 *    匹配同名歌手的第一个有封面的专辑
 */
const enrichTracksWithAlbumCover = (
  tracks: QobuzTrack[],
  albums: QobuzAlbum[],
): QobuzTrack[] => {
  const albumMap = new Map<string, QobuzAlbum>();
  for (const a of albums) {
    if (a?.id != null) albumMap.set(String(a.id), a);
  }
  // 预建歌手名 → 首个有封面的专辑 映射（用于 album 为空 {} 的 fallback）
  const artistAlbumFallback = new Map<string, QobuzAlbum>();
  for (const a of albums) {
    if (!a.image) continue;
    const name = a.artist?.name?.trim().toLowerCase();
    if (name && !artistAlbumFallback.has(name)) artistAlbumFallback.set(name, a);
  }
  return tracks.map((t) => {
    const albumId = t.album?.id;
    // 策略 1：按 album id 匹配
    if (albumId != null) {
      const matched = albumMap.get(String(albumId));
      // track.album 已有 image 则不覆盖
      if (t.album?.image?.large || t.album?.image?.small) return t;
      if (!matched?.image) return t;
      const image: QobuzImage = matched.image;
      return {
        ...t,
        album: {
          ...t.album,
          id: matched.id,
          title: matched.title,
          image,
          artist: matched.artist,
        },
      };
    }
    // 策略 2：album 为空 {} — 按 performer name 找同名歌手专辑的封面
    const performerName = t.performer?.name?.trim().toLowerCase();
    if (performerName) {
      const fallback = artistAlbumFallback.get(performerName);
      if (fallback?.image) {
        const image: QobuzImage = fallback.image;
        return {
          ...t,
          album: {
            id: fallback.id,
            title: fallback.title,
            image,
            artist: fallback.artist,
          },
        };
      }
    }
    return t;
  });
};

/**
 * 拉取歌手：基本资料 + 专辑列表 + 歌单列表 + 热门曲目
 *
 * 实现说明：
 * 1. artist/get + extra=albums,playlists 一次请求拿到歌手资料 + 专辑列表 + 歌单列表（封面完整）
 * 2. catalog/search 用歌手名搜 tracks 作为"热门曲目"（Qobuz 无 artist/getTracks）
 * 3. 专辑 / 歌单抓取失败均不阻塞其它数据
 *
 * @param artistId 歌手 id
 */
export const fetchArtist = async (
  artistId: string,
): Promise<{ artist: Artist; tracks: Track[]; albums: Album[]; playlists: CoverItem[] } | null> => {
  // 先拿歌手资料 + 专辑列表 + 歌单列表（extra=albums,playlists）
  const profile = await qobuzCall<QobuzArtistResponse>("artist_get", {
    artist_id: artistId,
    extra: "albums,playlists",
    limit: 100,
  });
  if (!profile?.id) return null;

  const artist = toArtist(profile);
  const albums = (profile.albums?.items ?? []).map(toAlbum);
  const playlists = (profile.playlists ?? []).map(playlistToCover);

  // 热门曲目：用歌手名搜 tracks（Qobuz 无专用端点）
  let tracks: Track[] = [];
  if (artist.name) {
    try {
      const searchResp = await qobuzCall<QobuzSearchResponse>("catalog_search", {
        query: artist.name,
        type: "tracks",
        limit: 30,
      });
      const rawTracks = searchResp?.tracks?.items ?? [];
      tracks = songsToTracks(enrichTracksWithAlbumCover(rawTracks, profile.albums?.items ?? []));
    } catch {
      // 搜索失败不阻塞，保留空曲目列表
    }
  }

  return { artist, tracks, albums, playlists };
};

/**
 * 收藏 / 取消收藏歌手
 * @param id 歌手 id
 * @param subscribe true 收藏，false 取消
 */
export const subscribeArtist = async (id: string, subscribe: boolean): Promise<void> => {
  if (subscribe) {
    await qobuzCall("favorite_create", { id, type: "artist" });
  } else {
    await qobuzCall("favorite_delete", { id, type: "artist" });
  }
};
