import { BrowserWindow } from "electron";
import { getMainWindow } from "@main/window";
import { playerLog } from "@main/utils/logger";

/**
 * 向所有窗口广播事件
 * @param channel 通道名称
 * @param data 发送的数据
 * @param visibleOnly 仅发给可见窗口
 */
export const broadcast = (channel: string, data: unknown, visibleOnly = false): void => {
  for (const win of BrowserWindow.getAllWindows()) {
    if (win.isDestroyed()) continue;
    if (visibleOnly && !win.isVisible()) continue;
    win.webContents.send(channel, data);
  }
};

/**
 * 向主窗口推送事件
 * @param channel 通道名称
 * @param data 发送的数据
 */
export const sendToMain = (channel: string, data?: unknown): void => {
  const win = getMainWindow();
  if (win && !win.isDestroyed()) {
    win.webContents.send(channel, data);
  } else {
    // renderer 崩溃/未就绪时，外部 API 的 /next 等队列级调用会走到这里：
    // 静默丢弃会让调用方无从排查，至少留下一条日志
    playerLog.warn(`sendToMain 丢弃事件 ${channel}：主窗口不存在（renderer 未就绪或已崩溃）`);
  }
};
