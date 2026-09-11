#!/usr/bin/env python3
"""v10 part 4: AppState 挂 ALSA DSD 流生命周期 + 控制端点适配 + probe 修正。"""

# ---- state.rs: 加字段 ----
P = "native/headless-server/src/state.rs"
s = open(P, encoding="utf-8").read()
OLD = """    /// 在途 load 请求的网络下载取消句柄（probe 物化阶段专用，注册即轮换）。"""
NEW = """    /// v10：ALSA 原生 DSD 直出流（alsammap + DSD 源时挂载；load/stop 轮换）
    pub alsa_dsd_stream: Arc<Mutex<Option<Arc<crate::api::AlsaDsdHandle>>>>,
    /// 在途 load 请求的网络下载取消句柄（probe 物化阶段专用，注册即轮换）。"""
assert s.count(OLD) == 1, "st: %d" % s.count(OLD)
s = s.replace(OLD, NEW)
open(P, "w", encoding="utf-8").write(s)
print("state.rs field added")

# ---- player.rs: AlsaDsdHandle 定义 + probe 修正 + 流挂载 + 端点适配 ----
P = "native/headless-server/src/api/player.rs"
s = open(P, encoding="utf-8").read()

# 4a. AlsaDsdHandle 公共类型（插在 update_now_playing 前）
ANCHOR = "pub(crate) fn update_now_playing("
HANDLE = """/// v10：ALSA DSD 直出流句柄（AppState 挂载，控制端点消费）
pub struct AlsaDsdHandle {
    pub stream: Arc<crate::audio_engine_core::playback::PlaybackStream>,
    /// 播放位置（秒，曲内递增）：写循环 EOF 原子更新，now-playing 消费
    pub position: Arc<std::sync::atomic::AtomicU64>,
    /// 曲目时长
    pub duration: f64,
    /// 播放状态：true=Playing
    pub playing: Arc<std::sync::atomic::AtomicBool>,
}

pub(crate) fn update_now_playing("""
assert s.count(ANCHOR) == 1, "h: %d" % s.count(ANCHOR)
s = s.replace(ANCHOR, HANDLE, 1)

open(P, "w", encoding="utf-8").write(s)
print("player.rs handle type added")
