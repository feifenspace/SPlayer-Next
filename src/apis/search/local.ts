import type { Track } from "@shared/types/player";
import type { CoverItem } from "@/types/artist";
import type { SearchResult } from "./index";
import { usePlaylistStore } from "@/stores/playlist";

/**
 * 搜索本地单曲
 */
export const songs = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<Track>> => {
  const res = await window.api.library.getTracksPage?.(limit, offset, keyword);
  if (!res?.success || !res.data) return { items: [], total: 0, hasMore: false };
  return { items: res.data.items, total: res.data.total, hasMore: res.data.hasMore };
};

/**
 * 搜索本地专辑
/**
 * 搜索本地专辑
 */
export const albums = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<CoverItem>> => {
  const res = await window.api.library.getAlbumsPage?.(limit, offset, keyword);
  if (!res?.success || !res.data) return { items: [], total: 0, hasMore: false };
  const items = res.data.items.map((a) => ({
    id: encodeURIComponent(a.name),
    title: a.name,
    subtitle: a.artist,
    cover: a.cover,
    trackCount: a.trackCount,
  }));
  return { items, total: res.data.total, hasMore: res.data.hasMore };
};

/**
 * 搜索本地歌手
/**
 * 搜索本地歌手
 */
export const artists = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<CoverItem>> => {
  const res = await window.api.library.getArtistsPage?.(limit, offset, keyword);
  if (!res?.success || !res.data) return { items: [], total: 0, hasMore: false };
  const items = res.data.items.map((a) => ({
    id: encodeURIComponent(a.name),
    title: a.name,
    cover: a.cover,
    trackCount: a.trackCount,
  }));
  return { items, total: res.data.total, hasMore: res.data.hasMore };
};

/**
 * 搜索本地歌单
/**
 * 搜索本地歌单
 */
export const playlists = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<CoverItem>> => {
  const playlistStore = usePlaylistStore();
  if (!playlistStore.initialized && playlistStore.playlists.length === 0) {
    await playlistStore.load();
  }

  const q = keyword.trim().toLowerCase();
  const matched = playlistStore.playlists.filter((p) => {
    if (p.type && p.type !== "local") return false;
    if (p.title && p.title.toLowerCase().includes(q)) return true;
    if (p.description && p.description.toLowerCase().includes(q)) return true;
    return false;
  });

  const total = matched.length;
  const paged = matched.slice(offset, offset + limit);
  const items: CoverItem[] = paged.map((p) => ({
    id: p.id,
    title: p.title,
    subtitle: p.description,
    cover: p.cover,
    trackCount: p.trackCount,
  }));
  const hasMore = offset + limit < total;

  return { items, total, hasMore };
};
