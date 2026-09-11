//! WebSocket 状态广播与 FFT 订阅。

//! REST API 控制器
//!
//! 基于 Axum 0.8 的路由定义，提供播放控制、状态查询、扫描和 WebSocket 端点。

use std::time::{Duration, Instant};

use super::spawn_isolated_blocking;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::IntoResponse,
};
use futures_util::sink::SinkExt;
use serde::Deserialize;

use crate::error::ApiError;
use crate::state::AppState;

/// 单次发送超时：对端停止读取（移动端后台冻结/NAT 静默重置导致的 TCP
/// 零窗口）时，无超时的 send 会永久阻塞 select 循环——recv 分支饿死、
/// Close 永远收不到、FFT 订阅无法释放。超时即放弃本连接。
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// 服务端心跳周期：浏览器对 Ping 自动回 Pong，超时无 Pong 判定半开连接
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// 允许的最大 Pong 静默期（略大于两个心跳周期，容忍偶发丢包）
const PONG_DEADLINE: Duration = Duration::from_secs(65);

/// 带超时的单条发送；返回 false 表示连接应终止
async fn send_msg(socket: &mut WebSocket, msg: Message) -> bool {
    matches!(
        tokio::time::timeout(SEND_TIMEOUT, socket.send(msg)).await,
        Ok(Ok(()))
    )
}

/// WebSocket 查询参数
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    token: Option<String>,
}

/// WebSocket 端点：实时推送播放器状态与扫描进度（支持 ?token=xxx 参数校验）
pub(crate) async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(state): State<AppState>,
) -> axum::response::Response {
    if let Some(expected_token) = state.config.api_token.as_ref() {
        if query.token.as_deref() != Some(expected_token.as_str()) {
            return ApiError::unauthorized().into_response();
        }
    }
    ws.on_upgrade(move |socket| ws_run(socket, state))
}

/// WebSocket 连接处理循环
pub(crate) async fn ws_run(mut socket: WebSocket, state: AppState) {
    let mut rx = state.ws_tx.subscribe();
    let mut rx_scan = state.scan_tx.subscribe();
    let mut interval = tokio::time::interval(Duration::from_millis(500));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut heartbeat = tokio::time::interval(PING_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_pong = Instant::now();
    // 本连接是否订阅 FFT 频谱（默认关闭；订阅计数归零时关闭引擎 FFT 定时器）
    let mut fft_subscribed = false;

    // 建连后先发送快照：interval 与 heartbeat 的首次 tick 都是就绪状态，
    // 不能让客户端先收到 Ping 而没有可渲染的播放器状态。
    let snapshot = state.snapshot();
    let payload = serde_json::json!({ "type": "snapshot", "data": snapshot }).to_string();
    if !send_msg(&mut socket, Message::Text(payload.into())).await {
        return;
    }

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // 定时推送当前播放器快照
                let snapshot = state.snapshot();
                let payload = serde_json::json!({ "type": "snapshot", "data": snapshot }).to_string();
                if !send_msg(&mut socket, Message::Text(payload.into())).await {
                    break;
                }
            }
            _ = heartbeat.tick() => {
                // 半开连接检测：一个 PONG_DEADLINE 内无任何 Pong 即判死
                if last_pong.elapsed() > PONG_DEADLINE {
                    tracing::debug!("ws 连接心跳超时，主动断开");
                    break;
                }
                if !send_msg(&mut socket, Message::Ping(vec![].into())).await {
                    break;
                }
            }
            Ok(msg) = rx.recv() => {
                // FFT 事件按本连接订阅状态过滤；其余事件原样转发
                let is_fft = msg.get("type").and_then(|t| t.as_str()) == Some("fftData");
                if is_fft && !fft_subscribed {
                    continue;
                }
                let payload = serde_json::to_string(&msg).unwrap_or_else(|_| "{}".into());
                if !send_msg(&mut socket, Message::Text(payload.into())).await {
                    break;
                }
            }
            Ok(scan_msg) = rx_scan.recv() => {
                // 推送扫描进度与完成事件
                let payload = serde_json::json!({
                    "type": "scanProgress",
                    "data": scan_msg,
                })
                .to_string();
                if !send_msg(&mut socket, Message::Text(payload.into())).await {
                    break;
                }
            }
            res = socket.recv() => {
                match res {
                    Some(Ok(Message::Text(text))) => {
                        // 客户端→服务端控制消息：目前仅支持 FFT 订阅
                        // {type:"subscribe", data:{fft:true|false}}
                        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                            continue;
                        };
                        if value.get("type").and_then(|t| t.as_str()) != Some("subscribe") {
                            continue;
                        }
                        let wants_fft = value
                            .pointer("/data/fft")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if wants_fft && !fft_subscribed {
                            fft_subscribed = true;
                            let first = state
                                .fft_subscriber_count
                                .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
                                == 0;
                            if first {
                                let st = state.clone();
                                let _ = spawn_isolated_blocking("fft-enable", move || {
                                    st.player.lock().set_fft_enabled(true);
                                })
                                .await;
                            }
                        } else if !wants_fft && fft_subscribed {
                            fft_subscribed = false;
                            fft_release(&state).await;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {
                        last_pong = Instant::now();
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }

    // 连接断开：退订 FFT，订阅计数归零时关闭引擎 FFT 定时器
    if fft_subscribed {
        fft_release(&state).await;
    }
    let _ = socket.close().await;
}

/// FFT 订阅计数 -1，归零时关闭播放器 FFT 定时器（避免无消费空转）
pub(crate) async fn fft_release(state: &AppState) {
    // CAS 防负：理论上每连接仅释放自己的订阅，但 send 超时/心跳超时
    // 新增了非常规退出路径，计数器不允许下穿 0
    let mut current = state
        .fft_subscriber_count
        .load(std::sync::atomic::Ordering::Acquire);
    let released = loop {
        if current == 0 {
            break false;
        }
        match state.fft_subscriber_count.compare_exchange_weak(
            current,
            current - 1,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => break true,
            Err(actual) => current = actual,
        }
    };
    if released {
        let st = state.clone();
        let _ = spawn_isolated_blocking("fft-disable", move || {
            st.player.lock().set_fft_enabled(false);
        })
        .await;
    }
}

// -------------------------------------------------------------------
// Diretta Audio-over-IP Handlers
// -------------------------------------------------------------------
