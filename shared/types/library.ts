import type { IpcResponse, Track } from "./player";
import type { TrackTags, TagEditRequest, TagWriteOutcome } from "./tagEditor";

/** 专辑聚合项 */
export interface AlbumSummary {
  name: string;
  cover?: string;
  artist: string;
  trackCount: number;
}

/** 歌手聚合项 */
export interface FolderSummary {
  name: string;
  path: string;
  trackCount: number;
}

export interface ArtistSummary {
  name: string;
  trackCount: number;
  /** 该歌手任一曲目的封面 */
  cover?: string;
}

/** 扫描进度事件 */
export interface ScanProgress {
  phase: "scanning" | "done" | "error";
  /** 总文件数 */
  total: number;
  /** 已扫描文件数 */
  scanned: number;
  /** 当前正在处理的文件名 */
  current?: string;
  /** 成功探测并写入的曲目数 */
  succeeded?: number;
  /** 探测失败或跳过的文件数 */
  failed?: number;
  /** 删除的记录数 */
  removed?: number;
  /** 本轮变化的 CUE 文件数 */
  cue_files?: number;
  /** 本轮变化的 SACD ISO 文件数 */
  iso_files?: number;
  /** 错误信息（仅 error 阶段） */
  error?: string;
}

/** 音乐库 API */
export interface LibraryApi {
  /**
   * 开始扫描
   * @param incremental 是否增量扫描（跳过未变化的文件）
   */
  scan: (incremental?: boolean) => Promise<IpcResponse>;
  /** 取消扫描 */
  cancelScan: () => Promise<IpcResponse>;
  /** 获取全部曲目 */
  getTracks: () => Promise<IpcResponse<Track[]>>;
  /** 分页获取曲目（Web/headless 模式，优先使用 cursor） */
  getTracksPage?: (
    limit?: number,
    offset?: number,
    query?: string,
    options?: {
      cursor?: string;
      sort?: string;
      order?: "asc" | "desc";
      codec?: string;
      sampleRate?: number;
    },
  ) => Promise<
    IpcResponse<{
      items: Track[];
      total: number;
      limit: number;
      offset: number;
      nextCursor?: string | null;
      hasMore: boolean;
    }>
  >;
  /** 获取专辑聚合列表 */
  getAlbums: () => Promise<IpcResponse<AlbumSummary[]>>;
  /** 分页获取专辑聚合列表（Web/headless 模式） */
  getAlbumsPage?: (
    limit?: number,
    offset?: number,
    query?: string,
    cursor?: string,
  ) => Promise<
    IpcResponse<{
      items: AlbumSummary[];
      total: number;
      limit: number;
      offset: number;
      nextCursor?: string | null;
      hasMore: boolean;
    }>
  >;
  /** 获取歌手聚合列表 */
  getArtists: () => Promise<IpcResponse<ArtistSummary[]>>;
  /** 分页获取歌手聚合列表（Web/headless 模式） */
  getArtistsPage?: (
    limit?: number,
    offset?: number,
    query?: string,
    cursor?: string,
  ) => Promise<
    IpcResponse<{
      items: ArtistSummary[];
      total: number;
      limit: number;
      offset: number;
      nextCursor?: string | null;
      hasMore: boolean;
    }>
  >;
  /** 获取某专辑下的全部曲目 */
  getAlbumTracks: (albumName: string) => Promise<IpcResponse<Track[]>>;
  /** 获取某歌手的全部曲目 */
  getArtistTracks: (artistName: string) => Promise<IpcResponse<Track[]>>;
  /** 按 ID 批量获取曲目 */
  getTracksByIds: (ids: string[]) => Promise<IpcResponse<Track[]>>;
  /** 搜索曲目 */
  searchTracks: (query: string) => Promise<IpcResponse<Track[]>>;
  /** 获取曲目总数 */
  getTrackCount: () => Promise<IpcResponse<number>>;
  /** 随机取一首曲目 */
  getRandomTrack: () => Promise<IpcResponse<Track | null>>;
  /** 随机取多首曲目 */
  getRandomTracks: (limit: number) => Promise<IpcResponse<Track[]>>;
  /** 获取扫描状态 */
  isScanning: () => Promise<IpcResponse<boolean>>;
  /** 弹出目录选择器，添加扫描目录（Web 模式下直接传入路径） */
  addScanDir: (dirPath?: string) => Promise<IpcResponse<string>>;
  /** 移除扫描目录及其下曲目 */
  removeScanDir: (dir: string) => Promise<IpcResponse>;
  /** 获取轻量目录索引 */
  getFolders?: () => Promise<IpcResponse<FolderSummary[]>>;
  /** 分页获取目录下曲目 */
  getFolderTracksPage?: (
    path: string,
    limit?: number,
    offset?: number,
  ) => Promise<
    IpcResponse<{ items: Track[]; total: number; limit: number; offset: number; hasMore: boolean }>
  >;
  /** 获取已配置的扫描目录 */
  getScanDirs: () => Promise<IpcResponse<string[]>>;
  /** 删除曲目文件并从数据库移除 */
  deleteTracks: (paths: string[]) => Promise<IpcResponse<{ deleted: number; failed: number }>>;
  clearLibrary?: () => Promise<IpcResponse<{ deleted: number }>>;
  /** 读取本地文件的可编辑标签 */
  readTags: (path: string) => Promise<IpcResponse<TrackTags>>;
  /** 批量写入文件标签 */
  writeTags: (edits: TagEditRequest[]) => Promise<IpcResponse<TagWriteOutcome[]>>;
  /** 弹出文件选择器，选择封面图片（返回路径与预览 dataUrl） */
  pickCoverImage: () => Promise<IpcResponse<{ path: string; dataUrl: string }>>;
  /** 获取本地歌手头像 */
  fetchArtistAvatar: (artistName: string) => Promise<IpcResponse<string | null>>;
  /** 批量预取歌手头像 */
  prefetchArtistAvatars: (artistNames: string[]) => Promise<IpcResponse<Record<string, string>>>;
  /** 订阅扫描进度事件 */
  onScanProgress: (callback: (progress: ScanProgress) => void) => () => void;
}
