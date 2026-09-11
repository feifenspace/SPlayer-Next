"""v10 fix12: run_alsa_dsd_load 漏 move direct_initial_take 导致 async 上下文 drop 崩溃.

在 gaoda 仓库根目录执行: python3 tools/patch_alsa_dsd_fix12.py

事故: 流媒体(HTTP)播放中切 DSD → 整进程 abort。
根因: run_alsa_dsd_load 解构 LoadReservation 时漏掉 direct_initial_take
(上一播放 OldThreads), 随 reservation 在主 tokio worker 上 drop;
HTTP 流源 drop 链含 ffmpeg_audio/reqwest 内部 runtime ->
"Cannot drop a runtime in an async context" panic → abort。
常规路径 finish_regular_load 是把它 move 进 spawn_isolated_blocking 的, 所以无恙。
修复: 解构补上 direct_initial_take, move 进已有 alsa-dsd-load-worker 闭包内 drop。
"""
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PLAYER = "native/headless-server/src/api/player.rs"

def patch(old: str, new: str, label: str) -> None:
    p = ROOT / PLAYER
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
    """    let LoadReservation {
        token,
        cover_dir,
        device_name,
        ..
    } = reservation;
    let selector = device_name.clone().unwrap_or_default();""",
    """    let LoadReservation {
        direct_initial_take,
        token,
        cover_dir,
        device_name,
        ..
    } = reservation;
    let selector = device_name.clone().unwrap_or_default();""",
    "解构补 direct_initial_take",
)

patch(
    """    let result = spawn_isolated_blocking("alsa-dsd-load-worker", move || {
        // 元数据：封面/标签走 ffmpeg 探测（DSF/DFF 均支持），失败不阻断播放""",
    """    let result = spawn_isolated_blocking("alsa-dsd-load-worker", move || {
        // old_threads 必须在普通线程 drop：HTTP 流源的 drop 链含 ffmpeg_audio/
        // reqwest 内部 tokio runtime，在 async 上下文（主 worker）drop 会触发
        // "Cannot drop a runtime" panic → 整进程 abort（2026-09-11 16:41 事故）
        drop(direct_initial_take);
        // 元数据：封面/标签走 ffmpeg 探测（DSF/DFF 均支持），失败不阻断播放""",
    "old_threads 挪进隔离线程 drop",
)

print("patch_alsa_dsd_fix12: all done")
