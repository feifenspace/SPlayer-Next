#!/usr/bin/env python3
"""v9e: alsammap 写循环水位限速（方案②）。

背景：写循环按"hw 有空间就拉"前倾式消费，把 Shared 队列（768KiB≈2s）一口气
灌进 hw buffer → 流媒体渐进供给间歇里 position 冻结 → watchdog 误判"输出停滞"
拆线重载循环。本地文件供给瞬时无限快不触发，仅流媒体中招。

修法：hw buffer 预填上限 ~200ms（高水位）。已填水位 = buffer_frames - avail；
每轮只写 min(avail, watermark-fill) 帧。消费节奏被拉回真实时 → position 平滑
前进 → watchdog 不再误杀。位纯真零改动：pacing 不碰样本路径（同样的样本、
同样的转换、同样的 MMAP DMA 直写），也不依赖用户音量（与音量互不影响）。

env 门控：SPLAYER_ALSAMMAP_WATERMARK_MS（毫秒，默认 200；0 = 恢复旧行为不限速）。
"""

import sys

PATH = "native/audio-engine-core/src/alsa_mmap_sink.rs"

HEADER_ANCHOR = """/// 设备事件等待上限（毫秒）：决定 play/pause 指令的响应延迟上界
const WAIT_CEILING_MS: u32 = 50;
"""

HEADER_NEW = """/// 设备事件等待上限（毫秒）：决定 play/pause 指令的响应延迟上界
const WAIT_CEILING_MS: u32 = 50;

/// hw buffer 预填高水位（毫秒）。写循环每轮只把 hw 已填水位补到该上限，
/// 而非"有空间就灌"：消费节奏贴回真实时，解码 Shared 队列得以保留网络
/// 缓冲垫，position 平滑前进，watchdog 不再误判流媒体"输出停滞"。
/// 预填 200ms 远超 RT 内核调度毛刺 + 已提权音频线程的最坏等待，无 xrun
/// 风险；本地文件场景同步受益（队列缓冲垫保留）。
/// env `SPLAYER_ALSAMMAP_WATERMARK_MS` 可覆盖（毫秒；0 = 恢复旧行为不限速）。
const HW_HIGH_WATERMARK_MS: u64 = 200;
"""


def main() -> int:
    src = open(PATH, encoding="utf-8").read()

    if "HW_HIGH_WATERMARK_MS" in src:
        print("already patched")
        return 0

    # ---- 1. 常量 ----
    assert src.count(HEADER_ANCHOR) == 1, "header anchor not unique"
    src = src.replace(HEADER_ANCHOR, HEADER_NEW)

    # ---- 2. write_loop 内：avail 之后插入水位计算 ----
    ANCHOR = """        if avail == 0 {
            let _ = pcm.wait(Some(WAIT_CEILING_MS));
            continue;
        }

        let frames = avail.min(4096);
"""
    NEW = """        if avail == 0 {
            let _ = pcm.wait(Some(WAIT_CEILING_MS));
            continue;
        }

        // 高水位限速（v9e）：hw 已填水位 = buffer_frames - avail。每轮只补到
        // ~HW_HIGH_WATERMARK_MS 上限，多余供给滞留在 Shared 队列作网络缓冲垫。
        // pacing 只约束"何时写"，不碰样本路径——位纯真不受影响。
        let buffer_frames = {
            let (buf, _per) = pcm.get_params()?;
            buf
        };
        let watermark_ms = std::env::var("SPLAYER_ALSAMMAP_WATERMARK_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(HW_HIGH_WATERMARK_MS);
        let watermark_frames = watermark_ms.saturating_mul(rate as u64) / 1000;
        let filled = buffer_frames.saturating_sub(avail as u64);
        let allowed = watermark_frames.saturating_sub(filled) as usize;
        let frames = if watermark_ms == 0 {
            avail.min(4096)
        } else {
            avail.min(4096).min(allowed)
        };
        if frames == 0 {
            // 已填至高水位：等一个周期，让 hw 消耗后再补
            let _ = pcm.wait(Some(WAIT_CEILING_MS));
            continue;
        }
"""
    assert src.count(ANCHOR) == 1, "write_loop anchor not unique"
    src = src.replace(ANCHOR, NEW)

    open(PATH, "w", encoding="utf-8").write(src)
    print("patched v9e watermark pacing OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
