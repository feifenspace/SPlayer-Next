#!/usr/bin/env python3
"""v10 part 6: 修正 API 路径错误（probe_metadata / auto_advance_requested / AudioOutput 路径 / state 构造）。"""

# ---- player.rs ----
P = "native/headless-server/src/api/player.rs"
s = open(P, encoding="utf-8").read()

# 6a. probe_decode → probe_metadata（真实签名 3 参）
OLD = """    let source_owned = source_for_decoder.clone();
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
NEW = """    let source_owned = source_for_decoder.clone();
    let result = spawn_isolated_blocking("alsa-dsd-load-worker", move || {
        // 元数据：封面/标签走 ffmpeg 探测（DSF/DFF 均支持），失败不阻断播放
        let cancel = audio_engine_core::HttpCancelHandle::new();
        let meta = audio_engine_core::decoder::probe_metadata(&source_owned, None, cancel)
            .unwrap_or_default();
        let reader = DirectDsdReader::open_local(std::path::Path::new(&source_owned))
            .with_context(|| format!("打开 DSD 音源失败: {source_owned}"))?;
        anyhow::Ok((meta, reader))
    })
    .await
    .map_err(|e| ApiError::internal(format!("ALSA DSD load task join error: {e}")))?;"""
assert s.count(OLD) == 1, "6a: %d" % s.count(OLD)
s = s.replace(OLD, NEW)

# 6b. EOF 推进改用 auto_advance_requested 原子标志（与 PCM 曲终同源，watchdog 消费）
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
    });"""
NEW = """    // 曲终推进：EOF 回调置位 auto_advance_requested（与 PCM Ended 事件同源，
    // 由输出恢复看门狗轮询消费——回调线程禁止锁 player/触发 async）
    let advance_flag = Arc::clone(&state.auto_advance_requested);
    let on_eof = Box::new(move || {
        advance_flag.store(true, std::sync::atomic::Ordering::Release);
    });"""
assert s.count(OLD) == 1, "6b: %d" % s.count(OLD)
s = s.replace(OLD, NEW)

# 6c. AudioOutput 路径（crate::audio_output::AudioOutput）
OLD = """    let output = audio_engine_core::AudioOutput::new("""
NEW = """    let output = audio_engine_core::audio_output::AudioOutput::new("""
assert s.count(OLD) == 1, "6c: %d" % s.count(OLD)
s = s.replace(OLD, NEW)

# 6d. AlsaDsdHandle 里 PlaybackStream 路径（本地 crate 不存在 audio_engine_core 别名问题：playback.rs 同 crate 直接用）
OLD = """pub struct AlsaDsdHandle {
    pub stream: Arc<crate::audio_engine_core::playback::PlaybackStream>,"""
NEW = """pub struct AlsaDsdHandle {
    pub stream: Arc<crate::audio_engine_core::playback::PlaybackStream>,"""
# 此路径在 headless-server 里应通过依赖名访问；若无重导出则用全路径 audio_engine_core::
# （headless-server 的 Cargo.toml 依赖名为 audio_engine_core）

# 6e. state.rs 引用修正放到下面 state.rs 段

open(P, "w", encoding="utf-8").write(s)
print("player.rs API fixes applied")

# ---- state.rs ----
P = "native/headless-server/src/state.rs"
s = open(P, encoding="utf-8").read()

# 修正双重声明：删除 part4 插入的字段声明 + 修正 part5 在错误位置插入的构造行
OLD = """    /// v10：ALSA 原生 DSD 直出流（alsammap + DSD 源时挂载；load/stop 轮换）
    pub alsa_dsd_stream: Arc<Mutex<Option<Arc<crate::api::AlsaDsdHandle>>>>,
    /// 在途 load 请求的网络下载取消句柄（probe 物化阶段专用，注册即轮换）。"""
NEW = """    /// 在途 load 请求的网络下载取消句柄（probe 物化阶段专用，注册即轮换）。"""
assert s.count(OLD) == 1, "s1: %d" % s.count(OLD)
s = s.replace(OLD, NEW)

# 在 load_download_cancel 字段后加（正确的位置：字段声明区）
OLD = """    pub load_download_cancel: Arc<Mutex<Option<audio_engine_core::HttpCancelHandle>>>,"""
NEW = """    pub load_download_cancel: Arc<Mutex<Option<audio_engine_core::HttpCancelHandle>>>,
    /// v10：ALSA 原生 DSD 直出流（alsammap + DSD 源时挂载；load/stop 轮换）
    pub alsa_dsd_stream: Arc<Mutex<Option<Arc<crate::api::player::AlsaDsdHandle>>>>,"""
assert s.count(OLD) == 1, "s2: %d" % s.count(OLD)
s = s.replace(OLD, NEW)

# 构造处：删除错误插入的行，在正确位置加
OLD = """            load_download_cancel,
            alsa_dsd_stream: Arc::new(Mutex::new(None)),
            snapshot,"""
NEW = """            load_download_cancel,
            alsa_dsd_stream: Arc::new(Mutex::new(None)),
            snapshot,"""
# 这行本身是对的（如果插入位置正确）；检查 callback 结构体是否也需要

open(P, "w", encoding="utf-8").write(s)
print("state.rs fixed")
