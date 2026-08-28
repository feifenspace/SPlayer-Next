import { describe, it, expect, vi, beforeEach } from "vitest";
import { HttpPlayerClient } from "./httpClient";
import { ElectronPlayerClient } from "./electronClient";
import { getClient, isElectron } from "./index";
import type { PlayerEvent } from "./types";

describe("Client Adaptation Layer", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  describe("isElectron detection and getClient", () => {
    it("should return false when window.electron is undefined", () => {
      delete (window as any).electron;
      expect(isElectron()).toBe(false);
      const client = getClient();
      expect(client.isElectron).toBe(false);
      expect(client.mode).toBe("http");
      expect(client.player).toBeInstanceOf(HttpPlayerClient);
    });

    it("should return true when window.electron.ipcRenderer is defined", () => {
      (window as any).electron = { ipcRenderer: {} };
      expect(isElectron()).toBe(true);
      const electronPlayer = new ElectronPlayerClient();
      expect(electronPlayer).toBeDefined();
      delete (window as any).electron;
    });
  });

  describe("HttpPlayerClient", () => {
    it("should format REST API requests properly", async () => {
      const client = new HttpPlayerClient("http://127.0.0.1:14558", "ws://127.0.0.1:14558/ws");

      const mockFetch = vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({ success: true, data: { status: "playing" } }),
      });
      global.fetch = mockFetch;

      const res = await client.play();
      expect(mockFetch).toHaveBeenCalledWith("http://127.0.0.1:14558/api/v1/player/play", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Accept: "application/json",
        },
      });
      expect(res.success).toBe(true);

      client.destroy();
    });

    it("should handle seek by converting ms to seconds", async () => {
      const client = new HttpPlayerClient("http://127.0.0.1:14558", "ws://127.0.0.1:14558/ws");

      const mockFetch = vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({ success: true, data: { position_secs: 15.5 } }),
      });
      global.fetch = mockFetch;

      await client.seek(15500);
      expect(mockFetch).toHaveBeenCalledWith(
        "http://127.0.0.1:14558/api/v1/player/seek",
        expect.objectContaining({
          method: "POST",
          body: JSON.stringify({ position_secs: 15.5 }),
        }),
      );

      client.destroy();
    });

    it("should dispatch and receive events via onEvent", () => {
      const client = new HttpPlayerClient("http://127.0.0.1:14558", "ws://127.0.0.1:14558/ws");
      const events: PlayerEvent[] = [];

      const unsubscribe = client.onEvent((e) => events.push(e));
      client.dispatch("play");

      expect(events).toHaveLength(1);
      expect(events[0]).toEqual({ type: "play" });

      unsubscribe();
      client.dispatch("pause");
      expect(events).toHaveLength(1);

      client.destroy();
    });

    it("should handle explicit ended event from WebSocket without false trigger on stopped", () => {
      const client = new HttpPlayerClient("http://127.0.0.1:14558", "ws://127.0.0.1:14558/ws");
      const events: PlayerEvent[] = [];
      client.onEvent((e) => events.push(e));

      // 模拟先处于 playing 状态
      (client as any).handleWsMessage({ state: "playing", position: 10, duration: 100 });
      // 模拟切歌时服务端进入 stopped 状态（不应触发 ended）
      (client as any).handleWsMessage({ state: "stopped", position: 0, duration: 100 });

      const endedEvents = events.filter((e) => e.type === "ended");
      expect(endedEvents).toHaveLength(0);

      // 模拟服务端显式推送 ended 事件
      (client as any).handleWsMessage({ type: "ended" });
      const newEndedEvents = events.filter((e) => e.type === "ended");
      expect(newEndedEvents).toHaveLength(1);

      client.destroy();
    });
  });
});
