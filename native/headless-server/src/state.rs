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
    /// 事件回调维护的最新状态快照（避免回调中加锁 player 导致死锁）
    snapshot: Arc<RwLock<Option<PlayerSnapshot>>>,
}

impl AppState {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        let db_path = config.resolved_database_path();
        let db_conn = crate::db::init_db(&db_path)?;

        // 输出设备恢复：显式配置的 diretta_target 优先（运营者写死，不被浏览器
        // 上次的选择覆盖）；否则用服务端记忆的上次选择（headless 自恢复，
        // 不依赖浏览器在场）
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
        if let Some(ref target) = config.diretta_target {
            let diretta_dev = format!("diretta:{}", target);
            inner_player.set_output_device(Some(diretta_dev));
        } else if let Some(saved) = saved_output_device {
            let dev = if saved.is_empty() { None } else { Some(saved) };
            info!(device = ?dev, "恢复上次输出设备");
            inner_player.set_output_device(dev);
        }
        let player = Arc::new(Mutex::new(inner_player));

        let (ws_tx, _rx) = broadcast::channel(128);
        let (scan_tx, _rx_scan) = broadcast::channel(128);
        let snapshot: Arc<RwLock<Option<PlayerSnapshot>>> = Arc::new(RwLock::new(None));
        let is_scanning = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let scan_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let output_recovery_requested = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let auto_advance_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fft_subscriber_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pending_next = Arc::new(Mutex::new(None));
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
                        let ws_state = WsState {
                            position: authoritative.position,
                            duration: authoritative.duration,
                            volume: authoritative.volume,
                            state,
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
                        let ws_state = WsState {
                            position,
                            duration,
                            volume: authoritative.volume,
                            state: authoritative.state,
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
                        let _ =
                            ws_tx.send(serde_json::json!({ "type": "sourceError", "data": {} }));
                    }
                    PlayerEvent::DirectTrackBoundary {
                        duration,
                        generation,
                    } => {
                        // 无缝边界：已 stage 的候选元数据转正为 now-playing 快照，
                        // 保证重开页面/无浏览器场景都能显示正确曲目
                        let mut promoted_source = None;
                        if let Some((g, meta)) = staged_meta.lock().take() {
                            if g == generation {
                                promoted_source = meta
                                    .get("source")
                                    .and_then(|v| v.as_str())
                                    .map(String::from);
                                *now_playing.lock() = Some(meta);
                            }
                        }
                        authoritative.position = 0.0;
                        authoritative.duration = duration;
                        authoritative.state = PlayerState::Playing;
                        if promoted_source.is_some() {
                            authoritative.current_source = promoted_source;
                            authoritative.is_finished = false;
                        }
                        *snapshot.write() = Some(authoritative);
                        // 边界即候选消费点：刚切入的曲子就是 pending_next 里注册的
                        // 那首，不清掉的话曲终自动连播会在它播完后重放一遍
                        // （引擎 current_source 不随边界更新，曲终时无法自证重复）。
                        // 浏览器在场时 position tick 一两秒内会重注册新的下一曲
                        *pending_next.lock() = None;
                        let _ = ws_tx.send(serde_json::json!({
                            "type": "directTrackBoundary",
                            "data": { "duration": duration, "generation": generation },
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
