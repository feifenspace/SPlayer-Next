import type { Album, Track } from "@shared/types/player";
import type {
  StreamingApi,
  StreamingConnectResult,
  StreamingErrorCode,
  StreamingLibrarySnapshot,
  StreamingPingResult,
  StreamingRuntimeConfig,
  StreamingSearchResult,
  StreamingServerConfig,
  StreamingServerInput,
} from "@shared/types/streaming";
import { subsonicWebAdapter } from "./subsonic";
import { authenticate, jellyfinWebAdapter, type StreamingAuthSession } from "./jellyfin";
import type { WebStreamingAdapter } from "./types";
import {
  loadActiveServerId,
  loadPersistedServers,
  loadServerSnapshot,
  removeServerSnapshot,
  saveActiveServerId,
  savePersistedServers,
  saveServerSnapshot,
  toPublicConfig,
  toRuntimeConfig,
  type PersistedServerRecord,
} from "./storage";

const sessionCache = new Map<string, StreamingAuthSession>();
const libraryListeners = new Set<(serverId: string) => void>();
const activeSyncTasks = new Map<string, Promise<void>>();

const resolveAdapter = async (
  config: StreamingRuntimeConfig,
): Promise<{ adapter: WebStreamingAdapter; runtimeConfig: StreamingRuntimeConfig }> => {
  if (config.type === "jellyfin" || config.type === "emby") {
    let session = sessionCache.get(config.id);
    if (!session) {
      session = await authenticate(config);
      sessionCache.set(config.id, session);
    }
    return {
      adapter: jellyfinWebAdapter,
      runtimeConfig: { ...config, ...session },
    };
  }
  return {
    adapter: subsonicWebAdapter,
    runtimeConfig: config,
  };
};

const getRuntimeConfigById = async (serverId: string): Promise<StreamingRuntimeConfig> => {
  const servers = await loadPersistedServers();
  const record = servers.find((s) => s.id === serverId);
  if (!record) throw new Error(`找不到服务器配置: ${serverId}`);
  return toRuntimeConfig(record);
};

export const createWebStreamingApi = (): StreamingApi => {
  return {
    async loadServers(): Promise<{
      servers: StreamingServerConfig[];
      activeServerId: string | null;
    }> {
      const [servers, activeServerId] = await Promise.all([
        loadPersistedServers(),
        loadActiveServerId(),
      ]);
      return {
        servers: servers.map(toPublicConfig),
        activeServerId,
      };
    },

    async addServer(input: StreamingServerInput): Promise<StreamingServerConfig> {
      const servers = await loadPersistedServers();
      const id = `srv-${Date.now()}-${Math.random().toString(36).substring(2, 6)}`;
      const newRecord: PersistedServerRecord = {
        id,
        name: input.name.trim(),
        type: input.type,
        url: input.url.trim(),
        username: input.username.trim(),
        password: input.password || "",
      };
      servers.push(newRecord);
      await savePersistedServers(servers);
      return toPublicConfig(newRecord);
    },

    async updateServer(
      serverId: string,
      input: StreamingServerInput,
    ): Promise<StreamingServerConfig> {
      const servers = await loadPersistedServers();
      const idx = servers.findIndex((s) => s.id === serverId);
      if (idx === -1) throw new Error(`未找到服务器: ${serverId}`);

      sessionCache.delete(serverId);
      const existing = servers[idx];
      const updatedRecord: PersistedServerRecord = {
        ...existing,
        name: input.name.trim(),
        type: input.type,
        url: input.url.trim(),
        username: input.username.trim(),
        password: input.password ? input.password : existing.password,
      };
      servers[idx] = updatedRecord;
      await savePersistedServers(servers);
      return toPublicConfig(updatedRecord);
    },

    async removeServer(serverId: string): Promise<void> {
      sessionCache.delete(serverId);
      const servers = await loadPersistedServers();
      const filtered = servers.filter((s) => s.id !== serverId);
      await savePersistedServers(filtered);
      await removeServerSnapshot(serverId);

      const activeId = await loadActiveServerId();
      if (activeId === serverId) {
        await saveActiveServerId(null);
      }
    },

    async setActiveServer(serverId: string | null): Promise<void> {
      await saveActiveServerId(serverId);
    },

    async testConnection(
      input: StreamingServerInput,
      serverId?: string,
    ): Promise<StreamingPingResult> {
      let password = input.password;
      if (!password && serverId) {
        const servers = await loadPersistedServers();
        const existing = servers.find((s) => s.id === serverId);
        if (existing) password = existing.password;
      }
      const testConfig: StreamingRuntimeConfig = {
        id: serverId || "temp-test",
        name: input.name,
        type: input.type,
        url: input.url,
        username: input.username,
        password: password || "",
        hasPassword: Boolean(password),
      };

      try {
        if (testConfig.type === "jellyfin" || testConfig.type === "emby") {
          const session = await authenticate(testConfig);
          return await jellyfinWebAdapter.ping({ ...testConfig, ...session });
        }
        return await subsonicWebAdapter.ping(testConfig);
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        let code: StreamingErrorCode = "unknown";
        if (message.toLowerCase().includes("auth") || message.includes("401") || message.includes("403")) {
          code = "auth";
        } else if (message.toLowerCase().includes("fetch") || message.includes("network") || message.includes("failed")) {
          code = "network";
        }
        return { ok: false, error: message, code };
      }
    },

    async connect(serverId: string): Promise<StreamingConnectResult> {
      try {
        const config = await getRuntimeConfigById(serverId);
        const { adapter, runtimeConfig } = await resolveAdapter(config);
        const pingRes = await adapter.ping(runtimeConfig);
        if (!pingRes.ok) {
          return {
            ok: false,
            error: pingRes.error || "连接失败",
            code: pingRes.code || "network",
          };
        }

        const servers = await loadPersistedServers();
        const s = servers.find((item) => item.id === serverId);
        if (s) {
          s.lastConnected = Date.now();
          await savePersistedServers(servers);
        }

        // 后台静默触发一次同步
        void this.sync(serverId, false);

        return {
          ok: true,
          server: s ? toPublicConfig(s) : toPublicConfig(config),
        };
      } catch (err) {
        const error = err instanceof Error ? err.message : String(err);
        return { ok: false, error, code: "network" };
      }
    },

    async disconnect(serverId: string): Promise<void> {
      sessionCache.delete(serverId);
    },

    async getSnapshot(serverId: string): Promise<StreamingLibrarySnapshot> {
      const cached = await loadServerSnapshot(serverId);
      if (cached) return cached;

      // 未缓存时立即拉取一次
      const config = await getRuntimeConfigById(serverId);
      const { adapter, runtimeConfig } = await resolveAdapter(config);
      const [songs, albums, artists, playlists] = await Promise.all([
        adapter.listSongs(runtimeConfig, { limit: 500 }),
        adapter.listAlbums(runtimeConfig, { limit: 500 }),
        adapter.listArtists(runtimeConfig),
        adapter.listPlaylists(runtimeConfig),
      ]);
      const snapshot: StreamingLibrarySnapshot = { songs, albums, artists, playlists };
      await saveServerSnapshot(serverId, snapshot);
      return snapshot;
    },

    async sync(serverId: string, force = false): Promise<boolean> {
      const running = activeSyncTasks.get(serverId);
      if (running) {
        await running;
        return true;
      }
      if (!force) {
        const cached = await loadServerSnapshot(serverId);
        if (cached && cached.songs.length > 0) return false;
      }

      const syncPromise = (async () => {
        try {
          const config = await getRuntimeConfigById(serverId);
          const { adapter, runtimeConfig } = await resolveAdapter(config);

          const notify = () => {
            for (const listener of libraryListeners) {
              try {
                listener(serverId);
              } catch {}
            }
          };

          // 1. 同步歌手与歌单
          const [artists, playlists] = await Promise.all([
            adapter.listArtists(runtimeConfig).catch(() => []),
            adapter.listPlaylists(runtimeConfig).catch(() => []),
          ]);

          // 2. 分页全量同步专辑
          const allAlbums: Album[] = [];
          let albumOffset = 0;
          while (true) {
            const albumBatch = await adapter.listAlbums(runtimeConfig, {
              offset: albumOffset,
              limit: 500,
            }).catch(() => []);
            if (albumBatch.length === 0) break;
            allAlbums.push(...albumBatch);
            albumOffset += albumBatch.length;
            if (albumBatch.length < 500) break;
          }

          // 3. 分页全量渐进式同步歌曲
          const allSongs: Track[] = [];
          const FIRST_BATCH = 200;
          const BATCH_SIZE = 1000;
          let songLimit = FIRST_BATCH;

          while (true) {
            const songBatch = await adapter.listSongs(runtimeConfig, {
              offset: allSongs.length,
              limit: songLimit,
            }).catch(() => []);
            if (songBatch.length === 0) break;
            allSongs.push(...songBatch);

            // 每拉取一批即保存快照并广播更新，让前端首屏秒开并实时增长
            const currentSnapshot: StreamingLibrarySnapshot = {
              songs: allSongs,
              albums: allAlbums,
              artists,
              playlists,
            };
            await saveServerSnapshot(serverId, currentSnapshot);
            notify();

            if (songBatch.length < songLimit) break;
            songLimit = BATCH_SIZE;
          }

          // 最终保存全量快照
          const finalSnapshot: StreamingLibrarySnapshot = {
            songs: allSongs,
            albums: allAlbums,
            artists,
            playlists,
          };
          await saveServerSnapshot(serverId, finalSnapshot);
          notify();
        } catch (err) {
          console.error(`[StreamingWeb] sync error for ${serverId}:`, err);
        } finally {
          activeSyncTasks.delete(serverId);
        }
      })();

      activeSyncTasks.set(serverId, syncPromise);
      await syncPromise;
      return true;
    },

    onLibraryUpdated(callback: (serverId: string) => void): () => void {
      libraryListeners.add(callback);
      return () => {
        libraryListeners.delete(callback);
      };
    },

    async search(serverId: string, query: string): Promise<StreamingSearchResult> {
      const q = query.trim();
      if (!q) return { songs: [], albums: [], artists: [] };

      // 1. 优先尝试向服务端发起实时全库检索（如 Subsonic search3 / Jellyfin search）
      try {
        const config = await getRuntimeConfigById(serverId);
        const { adapter, runtimeConfig } = await resolveAdapter(config);
        if (typeof adapter.search === "function") {
          const liveResult = await adapter.search(runtimeConfig, q);
          if (
            liveResult &&
            (liveResult.songs.length > 0 ||
              liveResult.albums.length > 0 ||
              liveResult.artists.length > 0)
          ) {
            return liveResult;
          }
        }
      } catch (err) {
        console.warn("[StreamingWeb] live search failed, falling back to local snapshot:", err);
      }

      // 2. 本地快照模糊过滤回退
      const snapshot = await loadServerSnapshot(serverId);
      if (!snapshot) return { songs: [], albums: [], artists: [] };

      const lower = q.toLowerCase();
      const songs = snapshot.songs.filter(
        (s) =>
          s.title.toLowerCase().includes(lower) ||
          s.artists.some((a) => a.name.toLowerCase().includes(lower)) ||
          (s.album?.name && s.album.name.toLowerCase().includes(lower)),
      );
      const albums = snapshot.albums.filter(
        (a) => a.name.toLowerCase().includes(lower) || (a.artist && a.artist.toLowerCase().includes(lower)),
      );
      const artists = snapshot.artists.filter((a) => a.name.toLowerCase().includes(lower));

      return { songs, albums, artists };
    },

    async getAlbumSongs(serverId: string, albumId: string): Promise<Track[]> {
      const cleanId = albumId.includes(":") ? albumId.split(":").slice(1).join(":") : albumId;
      const config = await getRuntimeConfigById(serverId);
      const { adapter, runtimeConfig } = await resolveAdapter(config);
      return adapter.getAlbumSongs(runtimeConfig, cleanId);
    },

    async getPlaylistSongs(serverId: string, playlistId: string): Promise<Track[]> {
      const cleanId = playlistId.includes(":") ? playlistId.split(":").slice(1).join(":") : playlistId;
      const config = await getRuntimeConfigById(serverId);
      const { adapter, runtimeConfig } = await resolveAdapter(config);
      return adapter.getPlaylistSongs(runtimeConfig, cleanId);
    },

    async getArtistAlbums(serverId: string, artistId: string): Promise<Album[]> {
      const cleanId = artistId.includes(":") ? artistId.split(":").slice(1).join(":") : artistId;
      const config = await getRuntimeConfigById(serverId);
      const { adapter, runtimeConfig } = await resolveAdapter(config);
      return adapter.getArtistAlbums(runtimeConfig, cleanId);
    },

    async getArtistSongs(serverId: string, artistId: string): Promise<Track[]> {
      const cleanId = artistId.includes(":") ? artistId.split(":").slice(1).join(":") : artistId;
      const config = await getRuntimeConfigById(serverId);
      const { adapter, runtimeConfig } = await resolveAdapter(config);
      return adapter.getArtistSongs(runtimeConfig, cleanId);
    },

    async getStreamUrl(
      serverId: string,
      trackId: string,
      playSessionId?: string,
    ): Promise<string> {
      const cleanId = trackId.includes(":") ? trackId.split(":").slice(1).join(":") : trackId;
      const config = await getRuntimeConfigById(serverId);
      const { adapter, runtimeConfig } = await resolveAdapter(config);
      return adapter.getStreamUrl(runtimeConfig, cleanId, playSessionId);
    },

    async getLyrics(
      serverId: string,
      trackId: string,
      hint?: { artist?: string; title?: string },
    ): Promise<string | null> {
      const cleanId = trackId.includes(":") ? trackId.split(":").slice(1).join(":") : trackId;
      const config = await getRuntimeConfigById(serverId);
      const { adapter, runtimeConfig } = await resolveAdapter(config);
      return adapter.getLyrics(runtimeConfig, cleanId, hint);
    },
  };
};
