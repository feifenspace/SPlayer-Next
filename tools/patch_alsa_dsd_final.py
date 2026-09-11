#!/usr/bin/env python3
"""v10 part 5: run_alsa_dsd_load 挂载到 AppState + stop/now-playing 端点适配 + state 初始化。"""

# ---- player.rs ----
P = "native/headless-server/src/api/player.rs"
s = open(P, encoding="utf-8").read()

# 5a. probe 改用现成 probe_decode（返回 PreparedDecoder → into_metadata）
OLD = """    let source_owned = source_for_decoder.clone();
    let result = spawn_isolated_blocking("alsa-dsd-load-worker", move || {
        // 元数据：封面/标签走 ffmpeg 探测（DSF/DFF 均支持），失败不阻断播放
        let meta = audio_engine_core::decoder::probe_metadata_only(
            &source_owned,
            None,
        )
        .map(|m| m.into_metadata())
        .unwrap_or_default();
        let reader = DirectDsdReader::open_local(std::path::Path::new(&source_owned))
            .with_context(|| format!("打开 DSD 音源失败: {source_owned}"))?;
        anyhow::Ok((meta, reader))
    })
    .await
    .map_err(|e| ApiError::internal(format!("ALSA DSD load task join error: {e}")))?;"""
NEW = """    let source_owned = source_for_decoder.clone();
    let result = spawn_isolated_blocking("alsa-dsd-load-worker", move || {
        // 元数据：封面/标签走 ffmpeg 探测（DSF/DFF 均支持），失败不阻断播放
        let cancel = audio_engine_core::HttpCancelHandle::new();
        let meta = audio_engine_core::decoder::probe_decode(&source_owned, None, cancel)
            .map(|p| p.into_metadata())
            .unwrap_or_default();
        let reader = DirectDsdReader::open_local(std::path::Path::new(&source_owned))
            .with_context(|| format!("打开 DSD 音源失败: {source_owned}"))?;
        anyhow::Ok((meta, reader))
    })
    .await
    .map_err(|e| ApiError::internal(format!("ALSA DSD load task join error: {e}")))?;"""
assert s.count(OLD) == 1, "5a: %d" % s.count(OLD)
s = s.replace(OLD, NEW)

# 5b. 流挂载改写：挂 AppState + 原子 position/playing + EOF 推进保持
OLD = """    // 曲终推进：EOF 回调 → tokio channel → 跳下一曲（与 PCM 曲终同源语义）
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let state_eof = state.clone();
    let source_eof = source.clone();
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            super::advance_after_finish(&state_eof, &source_eof).await;
            break;
        }
    });

    let on_eof = Box::new(move || {
        let _ = tx.send(());
    });

    // 输出配置：alsammap 后端（probe 协商）+ DSD 流
    let output = audio_engine_core::AudioOutput::new(
        Some(&selector),
        None, // DSD 不走 PCM 采样率协商
        0,
        Arc::new(|| {}),
    )
    .map_err(|e| ApiError::internal(format!("打开 alsammap 输出配置失败: {e}")))?;
    let stream = output
        .build_dsd_stream(reader, on_eof)
        .map_err(|e| ApiError::internal(format!("打开 ALSA DSD 流失败: {e}")))?;
    let stream = std::sync::Arc::new(stream);
    if auto_play {
        stream.play().map_err(|e| ApiError::internal(e.to_string()))?;
    }

    {
        let mut player = state.player.lock();
        player.clear_pending_load(token);
    }

    update_now_playing(&state, &source, &metadata);
    state.note_source_change(Some(&source));
    Ok(Json(PlayerResponse::ok(json!({
        "status": "loaded",
        "source": source,
        "output": "alsammap-dsd",
    }))))
}"""
NEW = """    // 曲终推进：EOF 回调 → tokio channel → 跳下一曲（与 PCM 曲终同源语义）
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let state_eof = state.clone();
    let source_eof = source.clone();
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            super::advance_after_finish(&state_eof, &source_eof).await;
            break;
        }
    });

    let on_eof = Box::new(move || {
        let _ = tx.send(());
    });

    // 输出配置：alsammap 后端（probe 协商）+ DSD 流
    let output = audio_engine_core::AudioOutput::new(
        Some(&selector),
        None, // DSD 不走 PCM 采样率协商
        0,
        Arc::new(|| {}),
    )
    .map_err(|e| ApiError::internal(format!("打开 alsammap 输出配置失败: {e}")))?;
    let stream = output
        .build_dsd_stream(reader, on_eof)
        .map_err(|e| ApiError::internal(format!("打开 ALSA DSD 流失败: {e}")))?;
    let stream = std::sync::Arc::new(stream);
    if auto_play {
        stream.play().map_err(|e| ApiError::internal(e.to_string()))?;
    }

    // v10：挂载到 AppState（流生命周期 = 本次播放；下次 load/stop 时轮换 drop）
    let duration = metadata.duration_secs;
    let handle = Arc::new(AlsaDsdHandle {
        stream,
        position: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        duration,
        playing: Arc::new(std::sync::atomic::AtomicBool::new(auto_play)),
    });
    {
        let mut player = state.player.lock();
        player.clear_pending_load(token);
    }
    *state.alsa_dsd_stream.lock() = Some(Arc::clone(&handle));

    update_now_playing(&state, &source, &metadata);
    state.note_source_change(Some(&source));
    Ok(Json(PlayerResponse::ok(json!({
        "status": "loaded",
        "source": source,
        "output": "alsammap-dsd",
    }))))
}"""
assert s.count(OLD) == 1, "5b: %d" % s.count(OLD)
s = s.replace(OLD, NEW)

# 5c. stop_handler 适配：先 drop DSD 流
OLD = """pub(crate) async fn stop_handler(State(state): State<AppState>) -> Json<PlayerResponse> {"""
NEW = """pub(crate) async fn stop_handler(State(state): State<AppState>) -> Json<PlayerResponse> {
    // v10：ALSA DSD 流先拆（写循环读 stop 标志退出，PCM 路径不受影响）
    {
        let taken = state.alsa_dsd_stream.lock().take();
        drop(taken);
    }"""
assert s.count(OLD) == 1, "5c: %d" % s.count(OLD)
s = s.replace(OLD, NEW)

open(P, "w", encoding="utf-8").write(s)
print("player.rs load/stop wired")

# ---- state.rs: AppState 初始化处加字段 ----
P = "native/headless-server/src/state.rs"
s = open(P, encoding="utf-8").read()
# 找 AppState 构造（impl 里 ..Default 或显式构造）——查 load_download_cancel 的初始化
import re
m = re.search(r"(load_download_cancel:\s*[^,\n]+,)", s)
if m:
    s = s.replace(m.group(1), m.group(1) + "\n            alsa_dsd_stream: Arc::new(Mutex::new(None)),", 1)
    open(P, "w", encoding="utf-8").write(s)
    print("state.rs init added at load_download_cancel")
else:
    print("WARN: AppState init not found - needs manual fix")
