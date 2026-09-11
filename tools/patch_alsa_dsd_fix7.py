#!/usr/bin/env python3
"""v10 part 7: 清理 state.rs 误插行 + 修正可见性路径。"""

# ---- state.rs ----
P = "native/headless-server/src/state.rs"
s = open(P, encoding="utf-8").read()

# 7a. 删除误插在字段声明区的构造行
OLD = """    /// v10：ALSA 原生 DSD 直出流（alsammap + DSD 源时挂载；load/stop 轮换）
    pub alsa_dsd_stream: Arc<Mutex<Option<Arc<crate::api::player::AlsaDsdHandle>>>>,
            alsa_dsd_stream: Arc::new(Mutex::new(None)),
    /// 事件回调维护的最新状态快照（避免回调中加锁 player 导致死锁）"""
NEW = """    /// v10：ALSA 原生 DSD 直出流（alsammap + DSD 源时挂载；load/stop 轮换）
    pub alsa_dsd_stream: Arc<Mutex<Option<Arc<crate::api::AlsaDsdHandle>>>>,
    /// 事件回调维护的最新状态快照（避免回调中加锁 player 导致死锁）"""
assert s.count(OLD) == 1, "7a: %d" % s.count(OLD)
s = s.replace(OLD, NEW)

open(P, "w", encoding="utf-8").write(s)
print("state.rs cleaned")

# ---- api/mod.rs: re-export AlsaDsdHandle ----
P = "native/headless-server/src/api/mod.rs"
s = open(P, encoding="utf-8").read()
OLD = "pub use player::{LoadMeta, LoadRequest};"
NEW = """pub use player::{AlsaDsdHandle, LoadMeta, LoadRequest};"""
assert s.count(OLD) == 1, "7b: %d" % s.count(OLD)
s = s.replace(OLD, NEW)
open(P, "w", encoding="utf-8").write(s)
print("api/mod.rs re-export added")

# ---- player.rs: PlaybackStream 路径修正（crate:: 误用 → 依赖名）----
P = "native/headless-server/src/api/player.rs"
s = open(P, encoding="utf-8").read()
OLD = "    pub stream: Arc<crate::audio_engine_core::playback::PlaybackStream>,"
NEW = "    pub stream: Arc<audio_engine_core::playback::PlaybackStream>,"
assert s.count(OLD) == 1, "7c: %d" % s.count(OLD)
s = s.replace(OLD, NEW)

# 检查 mod player 是否私有：把 api/mod.rs 里的 mod player 改 pub
open(P, "w", encoding="utf-8").write(s)
print("player.rs path fixed")

# ---- api/mod.rs: mod player 可见性 ----
P = "native/headless-server/src/api/mod.rs"
s = open(P, encoding="utf-8").read()
if "mod player;" in s:
    s = s.replace("mod player;", "pub mod player;", 1)
    open(P, "w", encoding="utf-8").write(s)
    print("mod player made pub")
else:
    print("mod player already pub or different decl")
