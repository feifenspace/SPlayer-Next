import type { Track } from "@shared/types/player";
import type { CoverItem } from "@/types/artist";
import type { SearchResult } from "./index";
import { useLibraryStore } from "@/stores/library";
import { usePlaylistStore } from "@/stores/playlist";

/**
 * 搜索本地单曲
 */
export const songs = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<Track>> => {
  const library = useLibraryStore();
  if (!library.initialized && library.tracks.length === 0) {
    await library.load();
  }

  const q = keyword.trim().toLowerCase();
  const matched = library.tracks.filter((t) => {
    if (t.title && t.title.toLowerCase().includes(q)) return true;
    if (t.artists && t.artists.some((a) => a.name && a.name.toLowerCase().includes(q))) return true;
    if (t.album?.name && t.album.name.toLowerCase().includes(q)) return true;
    if (t.path && t.path.toLowerCase().includes(q)) return true;
    return false;
  });

  const total = matched.length;
  const items = matched.slice(offset, offset + limit);
  const hasMore = offset + limit < total;

  return { items, total, hasMore };
};

/**
 * 搜索本地专辑
 */
export const albums = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<CoverItem>> => {
  const library = useLibraryStore();
  const albumList = await library.getAlbumList();

  const q = keyword.trim().toLowerCase();
  const matched = albumList.filter((a) => {
    if (a.name && a.name.toLowerCase().includes(q)) return true;
    if (a.artist && a.artist.toLowerCase().includes(q)) return true;
    return false;
  });

  const total = matched.length;
  const paged = matched.slice(offset, offset + limit);
  const items: CoverItem[] = paged.map((a) => ({
    id: encodeURIComponent(a.name),
    title: a.name,
    subtitle: a.artist,
    cover: a.cover,
    trackCount: a.trackCount,
  }));
  const hasMore = offset + limit < total;

  return { items, total, hasMore };
};

/**
 * 搜索本地歌手
 */
export const artists = async (
  keyword: string,
  offset: number,
  limit: number,
): Promise<SearchResult<CoverItem>> => {
  const library = useLibraryStore();
  const artistList = await library.getArtistList();

  const q = keyword.trim().toLowerCase();
  const matched = artistList.filter((a) => a.name && a.name.toLowerCase().includes(q));

  const total = matched.length;
  const paged = matched.slice(offset, offset + limit);
  const items: CoverItem[] = paged.map((a) => ({
    id: encodeURIComponent(a.name),
    title: a.name,
    cover: a.cover || library.getArtistAvatar(a.name),
    trackCount: a.trackCount,
  }));
  const hasMore = offset + limit < total;

  return { items, total, hasMore };
};

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
