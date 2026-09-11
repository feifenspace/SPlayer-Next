import type {
  LegacyPlaylistRecord,
  PlaylistApi,
  PlaylistCreateInput,
  PlaylistUpdateInput,
} from "@shared/types/playlist";
import type { HttpPlayerClient } from "./httpClient";

const requireData = <T>(response: { success: boolean; data?: T; error?: string }): T => {
  if (response.success && response.data !== undefined) return response.data;
  throw new Error(response.error || "Playlist request failed");
};

/** 将浏览器中的旧歌单落到 Headless 服务端，完成后调用方才清理旧 IndexedDB。 */
export const createWebPlaylistApi = (client: HttpPlayerClient): PlaylistApi => ({
  list: async () => requireData(await client.getPlaylists()),
  get: async (id) => {
    const response = await client.getPlaylist(id);
    return response.success ? response.data ?? null : null;
  },
  create: async (input: PlaylistCreateInput) => requireData(await client.createPlaylist(input)),
  update: async (id: string, input: PlaylistUpdateInput) => {
    requireData(await client.updatePlaylist(id, input));
    return requireData(await client.getPlaylist(id));
  },
  remove: async (id) => {
    requireData(await client.removePlaylist(id));
  },
  addTracks: async (id, trackIds) => {
    const data = requireData(await client.addPlaylistTracks(id, trackIds));
    return Number(data.added_count) || 0;
  },
  removeTracks: async (id, trackIds) => {
    const data = requireData(await client.removePlaylistTracks(id, trackIds));
    return Number(data.removed_count) || 0;
  },
  importLegacy: async (records: LegacyPlaylistRecord[]) => {
    for (const record of records) {
      const created = requireData(
        await client.createPlaylist({
          title: record.title,
          description: record.description,
        }),
      );
      if (record.trackIds.length > 0) {
        requireData(await client.addPlaylistTracks(created.id, record.trackIds));
      }
    }
  },
  clear: async () => {
    const playlists = requireData(await client.getPlaylists());
    for (const playlist of playlists) requireData(await client.removePlaylist(playlist.id));
  },
});
