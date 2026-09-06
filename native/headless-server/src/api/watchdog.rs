//! 输出恢复看门狗：输出停滞/失败的自动重载与跳曲。

//! REST API 控制器
//!
//! 基于 Axum 0.8 的路由定义，提供播放控制、状态查询、扫描和 WebSocket 端点。

use std::time::Duration;

use axum::{
    extract::{Query, State},
    Json,
};

use super::player::{
    load_handler, seek_handler, stop_handler, LoadQuery, LoadRequest, SeekRequest,
};
use crate::state::AppState;

/// 两次恢复尝试之间的冷却窗口（OutputStalled 以 1.2s 周期重发，靠此防抖）
pub(crate) const OUTPUT_RECOVERY_COOLDOWN: Duration = Duration::from_secs(5);
/// 同一故障段内允许的最大连续恢复次数，超过后进入暂停态并通知客户端
pub(crate) const OUTPUT_RECOVERY_MAX_CONSECUTIVE: u32 = 3;
/// 恢复放弃后的抑制时长，超时自动解除
pub(crate) const OUTPUT_RECOVERY_SUPPRESS_RESET: Duration = Duration::from_secs(60);
/// 恢复后回退进度小于该值时不 seek
pub(crate) const OUTPUT_RECOVERY_MIN_RESUME_POSITION: f64 = 1.0;
/// 输出恢复跳下一曲候选的最小间隔：防止设备整体故障时把整个队列烧穿
pub(crate) const OUTPUT_RECOVERY_SKIP_COOLDOWN: Duration = Duration::from_secs(120);

/// 启动输出恢复看门狗（服务启动时调用一次）。
///
/// headless 没有 Electron 主进程的 requestReinit 链路：OutputFailed/OutputStalled
/// 在事件回调里只置位请求标志（回调线程禁止锁 player / 触发 async），由本任务
/// 消费标志并对当前源做重载 + seek 回退。停滞时 handoff 会复用被设备端楔死的
/// Direct 连接，因此每次重载前先 stop 全量拆线，让 load 对 Target 全新握手。
/// 同一源的停滞重试（无论单次重载是否成功）计入上限，超限后优先跳下一曲候选
/// （可能是本曲特定内容触发的设备端故障），无候选/限流/跳转失败才进入暂停态、
/// 广播 outputRecoveryFailed 并抑制一段时长；用户换源立即解除。
pub fn spawn_output_recovery_watchdog(state: AppState) {
    tokio::spawn(async move {
        let mut episode_active = false;
        let mut consecutive_failures: u32 = 0;
        let mut last_attempt: Option<std::time::Instant> = None;
        let mut suppressed_since: Option<std::time::Instant> = None;
        let mut recovery_source: Option<String> = None;
        let mut last_recovery_skip: Option<std::time::Instant> = None;
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;

            // B 层自动连播：曲终（Ended 置位）且有候选 → 服务端直接加载播放，
            // 浏览器（遥控器）离场不影响接续。加载即消费候选
            if state
                .auto_advance_requested
                .swap(false, std::sync::atomic::Ordering::AcqRel)
            {
                // 曲终自动连播的重放由边界消费兜底：gapless 边界切换时已把
                // pending_next 清空（state.rs），候选不可能是刚无缝播完的曲子
                let candidate = state.pending_next.lock().take();
                if let Some(next) = candidate {
                    tracing::info!(source = %next.source, "曲终自动连播：加载下一曲候选");
                    let load_result = load_handler(
                        State(state.clone()),
                        Query(LoadQuery {}),
                        Json(LoadRequest {
                            source: next.source.clone(),
                            auto_play: Some(true),
                            meta: None,
                        }),
                    )
                    .await;
                    let failure = match &load_result {
                        Ok(response) if response.success => None,
                        Ok(response) => Some(
                            response
                                .error
                                .as_ref()
                                .map(|e| format!("{}: {}", e.code, e.message))
                                .unwrap_or_else(|| "success=false".to_string()),
                        ),
                        Err(e) => Some(format!("{}: {}", e.code, e.message)),
                    };
                    if let Some(detail) = failure {
                        tracing::warn!(source = %next.source, error = %detail, "自动连播加载失败");
                        let _ = state.ws_tx.send(serde_json::json!({
                            "type": "autoAdvanceFailed",
                            "data": { "source": next.source, "error": detail },
                        }));
                    }
                }
            }

            if state
                .output_recovery_requested
                .swap(0, std::sync::atomic::Ordering::AcqRel)
                != 0
            {
                episode_active = true;
            }

            let current_source = {
                let player = state.player.lock();
                player.current_source().map(String::from)
            };
            // 用户切换/重载曲目（与恢复目标不同）→ 清零计数并解除抑制
            if recovery_source.is_some() && current_source != recovery_source {
                consecutive_failures = 0;
                suppressed_since = None;
                episode_active = false;
                recovery_source = None;
            }

            if let Some(since) = suppressed_since {
                if since.elapsed() < OUTPUT_RECOVERY_SUPPRESS_RESET {
                    continue;
                }
                tracing::info!("输出恢复抑制期满，自动解除");
                suppressed_since = None;
                consecutive_failures = 0;
            }

            if !episode_active {
                continue;
            }
            if last_attempt.is_some_and(|t| t.elapsed() < OUTPUT_RECOVERY_COOLDOWN) {
                continue;
            }

            let (source, position_secs) = {
                let player = state.player.lock();
                if player.state() != audio_engine_core::PlayerState::Playing {
                    // 用户已暂停/停止或曲目自然结束：本段恢复作废，计数清零
                    // （用户介入后重新给予完整重试预算）
                    episode_active = false;
                    consecutive_failures = 0;
                    recovery_source = None;
                    continue;
                }
                match player.current_source().map(String::from) {
                    Some(source) => (source, player.position()),
                    None => {
                        episode_active = false;
                        continue;
                    }
                }
            };

            consecutive_failures += 1;
            last_attempt = Some(std::time::Instant::now());
            recovery_source = Some(source.clone());
            tracing::info!(
                source = %source,
                position_secs,
                attempt = consecutive_failures,
                "输出停滞/失败：拆线重载恢复"
            );

            // 停滞时 handoff 重载只会复用被设备端楔死的 Direct 连接（重载几次都
            // 无效）：先 stop 全量拆线（淡出+排空后异步关闭），load 时全新握手
            let _ = stop_handler(State(state.clone())).await;

            let load_result = load_handler(
                State(state.clone()),
                Query(LoadQuery {}),
                Json(LoadRequest {
                    source: source.clone(),
                    auto_play: Some(true),
                    meta: None,
                }),
            )
            .await;
            let load_ok = matches!(&load_result, Ok(response) if response.success);
            if load_ok {
                if position_secs > OUTPUT_RECOVERY_MIN_RESUME_POSITION {
                    if let Err(error) =
                        seek_handler(State(state.clone()), Json(SeekRequest { position_secs }))
                            .await
                    {
                        tracing::warn!(?error, "输出恢复 seek 回退失败");
                    }
                }
                episode_active = false;
            } else {
                tracing::warn!(
                    source = %source,
                    attempt = consecutive_failures,
                    "输出恢复重载失败"
                );
            }

            // 同一源反复停滞即超限——单次重载"成功"（下载/探测 OK）不代表输出
            // 在走，重载救不了设备端不拉流的情况。优先跳下一曲候选（可能是本曲
            // 特定内容/格式触发的故障），无候选、限流或跳转失败才进入暂停态
            if consecutive_failures >= OUTPUT_RECOVERY_MAX_CONSECUTIVE {
                let skip_allowed = last_recovery_skip
                    .map_or(true, |t| t.elapsed() >= OUTPUT_RECOVERY_SKIP_COOLDOWN);
                let candidate = if skip_allowed {
                    state.pending_next.lock().take()
                } else {
                    None
                };
                let mut advanced = false;
                if let Some(next) = candidate {
                    tracing::info!(from = %source, to = %next.source, "输出恢复重试超限，跳下一曲候选");
                    advanced = matches!(
                        load_handler(
                            State(state.clone()),
                            Query(LoadQuery {}),
                            Json(LoadRequest {
                                source: next.source.clone(),
                                auto_play: Some(true),
                                meta: None,
                            }),
                        )
                        .await,
                        Ok(response) if response.success
                    );
                    if advanced {
                        last_recovery_skip = Some(std::time::Instant::now());
                        let _ = state.ws_tx.send(serde_json::json!({
                            "type": "outputRecoveryAdvanced",
                            "data": { "from": source, "to": next.source },
                        }));
                    } else {
                        tracing::warn!(to = %next.source, "输出恢复跳下一曲失败");
                    }
                }
                if !advanced {
                    state.player.lock().enter_paused_for_recovery();
                    let _ = state.ws_tx.send(serde_json::json!({
                        "type": "outputRecoveryFailed",
                        "data": { "source": source },
                    }));
                    suppressed_since = Some(std::time::Instant::now());
                    tracing::warn!("输出恢复连续失败，进入暂停态等待客户端介入");
                }
                episode_active = false;
                consecutive_failures = 0;
                recovery_source = None;
            }
        }
    });
}
