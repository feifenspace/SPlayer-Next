import { HttpPlayerClient } from "./httpClient";
import { useStatusStore } from "@/stores/status";

/**
 * 为纯 Web / 浏览器环境提供完备的 window.api polyfill，
 * 彻底消除桌面端专属 API 在 Web 运行时抛出的 TypeError / Uncaught Exception
 */
export const installWebPolyfill = (): void => {
  if (typeof window === "undefined" || (window.api && window.electron)) return;

  const playerClient = new HttpPlayerClient();

  const detectPlatform = (): NodeJS.Platform => {
    if (typeof navigator !== "undefined") {
      const ua = navigator.userAgent.toLowerCase();
      if (ua.includes("win")) return "win32";
      if (ua.includes("mac")) return "darwin";
    }
    return "linux";
  };

  const createSafeProxy = (name: string, defaults: Record<string, any> = {}): any => {
    return new Proxy(defaults, {
      get(target, prop) {
        if (prop === "then") return undefined;
        if (prop in target) return (target as any)[prop];
        return (..._args: unknown[]) => {
          // 事件监听类方法返回解绑函数
          if (
            typeof prop === "string" &&
            (prop.startsWith("on") || prop.startsWith("subscribe"))
          ) {
            return () => {};
          }
          // 列表类查询方法默认返回空数组，避免 .map / .filter 抛错
          if (
            typeof prop === "string" &&
            (prop.startsWith("get") || prop.startsWith("list") || prop.startsWith("fetch"))
          ) {
            return Promise.resolve([]);
          }
          return Promise.resolve({
            success: false,
            ok: false,
            data: [],
            error: `${name}.${String(prop)} is not supported in Web mode`,
          });
        };
      },
    });
  };

  const polyfillApi = {
    player: playerClient,
    system: {
      platform: detectPlatform(),
      installType: "portable",
      osInfo: {
        type: "Linux (Headless)",
        arch: "x86_64",
        release: "1.0.0",
      },
      toggleDevTools: async () => {},
      showInExplorer: async () => {},
      openLogsDir: async () => "",
      setLocale: () => {},
      focusMainWindow: async () => {},
      openSettings: async () => {},
      onOpenSettings: () => () => {},
      listFonts: async () => [],
      fetchRemoteBytes: async (url: string) => {
        try {
          const res = await fetch(url);
          if (!res.ok) return { success: false, data: null };
          const buf = await res.arrayBuffer();
          return { success: true, data: new Uint8Array(buf) };
        } catch {
          return { success: false, data: null };
        }
      },
      saveFile: async () => ({ success: false }),
      relaunch: async () => {},
      testNetworkProxy: async () => true,
      onProtocolUrl: () => () => {},
      consumePendingProtocolUrl: async () => null,
    },
    window: {
      isMaximized: async () => false,
      minimize: async () => {},
      maximize: async () => {},
      unmaximize: async () => {},
      toggleMaximize: async () => {},
      close: async () => {},
      onMaximizeChange: () => () => {},
      onMaximizedChange: () => () => {},
      isFullscreen: async () => false,
      setFullscreen: async () => {},
      toggleFullscreen: async () => {},
      onFullscreenChange: () => () => {},
      hide: async () => {},
      quit: async () => {},
      toggleDesktopLyric: async () => {},
      closeDesktopLyric: async () => {},
      isDesktopLyricOpen: async () => false,
      onDesktopLyricVisibilityChange: () => () => {},
      isDynamicIslandOpen: async () => false,
      toggleDynamicIsland: async () => {},
      closeDynamicIsland: async () => {},
      onDynamicIslandVisibilityChange: () => () => {},
      isTaskbarLyricOpen: async () => false,
      toggleTaskbarLyric: async () => {},
      closeTaskbarLyric: async () => {},
      onTaskbarLyricVisibilityChange: () => () => {},
    },
    hotkey: {
      getAll: async () => ({ bindings: {}, globalEnabled: false }),
      getConflicts: async () => [],
      onConflicts: () => () => {},
      onTrigger: () => () => {},
      set: async () => ({ bindings: {}, globalEnabled: false }),
      reset: async () => ({ bindings: {}, globalEnabled: false }),
      setGlobalEnabled: async () => ({ bindings: {}, globalEnabled: false }),
      probe: async () => false,
    },
    config: {
      get: async (key: string) => {
        const res = await playerClient.getConfig(key);
        return res.success ? (res as any).data : null;
      },
      set: async (key: string, value: any) => {
        await playerClient.setConfig(key, value);
      },
      getAll: async () => {
        const res = await playerClient.getAllConfig();
        return res.success ? (res as any).data : {};
      },
      reset: async () => {
        await playerClient.resetConfig();
      },
      replaceAll: async (settings: any) => {
        await playerClient.setAllConfig(settings);
      },
      exportToFile: async (payload: any) => {
        const json = JSON.stringify(payload, null, 2);
        const blob = new Blob([json], { type: "application/json" });
        const url = URL.createObjectURL(blob);
        const a = document.createElement("a");
        a.href = url;
        a.download = `splayer-config-${Date.now()}.json`;
        a.click();
        URL.revokeObjectURL(url);
        return { success: true };
      },
      importFromFile: async () => {
        return new Promise((resolve) => {
          const input = document.createElement("input");
          input.type = "file";
          input.accept = ".json";
          input.onchange = () => {
            const file = input.files?.[0];
            if (!file) return resolve({ success: false });
            const reader = new FileReader();
            reader.onload = () => {
              try {
                const data = JSON.parse(reader.result as string);
                resolve({ success: true, data: { main: data } });
              } catch {
                resolve({ success: false, error: "Invalid JSON" });
              }
            };
            reader.readAsText(file);
          };
          input.oncancel = () => resolve({ success: false });
          input.click();
        });
      },
      hasPendingRestartKeys: () => false,
      restartToApplySettings: async () => {
        window.location.reload();
      },
    },
    auth: {
      onTokenUpdate: () => () => {},
      fetchRemoteJson: async (url: string, init?: RequestInit) => {
        try {
          const res = await fetch(url, init);
          const body = await res.json().catch(() => ({}));
          return {
            ok: res.ok,
            status: res.status,
            body,
            data: body,
          };
        } catch {
          return {
            ok: false,
            status: 500,
            body: {},
            data: [],
          };
        }
      },
      clearSession: async () => {},
    },
    apis: {
      call: async (
        platform: string,
        name: string,
        params?: Record<string, unknown>,
      ) => {
        return playerClient.callApi(platform, name, params || {});
      },
      setCookie: async (platform: string, cookie: string) => {
        return playerClient.callApi(platform, "set_cookie", { cookie });
      },
      getCookie: async (platform: string) => {
        const res = await playerClient.callApi(platform, "get_cookie", {});
        return (res?.body as any)?.cookie || "";
      },
      clearSession: async (platform: string) => {
        await playerClient.callApi(platform, "clear_session", {});
      },
      openLoginWeb: async () => ({ ok: false, error: "not supported in web mode" }),
    },
    library: createSafeProxy("library", {
      getTracks: async () => playerClient.getLibraryTracks(),
      getScanDirs: async () => playerClient.getLibraryScanDirs(),
      addScanDir: async (dirPath?: string) => {
        if (!dirPath) return { success: false, error: "Path required" };
        const res = await playerClient.addLibraryScanDir(dirPath);
        return { success: res.success, data: dirPath, error: res.error };
      },
      removeScanDir: async (dirPath: string) => {
        const res = await playerClient.removeLibraryScanDir(dirPath);
        return { success: res.success, error: res.error };
      },
      getAlbums: async () => playerClient.getLibraryAlbums(),
      getArtists: async () => playerClient.getLibraryArtists(),
      getAlbumTracks: async (albumName: string) => playerClient.getLibraryAlbumTracks(albumName),
      getArtistTracks: async (artistName: string) => playerClient.getLibraryArtistTracks(artistName),
      prefetchArtistAvatars: async () => ({ success: true, data: {} }),
      fetchArtistAvatar: async (artistName: string) => {
        try {
          const resp = await playerClient.callApi("netease", "search", {
            keywords: artistName,
            type: 100,
            limit: 1,
          });
          const artist = resp?.body?.result?.artists?.[0];
          const avatar = artist?.picUrl || artist?.img1v1Url;
          if (avatar) {
            return { success: true, data: avatar };
          }
        } catch {}
        return { success: false, data: null };
      },
      isScanning: async () => playerClient.isLibraryScanning(),
      browseFs: async (path?: string) => playerClient.browseFs(path),
      scan: async (incremental?: boolean) => playerClient.startLibraryScan(incremental),
      cancelScan: async () => playerClient.cancelLibraryScan(),
      onScanProgress: (callback: (event: any) => void) => playerClient.onScanProgress(callback),
      deleteTracks: async () => ({ success: true, data: { deleted: 0, failed: 0 } }),
    }),
    playlist: createSafeProxy("playlist", {
      list: async () => {
        const res = await playerClient.getPlaylists();
        return res.success ? (res as any).data : [];
      },
      getAll: async () => {
        const res = await playerClient.getPlaylists();
        return res.success ? { success: true, data: (res as any).data } : { success: false, data: [] };
      },
      get: async (id: string) => {
        const res = await playerClient.getPlaylist(id);
        return res.success ? (res as any).data : null;
      },
      create: async (input: any) => {
        const res = await playerClient.createPlaylist(input);
        return res.success ? (res as any).data : { id: `pl-${Date.now()}`, type: "local", tracks: [], ...input };
      },
      update: async (id: string, input: any) => {
        const res = await playerClient.updatePlaylist(id, input);
        return res.success ? (res as any).data : { id, type: "local", tracks: [], ...input };
      },
      remove: async (id: string) => {
        const res = await playerClient.removePlaylist(id);
        return res.success;
      },
      addTracks: async (id: string, tracks: any[]) => {
        const res = await playerClient.addPlaylistTracks(id, tracks);
        return res.success;
      },
      removeTracks: async (id: string, tracks: any[]) => {
        const res = await playerClient.removePlaylistTracks(id, tracks);
        return res.success;
      },
      importLegacy: async () => true,
      clear: async () => true,
    }),
    plugins: createSafeProxy("plugins", {
      list: async () => [],
      onStatus: () => () => {},
      pickAndInstall: async () => ({ ok: false }),
      fetchMarket: async () => [],
    }),
    stats: createSafeProxy("stats", {
      getLibraryStats: async () => {
        const res = await playerClient.getLibraryStats();
        return res.success && (res as any).data
          ? (res as any).data
          : {
              totalTracks: 0,
              totalDuration: 0,
              totalArtists: 0,
              totalAlbums: 0,
            };
      },
      recordPlay: async (payload: any) => {
        await playerClient.recordPlayHistory(payload);
      },
      getPlayHistoryDaily: async () => {
        const res = await playerClient.getPlayHistory();
        return res.success ? (res as any).data : [];
      },
      getPlayHistoryHourly: async () => [],
    }),
    diretta: createSafeProxy("diretta", {
      scan: async () => playerClient.scanDirettaTargets(),
      getStatus: async () => playerClient.getDirettaStatus(),
      selectTarget: async (target: string | null) => playerClient.selectDirettaTarget(target),
      getTargetInfo: async (target: string) => playerClient.getDirettaTargetInfo(target),
    }),
    download: createSafeProxy("download", {
      getDir: async () => "",
      getTasks: async () => [],
      onProgress: () => () => {},
      onComplete: () => () => {},
      onError: () => () => {},
    }),
    cloud: createSafeProxy("cloud", {
      getStatus: async () => ({ loggedIn: false }),
      getList: async () => ({
        count: 0,
        size: "0",
        maxSize: "0",
        data: [],
      }),
    }),
    theme: {
      pickBackgroundImage: async () => {
        return new Promise<string | null>((resolve) => {
          const input = document.createElement("input");
          input.type = "file";
          input.accept = "image/*";
          input.onchange = () => {
            const file = input.files?.[0];
            if (!file) {
              resolve(null);
              return;
            }
            const reader = new FileReader();
            reader.onload = () => resolve(reader.result as string);
            reader.onerror = () => resolve(null);
            reader.readAsDataURL(file);
          };
          input.oncancel = () => resolve(null);
          input.click();
        });
      },
      clearBackgroundImages: async () => {},
    },
    cache: createSafeProxy("cache", {
      getStats: async () => [],
      getDir: async () => "",
      clear: async () => {},
      clearAllByKind: async () => {},
    }),
    lastfm: createSafeProxy("lastfm", {
      getStatus: async () => ({ connected: false }),
      connect: async () => ({ success: false }),
      cancelConnect: async () => {},
      disconnect: async () => {},
    }),
    externalApi: createSafeProxy("externalApi", {
      getStatus: async () => ({ running: true, port: 14558 }),
      restart: async () => ({ running: true, port: 14558 }),
      onStatus: () => () => {},
    }),
    mcp: createSafeProxy("mcp", {
      getStatus: async () => ({ running: false }),
      restart: async () => ({ running: false }),
      getClientConfigParams: async () => ({}),
      detectAgents: async () => [],
      injectAgentConfig: async () => ({ success: false }),
      onStatus: () => () => {},
    }),
    aiModel: createSafeProxy("aiModel", {
      list: async () => ({ models: [], activeId: null }),
      save: async () => ({ models: [], activeId: null }),
      setActive: async () => ({ models: [], activeId: null }),
      remove: async () => ({ models: [], activeId: null }),
    }),
    update: createSafeProxy("update", {
      check: async () => ({ hasUpdate: false }),
      onChecking: () => () => {},
      onUpdateAvailable: () => () => {},
      onUpdateNotAvailable: () => () => {},
      onProgress: () => () => {},
      onDownloaded: () => () => {},
      onError: () => () => {},
    }),
    desktopLyric: createSafeProxy("desktopLyric"),
    dynamicIsland: createSafeProxy("dynamicIsland"),
    taskbarLyric: createSafeProxy("taskbarLyric"),
    nowPlaying: {
      update: () => Promise.resolve({ success: true, ok: true }),
      setLyricOffset: (_id: string, offsetMs: number) => {
        try {
          const status = useStatusStore();
          status.lyricOffsetMs = Number(offsetMs) || 0;
        } catch {}
        return Promise.resolve({ success: true, ok: true });
      },
      requestSnapshot: () => Promise.resolve({ lyricOffsetMs: 0 }),
      onLyricOffsetChange: (_cb: any) => () => {},
    },
    lyrics: {
      matchById: async (platform: string, id: string) => {
        try {
          if (platform === "netease") {
            const resp = await playerClient.callApi("netease", "lyric_new", { id });
            const body = resp?.body;
            if (resp?.ok && body && body.code === 200) {
              const yrc = body.yrc?.lyric?.trim();
              const lrc = body.lrc?.lyric?.trim();
              const mainContent = yrc || lrc;
              const mainFormat: "yrc" | "lrc" = yrc ? "yrc" : "lrc";

              const yTrans = body.ytlrc?.lyric?.trim();
              const tTrans = body.tlyric?.lyric?.trim();
              const transContent = yTrans || tTrans;

              const yRoma = body.yromalrc?.lyric?.trim();
              const tRoma = body.romalrc?.lyric?.trim();
              const romaContent = yRoma || tRoma;

              if (mainContent) {
                return {
                  ok: true,
                  data: {
                    platform: "netease",
                    format: mainFormat,
                    content: mainContent,
                    translation: transContent || undefined,
                    translationFormat: transContent ? "lrc" : undefined,
                    romaji: romaContent || undefined,
                    romajiFormat: romaContent ? "lrc" : undefined,
                  },
                };
              }
            }

            // Fallback 到旧版 lyric 接口
            const fallbackResp = await playerClient.callApi("netease", "lyric", { id });
            const fallbackBody = fallbackResp?.body;
            if (fallbackResp?.ok && fallbackBody) {
              const lrc = fallbackBody.lrc?.lyric?.trim();
              if (lrc) {
                return {
                  ok: true,
                  data: {
                    platform: "netease",
                    format: "lrc",
                    content: lrc,
                    translation: fallbackBody.tlyric?.lyric?.trim() || undefined,
                    translationFormat: fallbackBody.tlyric?.lyric?.trim() ? "lrc" : undefined,
                    romaji: fallbackBody.romalrc?.lyric?.trim() || undefined,
                    romajiFormat: fallbackBody.romalrc?.lyric?.trim() ? "lrc" : undefined,
                  },
                };
              }
            }
          } else if (platform === "qqmusic") {
            const resp = await playerClient.callApi("qqmusic", "lyric", { songmid: id });
            const rawLyric = resp?.data?.lyric || resp?.body?.lyric;
            if (rawLyric && String(rawLyric).trim()) {
              return {
                ok: true,
                data: {
                  platform: "qqmusic",
                  format: "lrc",
                  content: String(rawLyric).trim(),
                },
              };
            }
          } else if (platform === "kugou") {
            const resp = await playerClient.callApi("kugou", "lyric", { hash: id });
            const rawLyric = resp?.data?.lyric || resp?.body?.lyric || resp?.data?.decodeContent;
            if (rawLyric && String(rawLyric).trim()) {
              return {
                ok: true,
                data: {
                  platform: "kugou",
                  format: "lrc",
                  content: String(rawLyric).trim(),
                },
              };
            }
          }
          return { ok: false, error: `No lyric found for ${platform}:${id}` };
        } catch (e) {
          return { ok: false, error: String(e) };
        }
      },
      matchByQuery: async (platform: string, track: any) => {
        try {
          const keyword = `${track?.title || ""} ${track?.artists?.[0]?.name || ""}`.trim();
          if (!keyword) return { ok: false, error: "Empty search keyword" };
          if (platform === "netease") {
            const resp = await playerClient.callApi("netease", "search", {
              keywords: keyword,
              type: 1,
              limit: 5,
            });
            const songs = resp?.body?.result?.songs || [];
            if (songs.length > 0 && songs[0].id) {
              return (polyfillApi.lyrics as any).matchById("netease", String(songs[0].id));
            }
          } else if (platform === "qqmusic") {
            const resp = await playerClient.callApi("qqmusic", "search", {
              keyword,
              keywords: keyword,
              limit: 5,
            });
            const list = resp?.data?.songs || resp?.data?.list || resp?.body?.list || [];
            if (list.length > 0 && (list[0].mid || list[0].id)) {
              return (polyfillApi.lyrics as any).matchById("qqmusic", list[0].mid || list[0].id);
            }
          } else if (platform === "kugou") {
            const resp = await playerClient.callApi("kugou", "search", {
              keyword,
              keywords: keyword,
              type: 1,
              limit: 5,
            });
            const list = resp?.data?.songs || [];
            if (list.length > 0 && (list[0].hash || list[0].id)) {
              return (polyfillApi.lyrics as any).matchById("kugou", list[0].hash || list[0].id);
            }
          }
          return { ok: false, error: "No match found" };
        } catch (e) {
          return { ok: false, error: String(e) };
        }
      },

      fetchTTMLOverlay: async (track: any, platform: string) => {
        try {
          const path = platform === "netease" ? "ncm-lyrics" : "qq-lyrics";
          const id = track?.id || track?.extId;
          if (!id) return { ok: false, error: "No track id" };
          const url = `https://cdn.jsdelivr.net/gh/Steve-xmh/amll-ttml-db@main/lyrics/${path}/${encodeURIComponent(
            id,
          )}.ttml`;
          const res = await fetch(url);
          if (res.ok) {
            const content = await res.text();
            if (content.trim()) {
              return { ok: true, data: content };
            }
          }
          return { ok: false, error: "TTML not found" };
        } catch (e) {
          return { ok: false, error: String(e) };
        }
      },
      matchLocalTTML: async () => ({ ok: false, data: null }),
      pickLyricRepoDir: async () => ({ ok: false }),
    },
    comments: {
      sources: async () => [
        { id: "builtin:netease", name: "网易云音乐", enabled: true },
      ],
      get: async (args: any) => {
        try {
          const id = args?.track?.id;
          if (!id) return { ok: false, error: "No track ID" };
          const limit = args?.pageSize || 20;
          const offset = ((args?.page || 1) - 1) * limit;
          const resp = await playerClient.callApi("netease", "comment_music", {
            id,
            limit,
            offset,
          });
          const body = resp?.body;
          if (resp?.ok && body) {
            const mapItem = (c: any) => ({
              id: String(c.commentId),
              content: c.content,
              time: c.time,
              likedCount: c.likedCount || 0,
              liked: Boolean(c.liked),
              user: {
                userId: String(c.user?.userId || ""),
                nickname: c.user?.nickname || "匿名用户",
                avatarUrl: c.user?.avatarUrl || "",
              },
            });
            const comments = (body.comments || []).map(mapItem);
            const hotComments = (body.hotComments || []).map(mapItem);
            return {
              ok: true,
              data: {
                total: body.total || comments.length,
                comments,
                hotComments,
                hasMore: Boolean(body.more),
              },
            };
          }
          return { ok: false, error: "Failed to fetch comments" };
        } catch (e) {
          return { ok: false, error: String(e) };
        }
      },
    },
    streaming: createSafeProxy("streaming"),
    recognition: createSafeProxy("recognition"),
  };

  (window as unknown as { api: unknown }).api = polyfillApi;
};

// 自动在模块导入时执行初始化
installWebPolyfill();
