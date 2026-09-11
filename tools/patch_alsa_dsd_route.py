#!/usr/bin/env python3
"""v10 part 3: headless 路由——alsammap + DSD 分流到 run_alsa_dsd_load。"""

P = "native/headless-server/src/api/player.rs"

s = open(P, encoding="utf-8").read()

# 1) 移除 alsammap 的 DSD 降级
OLD1 = '''            let downgrade_reason = if is_native_dsd_source(source) {
                Some("DSD 源需 DSD→PCM 转换，alsammap 位纯直出不支持".to_string())
            } else {
                player
                    .validate_alsammap_entry()
                    .err()
                    .map(|e| e.to_string())
            };'''
NEW1 = '''            // v10：alsammap + native DSD 不再降级——GR40 等原生 DSD 声卡走
            // DSD_U32_BE 专用直出路径（run_alsa_dsd_load）；仅 PCM 源保持
            // 音量/EQ 门槛校验
            let downgrade_reason = if is_native_dsd_source(source) {
                None
            } else {
                player
                    .validate_alsammap_entry()
                    .err()
                    .map(|e| e.to_string())
            };'''
assert s.count(OLD1) == 1, "a1: %d" % s.count(OLD1)
s = s.replace(OLD1, NEW1)

# 2) load 分流
OLD2 = '''    if reservation.direct_selector.is_some() {
        return run_direct_load(
            &state,
            source,
            auto_play,
            source_for_decoder,
            meta_duration_secs,
            reservation,
        )
        .await;
    }'''
NEW2 = '''    // v10：alsammap 设备 + native DSD 源 → ALSA 原生 DSD 直出（DSD_U32_BE）
    let alsa_dsd = reservation
        .device_name
        .as_deref()
        .is_some_and(|d| d.starts_with("alsammap:"))
        && is_native_dsd_source(&source_for_decoder);
    if alsa_dsd {
        return run_alsa_dsd_load(state, source, auto_play, source_for_decoder, reservation).await;
    }
    if reservation.direct_selector.is_some() {
        return run_direct_load(
            &state,
            source,
            auto_play,
            source_for_decoder,
            meta_duration_secs,
            reservation,
        )
        .await;
    }'''
assert s.count(OLD2) == 1, "a2: %d" % s.count(OLD2)
s = s.replace(OLD2, NEW2)

# 3) 新增 run_alsa_dsd_load
ANCHOR = "async fn finish_regular_load("
FUNC = '''/// v10：ALSA 原生 DSD 直出（DSD_U32_BE，GR40 等 raw DSD 声卡）。
/// 独立于 Direct 家族：无 handoff/预载/watchdog 体系，单机直出。
/// 播放结束推进由 on_eof 回调驱动（tokio channel → advance_after_finish 同源语义）
async fn run_alsa_dsd_load(
    state: AppState,
    source: String,
    auto_play: bool,
    source_for_decoder: String,
    reservation: LoadReservation,
) -> Result<Json<PlayerResponse>, ApiError> {
    use audio_engine_core::direct_dsd::DirectDsdReader;

    let LoadReservation {
        token,
        cover_dir,
        device_name,
        ..
    } = reservation;
    let selector = device_name.clone().unwrap_or_default();

    let source_owned = source_for_decoder.clone();
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
    .map_err(|e| ApiError::internal(format!("ALSA DSD load task join error: {e}")))?;

    let (metadata, reader) = match result {
        Ok(res) => res,
        Err(err) => {
            let mut player = state.player.lock();
            player.clear_pending_load(token);
            return Err(ApiError::internal(format!("ALSA DSD 加载失败: {err}")));
        }
    };

    // 曲终推进：EOF 回调 → tokio channel → 跳下一曲（与 PCM 曲终同源语义）
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
}

async fn finish_regular_load('''
assert s.count(ANCHOR) == 1, "a3: %d" % s.count(ANCHOR)
s = s.replace(ANCHOR, FUNC, 1)
open(P, "w", encoding="utf-8").write(s)
print("headless routing wired")
