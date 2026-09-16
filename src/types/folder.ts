import type { Track } from "@shared/types/player";

/** 文件夹树节点 */
export interface FolderNode {
  /** 目录显示名 */
  name: string;
  /** 完整目录路径 */
  path: string;
  children: FolderNode[];
  /** 服务端统计的自身及子目录曲目数 */
  trackCount?: number;
  /** 自身及子目录下的全部曲目 */
  tracks: Track[];
}
