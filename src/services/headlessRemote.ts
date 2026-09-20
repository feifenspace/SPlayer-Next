import type {
  AudioDevice,
  IpcResponse,
  LoadOptions,
  LoadResult,
  PlayerEvent,
  PlayerStatus,
  Track,
  PlaybackQueueItem,
} from "@shared/types/player";

const STORAGE_KEY = "splayer.headless.remote.server";
const DEFAULT_TIMEOUT_MS = 8000;

export interface HeadlessServerProfile {
  id: string;
  name: string;
  baseUrl: string;
  token?: string;
}

export interface HeadlessQueueSnapshot {
  registered: boolean;
  items: PlaybackQueueItem[];
  index?: number;
  pos?: number;
  repeat?: string;
  total?: number;
}

export interface HeadlessNowPlaying {
  track_id?: string | null;
  track?: Track | null;
  source?: string | null;
  metadata?: Record<string, unknown> | null;
}

type Listener = (event: PlayerEvent) => void;

const normalizeBaseUrl = (value: string): string => value.trim().replace(/\/+$/, "");

const readProfile = (): HeadlessServerProfile | null => {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return null;
    const value = JSON.parse(raw) as Partial<HeadlessServerProfile>;
    if (!value.baseUrl) return null;
    return {
      id: value.id || normalizeBaseUrl(value.baseUrl),
      name: value.name || value.baseUrl,
      baseUrl: normalizeBaseUrl(value.baseUrl),
      token: value.token || undefined,
    };
  } catch {
    return null;
  }
};

const toPlayerState = (value: unknown): PlayerStatus["state"] => {
  switch (String(value ?? "").toLowerCase()) {
    case "playing": return "playing";
    case "paused": return "paused";
    case "loading": return "loading";
    case "stopped": return "stopped";
    default: return "idle";
  }
};

const unwrap = <T>(body: unknown): T => {
  if (body && typeof body === "object" && "success" in body) {
    const response = body as { success?: boolean; data?: T; error?: unknown };
    if (response.success === false) {
      throw new Error(typeof response.error === "string" ? response.error : "Headless API request failed");
    }
    return (response.data ?? {}) as T;
  }
  return body as T;
};

const toLoadResult = (track: Track | undefined): LoadResult => ({
  detail: {
    quality: track?.quality ?? {
      sampleRate: 0,
      channels: 0,
      bitsPerSample: 0,
      bitRate: 0,
      codec: "",
    },
    externalLyrics: [],
  },
  mediaInfo: {
    title: track?.title,
    artists: track?.artists,
    album: track?.album,
    duration: track?.duration ?? 0,
    cover: track?.cover,
    quality: track?.quality,
  },
});

class HeadlessRemoteClient {
  private profile: HeadlessServerProfile | null = readProfile();
  private socket: WebSocket | null = null;
  private socketGeneration = 0;
  private listeners = new Set<Listener>();
  private snapshot: PlayerStatus = {
    state: "idle",
    position: 0,
    duration: 0,
    volume: 1,
    speed: 1,
    isFinished: false,
  };

  configure(profile: Omit<HeadlessServerProfile, "id"> & { id?: string }): void {
    const next: HeadlessServerProfile = {
      id: profile.id || normalizeBaseUrl(profile.baseUrl),
      name: profile.name,
      baseUrl: normalizeBaseUrl(profile.baseUrl),
      token: profile.token || undefined,
    };
    localStorage.setItem(STORAGE_KEY, JSON.stringify(next));
    this.profile = next;
    this.disconnect();
  }

  getProfile(): HeadlessServerProfile | null {
    return this.profile;
  }

  isConfigured(): boolean {
    return this.profile !== null;
  }

  private requireProfile(): HeadlessServerProfile {
    if (!this.profile) throw new Error("Headless server is not configured");
    return this.profile;
  }

  private headers(): HeadersInit {
    const profile = this.requireProfile();
    return {
      Accept: "application/json",
      "Content-Type": "application/json",
      ...(profile.token ? { Authorization: `Bearer ${profile.token}` } : {}),
    };
  }

  async request<T>(path: string, init: RequestInit = {}): Promise<T> {
    const profile = this.requireProfile();
    const controller = new AbortController();
    const timer = window.setTimeout(() => controller.abort(), DEFAULT_TIMEOUT_MS);
    try {
      const response = await fetch(`${profile.baseUrl}${path}`, {
        ...init,
        headers: { ...this.headers(), ...(init.headers ?? {}) },
        signal: init.signal ?? controller.signal,
        cache: "no-store",
      });
      const text = await response.text();
      let body: unknown = {};
      try {
        body = text ? JSON.parse(text) : {};
      } catch {
        throw new Error(`Invalid JSON response (HTTP ${response.status})`);
      }
      if (!response.ok) throw new Error(`Headless request failed: HTTP ${response.status}`);
      return unwrap<T>(body);
    } finally {
      window.clearTimeout(timer);
    }
  }

  async connect(): Promise<void> {
    await this.request<Record<string, unknown>>("/api/status");
    this.connectWebSocket();
  }

  disconnect(): void {
    this.socketGeneration++;
    const socket = this.socket;
    this.socket = null;
    if (socket) socket.close();
  }

  private connectWebSocket(): void {
    const profile = this.requireProfile();
    const generation = ++this.socketGeneration;
    const wsBase = profile.baseUrl.replace(/^http/i, "ws");
    const token = profile.token ? `?token=${encodeURIComponent(profile.token)}` : "";
    const socket = new WebSocket(`${wsBase}/ws${token}`);
    this.socket = socket;
    socket.onmessage = (event) => {
      if (generation !== this.socketGeneration) return;
      let message: { type?: string; data?: unknown };
      try {
        message = JSON.parse(String(event.data)) as typeof message;
      } catch {
        return;
      }
      this.handleMessage(message.type, message.data);
    };
    socket.onclose = () => {
      if (generation !== this.socketGeneration) return;
      this.socket = null;
      window.setTimeout(() => {
        if (generation === this.socketGeneration && this.profile) this.connectWebSocket();
      }, 1500);
    };
  }

  private handleMessage(type: string | undefined, data: unknown): void {
    if (type === "snapshot" && data && typeof data === "object") {
      const raw = data as Record<string, unknown>;
      this.snapshot = {
        state: toPlayerState(raw.state),
        position: Number(raw.position ?? 0),
        duration: Number(raw.duration ?? 0),
        volume: Number(raw.volume ?? 1),
        speed: Number(raw.speed ?? 1),
        isFinished: Boolean(raw.is_finished ?? false),
      };
      this.emit({ type: "status", data: this.snapshot });
      this.emit({
        type: "position",
        data: {
          position: this.snapshot.position,
          duration: this.snapshot.duration,
          authoritative: true,
        },
      });
      return;
    }
    if (type === "play") this.emit({ type: "play" });
    else if (type === "pause") this.emit({ type: "pause" });
    else if (type === "ended") this.emit({ type: "ended" });
    else if (type === "sourceError") this.emit({ type: "sourceError" });
    else if (type === "next") this.emit({ type: "next" });
    else if (type === "prev") this.emit({ type: "prev" });
  }

  onEvent(listener: Listener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  private emit(event: PlayerEvent): void {
    for (const listener of this.listeners) listener(event);
  }

  async status(): Promise<IpcResponse<PlayerStatus>> {
    const raw = await this.request<Record<string, unknown>>("/api/status");
    this.snapshot = {
      state: toPlayerState(raw.state),
      position: Number(raw.position ?? 0),
      duration: Number(raw.duration ?? 0),
      volume: Number(raw.volume ?? 1),
      speed: Number(raw.speed ?? 1),
      isFinished: Boolean(raw.is_finished ?? false),
    };
    return { success: true, data: this.snapshot };
  }

  async queue(): Promise<HeadlessQueueSnapshot> {
    return this.request<HeadlessQueueSnapshot>("/api/v1/player/queue");
  }

  async nowPlaying(): Promise<HeadlessNowPlaying> {
    return this.request<HeadlessNowPlaying>("/api/v1/player/now-playing");
  }

  async devices(): Promise<AudioDevice[]> {
    const data = await this.request<unknown>("/api/v1/player/devices");
    const list = Array.isArray(data) ? data : [];
    return list.map((item) => {
      if (Array.isArray(item)) {
        return {
          id: String(item[0] ?? ""),
          name: String(item[1] ?? item[0] ?? ""),
          isDefault: Boolean(item[2]),
        };
      }
      const value = item as Record<string, unknown>;
      return {
        id: String(value.id ?? ""),
        name: String(value.name ?? value.description ?? value.id ?? ""),
        isDefault: Boolean(value.is_default ?? value.isDefault),
      };
    });
  }

  private async control(path: string, body?: unknown): Promise<IpcResponse> {
    await this.request(path, {
      method: "POST",
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { success: true };
  }

  async load(source: string, options?: LoadOptions): Promise<IpcResponse<LoadResult>> {
    await this.request("/api/v1/player/load", {
      method: "POST",
      body: JSON.stringify({
        source,
        auto_play: options?.autoPlay ?? true,
        meta: options?.meta
          ? {
              id: options.meta.id,
              title: options.meta.title,
              artist: options.meta.artists?.map((item) => item.name).join(", "),
              album: options.meta.album?.name,
              duration: options.meta.duration,
              track: options.meta.track,
              cue_path: options.meta.cuePath,
              cue_audio_path: options.meta.cueAudioPath,
              cue_start_ms: options.meta.cueStartMs,
              cue_end_ms: options.meta.cueEndMs,
            }
          : undefined,
      }),
    });
    return { success: true, data: toLoadResult(options?.meta) };
  }

  play(): Promise<IpcResponse> { return this.control("/api/v1/player/play"); }
  pause(): Promise<IpcResponse> { return this.control("/api/v1/player/pause"); }
  stop(): Promise<IpcResponse> { return this.control("/api/v1/player/stop"); }
  seek(positionMs: number): Promise<IpcResponse> {
    return this.control("/api/v1/player/seek", { position_secs: positionMs / 1000 });
  }
  setVolume(volume: number): Promise<IpcResponse> {
    return this.control("/api/v1/player/volume", { volume });
  }
  setOutputDevice(deviceId: string | null): Promise<IpcResponse> {
    return this.control("/api/v1/diretta/select", { target: deviceId });
  }
}

export const headlessRemote = new HeadlessRemoteClient();
export const configureHeadlessServer = (profile: Omit<HeadlessServerProfile, "id"> & { id?: string }): void =>
  headlessRemote.configure(profile);
export const getHeadlessServer = (): HeadlessServerProfile | null => headlessRemote.getProfile();
export const isHeadlessServerConfigured = (): boolean => headlessRemote.isConfigured();
export const connectHeadlessServer = (): Promise<void> => headlessRemote.connect();
export const disconnectHeadlessServer = (): void => headlessRemote.disconnect();
export const getHeadlessQueue = (): Promise<HeadlessQueueSnapshot> => headlessRemote.queue();
export const getHeadlessNowPlaying = (): Promise<HeadlessNowPlaying> => headlessRemote.nowPlaying();
export const getHeadlessDevices = (): Promise<AudioDevice[]> => headlessRemote.devices();
