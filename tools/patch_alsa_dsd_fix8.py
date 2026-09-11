"""v10 fix8: ALSA DSD 写循环 io_u8 -> io_i32 + 源位序->MSB-first 适配.

在 gaoda 上于 SPlayer-Next-Headless 仓库根目录执行:
    python3 tools/patch_alsa_dsd_fix8.py

修复点:
1. alsa_mmap_sink.rs dsd_write_loop: DSD_U32_BE 仅暴露 32-bit IO,
   io_u8 -> io_i32 (from_le_bytes 保持内存/线序).
2. read_block 输出是源位序 (DSF=LSB-first / DFF=MSB-first),
   ALSA 原生 DSD (kernel quirk bitrev=0) 期望 MSB-first 线序 -> 逐字节 reverse_bits.
3. SPLAYER_ALSADSD_BITREV=on/off 强制反转开关 (默认 auto 按源位序).
4. direct_dsd.rs: adapt_dsd_bit_order 改 pub(crate) (备用).
"""
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

def patch(rel: str, old: str, new: str, label: str) -> None:
    p = ROOT / rel
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

# 1) direct_dsd.rs: adapt_dsd_bit_order 提升可见性
patch(
    "native/audio-engine-core/src/direct_dsd.rs",
    "fn adapt_dsd_bit_order(",
    "pub(crate) fn adapt_dsd_bit_order(",
    "direct_dsd.rs: adapt_dsd_bit_order pub(crate)",
)

SINK = "native/audio-engine-core/src/alsa_mmap_sink.rs"

# 2) 修正误导性注释
patch(
    SINK,
    "/// DSD 写循环：read_block 拉 raw DSD（L4R4 单元交织）→ 水位限速写 MMAP。\n"
    "/// DirectDsdReader::read_block 已按 Diretta 语义输出 L4R4 单元流\n"
    "/// （DSF block 交织 / DFF 逐字节交织均已重排），DSD_U32_BE 每帧 8 字节\n"
    "/// 恰为一个 L4R4 单元，字节序在 repack 层已保证 MSB-first。",
    "/// DSD 写循环：read_block 拉 raw DSD（L4R4 单元交织）→ 水位限速写 MMAP。\n"
    "/// read_block 输出源位序（DSF=LSB-first / DFF=MSB-first），此处统一适配为\n"
    "/// MSB-first 线序（kernel quirk bitrev=0 → USB 原生 DSD 期望 MSB-first；\n"
    "/// Diretta 路径的线序由 SDK 协商，与本路径独立）。SPLAYER_ALSADSD_BITREV\n"
    "/// =on/off 可强制反转/不反转（默认 auto 按源位序决定）。",
    "alsa_mmap_sink.rs: 修正位序注释",
)

# 3) 循环开头: 位序反转判定
patch(
    SINK,
    "    let mut staging_pos: usize = 0;\n"
    "    let mut eof_signaled = false;",
    "    let mut staging_pos: usize = 0;\n"
    "    let mut eof_signaled = false;\n"
    "    // 位序适配：目标线序 MSB-first（kernel quirk bitrev=0）\n"
    "    let need_bitrev = match std::env::var(\"SPLAYER_ALSADSD_BITREV\").as_deref() {\n"
    "        Ok(\"on\") | Ok(\"1\") => true,\n"
    "        Ok(\"off\") | Ok(\"0\") => false,\n"
    "        _ => reader.format().bit_order == crate::direct_dsd::DirectDsdBitOrder::LsbFirst,\n"
    "    };\n"
    "    info!(need_bitrev, \"ALSA DSD 位序适配（目标线序 MSB-first）\");",
    "alsa_mmap_sink.rs: need_bitrev 判定",
)

# 4) read_block 成功分支: 反转后入 staging
patch(
    SINK,
    "                Ok(Some(n)) => staging.extend_from_slice(&chunk[..n]),",
    "                Ok(Some(n)) => {\n"
    "                    if need_bitrev {\n"
    "                        for byte in &mut chunk[..n] {\n"
    "                            *byte = byte.reverse_bits();\n"
    "                        }\n"
    "                    }\n"
    "                    staging.extend_from_slice(&chunk[..n]);\n"
    "                }",
    "alsa_mmap_sink.rs: read_block 位序反转",
)

# 5) mmap: io_u8 -> io_i32
patch(
    SINK,
    "        let wire = std::cmp::min(frames, (staging.len() - staging_pos) / 8);\n"
    "        let result = pcm.io_u8()?.mmap(frames, |buf: &mut [u8]| {\n"
    "            let src = &staging[staging_pos..];\n"
    "            let copy = std::cmp::min(buf.len(), src.len() / 8 * 8);\n"
    "            buf[..copy].copy_from_slice(&src[..copy]);\n"
    "            copy / 8\n"
    "        });",
    "        let wire = std::cmp::min(frames, (staging.len() - staging_pos) / 8);\n"
    "        // DSD_U32_BE 仅暴露 32-bit IO（io_u8 会 EOPNOTSUPP）；x86 LE 上\n"
    "        // from_le_bytes 落内存即保持字节流顺序 = DMA 线序（BE 格式内存布局）\n"
    "        let result = pcm.io_i32()?.mmap(frames, |buf: &mut [i32]| {\n"
    "            let src = &staging[staging_pos..];\n"
    "            let words = std::cmp::min(buf.len(), src.len() / 4);\n"
    "            for (slot, w) in buf.iter_mut().enumerate().take(words) {\n"
    "                let off = w * 4;\n"
    "                *slot = i32::from_le_bytes([src[off], src[off + 1], src[off + 2], src[off + 3]]);\n"
    "            }\n"
    "            words / 2\n"
    "        });",
    "alsa_mmap_sink.rs: io_u8 -> io_i32 mmap",
)

print("patch_alsa_dsd_fix8: all done")
