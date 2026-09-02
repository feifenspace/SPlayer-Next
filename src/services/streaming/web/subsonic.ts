import type { Album, Artist, Playlist, Track } from "@shared/types/player";
import type {
  StreamingListParams,
  StreamingPingResult,
  StreamingRuntimeConfig,
  StreamingSearchResult,
} from "@shared/types/streaming";
import { md5 } from "./md5";
import type { WebStreamingAdapter } from "./types";

const API_VERSION = "1.16.1";
const CLIENT_NAME = "SPlayer-Next";
const REQUEST_TIMEOUT_MS = 15_000;

interface SubsonicSong {
  id: string;
  title: string;
  artist?: string;
  artistId?: string;
  album?: string;
  albumId?: string;
  duration?: number;
  bitRate?: number;
  samplingRate?: number;
  bitDepth?: number;
  channelCount?: number;
  suffix?: string;
  size?: number;
  coverArt?: string;
  artists?: { id?: string; name: string }[];
  displayArtist?: string;
}

interface SubsonicAlbum {
  id: string;
  name: string;
  artist?: string;
  coverArt?: string;
  songCount?: number;
  year?: number;
  displayArtist?: string;
  song?: SubsonicSong[];
}

interface SubsonicArtist {
  id: string;
  name: string;
  albumCount?: number;
  coverArt?: string;
}

interface SubsonicPlaylist {
  id: string;
  name: string;
  comment?: string;
  songCount?: number;
  coverArt?: string;
  owner?: string;
  entry?: SubsonicSong[];
}

const randomSalt = (): string => {
  return Math.random().toString(36).substring(2, 10) + Math.random().toString(36).substring(2, 10);
};

const buildAuth = (config: StreamingRuntimeConfig, isRestData = true): URLSearchParams => {
  const salt = randomSalt();
  const params = new URLSearchParams({
    u: config.username,
    t: md5(config.password + salt),
    s: salt,
    v: API_VERSION,
    c: CLIENT_NAME,
  });
  if (isRestData) {
    params.set("f", "json");
  }
  return params;
};

const buildUrl = (
  config: StreamingRuntimeConfig,
  endpoint: string,
  extra: Record<string, string | number> = {},
  isRestData = true,
): string => {
  const params = buildAuth(config, isRestData);
  for (const [key, value] of Object.entries(extra)) {
    params.set(key, String(value));
  }
  return `${config.url.replace(/\/+$/, "")}/rest/${endpoint}?${params.toString()}`;
};

const callApi = async <T>(
  config: StreamingRuntimeConfig,
  endpoint: string,
  extra?: Record<string, string | number>,
): Promise<T> => {
  const response = await fetch(buildUrl(config, endpoint, extra, true), {
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });
  if (!response.ok) throw new Error(`${endpoint}: HTTP ${response.status}`);
  const body = (await response.json()) as { "subsonic-response"?: Record<string, unknown> };
  const result = body["subsonic-response"];
  if (!result) throw new Error("响应缺少 subsonic-response 包装");
  if (result.status !== "ok") {
    const error = result.error as { code?: number; message?: string } | undefined;
    throw new Error(error?.message ?? `Subsonic error code ${error?.code}`);
  }
  return result as T;
};

const coverUrl = (
  config: StreamingRuntimeConfig,
  coverId: string | undefined,
  size = 300,
): string | undefined => {
  if (!coverId) return undefined;
  return buildUrl(config, "getCoverArt", { id: coverId, size }, false);
};

const toTrack = (config: StreamingRuntimeConfig, song: SubsonicSong): Track => {
  const artists = song.artists?.length
    ? song.artists.map((artist) => ({ id: artist.id, name: artist.name }))
    : (song.displayArtist ?? song.artist ?? "").trim()
      ? [{ id: song.artistId, name: (song.displayArtist ?? song.artist ?? "").trim() }]
      : [];
  return {
    id: `${config.id}:${song.id}`,
    source: "streaming",
    serverId: config.id,
    originalId: song.id,
    title: song.title || "",
    artists,
    album: song.album ? { id: song.albumId, name: song.album } : undefined,
    duration: Math.round((song.duration ?? 0) * 1000),
    cover: coverUrl(config, song.coverArt, 300),
    coverOriginal: coverUrl(config, song.coverArt, 1500),
    fileSize: song.size,
    quality: {
      sampleRate: song.samplingRate ?? 0,
      channels: song.channelCount ?? 2,
      bitsPerSample: song.bitDepth ?? 0,
      bitRate: song.bitRate ? song.bitRate * 1000 : 0,
      codec: song.suffix ?? "",
    },
  };
};

const toAlbum = (config: StreamingRuntimeConfig, album: SubsonicAlbum): Album => ({
  id: album.id,
  name: album.name,
  artist: album.displayArtist ?? album.artist,
  cover: coverUrl(config, album.coverArt, 300),
  trackCount: album.songCount,
  year: album.year,
});

const toArtist = (config: StreamingRuntimeConfig, artist: SubsonicArtist): Artist => ({
  id: artist.id,
  name: artist.name,
  avatar: coverUrl(config, artist.coverArt, 300),
  albumCount: artist.albumCount,
});

const toPlaylist = (config: StreamingRuntimeConfig, playlist: SubsonicPlaylist): Playlist => ({
  id: playlist.id,
  name: playlist.name,
  description: playlist.comment,
  cover: coverUrl(config, playlist.coverArt, 300),
  trackCount: playlist.songCount,
  owner: playlist.owner,
});

export const subsonicWebAdapter: WebStreamingAdapter = {
  async ping(config: StreamingRuntimeConfig): Promise<StreamingPingResult> {
    try {
      const result = await callApi<{ version?: string; serverVersion?: string }>(config, "ping");
      return { ok: true, version: result.serverVersion ?? result.version };
    } catch (error) {
      return { ok: false, error: error instanceof Error ? error.message : String(error) };
    }
  },

  async listSongs(config: StreamingRuntimeConfig, params?: StreamingListParams): Promise<Track[]> {
    try {
      const result = await callApi<{ searchResult3?: { song?: SubsonicSong[] } }>(config, "search3", {
        query: "",
        songCount: params?.limit ?? 100,
        songOffset: params?.offset ?? 0,
        artistCount: 0,
        albumCount: 0,
      });
      const songs = result.searchResult3?.song ?? [];
      if (songs.length > 0) {
        return songs.map((song) => toTrack(config, song));
      }
    } catch {
      // search3 异常时回退
    }

    try {
      const randomResult = await callApi<{ randomSongs?: { song?: SubsonicSong[] } }>(
        config,
        "getRandomSongs",
        { size: params?.limit ?? 100 },
      );
      return (randomResult.randomSongs?.song ?? []).map((song) => toTrack(config, song));
    } catch {
      return [];
    }
  },

  async listAlbums(config: StreamingRuntimeConfig, params?: StreamingListParams): Promise<Album[]> {
    const result = await callApi<{ albumList2?: { album?: SubsonicAlbum[] } }>(
      config,
      "getAlbumList2",
      {
        type: "alphabeticalByName",
        size: params?.limit ?? 100,
        offset: params?.offset ?? 0,
      },
    );
    return (result.albumList2?.album ?? []).map((album) => toAlbum(config, album));
  },

  async listArtists(config: StreamingRuntimeConfig): Promise<Artist[]> {
    const result = await callApi<{
      artists?: { index?: { artist?: SubsonicArtist[] }[] };
      indexes?: { index?: { artist?: SubsonicArtist[] }[] };
    }>(config, "getArtists");
    const indexes = result.artists?.index ?? result.indexes?.index ?? [];
    return indexes.flatMap((entry) => (entry.artist ?? []).map((artist) => toArtist(config, artist)));
  },

  async listPlaylists(config: StreamingRuntimeConfig): Promise<Playlist[]> {
    const result = await callApi<{ playlists?: { playlist?: SubsonicPlaylist[] } }>(
      config,
      "getPlaylists",
    );
    return (result.playlists?.playlist ?? []).map((playlist) => toPlaylist(config, playlist));
  },

  async getAlbumSongs(config: StreamingRuntimeConfig, albumId: string): Promise<Track[]> {
    const cleanId = albumId.includes(":") ? albumId.split(":").slice(1).join(":") : albumId;
    const result = await callApi<{ album?: SubsonicAlbum }>(config, "getAlbum", { id: cleanId });
    return (result.album?.song ?? []).map((song) => toTrack(config, song));
  },

  async getPlaylistSongs(config: StreamingRuntimeConfig, playlistId: string): Promise<Track[]> {
    const cleanId = playlistId.includes(":") ? playlistId.split(":").slice(1).join(":") : playlistId;
    const result = await callApi<{ playlist?: SubsonicPlaylist }>(config, "getPlaylist", {
      id: cleanId,
    });
    return (result.playlist?.entry ?? []).map((song) => toTrack(config, song));
  },

  async getArtistAlbums(config: StreamingRuntimeConfig, artistId: string): Promise<Album[]> {
    const cleanId = artistId.includes(":") ? artistId.split(":").slice(1).join(":") : artistId;
    const result = await callApi<{ artist?: { album?: SubsonicAlbum[] } }>(config, "getArtist", {
      id: cleanId,
    });
    return (result.artist?.album ?? []).map((album) => toAlbum(config, album));
  },

  async getArtistSongs(config: StreamingRuntimeConfig, artistId: string): Promise<Track[]> {
    const cleanId = artistId.includes(":") ? artistId.split(":").slice(1).join(":") : artistId;
    const result = await callApi<{ artist?: { album?: SubsonicAlbum[] } }>(config, "getArtist", {
      id: cleanId,
    });
    const albums = result.artist?.album ?? [];
    const results = await Promise.allSettled(
      albums.map((album) =>
        callApi<{ album?: SubsonicAlbum }>(config, "getAlbum", { id: album.id }),
      ),
    );
    const tracks: Track[] = [];
    for (const res of results) {
      if (res.status === "fulfilled" && res.value.album?.song) {
        tracks.push(...res.value.album.song.map((song) => toTrack(config, song)));
      }
    }
    return tracks;
  },

  async search(config: StreamingRuntimeConfig, query: string): Promise<StreamingSearchResult> {
    const result = await callApi<{
      searchResult3?: {
        song?: SubsonicSong[];
        album?: SubsonicAlbum[];
        artist?: SubsonicArtist[];
      };
    }>(config, "search3", {
      query,
      songCount: 100,
      albumCount: 50,
      artistCount: 50,
    });
    const data = result.searchResult3;
    return {
      songs: (data?.song ?? []).map((song) => toTrack(config, song)),
      albums: (data?.album ?? []).map((album) => toAlbum(config, album)),
      artists: (data?.artist ?? []).map((artist) => toArtist(config, artist)),
    };
  },

  async getStreamUrl(config: StreamingRuntimeConfig, trackId: string): Promise<string> {
    const cleanId = trackId.includes(":") ? trackId.split(":").slice(1).join(":") : trackId;
    return buildUrl(config, "stream", {
      id: cleanId,
    }, false);
  },

  async getLyrics(
    config: StreamingRuntimeConfig,
    trackId: string,
    hint?: { artist?: string; title?: string },
  ): Promise<string | null> {
    try {
      const result = await callApi<{
        lyricsList?: { structuredLyrics?: { line?: { start?: number; value: string }[] }[] };
      }>(config, "getLyricsBySongId", { id: trackId });
      const lines = result.lyricsList?.structuredLyrics?.[0]?.line ?? [];
      if (lines.length > 0) {
        return lines
          .map((line) => {
            const milliseconds = line.start ?? 0;
            const minutes = Math.floor(milliseconds / 60_000);
            const seconds = Math.floor((milliseconds % 60_000) / 1000);
            const centiseconds = Math.floor((milliseconds % 1000) / 10);
            return `[${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}.${String(centiseconds).padStart(2, "0")}]${line.value ?? ""}`;
          })
          .join("\n");
      }
    } catch {
      // 回退
    }
    if (!hint?.artist && !hint?.title) return null;
    try {
      const result = await callApi<{ lyrics?: { value?: string } }>(config, "getLyrics", {
        artist: hint.artist ?? "",
        title: hint.title ?? "",
      });
      return result.lyrics?.value?.trim() ? result.lyrics.value : null;
    } catch {
      return null;
    }
  },

  getCoverUrl(config: StreamingRuntimeConfig, coverId: string, size = 300): string {
    return buildUrl(config, "getCoverArt", { id: coverId, size }, false);
  },
};
