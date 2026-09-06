//! WebSocket 状态广播与 FFT 订阅。

//! REST API 控制器
//!
//! 基于 Axum 0.8 的路由定义，提供播放控制、状态查询、扫描和 WebSocket 端点。

use std::time::Duration;

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
    // 本连接是否订阅 FFT 频谱（默认关闭；订阅计数归零时关闭引擎 FFT 定时器）
    let mut fft_subscribed = false;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // 定时推送当前播放器快照
                let snapshot = state.snapshot();
                let payload = serde_json::json!({ "type": "snapshot", "data": snapshot }).to_string();
                if socket.send(Message::Text(payload.into())).await.is_err() {
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
                if socket.send(Message::Text(payload.into())).await.is_err() {
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
                if socket.send(Message::Text(payload.into())).await.is_err() {
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
    let last = state
        .fft_subscriber_count
        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
        == 1;
    if last {
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
