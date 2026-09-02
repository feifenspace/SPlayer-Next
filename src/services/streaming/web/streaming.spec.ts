import { describe, it, expect, vi, beforeEach } from "vitest";
import { md5 } from "./md5";
import { subsonicWebAdapter } from "./subsonic";
import { jellyfinWebAdapter, authenticate } from "./jellyfin";
import { createWebStreamingApi } from "./service";
import type { StreamingRuntimeConfig, StreamingServerInput } from "@shared/types/streaming";

describe("Streaming Web Module", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  describe("MD5 Hash Utility", () => {
    it("should compute correct MD5 hashes for standard vectors", () => {
      expect(md5("")).toBe("d41d8cd98f00b204e9800998ecf8427e");
      expect(md5("hello")).toBe("5d41402abc4b2a76b9719d911017c592");
      expect(md5("password123")).toBe("482c811da5d5b4bc6d497ffa98491e38");
      expect(md5("中文测试")).toBe("089b4943ea034acfa445d050c7913e55");
    });
  });

  describe("Subsonic Web Adapter", () => {
    const mockConfig: StreamingRuntimeConfig = {
      id: "srv-subsonic-1",
      name: "My Navidrome",
      type: "navidrome",
      url: "https://music.example.com",
      username: "testuser",
      password: "testpassword",
      hasPassword: true,
    };

    it("should handle ping successfully", async () => {
      const mockFetch = vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({
          "subsonic-response": {
            status: "ok",
            version: "1.16.1",
            serverVersion: "0.52.0",
          },
        }),
      });
      global.fetch = mockFetch;

      const result = await subsonicWebAdapter.ping(mockConfig);
      expect(result.ok).toBe(true);
      expect(result.version).toBe("0.52.0");
      expect(mockFetch).toHaveBeenCalled();
      const calledUrl = mockFetch.mock.calls[0][0];
      expect(calledUrl).toContain("https://music.example.com/rest/ping");
      expect(calledUrl).toContain("u=testuser");
      expect(calledUrl).toContain("v=1.16.1");
    });

    it("should parse listSongs correctly into Unified Track structure", async () => {
      const mockFetch = vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({
          "subsonic-response": {
            status: "ok",
            searchResult3: {
              song: [
                {
                  id: "song-101",
                  title: "Test Song",
                  artist: "Test Artist",
                  artistId: "art-1",
                  album: "Test Album",
                  albumId: "alb-1",
                  duration: 215,
                  bitRate: 320,
                  samplingRate: 44100,
                  bitDepth: 16,
                  channelCount: 2,
                  suffix: "flac",
                  size: 25000000,
                  coverArt: "cover-101",
                },
              ],
            },
          },
        }),
      });
      global.fetch = mockFetch;

      const tracks = await subsonicWebAdapter.listSongs(mockConfig);
      expect(tracks.length).toBe(1);
      const t = tracks[0];
      expect(t.id).toBe("srv-subsonic-1:song-101");
      expect(t.source).toBe("streaming");
      expect(t.title).toBe("Test Song");
      expect(t.artists[0].name).toBe("Test Artist");
      expect(t.album?.name).toBe("Test Album");
      expect(t.duration).toBe(215000);
      expect(t.quality?.codec).toBe("flac");
      expect(t.quality?.sampleRate).toBe(44100);
      expect(t.cover).toContain("/rest/getCoverArt");
      expect(t.cover).toContain("id=cover-101");
    });

    it("should generate stream URL with correct auth params", async () => {
      const url = await subsonicWebAdapter.getStreamUrl(mockConfig, "song-101");
      expect(url).toContain("https://music.example.com/rest/stream");
      expect(url).toContain("id=song-101");
      expect(url).toContain("u=testuser");
      expect(url).not.toContain("f=json");
    });
  });

  describe("Jellyfin Web Adapter", () => {
    const mockConfig: StreamingRuntimeConfig = {
      id: "srv-jellyfin-1",
      name: "My Jellyfin",
      type: "jellyfin",
      url: "https://jellyfin.example.com",
      username: "jellyuser",
      password: "jellypassword",
      hasPassword: true,
      accessToken: "token-abc-123",
      userId: "user-guid-999",
    };

    it("should authenticate and extract token and userId", async () => {
      const mockFetch = vi.fn().mockResolvedValue({
        ok: true,
        status: 200,
        json: async () => ({
          AccessToken: "token-abc-123",
          User: { Id: "user-guid-999" },
        }),
      });
      global.fetch = mockFetch;

      const session = await authenticate(mockConfig);
      expect(session.accessToken).toBe("token-abc-123");
      expect(session.userId).toBe("user-guid-999");
      expect(mockFetch).toHaveBeenCalledWith(
        "https://jellyfin.example.com/Users/AuthenticateByName",
        expect.objectContaining({ method: "POST" }),
      );
    });

    it("should generate universal/stream URL with device and session ID", async () => {
      const streamUrl = await jellyfinWebAdapter.getStreamUrl(
        mockConfig,
        "item-123",
        "custom-session-uuid",
      );
      expect(streamUrl).toContain("https://jellyfin.example.com/Audio/item-123/stream");
      expect(streamUrl).toContain("UserId=user-guid-999");
      expect(streamUrl).toContain("PlaySessionId=custom-session-uuid");
      expect(streamUrl).toContain("api_key=token-abc-123");
    });
  });

  describe("WebStreamingApi Service", () => {
    it("should support adding, loading, and removing servers", async () => {
      const api = createWebStreamingApi();

      const input: StreamingServerInput = {
        name: "Test Navidrome Server",
        type: "navidrome",
        url: "https://navidrome.example.com",
        username: "demo",
        password: "secret",
      };

      const added = await api.addServer(input);
      expect(added.name).toBe("Test Navidrome Server");
      expect(added.hasPassword).toBe(true);
      expect(added.id).toBeDefined();

      const { servers, activeServerId } = await api.loadServers();
      expect(activeServerId).toBeNull();
      expect(servers.some((s) => s.id === added.id)).toBe(true);

      await api.setActiveServer(added.id);
      const afterActive = await api.loadServers();
      expect(afterActive.activeServerId).toBe(added.id);

      await api.removeServer(added.id);
      const afterRemove = await api.loadServers();
      expect(afterRemove.servers.some((s) => s.id === added.id)).toBe(false);
      expect(afterRemove.activeServerId).toBeNull();
    });

    it("should handle library update subscriptions", () => {
      const api = createWebStreamingApi();
      const listener = vi.fn();
      const unsub = api.onLibraryUpdated(listener);

      expect(typeof unsub).toBe("function");
      unsub();
    });

    it("should search across cached snapshots", async () => {
      const api = createWebStreamingApi();
      const emptyResult = await api.search("srv-non-exist", "keyword");
      expect(emptyResult.songs).toEqual([]);
      expect(emptyResult.albums).toEqual([]);
      expect(emptyResult.artists).toEqual([]);
    });
  });
});
