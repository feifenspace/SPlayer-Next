/** 平台类型 */
export type Platform = "netease" | "qqmusic" | "kugou" | "qobuz" | "tidal";

/** 搜索平台类型（含本地音乐库） */
export type SearchPlatform = Platform | "local";

/** 平台简写 */
export const PLATFORM_SHORT_NAME: Record<Platform, string> = {
  netease: "NCM",
  qqmusic: "QM",
  kugou: "KG",
  qobuz: "Qobuz",
  tidal: "TIDAL",
};

/** 搜索平台简写（含本地） */
export const SEARCH_PLATFORM_SHORT_NAME: Record<SearchPlatform, string> = {
  local: "本地",
  netease: "NCM",
  qqmusic: "QM",
  kugou: "KG",
  qobuz: "Qobuz",
  tidal: "TIDAL",
};

/** 全部在线平台 */
export const ALL_PLATFORMS: Platform[] = ["netease", "qqmusic", "kugou", "qobuz", "tidal"];

/** 全部搜索平台（本地优先，符合 HiFi 播放器以本地音乐为主的定位） */
export const ALL_SEARCH_PLATFORMS: SearchPlatform[] = [
  "local",
  "qobuz",
  "tidal",
  "netease",
  "qqmusic",
  "kugou",
];

const PLATFORM_SET = new Set<string>(ALL_PLATFORMS);

/** 判断给定 source 是否为在线平台（netease / qqmusic / kugou），同时类型收窄 */
export const isPlatform = (source: string | undefined): source is Platform =>
  source !== undefined && PLATFORM_SET.has(source);

/** 平台用户账号资料 */
export interface PlatformProfile {
  userId: string;
  nickname: string;
  avatarUrl: string;
  isVip: boolean;
  vipLevel?: number;
}
