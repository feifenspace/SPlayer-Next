export type ListenBrainzRuntimeState =
  | "disabled"
  | "starting"
  | "unconfigured"
  | "ready"
  | "retrying"
  | "auth_error"
  | "error";

/** Control 可见状态；永远不包含 token。 */
export interface ListenBrainzStatus {
  enabled: boolean;
  sendNowPlaying: boolean;
  linked: boolean;
  account: string | null;
  state: ListenBrainzRuntimeState;
  pending: number;
  dead: number;
  lastError: string | null;
  processActive: boolean;
}

/** Core listen session 在达到阈值后发送给 child 的不可变快照。 */
export interface ListenBrainzTrackSnapshot {
  artistName: string;
  trackName: string;
  releaseName?: string;
  durationMs: number;
  trackNumber?: number;
  listenedAt: number;
}
