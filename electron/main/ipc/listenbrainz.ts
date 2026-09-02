import { ipcMain } from "electron";
import * as listenbrainz from "@main/services/listenbrainz";

/** 注册 ListenBrainz 相关 IPC */
export const registerListenBrainzIpc = (): void => {
  ipcMain.handle("listenbrainz:getStatus", () => listenbrainz.getStatus());
  ipcMain.handle("listenbrainz:link", (_event, token: string) => listenbrainz.link(token));
  ipcMain.handle("listenbrainz:unlink", () => listenbrainz.unlink());
};
