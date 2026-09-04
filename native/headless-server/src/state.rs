//! 应用共享状态
//!
//! AppState 持有播放器、配置和 WebSocket 广播频道。
//! EventEmitter 回调通过快照缓存避免直接持有 player 锁，防止死锁。

use std::sync::Arc;

use audio_engine_core::{EventEmitter, InnerPlayer, PlayerEvent, PlayerState};
use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;

use crate::config::Config;

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
    /// 下一曲候选（B 层自动连播单槽；None = 未注册）
    pub pending_next: Arc<Mutex<Option<PendingNext>>>,
    /// 事件回调维护的最新状态快照（避免回调中加锁 player 导致死锁）
    snapshot: Arc<RwLock<Option<WsState>>>,
}

impl AppState {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        let db_path = config.resolved_database_path();
        let db_conn = crate::db::init_db(&db_path)?;
        let db = Arc::new(Mutex::new(db_conn));

        let mut inner_player = InnerPlayer::new()?;
        let cover_dir = config.resolved_cover_cache_dir();
        if let Some(cover_str) = cover_dir.to_str() {
            inner_player.set_cover_cache_dir(cover_str.to_string());
        }
        if let Some(ref target) = config.diretta_target {
            let diretta_dev = format!("diretta:{}", target);
            inner_player.set_output_device(Some(diretta_dev));
        }
        let player = Arc::new(Mutex::new(inner_player));

        let (ws_tx, _rx) = broadcast::channel(128);
        let (scan_tx, _rx_scan) = broadcast::channel(128);
        let snapshot: Arc<RwLock<Option<WsState>>> = Arc::new(RwLock::new(None));
        let is_scanning = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let scan_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let output_recovery_requested = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let auto_advance_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pending_next = Arc::new(Mutex::new(None));

        // 回调可能在播放器内部线程触发，不能在这里再次 lock player。
        let callback: EventEmitter = {
            let ws_tx = ws_tx.clone();
            let snapshot = Arc::clone(&snapshot);
            let output_recovery_requested = Arc::clone(&output_recovery_requested);
            let auto_advance_requested = Arc::clone(&auto_advance_requested);
            Arc::new(move |event: PlayerEvent| {
                // 先 clone 一份当前快照，避免持有读锁跨越后续写锁操作
                let current: Option<WsState> = snapshot.read().clone();
                match event {
                    PlayerEvent::StateChanged { state } => {
                        let ws_state = WsState {
                            position: current.as_ref().map(|s| s.position).unwrap_or(0.0),
                            duration: current.as_ref().map(|s| s.duration).unwrap_or(0.0),
                            volume: current.as_ref().map(|s| s.volume).unwrap_or(1.0),
                            state,
                        };
                        *snapshot.write() = Some(ws_state.clone());
                        if let Ok(data) = serde_json::to_value(&ws_state) {
                            let _ = ws_tx.send(serde_json::json!({ "type": "state", "data": data }));
                        }
                    }
                    PlayerEvent::Position { position, duration } => {
                        let ws_state = WsState {
                            position,
                            duration,
                            volume: current.as_ref().map(|s| s.volume).unwrap_or(1.0),
                            state: current
                                .as_ref()
                                .map(|s| s.state)
                                .unwrap_or(PlayerState::Idle),
                        };
                        *snapshot.write() = Some(ws_state.clone());
                        if let Ok(data) = serde_json::to_value(&ws_state) {
                            let _ = ws_tx.send(serde_json::json!({ "type": "state", "data": data }));
                        }
                    }
                    PlayerEvent::Ended => {
                        let ws_state = WsState {
                            position: current.as_ref().map(|s| s.duration).unwrap_or(0.0),
                            duration: current.as_ref().map(|s| s.duration).unwrap_or(0.0),
                            volume: current.as_ref().map(|s| s.volume).unwrap_or(1.0),
                            state: PlayerState::Stopped,
                        };
                        *snapshot.write() = Some(ws_state);
                        // 置自动连播标志：有注册候选时看门狗会在曲终自动接续
                        auto_advance_requested.store(true, std::sync::atomic::Ordering::Release);
                        let _ = ws_tx.send(serde_json::json!({ "type": "ended", "data": {} }));
                    }
                    PlayerEvent::SourceError => {
                        let ws_state = WsState {
                            position: 0.0,
                            duration: 0.0,
                            volume: current.as_ref().map(|s| s.volume).unwrap_or(1.0),
                            state: PlayerState::Idle,
                        };
                        *snapshot.write() = Some(ws_state);
                        let _ = ws_tx.send(serde_json::json!({ "type": "sourceError", "data": {} }));
                    }
                    PlayerEvent::DirectTrackBoundary { duration, generation } => {
                        let ws_state = WsState {
                            position: 0.0,
                            duration,
                            volume: current.as_ref().map(|s| s.volume).unwrap_or(1.0),
                            state: PlayerState::Playing,
                        };
                        *snapshot.write() = Some(ws_state);
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
            pending_next,
            snapshot,
        })
    }

    /// 从播放器读取当前状态快照（HTTP 接口使用，持锁时间极短）
    pub fn snapshot(&self) -> PlayerSnapshot {
        let player = self.player.lock();
        let state = WsState {
            position: player.position(),
            duration: player.duration(),
            volume: player.volume(),
            state: player.state(),
        };
        *self.snapshot.write() = Some(state.clone());
        PlayerSnapshot {
            position: state.position,
            duration: state.duration,
            volume: state.volume,
            speed: player.speed(),
            state: state.state,
            is_finished: player.is_finished(),
            current_source: player.current_source().map(String::from),
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
