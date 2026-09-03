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

/** link() 结果 */
export interface ListenBrainzLinkResult {
  /** 是否绑定成功 */
  ok: boolean;
  /** 成功时的账号名 */
  account?: string;
  /** 失败原因 */
  error?: string;
}

/** 渲染进程 ListenBrainz API（window.api.listenbrainz） */
export interface ListenBrainzApi {
  /** 查询连接状态（不包含 token） */
  getStatus: () => Promise<ListenBrainzStatus>;
  /** 绑定 Token */
  link: (token: string) => Promise<ListenBrainzLinkResult>;
  /** 解除绑定 */
  unlink: () => Promise<void>;
}
