import { ElectronPlayerClient } from "./electronClient";
import { HttpPlayerClient } from "./httpClient";
import type { IAppClient, IPlayerClient, ClientMode } from "./types";

export * from "./types";
export { ElectronPlayerClient } from "./electronClient";
export { HttpPlayerClient } from "./httpClient";

/**
 * 判断当前是否处于 Electron 桌面端环境
 * 注意：Web 模式下 webPolyfill 会注入 window.api，因此必须以 window.electron.ipcRenderer 作为原生桌面端判断基准
 */
export const isElectron = (): boolean => {
  return (
    typeof window !== "undefined" &&
    typeof (window as any).electron !== "undefined" &&
    typeof (window as any).electron.ipcRenderer !== "undefined"
  );
};

let clientInstance: IAppClient | null = null;

/**
 * 获取全局统一 Client 实例
 * 运行时自动识别 Electron 桌面端与 Web 端
 */
export const getClient = (): IAppClient => {
  if (clientInstance) {
    return clientInstance;
  }

  const electron = isElectron();
  const mode: ClientMode = electron ? "electron" : "http";
  const player: IPlayerClient = electron
    ? new ElectronPlayerClient()
    : new HttpPlayerClient();

  clientInstance = {
    mode,
    player,
    isElectron: electron,
  };

  return clientInstance;
};

/**
 * 便捷导出播放器客户端单例
 */
export const playerClient = new Proxy({} as IPlayerClient, {
  get(_target, prop: keyof IPlayerClient) {
    const client = getClient();
    const val = client.player[prop];
    if (typeof val === "function") {
      return val.bind(client.player);
    }
    return val;
  },
});
