import type { Track } from "@shared/types/player";
import type { CoverItem } from "@/types/artist";
import type { SearchResult } from "./index";
import { useStreamingStore } from "@/stores/streaming";

/**
 * 搜索媒体源单曲
 */
export const songs = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<Track>> => {
  const store = useStreamingStore();
  if (!store.activeServerId) {
    return { items: [], total: 0, hasMore: false };
  }
  const result = await store.search(keyword);
  const total = result.songs.length;
  const items = result.songs.slice(offset, offset + limit);
  const hasMore = offset + limit < total;
  return { items, total, hasMore };
};

/**
 * 搜索媒体源专辑
 */
export const albums = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<CoverItem>> => {
  const store = useStreamingStore();
  if (!store.activeServerId) {
    return { items: [], total: 0, hasMore: false };
  }
  const result = await store.search(keyword);
  const total = result.albums.length;
  const paged = result.albums.slice(offset, offset + limit);
  const items: CoverItem[] = paged.map((a) => ({
    id: a.id ? String(a.id) : a.name,
    title: a.name,
    subtitle: a.artist,
    cover: a.cover,
    trackCount: a.trackCount ?? 0,
  }));
  const hasMore = offset + limit < total;
  return { items, total, hasMore };
};

/**
 * 搜索媒体源歌手
 */
export const artists = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<CoverItem>> => {
  const store = useStreamingStore();
  if (!store.activeServerId) {
    return { items: [], total: 0, hasMore: false };
  }
  const result = await store.search(keyword);
  const total = result.artists.length;
  const paged = result.artists.slice(offset, offset + limit);
  const items: CoverItem[] = paged.map((a) => ({
    id: a.id ? String(a.id) : a.name,
    title: a.name,
    cover: a.avatar,
    trackCount: a.albumCount ?? 0,
  }));
  const hasMore = offset + limit < total;
  return { items, total, hasMore };
};

/**
 * 搜索媒体源歌单
 */
export const playlists = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<CoverItem>> => {
  const store = useStreamingStore();
  if (!store.activeServerId) {
    return { items: [], total: 0, hasMore: false };
  }
  const q = keyword.trim().toLowerCase();
  const matched = store.playlists.filter(
    (p) => p.name.toLowerCase().includes(q) || (p.description && p.description.toLowerCase().includes(q)),
  );
  const total = matched.length;
  const paged = matched.slice(offset, offset + limit);
  const items: CoverItem[] = paged.map((p) => ({
    id: p.id ? String(p.id) : p.name,
    title: p.name,
    subtitle: p.description,
    cover: p.cover,
    trackCount: p.trackCount ?? 0,
  }));
  const hasMore = offset + limit < total;
  return { items, total, hasMore };
};
