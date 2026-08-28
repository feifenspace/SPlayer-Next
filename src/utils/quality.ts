import type { AudioQuality } from "@shared/types/player";

/** 在线与下载音质等级档位 */
export type QualityLevel = "hi-res" | "lossless" | "hq" | "sq" | "lq";

/** 扩展展示音质等级 */
export type DisplayQualityLevel =
  | QualityLevel
  | "dsd"
  | "sacd"
  | "mqa"
  | "hdcd"
  | "dts"
  | "midi";

/** 无损与发烧级编解码器集合 */
const LOSSLESS_CODECS = new Set([
  "flac",
  "alac",
  "ape",
  "wav",
  "aiff",
  "wavpack",
  "tta",
  "dsf",
  "dff",
  "dsd",
  "dsd_dsf",
  "dsd_dff",
  "sacd",
  "sacd_dsd",
  "sacd_dst",
  "mqa",
  "dts",
  "hdcd",
  "midi",
  "mid",
  "cue",
]);

/**
 * 判断编解码器是否为无损格式
 * @param codec - 编解码器名称
 * @returns 是否为无损格式
 */
export const isLosslessCodec = (codec: string): boolean =>
  LOSSLESS_CODECS.has(codec.toLowerCase());

/** 等级短码文案 */
export const QUALITY_LABELS: Record<QualityLevel, string> = {
  "hi-res": "Hi-Res",
  lossless: "Lossless",
  hq: "HQ",
  sq: "SQ",
  lq: "LQ",
};

/** 等级完整文案 */
const QUALITY_FULL_LABELS: Record<QualityLevel, string> = {
  "hi-res": "Hi-Res",
  lossless: "Lossless",
  hq: "High Quality",
  sq: "Standard Quality",
  lq: "Low Quality",
};

/**
 * 获取音频文件的具体展示级别
 */
export const getDisplayQualityLevel = (quality: AudioQuality | undefined): DisplayQualityLevel => {
  if (!quality || !quality.codec || quality.codec === "unknown") return "lq";
  const lowerCodec = quality.codec.toLowerCase();

  // 1. DSD / SACD 系列
  if (
    lowerCodec === "dsf" ||
    lowerCodec === "dff" ||
    lowerCodec === "dsd" ||
    lowerCodec === "dsd_dsf" ||
    lowerCodec === "dsd_dff" ||
    quality.sampleRate >= 2_822_400
  ) {
    return "dsd";
  }
  if (lowerCodec.startsWith("sacd")) {
    return "sacd";
  }

  // 2. 发烧认证格式
  if (lowerCodec === "mqa") return "mqa";
  if (lowerCodec === "hdcd") return "hdcd";
  if (lowerCodec === "dts") return "dts";
  if (lowerCodec === "midi" || lowerCodec === "mid") return "midi";

  // 3. PCM 无损与高解析
  const isLossless = isLosslessCodec(lowerCodec);
  if (isLossless) {
    if (
      (quality.sampleRate >= 88_200 && quality.bitsPerSample >= 24) ||
      quality.sampleRate >= 96_000 ||
      quality.bitsPerSample >= 32
    ) {
      return "hi-res";
    }
    return "lossless";
  }

  // 4. 有损格式
  const kbps = quality.bitRate / 1000;
  if (kbps >= 320) return "hq";
  if (kbps >= 192) return "sq";
  return "lq";
};

/**
 * 判断音质等级；信息不全时回落到 LQ（用于在线档位切换与下载选择）
 * @param quality - AudioQuality；undefined / 无 codec 时按最低档处理
 * @returns 音质等级
 */
export const getQualityLevel = (quality: AudioQuality | undefined): QualityLevel => {
  const display = getDisplayQualityLevel(quality);
  if (display === "dsd" || display === "sacd" || display === "mqa" || display === "hdcd") {
    return "hi-res";
  }
  if (display === "dts" || display === "midi") {
    return "lossless";
  }
  return display;
};

/**
 * 取音质等级短码文案（动态呈现 DSD64/DSD128/SACD/MQA/HDCD/DTS/Hi-Res 等）
 * @param quality - 音质信息；缺少信息时使用默认 LQ
 * @returns 短码文案（DSD64 / SACD / MQA / HDCD / DTS / Hi-Res / Lossless 等）
 */
export const getQualityLabel = (quality: AudioQuality | undefined): string => {
  if (!quality) return QUALITY_LABELS.lq;
  const display = getDisplayQualityLevel(quality);

  switch (display) {
    case "dsd":
      if (quality.sampleRate >= 22_579_200) return "DSD512";
      if (quality.sampleRate >= 11_289_600) return "DSD256";
      if (quality.sampleRate >= 5_644_800) return "DSD128";
      return "DSD64";
    case "sacd":
      return "SACD";
    case "mqa":
      return "MQA";
    case "hdcd":
      return "HDCD";
    case "dts":
      return "DTS";
    case "midi":
      return "MIDI";
    default:
      return QUALITY_LABELS[display];
  }
};

/**
 * 取音质等级完整文案
 * @param quality - 音质信息；缺少信息时使用默认 Low Quality
 * @returns 完整文案
 */
export const getQualityFullLabel = (quality: AudioQuality | undefined): string =>
  QUALITY_FULL_LABELS[getQualityLevel(quality)];

/** 是否为无损级别（DSD / SACD / MQA / HDCD / DTS / hi-res / lossless） */
export const isLosslessQuality = (quality: AudioQuality | undefined): boolean => {
  const display = getDisplayQualityLevel(quality);
  return (
    display === "dsd" ||
    display === "sacd" ||
    display === "mqa" ||
    display === "hdcd" ||
    display === "dts" ||
    display === "hi-res" ||
    display === "lossless"
  );
};
