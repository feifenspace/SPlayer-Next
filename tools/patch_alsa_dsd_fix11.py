"""v10 fix11: ALSA DSD 状态接线（position 打点 / now-playing 覆盖 / play-pause 路由 / seek 拒绝 / 暂停垫静音）.

在 gaoda 仓库根目录执行: python3 tools/patch_alsa_dsd_fix11.py
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

SINK = "native/audio-engine-core/src/alsa_mmap_sink.rs"
PLAYER = "native/headless-server/src/api/player.rs"

# ---------- alsa_mmap_sink.rs ----------

# 1) open() 调用点传 paused
patch(
    SINK,
    "                    dsd_write_loop(&device_owned, bit_rate, &mut reader, worker_stop, on_eof, on_failure)",
    "                    dsd_write_loop(\n"
    "                        &device_owned,\n"
    "                        bit_rate,\n"
    "                        &mut reader,\n"
    "                        worker_stop,\n"
    "                        std::sync::Arc::clone(&paused),\n"
    "                        on_eof,\n"
    "                        on_failure,\n"
    "                    )",
    "sink: 调用点传 paused",
)

# 2) dsd_write_loop 签名加 paused
patch(
    SINK,
    "fn dsd_write_loop(\n"
    "    device: &str,\n"
    "    dsd_bit_rate: u32,\n"
    "    reader: &mut crate::direct_dsd::DirectDsdReader,\n"
    "    stop: Arc<AtomicBool>,\n"
    "    on_eof: Box<dyn FnOnce() + Send>,\n"
    "    on_failure: OutputFailureCallback,\n"
    ") -> Result<()> {",
    "fn dsd_write_loop(\n"
    "    device: &str,\n"
    "    dsd_bit_rate: u32,\n"
    "    reader: &mut crate::direct_dsd::DirectDsdReader,\n"
    "    stop: Arc<AtomicBool>,\n"
    "    paused: Arc<AtomicBool>,\n"
    "    on_eof: Box<dyn FnOnce() + Send>,\n"
    "    on_failure: OutputFailureCallback,\n"
    ") -> Result<()> {",
    "sink: 签名加 paused",
)

# 3) 暂停时垫 0x69 静音（保持 DMA 供流，避免 DSD 欠载噪声；恢复无缝）
patch(
    SINK,
    "        let need_bytes = frames * frame_bytes;\n"
    "        while staging.len() - staging_pos < need_bytes && !eof_signaled {",
    "        let need_bytes = frames * frame_bytes;\n"
    "        // 暂停：不读源，持续垫 0x69 静音（0x69 翻转密度最高 ≈ 零电荷，\n"
    "        // DAC 端等效静音；保持 DMA 供流避免欠载噪声，恢复播放无缝）\n"
    "        if paused.load(Ordering::Acquire) {\n"
    "            let gap = need_bytes.saturating_sub(staging.len() - staging_pos);\n"
    "            staging.extend(std::iter::repeat(0x69).take(gap));\n"
    "        }\n"
    "        while staging.len() - staging_pos < need_bytes && !eof_signaled {",
    "sink: 暂停垫 0x69 静音",
)

# ---------- player.rs ----------

# 4) play_handler 路由 DSD
patch(
    PLAYER,
    "    let revival_source = spawn_isolated_blocking(\"player-play-worker\", move || {",
    "    // v10：ALSA DSD 流挂载时优先接管（状态机对 DSD 流不感知）\n"
    "    {\n"
    "        let handle = state.alsa_dsd_stream.lock().clone();\n"
    "        if let Some(h) = handle {\n"
    "            h.stream.play().map_err(|e| ApiError::internal(e.to_string()))?;\n"
    "            h.playing.store(true, std::sync::atomic::Ordering::Release);\n"
    "            return Ok(Json(PlayerResponse::ok(json!({ \"status\": \"playing\" }))));\n"
    "        }\n"
    "    }\n"
    "    let revival_source = spawn_isolated_blocking(\"player-play-worker\", move || {",
    "player: play 路由 DSD",
)

# 5) pause_handler 路由 DSD
patch(
    PLAYER,
    "pub(crate) async fn pause_handler(State(state): State<AppState>) -> Json<PlayerResponse> {\n"
    "    let _ = spawn_isolated_blocking(\"player-pause-worker\", move || {",
    "pub(crate) async fn pause_handler(State(state): State<AppState>) -> Json<PlayerResponse> {\n"
    "    // v10：ALSA DSD 流挂载时优先接管\n"
    "    {\n"
    "        let handle = state.alsa_dsd_stream.lock().clone();\n"
    "        if let Some(h) = handle {\n"
    "            let _ = h.stream.pause();\n"
    "            h.playing.store(false, std::sync::atomic::Ordering::Release);\n"
    "            return Json(PlayerResponse::ok(json!({ \"status\": \"paused\" })));\n"
    "        }\n"
    "    }\n"
    "    let _ = spawn_isolated_blocking(\"player-pause-worker\", move || {",
    "player: pause 路由 DSD",
)

# 6) seek 对 DSD 明确拒绝
patch(
    PLAYER,
    "    let position = payload.position_secs.max(0.0);",
    "    let position = payload.position_secs.max(0.0);\n"
    "    // v10：ALSA DSD 位纯真流不可重定位，明确拒绝而非走状态机空转\n"
    "    if state.alsa_dsd_stream.lock().is_some() {\n"
    "        return Err(ApiError::bad_request(\n"
    "            \"ALSA DSD 直出暂不支持 seek；请重新加载曲目或使用切歌控制\",\n"
    "        ));\n"
    "    }",
    "player: seek 对 DSD 拒绝",
)

# 7) run_alsa_dsd_load：位置打点线程（Weak 挂 handle，drop 自动退出）
patch(
    PLAYER,
    "    *state.alsa_dsd_stream.lock() = Some(Arc::clone(&handle));",
    "    *state.alsa_dsd_stream.lock() = Some(Arc::clone(&handle));\n"
    "\n"
    "    // 位置打点线程：Weak 引用句柄（下一次 load/stop 换装 drop 后自动退出），\n"
    "    // 暂停不累计；position 单位毫秒，now-playing 查询端换算秒并封顶 duration\n"
    "    let ticker = Arc::downgrade(&handle);\n"
    "    let _ = std::thread::Builder::new()\n"
    "        .name(\"alsa-dsd-position\".into())\n"
    "        .spawn(move || {\n"
    "            while let Some(h) = ticker.upgrade() {\n"
    "                std::thread::sleep(std::time::Duration::from_millis(250));\n"
    "                if h.playing.load(std::sync::atomic::Ordering::Acquire) {\n"
    "                    h.position.fetch_add(250, std::sync::atomic::Ordering::Release);\n"
    "                }\n"
    "            }\n"
    "        });",
    "player: 位置打点线程",
)

# 8) now_playing_handler 对 DSD 覆盖
patch(
    PLAYER,
    "pub(crate) async fn now_playing_handler(State(state): State<AppState>) -> Json<PlayerResponse> {\n"
    "    let snap = state.snapshot();",
    "pub(crate) async fn now_playing_handler(State(state): State<AppState>) -> Json<PlayerResponse> {\n"
    "    // v10：ALSA DSD 流挂载时以句柄为准（position 打点线程维护，封顶 duration）\n"
    "    {\n"
    "        let handle = state.alsa_dsd_stream.lock().clone();\n"
    "        if let Some(h) = handle {\n"
    "            let meta = state.now_playing.lock().clone();\n"
    "            let source = meta\n"
    "                .as_ref()\n"
    "                .and_then(|m| m.get(\"source\").cloned())\n"
    "                .unwrap_or(serde_json::Value::Null);\n"
    "            let playing = h.playing.load(std::sync::atomic::Ordering::Acquire);\n"
    "            let pos_ms = h.position.load(std::sync::atomic::Ordering::Acquire);\n"
    "            let position = (pos_ms as f64 / 1000.0).min(h.duration.max(0.0));\n"
    "            return Json(PlayerResponse::ok(json!({\n"
    "                \"source\": source,\n"
    "                \"metadata\": meta,\n"
    "                \"state\": if playing { \"Playing\" } else { \"Paused\" },\n"
    "                \"position\": position,\n"
    "                \"duration\": h.duration,\n"
    "                \"playing\": playing,\n"
    "            })));\n"
    "        }\n"
    "    }\n"
    "    let snap = state.snapshot();",
    "player: now-playing DSD 覆盖",
)

print("patch_alsa_dsd_fix11: all done")
