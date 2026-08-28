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
  pcm_max_sample_rate: number;
  pcm_max_bits: number;
  pcm_channels: number;
  supports_dsd: boolean;
  dsd_max_sample_rate: number;
  dsd_format_desc: string;
  pcm_format_desc: string;
  supports_native_dsd: boolean;
  mtu: number;
  transmission_mode: string;
  bit_perfect_supported: boolean;
}

export type ClientMode = "electron" | "http";

export interface IPlayerClient extends PlayerApi {
  scanDirettaTargets(): Promise<IpcResponse<DirettaTarget[]>>;
  getDirettaStatus(): Promise<IpcResponse<DirettaStatus>>;
  selectDirettaTarget(target: string | null): Promise<IpcResponse<any>>;
  getDirettaTargetInfo(target: string): Promise<IpcResponse<DirettaTargetCapabilities>>;
  browseFs(path?: string): Promise<IpcResponse<any>>;
}

export interface IAppClient {
  readonly mode: ClientMode;
  readonly player: IPlayerClient;
  readonly isElectron: boolean;
}
