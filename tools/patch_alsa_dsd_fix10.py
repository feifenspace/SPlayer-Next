"""v10 fix10: mmap IO 改用 alsa-rs io_bytes()（免格式校验, 字节粒度, 三档统一）.

在 gaoda 仓库根目录执行: python3 tools/patch_alsa_dsd_fix10.py

根因: alsa-rs io_checked 以「协商格式 == 类型默认格式」严格等值校验,
DSD_U32_BE/U16_BE/U8 都没有对应 IoFormat 常量, io_i32/io_i16/io_u8 全部
Error::unsupported("io_xx") (errno 95 是库自己编的, 非内核拒绝).
官方逃生口 = io_bytes() (原 io(), 文档明言给 unusual format 用).
IO::mmap 对 u8 元素按 frames_to_bytes 换算, 完全通用.
"""
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SINK = "native/audio-engine-core/src/alsa_mmap_sink.rs"

def patch(old: str, new: str, label: str) -> None:
    p = ROOT / SINK
    text = p.read_text(encoding="utf-8")
    if new in text:
        print(f"[SKIP] {label}: 已应用")
        return
    if old not in text:
        print(f"[FAIL] {label}: old 片段未找到")
        sys.exit(1)
    if text.count(old) != 1:
        print(f"[FAIL] {label}: old 片段出现 {text.count(old)} 次，需唯一")
        sys.exit(1)
    p.write_text(text.replace(old, new, 1), encoding="utf-8")
    print(f"[OK] {label}")

patch(
    """        let wire = std::cmp::min(frames, (staging.len() - staging_pos) / frame_bytes);
        // 各容器宽度只暴露对应宽度的 IO（U32_BE 用 io_u8 会 EOPNOTSUPP）；
        // x86 LE 上 from_le_bytes 落内存即保持字节流顺序 = DMA 线序（BE 格式内存布局）
        let result = match wire_fmt {
            DsdWireFmt::U32Be => pcm.io_i32()?.mmap(frames, |buf: &mut [i32]| {
                let src = &staging[staging_pos..];
                let words = std::cmp::min(buf.len(), src.len() / 4);
                for (w, slot) in buf.iter_mut().enumerate().take(words) {
                    let off = w * 4;
                    *slot = i32::from_le_bytes([src[off], src[off + 1], src[off + 2], src[off + 3]]);
                }
                words / channels
            }),
            DsdWireFmt::U16Be => pcm.io_i16()?.mmap(frames, |buf: &mut [i16]| {
                let src = &staging[staging_pos..];
                let units = std::cmp::min(buf.len(), src.len() / 2);
                for (w, slot) in buf.iter_mut().enumerate().take(units) {
                    let off = w * 2;
                    *slot = i16::from_le_bytes([src[off], src[off + 1]]);
                }
                units / channels
            }),
            DsdWireFmt::U8 => pcm.io_u8()?.mmap(frames, |buf: &mut [u8]| {
                let src = &staging[staging_pos..];
                let copy = std::cmp::min(buf.len(), src.len());
                buf[..copy].copy_from_slice(&src[..copy]);
                copy / channels
            }),
        };
        staging_pos += (staging.len() - staging_pos).min(wire * frame_bytes);""",
    """        let wire = std::cmp::min(frames, (staging.len() - staging_pos) / frame_bytes);
        // alsa-rs 的 io_checked 以「协商格式 == 类型默认格式」严格等值校验，DSD
        // 三档格式均无 IoFormat 映射，io_i32/io_i16/io_u8 一律 unsupported("io_xx")。
        // 官方逃生口 io_bytes()（文档：unusual format 用）：免检、字节粒度 mmap，
        // IO::mmap 按 frames_to_bytes 换算，对 U32/U16/U8 三档统一适用
        let result = pcm.io_bytes().mmap(frames, |buf: &mut [u8]| {
            let src = &staging[staging_pos..];
            let copy = std::cmp::min(buf.len(), src.len() / frame_bytes * frame_bytes);
            buf[..copy].copy_from_slice(&src[..copy]);
            copy / frame_bytes
        });
        staging_pos += (staging.len() - staging_pos).min(wire * frame_bytes);""",
    "mmap 改 io_bytes 单路实现",
)

print("patch_alsa_dsd_fix10: all done")
