export type StreamingService = "qobuz" | "tidal";
export type CoverSizePreset = "small" | "medium" | "large";

const COVER_PAIR: Record<
  StreamingService,
  Record<CoverSizePreset, { small: number; large: number }>
> = {
  qobuz: {
    small: { small: 100, large: 200 },
    medium: { small: 300, large: 600 },
    large: { small: 600, large: 1000 },
  },
  tidal: {
    small: { small: 160, large: 320 },
    medium: { small: 160, large: 640 },
    large: { small: 640, large: 1280 },
  },
};

/** 取某服务当前封面尺寸档位对应的 small/large 像素值 */
export const coverSizePair = (
  svc: StreamingService,
  preset: CoverSizePreset = "medium",
): { small: number; large: number } => {
  return COVER_PAIR[svc][preset] ?? COVER_PAIR[svc].medium;
};
