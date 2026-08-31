import type {
  IPlayerClient,
  LoadOptions,
  LoadResult,
  IpcResponse,
  PlayerStatus,
  PlayerState,
  AudioDevice,
  FftData,
  PlayerEvent,
} from "./types";

interface ServerStatusResponse {
  state: string;
  position: number;
  duration: number;
  volume: number;
  is_finished?: boolean;
  current_source?: string | null;
  speed?: number;
}

interface ServerWsMessage {
  state: string;
  position: number;
  duration: number;
  volume: number;
  is_finished?: boolean;
  speed?: number;
}

/**
 * Web / HTTP + WebSocket 播放器客户端实现
 * 对接 Linux Headless Server (Axum)
 */
export class HttpPlayerClient implements IPlayerClient {
  private baseUrl: string;
  private wsUrl: string;
  private token: string | null = null;
  private ws: WebSocket | null = null;
  private eventListeners = new Set<(event: PlayerEvent) => void>();
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private isDestroyed = false;

  constructor(baseUrl?: string, wsUrl?: string) {
    if (typeof window !== "undefined") {
      const protocol = window.location.protocol === "https:" ? "https:" : "http:";
      const wsProtocol = window.location.protocol === "https:" ? "wss:" : "ws:";
      const host = window.location.host;

      this.baseUrl = baseUrl || `${protocol}//${host}`;
      this.wsUrl = wsUrl || `${wsProtocol}//${host}/ws`;
    } else {
      this.baseUrl = baseUrl || "http://127.0.0.1:14558";
      this.wsUrl = wsUrl || "ws://127.0.0.1:14558/ws";
    }

    if (typeof window !== "undefined") {
      this.initWebSocket();
    }
  }

  public setToken(token: string | null): void {
    this.token = token;
    if (this.ws) {
      this.ws.close();
    }
  }

  private getAuthHeaders(): Record<string, string> {
    const headers: Record<string, string> = {
      "Content-Type": "application/json",
      Accept: "application/json",
    };
    if (this.token) {
      headers.Authorization = `Bearer ${this.token}`;
    }
    return headers;
  }

  private async request<T = unknown>(
    path: string,
    options: RequestInit = {},
  ): Promise<IpcResponse<T>> {
    try {
      const url = `${this.baseUrl}${path}`;
      const res = await fetch(url, {
        ...options,
        headers: {
          ...this.getAuthHeaders(),
          ...(options.headers || {}),
        },
      });

      if (!res.ok) {
        return {
          success: false,
          error: `HTTP ${res.status}: ${res.statusText}`,
        };
      }

      const json = await res.json();
      // 如果后端包装了 PlayerResponse { success, data, error }
      if (typeof json === "object" && json !== null && "success" in json) {
        return {
          success: json.success,
          data: json.data as T,
          error: json.error ? JSON.stringify(json.error) : undefined,
        };
      }

      return {
        success: true,
        data: json as T,
      };
    } catch (e) {
      return {
        success: false,
        error: (e as Error).message || "Network error",
      };
    }
  }

  private initWebSocket(): void {
    if (this.isDestroyed || typeof WebSocket === "undefined") return;

    try {
      const wsUrlWithToken = this.token
        ? `${this.wsUrl}?token=${encodeURIComponent(this.token)}`
        : this.wsUrl;

      this.ws = new WebSocket(wsUrlWithToken);

      this.ws.onmessage = (event) => {
        try {
          const data: ServerWsMessage = JSON.parse(event.data);
          this.handleWsMessage(data);
        } catch {
          // ignore non-json messages
        }
      };

      this.ws.onclose = () => {
        this.ws = null;
        if (!this.isDestroyed) {
          this.scheduleReconnect();
        }
      };

      this.ws.onerror = () => {
        if (this.ws) {
          this.ws.close();
        }
      };
    } catch {
      this.scheduleReconnect();
    }
  }

  private scheduleReconnect(): void {
    if (this.reconnectTimer || this.isDestroyed) return;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.initWebSocket();
    }, 3000);
  }

  private normalizeState(serverState: string): PlayerState {
    const s = serverState.toLowerCase();
    if (s === "playing") return "playing";
    if (s === "paused") return "paused";
    if (s === "stopped") return "stopped";
    return "idle";
  }

  private scanProgressListeners = new Set<(event: any) => void>();

  private handleWsMessage(msg: any): void {
    if (msg.event === "scan_progress" && msg.data) {
      for (const listener of this.scanProgressListeners) {
        try {
          listener(msg.data);
        } catch (e) {
          console.error("[HttpClient] Error in scan listener", e);
        }
      }
      return;
    }

    if (
      msg.type === "ended" ||
      (msg.kind === "event" && msg.type === "ended") ||
      msg.event === "ended"
    ) {
      this.emitEvent({ type: "ended" });
      return;
    }

    if (
      msg.type === "sourceError" ||
      (msg.kind === "event" && msg.type === "sourceError") ||
      msg.event === "sourceError"
    ) {
      this.emitEvent({ type: "sourceError" });
      return;
    }

    if (!msg || typeof msg.state !== "string") return;

    const currentState = this.normalizeState(msg.state);
    const posMs = Math.round((msg.position || 0) * 1000);
    const durMs = Math.round((msg.duration || 0) * 1000);

    // 1. 推送位置
    this.emitEvent({
      type: "position",
      data: {
        position: posMs,
        duration: durMs,
      },
    });

    // 2. 推送状态快照
    this.emitEvent({
      type: "status",
      data: {
        state: currentState,
        position: posMs,
        duration: durMs,
        volume: msg.volume ?? 1.0,
        isFinished: Boolean(msg.is_finished),
        speed: Number(msg.speed ?? 1.0),
      },
    });
  }

  // -------------------------------------------------------------
  // 音乐库与扫描 API
  // -------------------------------------------------------------

  async getLibraryTracks(): Promise<IpcResponse<any[]>> {
    const res = await this.request<any[]>("/api/v1/library/tracks");
    if (!res.success) return { success: false, data: [] };
    return { success: true, data: (res as any).data ?? [] };
  }

  async getLibraryAlbums(): Promise<IpcResponse<any[]>> {
    const res = await this.request<any[]>("/api/v1/library/albums");
    if (!res.success) return { success: false, data: [] };
    return { success: true, data: (res as any).data ?? [] };
  }

  async getLibraryArtists(): Promise<IpcResponse<any[]>> {
    const res = await this.request<any[]>("/api/v1/library/artists");
    if (!res.success) return { success: false, data: [] };
    return { success: true, data: (res as any).data ?? [] };
  }

  async getLibraryAlbumTracks(name: string): Promise<IpcResponse<any[]>> {
    const res = await this.request<any[]>(`/api/v1/library/albums/${encodeURIComponent(name)}/tracks`);
    if (!res.success) return { success: false, data: [] };
    return { success: true, data: (res as any).data ?? [] };
  }

  async getLibraryArtistTracks(name: string): Promise<IpcResponse<any[]>> {
    const res = await this.request<any[]>(`/api/v1/library/artists/${encodeURIComponent(name)}/tracks`);
    if (!res.success) return { success: false, data: [] };
    return { success: true, data: (res as any).data ?? [] };
  }

  async getLibraryScanDirs(): Promise<IpcResponse<string[]>> {
    const res = await this.request<string[]>("/api/v1/library/scan_dirs");
    if (!res.success) return { success: false, data: [] };
    return { success: true, data: (res as any).data ?? [] };
  }

  async addLibraryScanDir(dirPath: string): Promise<IpcResponse> {
    return this.request("/api/v1/library/scan_dirs", {
      method: "POST",
      body: JSON.stringify({ path: dirPath }),
    });
  }

  async removeLibraryScanDir(dirPath: string): Promise<IpcResponse> {
    return this.request("/api/v1/library/scan_dirs", {
      method: "DELETE",
      body: JSON.stringify({ path: dirPath }),
    });
  }

  async startLibraryScan(incremental = true): Promise<IpcResponse> {
    return this.request("/api/v1/library/scan", {
      method: "POST",
      body: JSON.stringify({ incremental }),
    });
  }

  async cancelLibraryScan(): Promise<IpcResponse> {
    return this.request("/api/v1/library/cancel_scan", {
      method: "POST",
    });
  }

  async isLibraryScanning(): Promise<IpcResponse<{ is_scanning: boolean }>> {
    return this.request("/api/v1/library/scan/status");
  }

  async browseFs(path?: string): Promise<IpcResponse<{
    current_path: string;
    parent_path: string | null;
    dirs: Array<{ name: string; path: string; has_children?: boolean; is_dir?: boolean }>;
    audio_count: number;
  }>> {
    const url = path ? `/api/v1/fs/browse?path=${encodeURIComponent(path)}` : "/api/v1/fs/browse";
    const res = await this.request<any>(url);
    if (!res.success) {
      return {
        success: false,
        data: { current_path: path || "/", parent_path: null, dirs: [], audio_count: 0 },
        error: res.error,
      };
    }
    return { success: true, data: (res as any).data };
  }

  onScanProgress(callback: (event: any) => void): () => void {
    this.scanProgressListeners.add(callback);
    return () => {
      this.scanProgressListeners.delete(callback);
    };
  }

  // -------------------------------------------------------------
  // 歌单持久化 API
  // -------------------------------------------------------------

  async getPlaylists(): Promise<IpcResponse<any[]>> {
    const res = await this.request<any[]>("/api/v1/playlist/list");
    if (!res.success) return { success: false, data: [] };
    return { success: true, data: (res as any).data ?? [] };
  }

  async getPlaylist(id: string): Promise<IpcResponse<any>> {
    const res = await this.request<any>(`/api/v1/playlist/${encodeURIComponent(id)}`);
    return res;
  }

  async createPlaylist(input: { id?: string; title?: string; name?: string; description?: string; cover?: string }): Promise<IpcResponse<any>> {
    return this.request("/api/v1/playlist/create", {
      method: "POST",
      body: JSON.stringify({
        id: input.id,
        title: input.title || input.name || "新歌单",
        description: input.description,
        cover: input.cover,
      }),
    });
  }

  async updatePlaylist(id: string, input: { title?: string; name?: string; description?: string; cover?: string }): Promise<IpcResponse<any>> {
    return this.request(`/api/v1/playlist/${encodeURIComponent(id)}`, {
      method: "PUT",
      body: JSON.stringify({
        title: input.title || input.name,
        description: input.description,
        cover: input.cover,
      }),
    });
  }

  async removePlaylist(id: string): Promise<IpcResponse<any>> {
    return this.request(`/api/v1/playlist/${encodeURIComponent(id)}`, {
      method: "DELETE",
    });
  }

  async addPlaylistTracks(id: string, tracksOrIds: any[]): Promise<IpcResponse<any>> {
    const trackIds = tracksOrIds.map((item) => (typeof item === "string" ? item : item.id));
    return this.request(`/api/v1/playlist/${encodeURIComponent(id)}/tracks`, {
      method: "POST",
      body: JSON.stringify({ track_ids: trackIds, tracks: tracksOrIds }),
    });
  }

  async removePlaylistTracks(id: string, tracksOrIds: any[]): Promise<IpcResponse<any>> {
    const trackIds = tracksOrIds.map((item) => (typeof item === "string" ? item : item.id));
    return this.request(`/api/v1/playlist/${encodeURIComponent(id)}/tracks`, {
      method: "DELETE",
      body: JSON.stringify({ track_ids: trackIds }),
    });
  }

  // -------------------------------------------------------------
  // 用户设置持久化 API
  // -------------------------------------------------------------

  async getAllConfig(): Promise<IpcResponse<Record<string, any>>> {
    const res = await this.request<Record<string, any>>("/api/v1/config/all");
    if (!res.success) return { success: false, data: {} };
    return { success: true, data: (res as any).data ?? {} };
  }

  async getConfig(key: string): Promise<IpcResponse<any>> {
    const res = await this.request<any>(`/api/v1/config/${encodeURIComponent(key)}`);
    return res;
  }

  async setConfig(key: string, value: any): Promise<IpcResponse<any>> {
    return this.request("/api/v1/config/set", {
      method: "POST",
      body: JSON.stringify({ key, value }),
    });
  }

  async setAllConfig(settings: Record<string, any>): Promise<IpcResponse<any>> {
    return this.request("/api/v1/config/set", {
      method: "POST",
      body: JSON.stringify({ settings }),
    });
  }

  async resetConfig(): Promise<IpcResponse<any>> {
    return this.request("/api/v1/config/reset", {
      method: "POST",
    });
  }

  // -------------------------------------------------------------
  // 播放统计与历史 API
  // -------------------------------------------------------------

  async recordPlayHistory(payload: any): Promise<IpcResponse<any>> {
    return this.request("/api/v1/stats/record", {
      method: "POST",
      body: JSON.stringify(payload),
    });
  }

  async getPlayHistory(limit = 100): Promise<IpcResponse<any[]>> {
    const res = await this.request<any[]>(`/api/v1/stats/history?limit=${limit}`);
    if (!res.success) return { success: false, data: [] };
    return { success: true, data: (res as any).data ?? [] };
  }

  async getLibraryStats(): Promise<IpcResponse<any>> {
    return this.request("/api/v1/stats/summary");
  }

  // -------------------------------------------------------------
  // 歌词 API
  // -------------------------------------------------------------

  async getFileLyric(path: string): Promise<IpcResponse<any>> {
    return this.request(`/api/v1/lyrics/file?path=${encodeURIComponent(path)}`);
  }

  // -------------------------------------------------------------
  // 在线音源 API 统一调用
  // -------------------------------------------------------------

  async callApi(platform: string, name: string, params: Record<string, any> = {}): Promise<any> {
    const res = await this.request<any>("/api/v1/proxy/apis/call", {
      method: "POST",
      body: JSON.stringify({ platform, name, params }),
    });
    if (res.success && (res as any).data) {
      return (res as any).data;
    }
    return { ok: false, error: res.error || "Request failed" };
  }

  // -------------------------------------------------------------
  // Diretta Audio-over-IP API
  // -------------------------------------------------------------

  async scanDirettaTargets(): Promise<IpcResponse<any[]>> {
    const res = await this.request<any[]>("/api/v1/diretta/scan");
    if (!res.success) return { success: false, data: [] };
    return { success: true, data: (res as any).data ?? [] };
  }

  async getDirettaStatus(): Promise<IpcResponse<any>> {
    return this.request("/api/v1/diretta/status");
  }

  async selectDirettaTarget(target: string | null): Promise<IpcResponse<any>> {
    return this.request("/api/v1/diretta/select", {
      method: "POST",
      body: JSON.stringify({ target }),
    });
  }

  async getDirettaTargetInfo(target: string): Promise<IpcResponse<any>> {
    return this.request("/api/v1/diretta/target_info", {
      method: "POST",
      body: JSON.stringify({ target }),
    });
  }

  private emitEvent(event: PlayerEvent): void {
    for (const listener of this.eventListeners) {
      try {
        listener(event);
      } catch (e) {
        console.error("[HttpClient] Error in event listener", e);
      }
    }
  }

  // --- IPlayerClient 接口实现 ---

  async load(source: string, options?: LoadOptions): Promise<IpcResponse<LoadResult>> {
    // 若为冷启动恢复（autoPlay: false，如 initPlayer），且服务端当前已处于播放/暂停状态，直接复用状态，不重载打断
    if (options?.autoPlay === false) {
      try {
        const statusRes = await this.getStatus();
        if (
          statusRes.success &&
          statusRes.data &&
          (statusRes.data.state === "playing" || statusRes.data.state === "paused")
        ) {
          return {
            success: true,
            data: {
              detail: {
                quality: {
                  sampleRate: 44100,
                  channels: 2,
                  bitsPerSample: 16,
                  bitRate: 1411200,
                  codec: "flac",
                },
                externalLyrics: [],
              },
              mediaInfo: {
                duration: statusRes.data.duration,
                cover: options?.meta?.cover,
              },
            },
          };
        }
      } catch {
        // 忽略状态探测错误
      }
    }

    const res = await this.request<{
      status: string;
      source: string;
      title?: string;
      artist?: string;
      album?: string;
      duration?: number;
      sample_rate?: number;
      original_sample_rate?: number;
      channels?: number;
      bits_per_sample?: number;
      bit_rate?: number;
      codec?: string;
      cover?: string;
    }>(
      "/api/v1/player/load",
      {
        method: "POST",
        body: JSON.stringify({
          source,
          auto_play: options?.autoPlay ?? true,
          meta: options?.meta
            ? {
                id: options.meta.id,
                title: options.meta.title,
                artist: Array.isArray(options.meta.artists)
                  ? options.meta.artists.map((a: any) => (typeof a === "string" ? a : a.name)).join(", ")
                  : (options.meta as any).artist,
                album:
                  typeof options.meta.album === "string"
                    ? options.meta.album
                    : options.meta.album?.name,
                duration: options.meta.duration,
                track: options.meta.track,
                cue_path: options.meta.cuePath,
                cue_audio_path: options.meta.cueAudioPath,
                cue_start_ms: options.meta.cueStartMs,
                cue_end_ms: options.meta.cueEndMs,
              }
            : undefined,
        }),
      },
    );

    if (!res.success) {
      return { success: false, error: res.error };
    }

    const payload = res.data;
    const sampleRate = payload?.original_sample_rate || payload?.sample_rate || 44100;
    const channels = payload?.channels || 2;
    const bitsPerSample = payload?.bits_per_sample || 16;
    const bitRate = payload?.bit_rate || (bitsPerSample * sampleRate * channels);
    const codec = payload?.codec || (source.endsWith(".flac") ? "flac" : source.endsWith(".mp3") ? "mp3" : "flac");

    const loadResult: LoadResult = {
      detail: {
        quality: {
          sampleRate,
          channels,
          bitsPerSample,
          bitRate,
          codec,
        },
        externalLyrics: [],
      },
      mediaInfo: {
        duration: payload?.duration != null ? Math.round(payload.duration * 1000) : (options?.meta?.duration ?? 0),
        cover: payload?.cover ?? options?.meta?.cover,
      },
    };

    return {
      success: true,
      data: loadResult,
    };
  }

  async play(): Promise<IpcResponse> {
    return this.request("/api/v1/player/play", { method: "POST" });
  }

  async pause(): Promise<IpcResponse> {
    return this.request("/api/v1/player/pause", { method: "POST" });
  }

  async stop(): Promise<IpcResponse> {
    return this.request("/api/v1/player/stop", { method: "POST" });
  }

  async seek(positionMs: number): Promise<IpcResponse> {
    return this.request("/api/v1/player/seek", {
      method: "POST",
      body: JSON.stringify({
        position_secs: positionMs / 1000,
      }),
    });
  }

  async setVolume(volume: number): Promise<IpcResponse> {
    return this.request("/api/v1/player/volume", {
      method: "POST",
      body: JSON.stringify({ volume }),
    });
  }

  async getVolume(): Promise<IpcResponse<number>> {
    const res = await this.getStatus();
    if (!res.success || !res.data) {
      return { success: false, error: res.error };
    }
    return { success: true, data: res.data.volume };
  }

  async getStatus(): Promise<IpcResponse<PlayerStatus>> {
    const res = await this.request<ServerStatusResponse>("/api/status");
    if (!res.success || !res.data) {
      return { success: false, error: res.error };
    }
    const data = res.data;
    return {
      success: true,
      data: {
        state: this.normalizeState(data.state),
        position: Math.round(data.position * 1000),
        duration: Math.round(data.duration * 1000),
        volume: data.volume,
        isFinished: Boolean(data.is_finished),
        speed: data.speed ?? 1.0,
      },
    };
  }

  async setFftEnabled(_enabled: boolean): Promise<IpcResponse> {
    return { success: true };
  }

  async getFftData(): Promise<IpcResponse<FftData>> {
    return {
      success: true,
      data: {
        ldata: new Array(64).fill(0),
        rdata: new Array(64).fill(0),
      },
    };
  }

  async setFadeDuration(_ms: number): Promise<IpcResponse> {
    return { success: true };
  }

  async getFadeDuration(): Promise<IpcResponse<number>> {
    return { success: true, data: 0 };
  }

  async getCoverRaw(): Promise<IpcResponse<string | null>> {
    return { success: true, data: null };
  }

  async readLyricFile(_filePath: string): Promise<IpcResponse<string>> {
    return {
      success: false,
      error: "External lyric file reading is not available in web mode",
    };
  }

  async reinit(): Promise<IpcResponse> {
    return { success: true };
  }

  async setNormalizationEnabled(_enabled: boolean): Promise<IpcResponse> {
    return { success: true };
  }

  async setEqualizerEnabled(_enabled: boolean): Promise<IpcResponse> {
    return { success: true };
  }

  async setEqualizerBands(_gainsDb: number[]): Promise<IpcResponse> {
    return { success: true };
  }

  async setPreampGain(_preampDb: number): Promise<IpcResponse> {
    return { success: true };
  }

  async setSpeed(_speed: number): Promise<IpcResponse> {
    return { success: true };
  }

  async setPitch(_semitones: number): Promise<IpcResponse> {
    return { success: true };
  }

  async setPitchSync(_sync: boolean): Promise<IpcResponse> {
    return { success: true };
  }

  // 设备切换前暂停开关：headless 模式无热拔插场景，Diretta 目标切换由
  // selectDirettaTarget 内部处理，此处按 Web 降级语义接受但不生效
  async setPauseOnDeviceSwitch(_enabled: boolean): Promise<IpcResponse> {
    return { success: true };
  }

  async getOutputDevices(): Promise<IpcResponse<AudioDevice[]>> {
    return { success: true, data: [] };
  }

  async getDefaultDeviceName(): Promise<IpcResponse<string | null>> {
    return { success: true, data: null };
  }

  async setOutputDevice(deviceName: string | null): Promise<IpcResponse> {
    return this.selectDirettaTarget(deviceName);
  }

  async getSelectedDeviceName(): Promise<IpcResponse<string | null>> {
    const status = await this.getDirettaStatus();
    if (status.success && status.data) {
      return { success: true, data: (status.data as any).selected_device ?? null };
    }
    return { success: true, data: null };
  }

  syncPlayMode(_repeatMode: string, _shuffleMode: string): void {
    // Web 模式无需向托盘同步
  }

  syncLikeState(_liked: boolean): void {
    // Web 模式无需向托盘同步
  }

  dispatch(type: string): void {
    if (type === "play" || type === "pause" || type === "next" || type === "prev") {
      this.emitEvent({ type: type as "play" | "pause" | "next" | "prev" });
    }
  }

  onEvent(callback: (event: PlayerEvent) => void): () => void {
    this.eventListeners.add(callback);
    return () => {
      this.eventListeners.delete(callback);
    };
  }

  destroy(): void {
    this.isDestroyed = true;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.ws) {
      this.ws.close();
      this.ws = null;
    }
    this.eventListeners.clear();
  }
}
