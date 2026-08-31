/**
 * Qobuz 响应 → 应用层模型 转换工具
 *
 * 参照 src/utils/format/netease.ts 的模式，将 Qobuz API 响应转换为
 * 应用层 Track / Album / Artist / Playlist / CoverItem。
 *
 * Qobuz 响应字段结构（移植自 MemoryPlay memoryplay_controller.js L9810-10100）：
 * - track: { id, title, duration(秒), performer: {id,name}, album: {id,title,image:{large,small,thumbnail}}, maximum_bit_depth, maximum_sampling_rate, isrc }
 * - album: { id, title, artist: {name}, image: {large,small}, tracks_count, release_date }
 * - artist: { id, name, picture }
 * - playlist: { id, name, image: {large,small}, tracks_count, owner: {name} }
 *
 * 时长：Qobuz duration 单位为秒，应用层 Track.duration 单位为毫秒，需 ×1000
 * 封面：直接用 image.large，可通过 deriveCoverUrl 派生不同尺寸
 */

import type { Album, Artist, AudioQuality, Playlist, Track } from "@shared/types/player";
import type { CoverItem } from "@/types/artist";
import { coverSizePair } from "@/utils/streamingSettings";

// 注：Qobuz 封面 URL 派生逻辑（deriveCoverUrl）位于 electron/main/apis/qobuz/core/cover.ts，
// 前端无法跨进程边界 import，这里实现等价的本地版本 withCoverSize。

// 注：Qobuz 封面 URL 派生逻辑（deriveCoverUrl）位于 electron/main/apis/qobuz/core/cover.ts，
// 前端无法跨进程边界 import，这里实现等价的本地版本 withCoverSize。

/**
 * 将 Qobuz CDN 图片 URL 改写为经部署服务器代理的地址。
 * 解决：客户端（异地 / 内网外的浏览器）无法直连 static.qobuz.com，
 * 导致 Qobuz 专辑 / 歌手 / 歌单封面空白。仅对 *.qobuz.com 生效，其它来源原样返回。
 */
const QOBUZ_IMG_HOST_RE = /(^|\.)qobuz\.com$/i;
const proxyQobuzImage = (url: string | undefined | null): string | undefined => {
  if (!url) return undefined;
  try {
    const u = new URL(url);
    if (
      (u.protocol === "http:" || u.protocol === "https:") &&
      QOBUZ_IMG_HOST_RE.test(u.hostname)
    ) {
      return `/api/proxy/image?url=${encodeURIComponent(url)}`;
    }
  } catch {
    /* 非法 URL 直接返回原值 */
  }
  return url;
};

/**
 * 派生指定尺寸的封面 URL
 * 原始封面 URL 形如：https://static.qobuz.com/images/covers/xx/xxxxxx_600.jpg
 * 替换 _NNN 为目标尺寸；Qobuz CDN 图片统一经服务器代理穿透（解决异地客户端无法直连）。
 */
const withCoverSize = (url: string | undefined | null, size = 600): string | undefined => {
  if (!url) return undefined;
  const sized = url.replace(/_\d+\.(jpg|jpeg|png|webp)$/i, `_${size}.$1`) || url;
  return proxyQobuzImage(sized) ?? sized;
};

// ─── 响应类型定义 ────────────────────────────────────────────────────────

interface QobuzImage {
  large?: string;
  small?: string;
  thumbnail?: string;
}

interface QobuzPerformer {
  id?: number | string;
  name?: string;
}

interface QobuzAlbumRef {
  id?: number | string;
  title?: string;
  image?: QobuzImage;
  artist?: QobuzPerformer;
}

interface QobuzTrack {
  id: number | string;
  title?: string;
  /** 秒 */
  duration?: number;
  performer?: QobuzPerformer;
  album?: QobuzAlbumRef;
  /** HiRes 元数据 */
  maximum_bit_depth?: number;
  maximum_sampling_rate?: number;
  /** 是否已购买/可流式播放 */
  streamable?: boolean;
  isrc?: string;
  track_number?: number;
}

interface QobuzAlbum {
  id: number | string;
  title?: string;
  artist?: QobuzPerformer;
  image?: QobuzImage;
  tracks_count?: number;
  release_date?: string;
  description?: string;
  tracks?: { items?: QobuzTrack[] };
}

interface QobuzArtist {
  id: number | string;
  name?: string;
  picture?: string;
  /** Qobuz artist/get 返回的图片对象（含多尺寸） */
  image?: {
    small?: string;
    medium?: string;
    large?: string;
    extralarge?: string;
    mega?: string;
  };
  album_count?: number;
  biography?: string;
}

interface QobuzPlaylist {
  id: number | string;
  name?: string;
  title?: string;
  image?: QobuzImage;
  // [FIX] Qobuz 歌单封面字段可能是 images300/images150 数组
  images300?: string[];
  images150?: string[];
  images?: string[];
  tracks_count?: number;
  owner?: { name?: string; id?: number | string };
  description?: string;
  tracks?: { items?: QobuzTrack[] };
}

// ─── 转换函数 ────────────────────────────────────────────────────────────

/**
 * 从 track 的 maximum_bit_depth / maximum_sampling_rate 推断音质
 */
const pickQuality = (track: QobuzTrack): AudioQuality | undefined => {
  const bitDepth = track.maximum_bit_depth ?? 16;
  const sampleRate = (track.maximum_sampling_rate ?? 44.1) * 1000;
  if (bitDepth > 16 || sampleRate >= 96000) {
    return {
      codec: "flac",
      sampleRate,
      bitsPerSample: bitDepth,
      bitRate: 0,
      channels: 2,
    };
  }
  if (bitDepth === 16) {
    return {
      codec: "flac",
      sampleRate: 44100,
      bitsPerSample: 16,
      bitRate: 0,
      channels: 2,
    };
  }
  return undefined;
};

/**
 * Qobuz track → 应用层 Track
 * @param track 原始 track 对象
 */
export const songToTrack = (track: QobuzTrack): Track => {
  const album = track.album;
  const cover = album?.image?.large ?? album?.image?.small;
  const coverOriginal = withCoverSize(cover, coverSizePair("qobuz").large);
  const performer = track.performer;
  return {
    id: String(track.id),
    source: "qobuz",
    title: track.title ?? "",
    artists: performer?.name ? [{ id: String(performer.id ?? ""), name: performer.name }] : [],
    album: album
      ? {
          id: String(album.id ?? ""),
          name: album.title ?? "",
          cover: withCoverSize(cover, coverSizePair("qobuz").small),
        }
      : undefined,
    duration: (track.duration ?? 0) * 1000,
    cover: withCoverSize(cover, coverSizePair("qobuz").small),
    coverOriginal,
    quality: pickQuality(track),
    track: track.track_number,
  };
};

/**
 * Qobuz track 列表 → Track 列表
 */
export const songsToTracks = (tracks: QobuzTrack[] | undefined | null): Track[] =>
  tracks?.map(songToTrack) ?? [];

/**
 * Qobuz album → 应用层 Album
 */
export const toAlbum = (raw: QobuzAlbum): Album => ({
  id: String(raw.id),
  name: raw.title ?? "",
  cover: albumToCover(raw).cover,
  artist: raw.artist?.name,
  trackCount: raw.tracks_count,
  year: raw.release_date ? new Date(raw.release_date).getFullYear() : undefined,
});

/**
 * Qobuz album → CoverItem（搜索/浏览列表用）
 */
export const albumToCover = (album: QobuzAlbum): CoverItem => ({
  id: String(album.id),
  title: album.title ?? "",
  cover: withCoverSize(album.image?.large ?? album.image?.small, coverSizePair("qobuz").small),
  subtitle: album.artist?.name ?? "",
  trackCount: album.tracks_count ?? 0,
});

/**
 * 提取 Qobuz 歌手封面 URL
 * 优先级：image.mega > image.extralarge > image.large > image.medium > image.small > picture
 * artist/get 返回 image 对象；catalog/search 返回 picture 字符串。
 */
const artistAvatarUrl = (raw: QobuzArtist, size = 300): string | undefined => {
  const img = raw.image;
  const url =
    img?.mega ?? img?.extralarge ?? img?.large ?? img?.medium ?? img?.small ?? raw.picture;
  return withCoverSize(url, size);
};

/**
 * Qobuz artist → 应用层 Artist
 */
export const toArtist = (raw: QobuzArtist): Artist => ({
  id: String(raw.id),
  name: raw.name ?? "",
  avatar: artistAvatarUrl(raw, coverSizePair("qobuz").small),
  albumCount: raw.album_count,
});

/**
 * Qobuz artist → CoverItem（搜索/浏览列表用）
 */
export const artistToCover = (artist: QobuzArtist): CoverItem => ({
  id: String(artist.id),
  title: artist.name ?? "",
  cover: artistAvatarUrl(artist, coverSizePair("qobuz").small),
  subtitle: "",
  trackCount: artist.album_count ?? 0,
});

/**
 * 提取 Qobuz 歌单封面 URL
 * 优先级：images300[0] > images150[0] > images[0] > image.large > image.small
 * 参照 MemoryPlay L9977-9978
 */
const playlistCoverUrl = (raw: QobuzPlaylist, size = 300): string | undefined => {
  const arr = raw.images300?.[0] || raw.images150?.[0] || raw.images?.[0];
  if (arr) return withCoverSize(arr, size);
  return withCoverSize(raw.image?.large ?? raw.image?.small, size);
};

/**
 * Qobuz playlist → 应用层 Playlist
 */
export const toPlaylist = (raw: QobuzPlaylist): Playlist => ({
  id: String(raw.id),
  name: raw.name ?? raw.title ?? "",
  cover: playlistCoverUrl(raw, coverSizePair("qobuz").small),
  description: raw.description,
  trackCount: raw.tracks_count,
  owner: raw.owner?.name,
});

/**
 * Qobuz playlist → CoverItem
 */
export const playlistToCover = (playlist: QobuzPlaylist): CoverItem => ({
  id: String(playlist.id),
  title: playlist.name ?? playlist.title ?? "",
  cover: playlistCoverUrl(playlist, coverSizePair("qobuz").small),
  subtitle: playlist.owner?.name ?? "",
  trackCount: playlist.tracks_count ?? 0,
});

export type {
  QobuzTrack,
  QobuzAlbum,
  QobuzArtist,
  QobuzPlaylist,
  QobuzImage,
  QobuzPerformer,
  QobuzAlbumRef,
};
