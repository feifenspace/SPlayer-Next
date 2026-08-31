import type { Track, AudioQuality, Artist, Album, Playlist } from "@shared/types/player";
import type { CoverItem } from "@/types/artist";
import { coverSizePair } from "@/utils/streamingSettings";

/**
 * 将 TIDAL CDN 图片 URL 改写为经部署服务器代理的地址。
 * 解决：客户端（异地 / 内网外浏览器 / 受 GFW 或运营商限制的客户端）
 * 无法直连 resources.tidal.com，导致 TIDAL 封面空白。
 * 仅对 *.tidal.com 生效，其它来源原样返回。
 */
const TIDAL_IMG_HOST_RE = /(^|\.)tidal\.com$/i;
const proxyTidalImage = (url: string | undefined | null): string | undefined => {
  if (!url) return undefined;
  try {
    const u = new URL(url);
    if (
      (u.protocol === "http:" || u.protocol === "https:") &&
      TIDAL_IMG_HOST_RE.test(u.hostname)
    ) {
      return "/api/proxy/image?url=" + encodeURIComponent(url);
    }
  } catch {
    /* 非法 URL 直接返回原值 */
  }
  return url;
};

/**
 * TIDAL 封面 UUID -> 图片 URL（内部构造，不经代理）
 *
 * TIDAL 的封面 ID 是 UUID 格式（如 "19b1e49c-e8e1-4a9d-a5cb-608e3968c7ca"），
 * 将 "-" 替换为 "/" 后拼接到 resources.tidal.com/images/。
 * 如果已经是完整 URL，直接替换尺寸。
 */
const buildTidalCover = (
  coverId: TidalImageField,
  size = 640,
): string | undefined => {
  if (!coverId) return undefined;
  // 字符串：UUID 或完整 URL
  if (typeof coverId === "string") {
    if (coverId.startsWith("http")) {
      return coverId.replace(/\d+x\d+/, size + "x" + size);
    }
    const path = coverId.replace(/-/g, "/");
    return "https://resources.tidal.com/images/" + path + "/" + size + "x" + size + ".jpg";
  }
  // 对象：{ href, small, medium, large } 或 { url }
  if (typeof coverId === "object" && !Array.isArray(coverId)) {
    const obj = coverId as { href?: string; small?: string; medium?: string; large?: string; url?: string };
    const raw = obj.href || obj.large || obj.medium || obj.small || obj.url;
    return raw ? buildTidalCover(raw, size) : undefined;
  }
  // 数组：[{ url }] / [{ href }]
  if (Array.isArray(coverId) && coverId.length > 0) {
    const first = coverId[0] as { url?: string; href?: string };
    const raw = first?.url || first?.href;
    return raw ? buildTidalCover(raw, size) : undefined;
  }
  return undefined;
};

/**
 * TIDAL 封面 UUID -> 图片 URL（对外出口，自动经服务器代理）
 *
 * [FIX] 新增对对象/数组格式封面字段的支持（image/squareImage 可能返回对象）。
 * 所有封面统一走 /api/proxy/image，规避客户端无法直连 resources.tidal.com 的问题。
 */
export const tidalCoverUrl = (
  coverId: TidalImageField,
  size = 640,
): string | undefined => {
  const raw = buildTidalCover(coverId, size);
  return raw ? proxyTidalImage(raw) : undefined;
};

/**
 * 从多个候选字段中提取第一个可用的封面 URL。
 *
 * [FIX] TIDAL 歌单封面字段优先级（参照 MemoryPlay L10024-10025）：
 *   squareImage > image > imageCover > cover
 *
 * - squareImage: 640x640 方形封面，resources.tidal.com 可正常访问
 * - image: 矩形编辑图（640x640 尺寸返回 403），仅作为兜底
 */
export const resolveTidalCover = (
  fields: { squareImage?: TidalImageField; image?: TidalImageField; imageCover?: TidalImageField; cover?: TidalImageField; picture?: TidalImageField },
  size = 640,
): string | undefined => {
  const { squareImage, image, imageCover, cover, picture } = fields;
  return (
    tidalCoverUrl(squareImage, size) ||
    tidalCoverUrl(image, size) ||
    tidalCoverUrl(imageCover, size) ||
    tidalCoverUrl(cover, size) ||
    tidalCoverUrl(picture, size)
  );
};

// ─── 响应类型定义 ────────────────────────────────────────────────────────

interface TidalArtistRef {
  id?: number | string;
  name?: string;
}

/**
 * TIDAL 图片字段的多种返回格式：
 * - 字符串 UUID
 * - 字符串 URL（已含尺寸）
 * - 对象：{ href, small, medium, large }
 * - 数组（兼容）：[{ url }] / [{ href }]
 */
type TidalImageField = string | { href?: string; small?: string; medium?: string; large?: string } | Array<{ url?: string; href?: string }> | undefined | null;

interface TidalAlbumRef {
  id?: number | string;
  title?: string;
  cover?: TidalImageField;
  /** 某些接口用 image / imageCover 字段 */
  image?: TidalImageField;
  imageCover?: TidalImageField;
  artist?: TidalArtistRef;
}

interface TidalTrack {
  id: number | string;
  title?: string;
  /** 秒 */
  duration?: number;
  artist?: TidalArtistRef;
  artists?: TidalArtistRef[];
  album?: TidalAlbumRef;
  /** 封面 ID（兼容多种字段名） */
  cover?: TidalImageField;
  image?: TidalImageField;
  imageCover?: TidalImageField;
  trackNumber?: number;
  /** [FIX] TIDAL 音质档位：LOW / HIGH / LOSSLESS / HI_RES / HI_RES_LOSSLESS */
  audioQuality?: string;
  /** TIDAL 声音模式数组（STEREO / DOLBY_ATMOS 等） */
  audioModes?: string[];
  /**
   * [FIX] TIDAL 实际可用的全部音质版本（标签数组）
   * 例：[ "LOSSLESS", "HIRES_LOSSLESS" ] 表示这首歌既有 CD 也有 Hi-Res 96k/24bit。
   * `audioQuality` 字段是用户套餐默认档位，**不代表**实际最高音质，
   * 必须读这个 tags 数组才能正确显示 Hi-Res 徽章。
   */
  mediaMetadata?: { tags?: string[] };
  /** EXPLICIT / CLEAN */
  version?: string;
  isrc?: string;
  popularity?: number;
}

interface TidalAlbum {
  id: number | string;
  title?: string;
  artist?: TidalArtistRef;
  /** 封面 ID（UUID 或 URL） */
  cover?: TidalImageField;
  image?: TidalImageField;
  imageCover?: TidalImageField;
  /** [FIX] 歌单/专辑用，squareImage 是 640x640 可用方形封面，image 是编辑版矩形图 */
  squareImage?: TidalImageField;
  numberOfTracks?: number;
  numberOfVolumes?: number;
  releaseDate?: string;
  description?: string;
  items?: TidalTrack[];
}

interface TidalArtist {
  id: number | string;
  name?: string;
  picture?: TidalImageField;
  /** 封面 ID */
  cover?: TidalImageField;
  image?: TidalImageField;
  imageCover?: TidalImageField;
  albumCount?: number;
  bio?: string;
}

interface TidalPlaylist {
  id: number | string;
  /** TIDAL v2 API returns uuid instead of id for playlists */
  uuid?: string | number;
  title?: string;
  name?: string;
  /** 封面 ID */
  cover?: TidalImageField;
  image?: TidalImageField;
  imageCover?: TidalImageField;
  /** [FIX] 歌单/专辑用，squareImage 是 640x640 可用方形封面，image 是编辑版矩形图 */
  squareImage?: TidalImageField;
  /** TIDAL 歌单封面字段（歌单用 picture，而非 cover） */
  picture?: TidalImageField;
  numberOfTracks?: number;
  creator?: { id?: number | string; name?: string };
  description?: string;
  items?: TidalTrack[];
}

// ─── 辅助函数 ────────────────────────────────────────────────────────────

/**
 * 从 track 对象提取封面（已是完整 URL 形式，按需重新调整尺寸）
 */
const resolveCover = (
  item: { cover?: TidalImageField; image?: TidalImageField; imageCover?: TidalImageField; album?: TidalAlbumRef },
  size = 640,
): string | undefined => {
  return (
    tidalCoverUrl(item.cover, size) ||
    tidalCoverUrl(item.image, size) ||
    tidalCoverUrl(item.imageCover, size) ||
    (item.album && resolveTidalCover(item.album, size)) ||
    undefined
  );
};

/**
 * 从 track 对象提取艺术家名称
 */
const extractArtistName = (item: { artist?: TidalArtistRef; artists?: TidalArtistRef[]; album?: TidalAlbumRef }): string => {
  if (item.artist?.name) return item.artist.name;
  if (typeof item.artist === "string") return item.artist;
  if (item.artists?.length && item.artists[0]?.name) return item.artists[0].name;
  if (item.album?.artist?.name) return item.album.artist.name;
  if (typeof item.album?.artist === "string") return item.album.artist;
  return "Unknown Artist";
};

/**
 * 构造 Track.artists 数组（带真实 id，供 SongList 点击跳转用）。
 *
 * [FIX] TIDAL 部分接口只返回 `artists[]` 数组（无单数 `artist`），
 * 另一部分两者都有。这里统一从 `track.artist` 或 `track.artists[0]` 取 id，
 * 保证歌手名始终可点击跳转到歌手详情页；同时保留多歌手列表。
 */
const buildArtists = (track: TidalTrack): Artist[] => {
  const list: TidalArtistRef[] = track.artists?.length
    ? track.artists
    : track.artist
      ? [track.artist]
      : [];
  return list
    .filter((a) => a?.name)
    .map((a) => ({ id: String(a?.id ?? ""), name: a.name as string }));
};

// ─── 转换函数 ────────────────────────────────────────────────────────────

/**
 * TIDAL 音质标签优先级（顺序：上比下音质好）
 *
 * [FIX] `mediaMetadata.tags` 是 TIDAL **实际可用**的全部音质版本；
 * `audioQuality` 字段只是用户套餐的默认请求档位，不一定代表最高音质。
 *
 * 例：Jay Chou《床边故事》用 HiFi/Master 套餐拉取：
 *   - audioQuality: "LOSSLESS"        ← 套餐默认
 *   - mediaMetadata.tags: [ "LOSSLESS", "HIRES_LOSSLESS" ]
 *   实际可拉到 96k/24bit FLAC，前端徽章必须显示 Hi-Res 而不是 Lossless。
 *
 * 注：HIRES_LOSSLESS 与 HI_RES_LOSSLESS 是 TIDAL 不同时段同义标签；
 *     HI_RES 是早期 v2 名字，已统一到 HIRES_LOSSLESS，三者都按 hi-res 处理。
 */
const TIDAL_TAG_PRIORITY: ReadonlyArray<string> = [
  "HIRES_LOSSLESS",
  "HI_RES_LOSSLESS",
  "HI_RES",
  "LOSSLESS",
  "HIGH",
  "LOW",
];

const pickFromTag = (tag: string): AudioQuality | undefined => {
  switch (tag) {
    case "HIRES_LOSSLESS":
    case "HI_RES_LOSSLESS":
    case "HI_RES":
      return { codec: "flac", sampleRate: 96000, bitsPerSample: 24, bitRate: 0, channels: 2 };
    case "LOSSLESS":
      return { codec: "flac", sampleRate: 44100, bitsPerSample: 16, bitRate: 0, channels: 2 };
    case "HIGH":
      return { codec: "aac", sampleRate: 44100, bitsPerSample: 16, bitRate: 320000, channels: 2 };
    case "LOW":
      return { codec: "aac", sampleRate: 44100, bitsPerSample: 16, bitRate: 96000, channels: 2 };
    default:
      return undefined;
  }
};

/**
 * 从 `mediaMetadata.tags` 数组里挑出"最高"的音质档位。
 * tags 为空 / 不可信时返回 undefined，调用方应兜底用 audioQuality 字段。
 */
const pickQualityFromTags = (track: TidalTrack): AudioQuality | undefined => {
  const tags = track.mediaMetadata?.tags;
  if (!Array.isArray(tags) || tags.length === 0) return undefined;
  for (const tag of TIDAL_TAG_PRIORITY) {
    if (tags.includes(tag)) return pickFromTag(tag);
  }
  return undefined;
};

/**
 * TIDAL 音质档位 → 统一 AudioQuality
 *
 * 优先读 `mediaMetadata.tags` 反映**音轨实际可用**的最高音质；
 * tags 缺失或不可信时，回退到 `audioQuality` 字段（套餐默认请求档位）。
 *
 * 用于搜索结果 / 专辑曲目列表 / 歌单曲目列表（与 SongList 渲染一致）。
 */
const pickQuality = (track: TidalTrack): AudioQuality | undefined => {
  const byTag = pickQualityFromTags(track);
  if (byTag) return byTag;
  return pickFromTag((track.audioQuality || "").toUpperCase());
};

/**
 * Tidal track → 应用层 Track
 */
export const songToTrack = (track: TidalTrack): Track => {
  const artistName = extractArtistName(track);
  return {
    id: String(track.id),
    source: "tidal",
    title: track.title ?? "",
    artists: artistName !== "Unknown Artist" ? buildArtists(track) : [],
    album: track.album
      ? {
          id: String(track.album.id ?? ""),
          name: track.album.title ?? "",
          cover: resolveCover(track.album, coverSizePair("tidal").small),
        }
      : undefined,
    duration: (track.duration ?? 0) * 1000,
    cover: resolveCover(track, coverSizePair("tidal").small),
    coverOriginal: resolveCover(track, coverSizePair("tidal").large),
    track: track.trackNumber,
    quality: pickQuality(track),
  };
};

/**
 * Tidal track 列表 → Track 列表
 */
export const songsToTracks = (tracks: TidalTrack[] | undefined | null): Track[] =>
  tracks?.map(songToTrack) ?? [];

/**
 * Tidal album → 应用层 Album
 */
export const toAlbum = (raw: TidalAlbum): Album => ({
  id: String(raw.id),
  name: raw.title ?? "",
  cover: resolveTidalCover(raw, coverSizePair("tidal").small),
  artist: raw.artist?.name,
  trackCount: raw.numberOfTracks,
  year: raw.releaseDate ? new Date(raw.releaseDate).getFullYear() : undefined,
});

/**
 * Tidal album → CoverItem（搜索/浏览列表用）
 */
export const albumToCover = (album: TidalAlbum): CoverItem => ({
  id: String(album.id),
  title: album.title ?? "",
  cover: resolveTidalCover(album, coverSizePair("tidal").small),
  subtitle: album.artist?.name ?? "",
  trackCount: album.numberOfTracks ?? 0,
});

/**
 * Tidal artist → 应用层 Artist
 */
export const toArtist = (raw: TidalArtist): Artist => ({
  id: String(raw.id),
  name: raw.name ?? "",
  avatar: tidalCoverUrl(raw.picture, coverSizePair("tidal").small) || resolveTidalCover(raw, coverSizePair("tidal").small),
  albumCount: raw.albumCount,
});

/**
 * Tidal artist → CoverItem（搜索/浏览列表用）
 */
export const artistToCover = (artist: TidalArtist): CoverItem => ({
  id: String(artist.id),
  title: artist.name ?? "",
  cover: tidalCoverUrl(artist.picture, coverSizePair("tidal").small) || resolveTidalCover(artist, coverSizePair("tidal").small),
  subtitle: "",
  trackCount: artist.albumCount ?? 0,
});

/**
 * Tidal playlist → 应用层 Playlist
 *
 * [FIX] 歌单封面必须优先使用 squareImage（参照 MemoryPlay L10024-10025）。
 *       image 字段是编辑版矩形图，640x640 尺寸会 403。
 */
export const toPlaylist = (raw: TidalPlaylist): Playlist => ({
  id: String(raw.uuid ?? raw.id),
  name: raw.title ?? raw.name ?? "",
  cover: resolveTidalCover(raw, coverSizePair("tidal").small),
  description: raw.description,
  trackCount: raw.numberOfTracks,
  owner: raw.creator?.name,
});

/**
 * Tidal playlist → CoverItem
 */
export const playlistToCover = (playlist: TidalPlaylist): CoverItem => ({
  id: String(playlist.uuid ?? playlist.id),
  title: playlist.title ?? playlist.name ?? "",
  cover: resolveTidalCover(playlist, coverSizePair("tidal").small),
  subtitle: playlist.creator?.name ?? "",
  trackCount: playlist.numberOfTracks ?? 0,
});

export type {
  TidalTrack,
  TidalAlbum,
  TidalArtist,
  TidalPlaylist,
  TidalArtistRef,
  TidalAlbumRef,
};
