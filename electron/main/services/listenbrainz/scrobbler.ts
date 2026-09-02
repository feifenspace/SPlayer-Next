import type { ListenBrainzTrackSnapshot } from "@shared/types/listenbrainz";
import { createPlayProgress } from "@main/services/playProgress";

export interface ScrobblerHandlers {
  onNowPlaying: (track: ListenBrainzTrackSnapshot) => void;
  onScrobble: (track: ListenBrainzTrackSnapshot) => void;
}

let handlers: ScrobblerHandlers | null = null;
let current: ListenBrainzTrackSnapshot | null = null;
let nowPlayingSent = false;

const progress = createPlayProgress<ListenBrainzTrackSnapshot>({
  onThreshold: (track) => handlers?.onScrobble(track),
});

export const setHandlers = (next: ScrobblerHandlers): void => {
  handlers = next;
};

const sendNowPlaying = (): void => {
  if (!current || nowPlayingSent) return;
  nowPlayingSent = true;
  handlers?.onNowPlaying(current);
};

export const onTrackLoaded = (meta: {
  title: string;
  artist: string;
  album: string;
  durationMs: number;
  autoPlay: boolean;
  trackNumber?: number;
}): void => {
  current =
    meta.durationMs <= 0 || !meta.title
      ? null
      : {
          artistName: meta.artist || "Unknown Artist",
          trackName: meta.title,
          releaseName: meta.album || undefined,
          durationMs: meta.durationMs,
          trackNumber: meta.trackNumber,
          listenedAt: Date.now(),
        };
  nowPlayingSent = false;
  progress.load(Math.round(meta.durationMs / 1000), current, meta.autoPlay);
  if (current && meta.autoPlay) sendNowPlaying();
};

export const onState = (playing: boolean): void => {
  if (playing) sendNowPlaying();
  progress.setPlaying(playing);
};

export const onPosition = (): void => {
  progress.tick();
};

export const onEnded = (): void => {
  progress.end();
  current = null;
  nowPlayingSent = false;
};

export const reset = (): void => {
  progress.reset();
  current = null;
  nowPlayingSent = false;
};
