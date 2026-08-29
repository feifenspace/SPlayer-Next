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

        // 回调可能在播放器内部线程触发，不能在这里再次 lock player。
        let callback: EventEmitter = {
            let ws_tx = ws_tx.clone();
            let snapshot = Arc::clone(&snapshot);
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
                        if let Ok(val) = serde_json::to_value(&ws_state) {
                            let _ = ws_tx.send(val);
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
                        if let Ok(val) = serde_json::to_value(&ws_state) {
                            let _ = ws_tx.send(val);
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
                        let _ = ws_tx.send(serde_json::json!({ "type": "ended" }));
                    }
                    PlayerEvent::SourceError => {
                        let ws_state = WsState {
                            position: 0.0,
                            duration: 0.0,
                            volume: current.as_ref().map(|s| s.volume).unwrap_or(1.0),
                            state: PlayerState::Idle,
                        };
                        *snapshot.write() = Some(ws_state);
                        let _ = ws_tx.send(serde_json::json!({ "type": "sourceError" }));
                    }
                    // 输出停滞/失败：InnerPlayer 已通过 failure 回调异步重建输出流，WS 侧无需动作
                    PlayerEvent::OutputStalled | PlayerEvent::OutputFailed => {}
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
