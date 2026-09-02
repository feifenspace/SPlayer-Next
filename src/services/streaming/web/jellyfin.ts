import type { Album, Artist, Playlist, Track } from "@shared/types/player";
import type {
  StreamingListParams,
  StreamingPingResult,
  StreamingRuntimeConfig,
  StreamingSearchResult,
} from "@shared/types/streaming";
import type { WebStreamingAdapter } from "./types";
import { generateUUID } from "@/utils/uuid";

export interface StreamingAuthSession {
  accessToken: string;
  userId: string;
}

const CLIENT_NAME = "SPlayer-Next";
const CLIENT_VERSION = "1.0.0";
const DEVICE_NAME = "SPlayer Web";
const REQUEST_TIMEOUT_MS = 15_000;

interface JellyItem {
  Id: string;
  Name?: string;
  Album?: string;
  AlbumId?: string;
  AlbumArtist?: string;
  Artists?: string[];
  ArtistItems?: { Id: string; Name: string }[];
  RunTimeTicks?: number;
  ProductionYear?: number;
  ChildCount?: number;
  ImageTags?: { Primary?: string };
  MediaSources?: {
    Container?: string;
    Bitrate?: number;
    Size?: number;
    MediaStreams?: {
      Type?: string;
      SampleRate?: number;
      BitDepth?: number;
      Channels?: number;
      Codec?: string;
    }[];
  }[];
}

const deviceId = (config: StreamingRuntimeConfig): string => `splayer-web-${config.id}`;

const callApi = async <T>(
  config: StreamingRuntimeConfig,
  apiPath: string,
  init?: RequestInit,
): Promise<T> => {
  const parts = [
    `Client="${CLIENT_NAME}"`,
    `Device="${DEVICE_NAME}"`,
    `DeviceId="${deviceId(config)}"`,
    `Version="${CLIENT_VERSION}"`,
  ];
  if (config.accessToken) parts.push(`Token="${config.accessToken}"`);
  const authHeader = config.type === "emby" ? "X-Emby-Authorization" : "Authorization";
  const response = await fetch(`${config.url.replace(/\/+$/, "")}/${apiPath.replace(/^\//, "")}`, {
    ...init,
    headers: {
      "Content-Type": "application/json",
      [authHeader]: `MediaBrowser ${parts.join(", ")}`,
      ...(init?.headers ?? {}),
    },
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });
  if (!response.ok) {
    const detail = (await response.text()).trim().slice(0, 500);
    throw new Error(`${apiPath}: HTTP ${response.status}${detail ? ` - ${detail}` : ""}`);
  }
  if (response.status === 204) return null as T;
  return (await response.json()) as T;
};

const requireUserId = (config: StreamingRuntimeConfig): string => {
  if (!config.accessToken || !config.userId) throw new Error("缺少 accessToken / userId");
  return config.userId;
};

const fetchUserItems = async (
  config: StreamingRuntimeConfig,
  query: Record<string, string | number>,
): Promise<JellyItem[]> => {
  const userId = requireUserId(config);
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) params.set(key, String(value));
  const result = await callApi<{ Items?: JellyItem[] }>(
    config,
    `Users/${userId}/Items?${params.toString()}`,
  );
  return result.Items ?? [];
};

const imageUrl = (
  config: StreamingRuntimeConfig,
  itemId: string,
  tag: string | undefined,
  maxHeight: number,
): string | undefined => {
  const params = new URLSearchParams();
  if (config.accessToken) params.set("api_key", config.accessToken);
  params.set("maxHeight", String(maxHeight));
  params.set("maxWidth", String(maxHeight));
  if (tag) params.set("tag", tag);
  return `${config.url.replace(/\/+$/, "")}/Items/${itemId}/Images/Primary?${params.toString()}`;
};

const toTrack = (config: StreamingRuntimeConfig, item: JellyItem): Track => {
  const mediaSource = item.MediaSources?.[0];
  const audioStream = mediaSource?.MediaStreams?.find((stream) => stream.Type === "Audio");
  const imageTag = item.ImageTags?.Primary;
  return {
    id: `${config.id}:${item.Id}`,
    source: "streaming",
    serverId: config.id,
    originalId: item.Id,
    title: item.Name ?? "",
    artists:
      item.ArtistItems?.map((artist) => ({ id: artist.Id, name: artist.Name })) ??
      item.Artists?.map((name) => ({ name })) ??
      [],
    album: item.Album ? { id: item.AlbumId, name: item.Album } : undefined,
    duration: item.RunTimeTicks ? Math.floor(item.RunTimeTicks / 10_000) : 0,
    cover: imageTag ? imageUrl(config, item.Id, imageTag, 300) : undefined,
    coverOriginal: imageTag ? imageUrl(config, item.Id, imageTag, 1500) : undefined,
    fileSize: mediaSource?.Size,
    quality: {
      sampleRate: audioStream?.SampleRate ?? 0,
      channels: audioStream?.Channels ?? 2,
      bitsPerSample: audioStream?.BitDepth ?? 0,
      bitRate: mediaSource?.Bitrate ?? 0,
      codec: audioStream?.Codec ?? mediaSource?.Container ?? "",
    },
  };
};

const toAlbum = (config: StreamingRuntimeConfig, item: JellyItem): Album => ({
  id: item.Id,
  name: item.Name ?? "",
  artist: item.AlbumArtist,
  cover: imageUrl(config, item.Id, item.ImageTags?.Primary, 300),
  trackCount: item.ChildCount,
  year: item.ProductionYear,
});

const toArtist = (config: StreamingRuntimeConfig, item: JellyItem): Artist => ({
  id: item.Id,
  name: item.Name ?? "",
  avatar: imageUrl(config, item.Id, item.ImageTags?.Primary, 300),
  albumCount: item.ChildCount,
});

const toPlaylist = (config: StreamingRuntimeConfig, item: JellyItem): Playlist => ({
  id: item.Id,
  name: item.Name ?? "",
  cover: imageUrl(config, item.Id, item.ImageTags?.Primary, 300),
  trackCount: item.ChildCount,
});

export const authenticate = async (
  config: StreamingRuntimeConfig,
): Promise<StreamingAuthSession> => {
  const result = await callApi<{ AccessToken?: string; User?: { Id?: string } }>(
    { ...config, accessToken: undefined, userId: undefined },
    "Users/AuthenticateByName",
    {
      method: "POST",
      body: JSON.stringify({ Username: config.username, Pw: config.password }),
    },
  );
  if (!result.AccessToken || !result.User?.Id) {
    throw new Error("登录响应缺少 AccessToken/UserId");
  }
  return { accessToken: result.AccessToken, userId: result.User.Id };
};

export const jellyfinWebAdapter: WebStreamingAdapter = {
  async ping(config: StreamingRuntimeConfig): Promise<StreamingPingResult> {
    try {
      const result = await callApi<{ Version?: string }>(config, "System/Info/Public");
      return { ok: true, version: result.Version };
    } catch (error) {
      return { ok: false, error: error instanceof Error ? error.message : String(error) };
    }
  },

  async listSongs(config: StreamingRuntimeConfig, params?: StreamingListParams): Promise<Track[]> {
    const items = await fetchUserItems(config, {
      IncludeItemTypes: "Audio",
      Recursive: "true",
      SortBy: "DateCreated,SortName",
      SortOrder: "Descending",
      Fields: "MediaSources",
      Limit: params?.limit ?? 100,
      StartIndex: params?.offset ?? 0,
    });
    return items.map((item) => toTrack(config, item));
  },

  async listAlbums(config: StreamingRuntimeConfig, params?: StreamingListParams): Promise<Album[]> {
    const items = await fetchUserItems(config, {
      IncludeItemTypes: "MusicAlbum",
      Recursive: "true",
      SortBy: "SortName",
      SortOrder: "Ascending",
      Limit: params?.limit ?? 500,
      StartIndex: params?.offset ?? 0,
    });
    return items.map((item) => toAlbum(config, item));
  },

  async listArtists(config: StreamingRuntimeConfig): Promise<Artist[]> {
    const userId = requireUserId(config);
    const result = await callApi<{ Items?: JellyItem[] }>(
      config,
      `Artists?userId=${userId}&Recursive=true&SortBy=Name&SortOrder=Ascending`,
    );
    return (result.Items ?? []).map((item) => toArtist(config, item));
  },

  async listPlaylists(config: StreamingRuntimeConfig): Promise<Playlist[]> {
    const items = await fetchUserItems(config, {
      IncludeItemTypes: "Playlist",
      Recursive: "true",
      SortBy: "SortName",
    });
    return items.map((item) => toPlaylist(config, item));
  },

  async getAlbumSongs(config: StreamingRuntimeConfig, albumId: string): Promise<Track[]> {
    const items = await fetchUserItems(config, {
      ParentId: albumId,
      IncludeItemTypes: "Audio",
      Fields: "MediaSources",
      SortBy: "ParentIndexNumber,IndexNumber,SortName",
    });
    return items.map((item) => toTrack(config, item));
  },

  async getPlaylistSongs(config: StreamingRuntimeConfig, playlistId: string): Promise<Track[]> {
    const userId = requireUserId(config);
    const params = new URLSearchParams({ UserId: userId, Fields: "MediaSources" });
    const result = await callApi<{ Items?: JellyItem[] }>(
      config,
      `Playlists/${playlistId}/Items?${params.toString()}`,
    );
    return (result.Items ?? []).map((item) => toTrack(config, item));
  },

  async getArtistAlbums(config: StreamingRuntimeConfig, artistId: string): Promise<Album[]> {
    const items = await fetchUserItems(config, {
      AlbumArtistIds: artistId,
      IncludeItemTypes: "MusicAlbum",
      Recursive: "true",
      SortBy: "ProductionYear,SortName",
      SortOrder: "Descending",
    });
    return items.map((item) => toAlbum(config, item));
  },

  async getArtistSongs(config: StreamingRuntimeConfig, artistId: string): Promise<Track[]> {
    const items = await fetchUserItems(config, {
      ArtistIds: artistId,
      IncludeItemTypes: "Audio",
      Recursive: "true",
      Fields: "MediaSources",
      SortBy: "Album,ParentIndexNumber,IndexNumber,SortName",
    });
    return items.map((item) => toTrack(config, item));
  },

  async search(config: StreamingRuntimeConfig, query: string): Promise<StreamingSearchResult> {
    const userId = requireUserId(config);
    const params = new URLSearchParams({
      searchTerm: query,
      IncludeItemTypes: "Audio,MusicAlbum,MusicArtist",
      Recursive: "true",
      Limit: "100",
    });
    const result = await callApi<{ Items?: (JellyItem & { Type?: string })[] }>(
      config,
      `Users/${userId}/Items?${params.toString()}`,
    );
    const songs: Track[] = [];
    const albums: Album[] = [];
    const artists: Artist[] = [];
    for (const item of result.Items ?? []) {
      if (item.Type === "Audio") songs.push(toTrack(config, item));
      else if (item.Type === "MusicAlbum") albums.push(toAlbum(config, item));
      else if (item.Type === "MusicArtist") artists.push(toArtist(config, item));
    }
    return { songs, albums, artists };
  },

  async getStreamUrl(config: StreamingRuntimeConfig, trackId: string, playSessionId?: string): Promise<string> {
    const cleanId = trackId.includes(":") ? trackId.split(":").slice(1).join(":") : trackId;
    const userId = requireUserId(config);
    const params = new URLSearchParams({
      UserId: userId,
      DeviceId: deviceId(config),
      PlaySessionId: playSessionId ?? generateUUID(),
      api_key: config.accessToken!,
      StartTimeTicks: "0",
      Static: "true",
    });
    if (config.type === "emby") {
      params.set("EnableRedirection", "true");
      params.set("EnableRemoteMedia", "true");
      return `${config.url.replace(/\/+$/, "")}/Audio/${cleanId}/universal?${params.toString()}`;
    }
    return `${config.url.replace(/\/+$/, "")}/Audio/${cleanId}/stream?${params.toString()}`;
  },

  async getLyrics(config: StreamingRuntimeConfig, trackId: string): Promise<string | null> {
    try {
      const result = await callApi<{
        Metadata?: { IsSynced?: boolean | null };
        Lyrics?: { Start?: number; Text?: string }[];
      }>(config, `Audio/${trackId}/Lyrics`);
      const lines = result.Lyrics ?? [];
      if (lines.length === 0) return null;
      const synced = result.Metadata?.IsSynced ?? lines.some((line) => (line.Start ?? 0) > 0);
      if (!synced) {
        const text = lines
          .map((line) => line.Text ?? "")
          .filter(Boolean)
          .join("\n");
        return text || null;
      }
      return lines
        .map((line) => {
          const milliseconds = Math.floor((line.Start ?? 0) / 10_000);
          const minutes = Math.floor(milliseconds / 60_000);
          const seconds = Math.floor((milliseconds % 60_000) / 1000);
          const centiseconds = Math.floor((milliseconds % 1000) / 10);
          return `[${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}.${String(centiseconds).padStart(2, "0")}]${line.Text ?? ""}`;
        })
        .join("\n");
    } catch {
      return null;
    }
  },

  getCoverUrl(config: StreamingRuntimeConfig, coverId: string, size = 300): string {
    const params = new URLSearchParams();
    if (config.accessToken) params.set("api_key", config.accessToken);
    params.set("maxHeight", String(size));
    params.set("maxWidth", String(size));
    return `${config.url.replace(/\/+$/, "")}/Items/${coverId}/Images/Primary?${params.toString()}`;
  },
};
