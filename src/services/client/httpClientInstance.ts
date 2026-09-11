import { HttpPlayerClient } from "./httpClient";

let instance: HttpPlayerClient | undefined;

/** 兼容桥与 Client 入口共用连接，避免重复订阅服务端事件。 */
export const getHttpPlayerClient = (): HttpPlayerClient => {
  instance ??= new HttpPlayerClient();
  return instance;
};
