export interface ServerQueueItem {
  source: string;
  duration_ms: number | null;
  title: string | null;
  artist: string | null;
  album: string | null;
  cover: string | null;
  /** 完整曲目快照（新版快照携带；旧版快照/未推快照时为 null） */
  track?: Track | null;
}

export interface ServerQueueSnapshot {
  registered: boolean;
  items: ServerQueueItem[];
  index: number;
  pos: number;
  repeat: string;
  total: number;
}

import type {
  PlayerApi,
  PlayerEvent,
  PlayerState,
  PlayerStatus,
  LoadOptions,
  LoadResult,
  Track,
  AudioQuality,
  IpcResponse,
  AudioDevice,
  FftData,
} from "@shared/types/player";

export type {
  PlayerApi,
  PlayerEvent,
  PlayerState,
  PlayerStatus,
  LoadOptions,
  LoadResult,
  Track,
  AudioQuality,
  IpcResponse,
  AudioDevice,
  FftData,
};

export interface DirettaTarget {
  ipv6_addr: string;
  full_addr: string;
  if_idx: number;
  target_name: string;
  output_name: string;
  model_name: string;
  mtu: number;
}

export interface DirettaStatus {
  selected_device: string | null;
  is_diretta_active: boolean;
}

export interface DirettaTargetCapabilities {
  target_address: string;
  target_name: string;
  output_name: string;
  firmware_version: string;
  ipv6_addr: string;
  full_addr: string;
  if_idx: number;
  pcm_format_desc: string;
  dsd_format_desc: string;
  transmission_mode: string;
  mtu: number;
  mtu_measured: number;
  mtu_min: number;
  mtu_req: number;
  mtu_max: number;
  max_packet_size: number;
  supports_pcm: boolean;
  pcm_min_sample_rate: number;
  pcm_max_sample_rate: number;
  pcm_min_bits: number;
  pcm_max_bits: number;
  pcm_min_channels: number;
  pcm_max_channels: number;
  pcm_channels: number;
  supports_dsd: boolean;
  supports_dsd_lsb: boolean;
  supports_dsd_msb: boolean;
  supports_native_dsd: boolean;
  dsd_min_sample_rate: number;
  dsd_max_sample_rate: number;
  dsd_min_bits: number;
  dsd_max_bits: number;
  dsd_min_channels: number;
  dsd_max_channels: number;
  support_ms_mode: number;
  bit_perfect_supported: boolean;
  available: boolean;
}

export type ClientMode = "electron" | "http";

export interface IPlayerClient extends PlayerApi {
  /** 是否支持服务端自动连播（headless HTTP 为 true，桌面 IPC 为 false） */
  readonly supportsServerAutoAdvance: boolean;
  scanDirettaTargets(): Promise<IpcResponse<DirettaTarget[]>>;
  getDirettaStatus(): Promise<IpcResponse<DirettaStatus>>;
  selectDirettaTarget(target: string | null): Promise<IpcResponse<any>>;
  getDirettaTargetInfo(target: string): Promise<IpcResponse<DirettaTargetCapabilities>>;
  browseFs(path?: string): Promise<IpcResponse<any>>;
  /** 注册下一曲候选（headless 自动连播；浏览器关闭后服务端仍可在曲终接续） */
  registerNextCandidate(source: string, durationHintSecs?: number): Promise<IpcResponse>;
  /** 清除下一曲候选 */
  clearNextCandidate(): Promise<IpcResponse>;
  /**
   * 推送服务端播放队列快照（headless 服务端自治无缝预载）：
   * 注册后服务端自治完成"下一曲 stage → boundary 簿记/队列推进 → 再预载"，
   * 浏览器离场无缝播放与曲终接力照常
   */
  pushQueueSnapshot(payload: {
    items: Array<{
      source: string;
      duration_ms: number | null;
      title: string | null;
      artist: string | null;
      album: string | null;
      cover: string | null;
    }>;
    index: number;
    repeat: string;
    shuffle: boolean;
  }): Promise<IpcResponse>;
  /** 服务端权威"正在播放"快照（headless：重开页面恢复曲目显示） */
  /** 获取服务端权威播放队列快照（headless：重开/刷新页面恢复对齐队列与索引） */
  getQueueSnapshot(): Promise<IpcResponse<ServerQueueSnapshot>>;
  getNowPlaying(): Promise<IpcResponse<any>>;
}

export interface IAppClient {
  readonly mode: ClientMode;
  readonly player: IPlayerClient;
  readonly isElectron: boolean;
}
