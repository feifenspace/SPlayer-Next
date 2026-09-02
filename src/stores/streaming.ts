import type { Album, Artist, Playlist, Track } from "@shared/types/player";
import type {
  StreamingErrorCode,
  StreamingPingResult,
  StreamingSearchResult,
  StreamingServerConfig,
  StreamingServerInput,
} from "@shared/types/streaming";
import * as session from "@/services/streaming/session";
import { removeServerTracks } from "@/stores/queue";

export const useStreamingStore = defineStore("streaming", () => {
  /** 服务器列表 */
  const servers = ref<StreamingServerConfig[]>([]);
  /** 当前激活服务器 ID */
  const activeServerId = ref<string | null>(null);
  /** 连接状态（仅运行时） */
  const connectionStatus = ref<{
    connected: boolean;
    error?: string;
    errorCode?: StreamingErrorCode;
  }>({ connected: false });
  /** 是否正在等待首个媒体库快照 */
  const loading = ref(false);

  /** 运行时缓存 */
  const songs = shallowRef<Track[]>([]);
  const albums = shallowRef<Album[]>([]);
  const artists = shallowRef<Artist[]>([]);
  const playlists = shallowRef<Playlist[]>([]);
  let initialized = false;
  /** 防止服务器切换后旧请求覆盖当前列表 */
  let snapshotFetchSeq = 0;

  const activeServer = computed<StreamingServerConfig | null>(
    () => servers.value.find((s) => s.id === activeServerId.value) ?? null,
  );
  const hasServer = computed(() => servers.value.length > 0);
  const isConnected = computed(() => connectionStatus.value.connected);

  /** 清空当前服务器的内存列表 */
  const clearMemoryLists = (): void => {
    snapshotFetchSeq += 1;
    songs.value = [];
    albums.value = [];
    artists.value = [];
    playlists.value = [];
  };

  /**
   * 新增服务器
   * @param input - 用户填的表单（name/type/url/username/password）
   * @returns 不包含凭据的服务器视图
   */
  const addServer = async (input: StreamingServerInput): Promise<StreamingServerConfig> => {
    const server = await window.api.streaming.addServer(toRaw(input));
    servers.value = [...servers.value, server];
    return server;
  };

  /**
   * 更新主进程保存的服务器配置
   * @param id - 目标 server id
   * @param input - 服务器表单
   * @returns 更新后的服务器视图
   */
  const updateServer = async (
    id: string,
    input: StreamingServerInput,
  ): Promise<StreamingServerConfig> => {
    const updated = await window.api.streaming.updateServer(id, toRaw(input));
    const idx = servers.value.findIndex((server) => server.id === id);
    if (idx < 0) return updated;
    const list = [...servers.value];
    list[idx] = updated;
    servers.value = list;
    return updated;
  };

  /**
   * 移除服务器；若目标是当前激活的，同时清空界面状态
   * @param id - 目标 server id
   * @returns 删除完成
   */
  const removeServer = async (id: string): Promise<void> => {
    await window.api.streaming.removeServer(id);
    removeServerTracks(id);
    servers.value = servers.value.filter((s) => s.id !== id);
    if (activeServerId.value === id) {
      activeServerId.value = null;
      connectionStatus.value = { connected: false };
      clearMemoryLists();
    }
  };

  /**
   * 通过主进程测试服务器连接
   * @param input - 用户填的表单
   * @param serverId - 编辑中的服务器 ID
   * @returns 连通性结果
   */
  const testConnection = async (
    input: StreamingServerInput,
    serverId?: string,
  ): Promise<StreamingPingResult> => {
    return window.api.streaming.testConnection(toRaw(input), serverId);
  };

  /**
   * 从主进程读取媒体库快照
   * @param serverId - 服务器 ID
   */
  const loadLibrarySnapshot = async (serverId: string): Promise<void> => {
    const seq = ++snapshotFetchSeq;
    try {
      const snapshot = await window.api.streaming.getSnapshot(serverId);
      if (seq !== snapshotFetchSeq || activeServerId.value !== serverId || !snapshot) return;
      songs.value = snapshot.songs ?? [];
      albums.value = snapshot.albums ?? [];
      artists.value = snapshot.artists ?? [];
      playlists.value = snapshot.playlists ?? [];
    } catch (err) {
      console.error("[streaming] loadLibrarySnapshot failed:", err);
    }
  };

  /**
   * 显示本地快照并请求主进程后台刷新
   * @param force - 是否强制重新同步
   */
  const refreshLibrary = async (force = false): Promise<void> => {
    const id = activeServerId.value;
    if (!id) return;
    loading.value = true;
    try {
      await loadLibrarySnapshot(id);
      await window.api.streaming.sync(id, force);
    } finally {
      loading.value = false;
    }
  };

  /**
   * 运行一次带状态同步的连接
   * @param id - 目标 server id
   * @param isActive - 校验是否仍为当前激活服务器
   * @returns 连接结果
   */
  const runConnect = async (
    id: string,
    isActive: () => boolean,
  ): Promise<{ ok: true } | { ok: false; error: string; code?: StreamingErrorCode }> => {
    const cfg = servers.value.find((server) => server.id === id);
    if (!cfg) return { ok: false, error: "找不到服务器配置", code: "unknown" };
    /** 只更新仍然激活的服务器状态 */
    const writeStatus = (next: typeof connectionStatus.value): void => {
      if (isActive()) connectionStatus.value = next;
    };
    try {
      const result = await window.api.streaming.connect(id);
      if (result.ok) {
        const index = servers.value.findIndex((server) => server.id === id);
        if (index >= 0) {
          const list = [...servers.value];
          list[index] = result.server;
          servers.value = list;
        }
        writeStatus({ connected: true });
        return { ok: true };
      }
      writeStatus({ connected: false, error: result.error, errorCode: result.code });
      return result;
    } catch (err) {
      const error = err instanceof Error ? err.message : String(err);
      const code = "unknown" as const;
      writeStatus({ connected: false, error, errorCode: code });
      return { ok: false, error, code };
    }
  };

  /**
   * 连接到指定服务器
   * @param id - 目标 server id
   * @returns 是否连接成功
   */
  const connectToServer = async (id: string): Promise<boolean> => {
    const r = await runConnect(id, () => id === activeServerId.value);
    return r.ok;
  };

  /**
   * 设为激活服务器并触发连接；id 与当前相同（断开状态重连）也会再走一遍
   * @param id - 目标 server id；传 null 则仅清空激活态
   */
  const setActiveServer = async (id: string | null): Promise<void> => {
    if (id !== activeServerId.value) {
      activeServerId.value = id;
      connectionStatus.value = { connected: false };
      clearMemoryLists();
      await window.api.streaming.setActiveServer(id);
    }
    if (!id) return;
    await connectToServer(id);
  };

  /** 断开当前激活服务器 */
  const disconnect = (): void => {
    const id = activeServerId.value;
    if (id) void window.api.streaming.disconnect(id);
    connectionStatus.value = { connected: false };
  };

  /**
   * 读取专辑歌曲
   * @param albumId - 服务端专辑 ID
   * @returns 专辑歌曲
   */
  const ensureActiveServerId = async (): Promise<string> => {
    if (!initialized) {
      await init();
    }
    if (activeServerId.value) return activeServerId.value;
    if (servers.value.length > 0) {
      activeServerId.value = servers.value[0].id;
      return servers.value[0].id;
    }
    throw new Error("没有激活的流媒体服务器");
  };

  /**
   * 读取专辑歌曲
   * @param albumId - 服务端专辑 ID
   * @returns 专辑歌曲
   */
  const fetchAlbumSongs = async (albumId: string): Promise<Track[]> => {
    const srvId = await ensureActiveServerId();
    return window.api.streaming.getAlbumSongs(srvId, albumId);
  };

  /**
   * 读取歌单歌曲
   * @param playlistId - 服务端歌单 ID
   * @returns 歌单歌曲
   */
  const fetchPlaylistSongs = async (playlistId: string): Promise<Track[]> => {
    const srvId = await ensureActiveServerId();
    return window.api.streaming.getPlaylistSongs(srvId, playlistId);
  };

  /**
   * 读取歌手专辑
   * @param artistId - 服务端歌手 ID
   * @returns 歌手专辑
   */
  const fetchArtistAlbums = async (artistId: string): Promise<Album[]> => {
    const srvId = await ensureActiveServerId();
    return window.api.streaming.getArtistAlbums(srvId, artistId);
  };

  /**
   * 读取歌手歌曲
   * @param artistId - 服务端歌手 ID
   * @returns 歌手歌曲
   */
  const fetchArtistSongs = async (artistId: string): Promise<Track[]> => {
    const srvId = await ensureActiveServerId();
    return window.api.streaming.getArtistSongs(srvId, artistId);
  };

  /**
   * 在激活服务器上搜索（歌曲/专辑/歌手聚合）
   * @param query - 搜索关键词
   * @returns 聚合搜索结果
   */
  const search = async (query: string): Promise<StreamingSearchResult> => {
    const srvId = await ensureActiveServerId();
    return window.api.streaming.search(srvId, query);
  };

  /**
   * 取流播放 URL
   * 非激活服务器静默重连
   * @param track - source="streaming" 的 Track（必须带 serverId/originalId）
   * @param opts.playSessionId - 覆盖默认 PlaySessionId；用于背景缓存下载与播放流并发时区分会话
   * @returns 当前会话可用的播放地址
   */
  const getStreamUrl = async (track: Track, opts?: { playSessionId?: string }): Promise<string> => {
    let serverId = track.serverId;
    let originalId = track.originalId;
    if (!serverId && track.id?.includes(":")) {
      const parts = track.id.split(":");
      serverId = parts[0];
      originalId = originalId || parts.slice(1).join(":");
    }
    serverId = serverId || activeServerId.value || "";
    originalId = originalId || track.id;

    const cfg = servers.value.find((server) => server.id === serverId) || activeServer.value;
    if (!cfg) throw new Error("找不到流媒体服务器配置");

    const sessionId = opts?.playSessionId ?? session.sessionIdForTrack(track.id);
    return window.api.streaming.getStreamUrl(cfg.id, originalId, sessionId);
  };

  /** 初始化服务器配置和媒体库更新订阅 */
  const init = async (): Promise<void> => {
    if (initialized) return;
    window.api.streaming.onLibraryUpdated(async (serverId) => {
      if (serverId !== activeServerId.value) return;
      await loadLibrarySnapshot(serverId);
      if (serverId === activeServerId.value) loading.value = false;
    });
    const result = await window.api.streaming.loadServers();
    servers.value = Array.isArray(result?.servers) ? result.servers : [];
    activeServerId.value = result?.activeServerId ?? null;
    if (activeServerId.value && !servers.value.find((s) => s.id === activeServerId.value)) {
      activeServerId.value = null;
    }
    initialized = true;
    if (activeServerId.value) void connectToServer(activeServerId.value);
  };

  return {
    servers,
    activeServerId,
    activeServer,
    connectionStatus,
    loading,
    hasServer,
    isConnected,
    songs,
    albums,
    artists,
    playlists,
    init,
    addServer,
    updateServer,
    removeServer,
    setActiveServer,
    connectToServer,
    disconnect,
    testConnection,
    refreshLibrary,
    fetchAlbumSongs,
    fetchPlaylistSongs,
    fetchArtistAlbums,
    fetchArtistSongs,
    search,
    getStreamUrl,
  };
});
