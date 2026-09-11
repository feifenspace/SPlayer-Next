import { describe, expect, it, vi } from "vitest";
import { createWebPlaylistApi } from "./webPlaylistApi";

const response = <T>(data: T) => ({ success: true, data });
const client = () => ({
  getPlaylists: vi.fn(),
  getPlaylist: vi.fn(),
  createPlaylist: vi.fn(),
  updatePlaylist: vi.fn(),
  removePlaylist: vi.fn(),
  addPlaylistTracks: vi.fn(),
  removePlaylistTracks: vi.fn(),
});

describe("Web 歌单适配", () => {
  it("更新后读取服务端详情，返回完整歌单", async () => {
    const http = client();
    http.updatePlaylist.mockResolvedValue(response({ updated: "p1" }));
    http.getPlaylist.mockResolvedValue(response({ id: "p1", title: "new", tracks: [] }));
    expect(await createWebPlaylistApi(http as any).update("p1", { title: "new" })).toMatchObject({
      title: "new",
    });
    expect(http.getPlaylist).toHaveBeenCalledWith("p1");
  });

  it("返回服务端实际添加和移除数量", async () => {
    const http = client();
    http.addPlaylistTracks.mockResolvedValue(response({ added_count: 2 }));
    http.removePlaylistTracks.mockResolvedValue(response({ removed_count: 3 }));
    const api = createWebPlaylistApi(http as any);
    await expect(api.addTracks("p1", ["a", "b"])).resolves.toBe(2);
    await expect(api.removeTracks("p1", ["a", "b", "c"])).resolves.toBe(3);
  });

  it("旧歌单迁移成功后才继续处理下一项", async () => {
    const http = client();
    http.createPlaylist
      .mockResolvedValueOnce(response({ id: "p1" }))
      .mockResolvedValueOnce(response({ id: "p2" }));
    http.addPlaylistTracks.mockResolvedValue(response({ added_count: 1 }));
    const api = createWebPlaylistApi(http as any);
    await api.importLegacy([
      { id: "old", title: "one", trackIds: ["a"] },
      { id: "old2", title: "two", trackIds: [] },
    ]);
    expect(http.addPlaylistTracks).toHaveBeenCalledWith("p1", ["a"]);
    expect(http.createPlaylist).toHaveBeenCalledTimes(2);
  });

  it("请求失败会抛出，调用方不会把服务端失败当作完成", async () => {
    const http = client();
    http.removePlaylist.mockResolvedValue({ success: false, error: "offline" });
    await expect(createWebPlaylistApi(http as any).remove("p1")).rejects.toThrow("offline");
  });
});
