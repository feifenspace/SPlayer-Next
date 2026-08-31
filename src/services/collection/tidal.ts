import type { CollectionType } from "@/types/collection";
import { fetchAlbum } from "@/apis/album/tidal";
import { fetchPlaylist } from "@/apis/playlist/tidal";
import type { LoadCollectionOptions } from "./types";

export const loadTidalCollection = async (
  type: CollectionType,
  id: string,
  options: LoadCollectionOptions,
): Promise<void> => {
  const originalId = decodeURIComponent(id);
  const fallbackName = options.fallbackName ?? originalId;

  if (type === "album") {
    const result = await fetchAlbum(originalId);
    if (result && !options.signal?.aborted) {
      options.onUpdate({
        id: result.album.id ?? originalId,
        type,
        source: "tidal",
        title: result.album.name || fallbackName,
        cover: result.album.cover,
        creator: result.album.artist,
        tracks: result.tracks,
        trackCount: result.album.trackCount ?? result.tracks.length,
        description: result.description,
      });
    }
    return;
  }

  if (type === "playlist") {
    const result = await fetchPlaylist(originalId);
    if (result && !options.signal?.aborted) {
      options.onUpdate({
        id: result.playlist.id ?? originalId,
        type,
        source: "tidal",
        title: result.playlist.name || fallbackName,
        cover: result.playlist.cover,
        description: result.playlist.description,
        creator: result.playlist.owner,
        tracks: result.tracks,
        trackCount: result.playlist.trackCount ?? result.tracks.length,
      });
    }
    return;
  }

  options.onUpdate(null);
};
