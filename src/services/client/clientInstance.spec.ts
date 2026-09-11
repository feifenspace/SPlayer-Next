import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const { constructed } = vi.hoisted(() => ({ constructed: vi.fn() }));

vi.mock("./httpClient", () => ({
  HttpPlayerClient: class {
    constructor() {
      constructed();
    }
  },
}));
vi.mock("@/stores/status", () => ({ useStatusStore: vi.fn() }));
vi.mock("@/services/streaming/web/service", () => ({
  createWebStreamingApi: () => ({}),
}));

describe("Web Client 生命周期", () => {
  beforeEach(() => {
    vi.resetModules();
    constructed.mockClear();
    vi.stubGlobal("electron", undefined);
    vi.stubGlobal("api", undefined);
  });

  afterEach(() => vi.unstubAllGlobals());

  it("兼容桥先初始化时，业务入口复用同一连接", async () => {
    await import("./webPolyfill");
    const { getClient } = await import("./index");
    expect(window.api.player).toBe(getClient().player);
    expect(getClient()).toBe(getClient());
    expect(constructed).toHaveBeenCalledTimes(1);
  });

  it("业务入口先初始化时，兼容桥复用同一连接", async () => {
    const { getClient } = await import("./index");
    const client = getClient();
    await import("./webPolyfill");
    expect(window.api.player).toBe(client.player);
    expect(constructed).toHaveBeenCalledTimes(1);
  });

  it("重复安装保留已有接口对象", async () => {
    const { installWebPolyfill } = await import("./webPolyfill");
    const api = window.api;
    installWebPolyfill();
    expect(window.api).toBe(api);
    expect(constructed).toHaveBeenCalledTimes(1);
  });

  it("桌面环境保留原生接口且不创建 HTTP 连接", async () => {
    const nativeApi = { player: {} };
    vi.stubGlobal("electron", { ipcRenderer: {} });
    vi.stubGlobal("api", nativeApi);
    await import("./webPolyfill");
    const { getClient } = await import("./index");
    expect(getClient().mode).toBe("electron");
    expect(window.api).toBe(nativeApi);
    expect(constructed).not.toHaveBeenCalled();
  });
});
