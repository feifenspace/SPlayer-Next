//! 应用共享状态
//!
//! AppState 持有播放器、配置和 WebSocket 广播频道。
//! EventEmitter 回调通过快照缓存避免直接持有 player 锁，防止死锁。

use std::sync::Arc;

use audio_engine_core::{EventEmitter, InnerPlayer, PlayerEvent, PlayerState};
use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;
use tracing::info;

use crate::config::Config;

/// server_state 表键：最后一次选择的输出设备（重启自动恢复，浏览器不在场也能连对设备）
pub const OUTPUT_DEVICE_STATE_KEY: &str = "output_device";

fn serialize_player_state<S>(state: &PlayerState, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(match state {
        PlayerState::Idle => "idle",
        PlayerState::Playing => "playing",
        PlayerState::Paused => "paused",
        PlayerState::Stopped => "stopped",
    })
}

/// WebSocket 状态推送消息
#[derive(Clone, serde::Serialize)]
pub struct WsState {
    pub position: f64,
    pub duration: f64,
    pub volume: f32,
    #[serde(serialize_with = "serialize_player_state")]
    pub state: PlayerState,
    /// 当前播放 source：服务端接力/boundary 切曲后，前端据此采纳队列曲目推进 UI
    /// （缺失时前端 UI 与服务端音频脱节——遥控器显示已切曲前的曲目）
    pub current_source: Option<String>,
    /// 当前曲在队列快照中的 track_id：前端 adopt 按 id 匹配，避免依赖
    /// 直链串一致（同一曲两次解析的 URL 不同）
    pub current_track_id: Option<String>,
}

/// 播放器状态快照（用于 HTTP 响应和 WebSocket 推送）
#[derive(Clone, serde::Serialize)]
pub struct PlayerSnapshot {
    pub position: f64,
    pub duration: f64,
    pub volume: f32,
    /// 当前播放速度（1.0 = 原速）
    pub speed: f32,
    #[serde(serialize_with = "serialize_player_state")]
    pub state: PlayerState,
    pub is_finished: bool,
    pub current_source: Option<String>,
}

/// 扫描进度推送消息
#[derive(Clone, serde::Serialize)]
pub struct ScanProgressMessage {
    pub r#type: String, // "progress" | "done"
    pub phase: String,  // "scanning" | "done" | "error"
    pub scanned: u32,
    pub total: u32,
    pub current: Option<String>,
}

/// 下一曲自动连播候选（B 层单槽：后写覆盖，曲终加载后即消费）
#[derive(Debug, Clone)]
pub struct PendingNext {
    pub source: String,
    pub duration_hint: Option<f64>,
}

/// 播放队列重复模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum QueueRepeat {
    Off,
    All,
    One,
}

impl QueueRepeat {
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::to_ascii_lowercase).as_deref() {
            Some("all") => Self::All,
            Some("one") => Self::One,
            _ => Self::Off,
        }
    }
}

/// 队列条目：source 与 load API 的取值语义一致（绝对路径/HTTP 直链/cue:// 等）。
/// track 为前端完整曲目快照（透传字段，服务端不解释）：浏览器存储清空后
/// 重开页面时前端据此恢复平台身份（id/source/流媒体 serverId/CUE 分段等）
#[derive(Debug, Clone, serde::Serialize)]
pub struct QueueItem {
    pub source: String,
    pub duration_ms: Option<u64>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub cover: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track: Option<serde_json::Value>,
}

/// 服务端播放队列快照：Direct 无缝预载与曲终接力的队列权威。
/// 由前端整表推送（PUT /api/v1/player/queue）；服务端在 boundary 提交时
/// 推进游标（align_by_source 自愈，队列被重排后按 source 重新对齐）。
/// 未注册队列时无缝预载不工作，回退前端驱动的候选/接力旧链路。
#[derive(Debug, Clone)]
pub struct QueueSnapshot {
    pub items: Vec<QueueItem>,
    /// 播放顺序（shuffle 在注册时物化为本快照内的确定性排列）
    pub order: Vec<usize>,
    /// order 中当前播放位置
    pub pos: usize,
    pub repeat: QueueRepeat,
}

impl QueueSnapshot {
    /// 按 source 反查队列条目的前端曲目 id（接力/边界后随 WS 带回，前端按 id
    /// 采纳新曲）。找不到（手动 load 队列外曲目）返回 None
    pub fn track_id_for_source(&self, source: &str) -> Option<String> {
        self.items
            .iter()
            .find(|item| item.source == source)
            .and_then(|item| {
                item.track
                    .as_ref()
                    .and_then(|track| track.get("id"))
                    .and_then(|id| id.as_str())
                    .map(String::from)
            })
    }

    pub fn new(items: Vec<QueueItem>, index: usize, repeat: QueueRepeat, shuffle: bool) -> Self {
        let n = items.len();
        let start = if n == 0 { 0 } else { index.min(n - 1) };
        let mut order: Vec<usize> = (0..n).collect();
        if shuffle && n > 1 {
            // 确定性洗牌：当前播放条目固定在播放顺序首位（repeat=off 时其余
            // 曲目全部可播到，不会因当前曲落在排列中段而被截断跳过），
            // 其余条目 LCG 确定性洗牌接在其后——服务端自治推进时顺序自洽
            order.remove(start);
            let mut seed = n as u64 ^ 0x9E37_79B9_7F4A_7C15 ^ ((start as u64) << 32);
            for i in (1..order.len()).rev() {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let j = (seed >> 33) as usize % (i + 1);
                order.swap(i, j);
            }
            order.insert(0, start);
        }
        let pos = if n == 0 {
            0
        } else {
            order.iter().position(|&v| v == start).unwrap_or(0)
        };
        Self {
            items,
            order,
            pos,
            repeat,
        }
    }

    pub fn current(&self) -> Option<&QueueItem> {
        self.items.get(*self.order.get(self.pos)?)
    }

    /// 下一个播放条目及其 order 位置。repeat=one 返回 None
    /// （单曲重播由既有 Ended 接力/前端处理，不做无缝预载）
    pub fn next(&self) -> Option<(usize, &QueueItem)> {
        if self.items.is_empty() || self.repeat == QueueRepeat::One {
            return None;
        }
        let next_pos = match self.pos + 1 {
            next if next < self.order.len() => next,
            // 队尾：repeat=all 回卷到顺序头
            _ if self.repeat == QueueRepeat::All => 0,
            _ => return None,
        };
        let item = self.items.get(*self.order.get(next_pos)?)?;
        Some((next_pos, item))
    }

    /// 按条目 source 把当前播放位置对齐到队列（队列重排/换歌后自愈）。
    /// 找不到时保持原位
    pub fn align_by_source(&mut self, source: Option<&str>) {
        let Some(source) = source else { return };
        if let Some(pos) = self
            .order
            .iter()
            .position(|&idx| self.items.get(idx).is_some_and(|it| it.source == source))
        {
            self.pos = pos;
        }
    }
}

/// 应用全局状态
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub player: Arc<Mutex<InnerPlayer>>,
    pub db: Arc<Mutex<rusqlite::Connection>>,
    /// WebSocket 状态广播频道
    pub ws_tx: broadcast::Sender<serde_json::Value>,
    pub scan_tx: broadcast::Sender<ScanProgressMessage>,
    pub is_scanning: Arc<std::sync::atomic::AtomicBool>,
    pub scan_cancel: Arc<std::sync::atomic::AtomicBool>,
    /// 输出停滞/失败恢复请求（最后一次请求的 Unix 毫秒时间戳，0 = 无请求）。
    /// 事件回调置位，由输出恢复看门狗消费——回调线程禁止锁 player 或触发 async
    pub output_recovery_requested: Arc<std::sync::atomic::AtomicU64>,
    /// 曲终自动连播请求（Ended 事件置位，输出恢复看门狗消费）
    pub auto_advance_requested: Arc<std::sync::atomic::AtomicBool>,
    /// FFT 频谱订阅连接数（ws 维护）：归零时关闭引擎 FFT 定时器避免无消费空转
    pub fft_subscriber_count: Arc<std::sync::atomic::AtomicUsize>,
    /// 正在播放曲目的服务端元数据快照（load/自动接续成功时更新）
    pub now_playing: Arc<Mutex<Option<serde_json::Value>>>,
    /// 已 stage 的下一曲元数据（generation, metadata）：boundary 切换时转正到 now_playing
    pub staged_meta: Arc<Mutex<Option<(u64, serde_json::Value)>>>,
    /// 下一曲候选（B 层自动连播单槽；None = 未注册）
    pub pending_next: Arc<Mutex<Option<PendingNext>>>,
    /// 服务端播放队列快照（PUT /api/v1/player/queue 注册；None = 前端驱动旧链路）。
    /// Direct 无缝预载与 boundary 自治推进的队列权威
    pub queue: Arc<Mutex<Option<QueueSnapshot>>>,
    /// 待自治消费的 Direct boundary generation（事件回调置位，看门狗消费——
    /// 回调线程禁止锁 player）：无缝边界后的簿记提交与再预载调度
    pub direct_boundary_event: Arc<Mutex<Option<u64>>>,
    /// 在途 load 请求的网络下载取消句柄（probe 物化阶段专用，注册即轮换）。
    /// 下一次 load/stop 时 cancel 上一请求仍在途的全量下载——下载不受
    /// load token 校验中断，无此机制会占满线程与带宽直到自身超时
    pub load_download_cancel: Arc<Mutex<Option<audio_engine_core::HttpCancelHandle>>>,
    /// 事件回调维护的最新状态快照（避免回调中加锁 player 导致死锁）
    snapshot: Arc<RwLock<Option<PlayerSnapshot>>>,
}

impl AppState {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        let db_path = config.resolved_database_path();
        let db_conn = crate::db::init_db(&db_path)?;

        // 输出设备恢复：显式配置的 diretta_target 优先（运营者写死，不被浏览器
        // 上次的选择覆盖）；headless 输出路径只有 ALSA MMAP 与 Diretta——
        // 记忆的 Diretta 目标直接恢复；本地设备（alsammap）与空选择均不自动
        // 连接，转为后台扫描局域网 Diretta 自动补位（运行时选择，不覆盖 db
        // 中的用户显式选择）
        let saved_output_device = if config.diretta_target.is_none() {
            crate::db::get_server_state(&db_conn, OUTPUT_DEVICE_STATE_KEY)
                .ok()
                .flatten()
        } else {
            None
        };
        let db = Arc::new(Mutex::new(db_conn));

        let mut inner_player = InnerPlayer::new()?;
        let cover_dir = config.resolved_cover_cache_dir();
        if let Some(cover_str) = cover_dir.to_str() {
            inner_player.set_cover_cache_dir(cover_str.to_string());
        }
        let mut auto_scan_diretta = false;
        if let Some(ref target) = config.diretta_target {
            let diretta_dev = format!("diretta:{}", target);
            inner_player.set_output_device(Some(diretta_dev));
        } else {
            let saved = saved_output_device.filter(|s| !s.is_empty());
            match saved.as_deref() {
                Some(dev) if dev.starts_with("diretta:") || dev.starts_with("diretta@") => {
                    info!(device = %dev, "恢复上次输出设备（Diretta）");
                    inner_player.set_output_device(Some(dev.to_owned()));
                }
                other => {
                    info!(device = ?other, "非 Diretta 记忆，不自动连接本地设备，转入局域网 Diretta 自动发现");
                    auto_scan_diretta = true;
                }
            }
        }
        let player = Arc::new(Mutex::new(inner_player));

        // 后台自动发现：未记忆 Diretta 目标时扫描局域网，首个在线目标设为
        // 运行时输出（不写 db——db 保留用户显式选择，下次启动仍按本规则判定）。
        // 扫描阻塞（DKS 发现重试），不能卡 AppState::new 主链路
        if auto_scan_diretta {
            let player_for_auto_scan = Arc::clone(&player);
            let _ = std::thread::Builder::new()
                .name("diretta-autoselect".into())
                .spawn(move || {
                    let targets = audio_engine_core::diretta::scan_devices().unwrap_or_default();
                    if let Some(first) = targets.first() {
                        info!(
                            device = %first.id,
                            name = %first.output_name,
                            "局域网 Diretta 自动发现，设为默认输出"
                        );
                        player_for_auto_scan.lock().set_output_device(Some(first.id.clone()));
                    } else {
                        info!("局域网未发现 Diretta 目标，保持未选择输出（等待手动选择）");
                    }
                });
        }

        let (ws_tx, _rx) = broadcast::channel(128);
        let (scan_tx, _rx_scan) = broadcast::channel(128);
        let snapshot: Arc<RwLock<Option<PlayerSnapshot>>> = Arc::new(RwLock::new(None));
        let is_scanning = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let scan_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let output_recovery_requested = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let auto_advance_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fft_subscriber_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pending_next = Arc::new(Mutex::new(None));
        let queue: Arc<Mutex<Option<QueueSnapshot>>> = Arc::new(Mutex::new(None));
        let direct_boundary_event: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let load_download_cancel: Arc<Mutex<Option<audio_engine_core::HttpCancelHandle>>> =
            Arc::new(Mutex::new(None));
        let now_playing: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let staged_meta: Arc<Mutex<Option<(u64, serde_json::Value)>>> = Arc::new(Mutex::new(None));

        // 回调可能在播放器内部线程触发，不能在这里再次 lock player。
        let callback: EventEmitter = {
            let ws_tx = ws_tx.clone();
            let snapshot = Arc::clone(&snapshot);
            let output_recovery_requested = Arc::clone(&output_recovery_requested);
            let auto_advance_requested = Arc::clone(&auto_advance_requested);
            let now_playing = Arc::clone(&now_playing);
            let staged_meta = Arc::clone(&staged_meta);
            let pending_next = Arc::clone(&pending_next);
            let direct_boundary_event = Arc::clone(&direct_boundary_event);
            let queue = Arc::clone(&queue);
            // 当前 source → 队列条目 track id（WS 每次状态推送随带，前端按 id 采纳）
            let resolve_current_track_id = move |source: Option<&str>| -> Option<String> {
                let source = source?;
                queue
                    .lock()
                    .as_ref()
                    .and_then(|q| q.track_id_for_source(source))
            };
            Arc::new(move |event: PlayerEvent| {
                // 先 clone 一份当前快照，避免持有读锁跨越后续写锁操作。
                // 缓存承载完整 PlayerSnapshot：HTTP/WS 轮询不再碰 player 锁（A4）
                let current: Option<PlayerSnapshot> = snapshot.read().clone();
                let mut authoritative = current.clone().unwrap_or(PlayerSnapshot {
                    position: 0.0,
                    duration: 0.0,
                    volume: 1.0,
                    speed: 1.0,
                    state: PlayerState::Idle,
                    is_finished: false,
                    current_source: None,
                });
                match event {
                    PlayerEvent::StateChanged { state } => {
                        authoritative.state = state;
                        let current_source = authoritative.current_source.clone();
                        let current_track_id = resolve_current_track_id(current_source.as_deref());
                        let ws_state = WsState {
                            position: authoritative.position,
                            duration: authoritative.duration,
                            volume: authoritative.volume,
                            state,
                            current_source,
                            current_track_id,
                        };
                        *snapshot.write() = Some(authoritative.clone());
                        if let Ok(data) = serde_json::to_value(&ws_state) {
                            let _ =
                                ws_tx.send(serde_json::json!({ "type": "state", "data": data }));
                        }
                    }
                    PlayerEvent::Position { position, duration } => {
                        authoritative.position = position;
                        authoritative.duration = duration;
                        let current_source = authoritative.current_source.clone();
                        let current_track_id = resolve_current_track_id(current_source.as_deref());
                        let ws_state = WsState {
                            position,
                            duration,
                            volume: authoritative.volume,
                            state: authoritative.state,
                            current_source,
                            current_track_id,
                        };
                        *snapshot.write() = Some(authoritative.clone());
                        if let Ok(data) = serde_json::to_value(&ws_state) {
                            let _ =
                                ws_tx.send(serde_json::json!({ "type": "state", "data": data }));
                        }
                    }
                    PlayerEvent::Ended => {
                        authoritative.is_finished = true;
                        authoritative.state = PlayerState::Stopped;
                        authoritative.position = authoritative.duration;
                        *snapshot.write() = Some(authoritative);
                        // 置自动连播标志：有注册候选时看门狗会在曲终自动接续
                        auto_advance_requested.store(true, std::sync::atomic::Ordering::Release);
                        let _ = ws_tx.send(serde_json::json!({ "type": "ended", "data": {} }));
                    }
                    PlayerEvent::SourceError => {
                        authoritative.position = 0.0;
                        authoritative.duration = 0.0;
                        authoritative.state = PlayerState::Idle;
                        *snapshot.write() = Some(authoritative);
                        // 接入输出恢复看门狗：音源中途失败（解码/读文件/格式突变）
                        // 原先只置 Idle 广播 sourceError，无任何自动恢复 = 直接停播。
                        // 复用 OutputStalled 同一条恢复链路（重载有次数上限与跳曲
                        // 兜底，坏源不会无限循环）
                        output_recovery_requested
                            .store(unix_millis(), std::sync::atomic::Ordering::Release);
                        let _ =
                            ws_tx.send(serde_json::json!({ "type": "sourceError", "data": {} }));
                    }
                    PlayerEvent::DirectTrackBoundary {
                        duration,
                        generation,
                    } => {
                        // 无缝边界：已 stage 的候选元数据转正为 now-playing 快照，
                        // 保证重开页面/无浏览器场景都能显示正确曲目。
                        // generation 不匹配时保留 staged_meta：consume_boundary 的
                        // 孤儿 boundary 兜底还要用它做簿记（取走会丢 source）
                        let mut promoted_source = None;
                        let mut promoted_track_id = None;
                        {
                            let mut guard = staged_meta.lock();
                            if let Some((g, meta)) = guard.as_ref() {
                                if *g == generation {
                                    promoted_source = meta
                                        .get("source")
                                        .and_then(|v| v.as_str())
                                        .map(String::from);
                                    promoted_track_id = meta
                                        .get("track_id")
                                        .and_then(|v| v.as_str())
                                        .map(String::from);
                                    *now_playing.lock() = Some(meta.clone());
                                    guard.take();
                                }
                            }
                        }
                        authoritative.position = 0.0;
                        authoritative.duration = duration;
                        authoritative.state = PlayerState::Playing;
                        if promoted_source.is_some() {
                            authoritative.current_source = promoted_source.clone();
                            authoritative.is_finished = false;
                        }
                        *snapshot.write() = Some(authoritative);
                        // 边界即候选消费点：刚切入的曲子就是 pending_next 里注册的
                        // 那首，不清掉的话曲终自动连播会在它播完后重放一遍
                        // （引擎 current_source 不随边界更新，曲终时无法自证重复）。
                        // 浏览器在场时 position tick 一两秒内会重注册新的下一曲
                        *pending_next.lock() = None;
                        // 交由看门狗自治消费：簿记提交 + 队列推进 + 再预载调度
                        *direct_boundary_event.lock() = Some(generation);
                        let _ = ws_tx.send(serde_json::json!({
                            "type": "directTrackBoundary",
                            "data": {
                                "duration": duration,
                                "generation": generation,
                                "source": promoted_source,
                                "track_id": promoted_track_id,
                            },
                        }));
                    }
                    // 输出停滞/失败：置恢复请求标志交由输出恢复看门狗全量重载，
                    // 并向 WS 转发供客户端感知（InnerPlayer 核心不会自行重建输出）
                    PlayerEvent::OutputStalled | PlayerEvent::OutputFailed => {
                        let kind = if matches!(event, PlayerEvent::OutputFailed) {
                            "outputFailed"
                        } else {
                            "outputStalled"
                        };
                        output_recovery_requested
                            .store(unix_millis(), std::sync::atomic::Ordering::Release);
                        let _ = ws_tx.send(serde_json::json!({ "type": kind, "data": {} }));
                    }
                    PlayerEvent::FftData { ldata, rdata } => {
                        // 仅进广播频道：ws_run 按每连接订阅过滤；FFT 定时器只在
                        // 有订阅者时开启（ws_run 负责 set_fft_enabled），避免无消费空转
                        let _ = ws_tx.send(serde_json::json!({
                            "type": "fftData",
                            "data": { "ldata": ldata, "rdata": rdata },
                        }));
                    }
                    PlayerEvent::Seeked { position } => {
                        let _ = ws_tx.send(serde_json::json!({
                            "type": "seeked",
                            "data": { "position": position },
                        }));
                    }
                    #[allow(unreachable_patterns)]
                    _ => {}
                }
            })
        };
        player.lock().set_event_callback(callback);

        Ok(Self {
            config: Arc::new(config.clone()),
            player,
            db,
            ws_tx,
            scan_tx,
            is_scanning,
            scan_cancel,
            output_recovery_requested,
            auto_advance_requested,
            fft_subscriber_count,
            now_playing,
            staged_meta,
            pending_next,
            queue,
            direct_boundary_event,
            load_download_cancel,
            snapshot,
        })
    }

    /// 从播放器读取当前状态快照（HTTP 接口使用，持锁时间极短）
    /// HTTP/WS 快照读取（A4）：优先读事件回调维护的缓存，热路径全程不碰
    /// player 锁；仅冷启动（尚无任何事件）短锁补齐一次
    pub fn snapshot(&self) -> PlayerSnapshot {
        if let Some(cached) = self.snapshot.read().clone() {
            return cached;
        }
        let player = self.player.lock();
        let snap = PlayerSnapshot {
            position: player.position(),
            duration: player.duration(),
            volume: player.volume(),
            speed: player.speed(),
            state: player.state(),
            is_finished: player.is_finished(),
            current_source: player.current_source().map(String::from),
        };
        *self.snapshot.write() = Some(snap.clone());
        snap
    }

    /// 位置注记（seek 提交后调用：暂停态没有 position 事件，缓存需显式刷新）
    pub fn note_position(&self, position: f64) {
        if let Some(snap) = self.snapshot.write().as_mut() {
            snap.position = position;
        }
    }

    /// 音量注记（volume 变更无对应事件，缓存需显式刷新）
    pub fn note_volume(&self, volume: f32) {
        if let Some(snap) = self.snapshot.write().as_mut() {
            snap.volume = volume;
        }
    }

    /// 当前曲目变更注记（load 提交/停止后调用；载入即未完成）
    pub fn note_source_change(&self, source: Option<&str>) {
        if let Some(snap) = self.snapshot.write().as_mut() {
            snap.current_source = source.map(String::from);
            snap.is_finished = false;
        }
    }
}

/// 当前 Unix 毫秒时间戳
fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WS state 消息契约：前端接力采纳（adoptServerAdvancedTrack）依赖
    /// current_source/current_track_id 字段，序列化命名不得漂移
    #[test]
    fn ws_state_serializes_adoption_fields() {
        let ws = WsState {
            position: 1.0,
            duration: 2.0,
            volume: 0.5,
            state: PlayerState::Playing,
            current_source: Some("/music/01.flac".into()),
            current_track_id: Some("local:1a2b".into()),
        };
        let v = serde_json::to_value(&ws).unwrap();
        assert_eq!(v["current_source"], "/music/01.flac");
        assert_eq!(v["current_track_id"], "local:1a2b");
        assert_eq!(v["state"], "playing");
        // 缺省时序列化为 null 而非省略键（前端以 presence 判断是否采纳）
        let ws_empty = WsState {
            current_source: None,
            current_track_id: None,
            ..ws
        };
        let v = serde_json::to_value(&ws_empty).unwrap();
        assert!(v.get("current_source").is_some());
        assert!(v["current_source"].is_null());
    }
}
