/**
 * TIDAL 艺术家详情（基本资料 + 热门曲目 + 专辑列表）
 *
 * 参照 src/apis/artist/qobuz.ts 的模式。
 * TIDAL 需并行调用 artist_get + artist_getAlbums + artist_getTopTracks。
 */

import type { Album, Artist, Track } from "@shared/types/player";
import { tidalCall } from "@/apis/tidal";
import {
  songsToTracks,
  toAlbum,
  toArtist,
  type TidalAlbum,
  type TidalArtist,
  type TidalTrack,
} from "@/utils/format/tidal";

interface TidalArtistResponse extends TidalArtist {
  bio?: string;
}

interface TidalArtistAlbumsResponse {
  items?: TidalAlbum[];
  totalNumberOfItems?: number;
}

interface TidalArtistTopTracksResponse {
  items?: TidalTrack[];
  totalNumberOfItems?: number;
}

/**
 * 拉取艺术家：基本资料 + 热门曲目 + 专辑列表
 * @param artistId 艺术家 id
 */
export const fetchArtist = async (
  artistId: string,
): Promise<{ artist: Artist; tracks: Track[]; albums: Album[] } | null> => {
  const [artistResp, topTracksResp, albumsResp] = await Promise.all([
    tidalCall<TidalArtistResponse>("artist_get", { artist_id: artistId }),
    tidalCall<TidalArtistTopTracksResponse>("artist_getTopTracks", {
      artist_id: artistId,
      limit: 50,
    }),
    tidalCall<TidalArtistAlbumsResponse>("artist_getAlbums", {
      artist_id: artistId,
      limit: 50,
    }),
  ]);
  if (!artistResp?.id) return null;
  return {
    artist: toArtist(artistResp),
    tracks: songsToTracks(topTracksResp?.items),
    albums: (albumsResp?.items ?? []).map(toAlbum),
  };
};
