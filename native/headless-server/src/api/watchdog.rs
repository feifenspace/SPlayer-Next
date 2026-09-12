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
    load_handler, seek_handler, stop_handler, LoadMeta, LoadQuery, LoadRequest, SeekRequest,
};
use crate::state::{AppState, PendingNext, QueueRepeat, QueueSnapshot};

/// 候选接力 load 的 meta：仅携带时长提示（秒 → 毫秒）。stream 模式在线源
/// 无法廉价探测时长，接力加载丢失提示会让 duration 归零（进度条/曲终判定退化）
fn duration_hint_meta(duration_hint: Option<f64>) -> Option<LoadMeta> {
    Some(LoadMeta {
        id: None,
        title: None,
        artist: None,
        album: None,
        duration: duration_hint.map(|secs| (secs * 1000.0).round() as u64),
        track: None,
        cue_path: None,
        cue_audio_path: None,
        cue_start_ms: None,
        cue_end_ms: None,
    })
}

/// 曲终接力加载失败的退避参数：同一候选 source 连续失败时按指数退避重试
/// （5s 起，倍增至上限），超过次数上限放弃重试——等队列/候选重新注册
/// （直链重解析）后按新 source 立即恢复。修复点：此前单次失败即永久停播
pub(crate) const AUTO_ADVANCE_RETRY_BASE: Duration = Duration::from_secs(5);
pub(crate) const AUTO_ADVANCE_RETRY_MAX: Duration = Duration::from_secs(60);
pub(crate) const AUTO_ADVANCE_MAX_ATTEMPTS: u32 = 5;

/// 曲终候选退避状态：（失败 source, 已失败次数, 下次重试时刻）
struct AdvanceBackoff {
    source: String,
    attempts: u32,
    next_at: std::time::Instant,
}

impl AdvanceBackoff {
    /// 候选 source 仍为失败源时的门控：未到重试时刻返回 false（保持置位等待）
    fn gate_open(&self, source: &str) -> bool {
        self.source != source || std::time::Instant::now() >= self.next_at
    }
}

/// 曲终自动连播候选推导：队列快照注册时以队列权威（按当前曲 source 对齐后取
/// 下一曲）——手动切歌后旧接力候选不会复活；未注册队列时回退旧单槽候选
/// （前端驱动旧链路）。返回 None 即无候选（曲终停止）。
/// 队列候选 source 不可加载（在线直链未解析落定的空串占位）时同样返回 None：
/// 归入"无候选等待"语义（保持置位等快照更新），而不是加载失败走退避烧完
/// 重试预算——快照落定后 queue_snapshot_handler 会置位重试，自动接续
fn auto_advance_candidate(
    queue: Option<QueueSnapshot>,
    current_source: Option<&str>,
    legacy: Option<PendingNext>,
) -> Option<PendingNext> {
    match queue {
        Some(mut snapshot) => {
            // 队列权威：注册后旧接力候选一律不参与（含队尾/repeat=one 的无候选）
            snapshot.align_by_source(current_source);
            // repeat=one：以既有接力重播当前曲（不做无缝预载）。此前返回 None
            // 会在浏览器离场时曲终停播（服务端自治缺口）——现在遥控器不在场
            // 也能正确单曲循环；浏览器在场时其 seek(0)+play 兜底先到先得，
            // 看门狗发现状态已离开曲终即自动放弃，两者不冲突
            if snapshot.repeat == QueueRepeat::One {
                let item = snapshot.current()?;
                if !super::player::is_loadable_candidate_source(&item.source) {
                    return None;
                }
                return Some(PendingNext {
                    source: item.source.clone(),
                    duration_hint: item.duration_ms.map(|ms| ms as f64 / 1000.0),
                });
            }
            // 在线链接异步解析时，队列可能短暂含有空 source 占位。跳过这些
            // 无法加载的条目，寻找当前播放顺序内后续的有效曲目，避免整队停止。
            let mut remaining = snapshot.items.len().saturating_sub(1);
            let mut next = snapshot.next();
            loop {
                let Some((next_pos, item)) = next else {
                    return None;
                };
                if super::player::is_loadable_candidate_source(&item.source) {
                    return Some(PendingNext {
                        source: item.source.clone(),
                        duration_hint: item.duration_ms.map(|ms| ms as f64 / 1000.0),
                    });
                }
                remaining = remaining.saturating_sub(1);
                if remaining == 0 {
                    return None;
                }
                snapshot.pos = next_pos;
                next = snapshot.next();
            }
        }
        // 未注册队列：回退前端驱动的旧单槽候选
        None => legacy,
    }
}

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

// -------------------------------------------------------------------
// 服务端自治播放统计（浏览器离场也持续记录 play_history）
// -------------------------------------------------------------------

fn persist_server_play_session(state: &AppState, session: crate::state::ServerPlaySession) {
    if session.listened_ms < 5000 {
        return;
    }
    let conn = state.db.lock();
    if let Err(error) = crate::db::upsert_server_play_history(
        &conn,
        &session.track_id,
        &session.source,
        session.started_at,
        session.listened_ms,
        &session.track_json,
    ) {
        tracing::warn!(error = %error, "服务端自治统计写入失败");
    }
}

/// 结算当前会话：供 stop/load 等非音频回调路径调用。
pub(crate) fn finalize_server_play_session(state: &AppState) {
    let Some(mut session) = state.server_play_session.lock().take() else {
        return;
    };
    if let Some(since) = session.playing_since.take() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        session.listened_ms += now_ms.saturating_sub(since);
    }
    persist_server_play_session(state, session);
}

/// 写入音频事件回调移交的已完成会话。
fn flush_completed_server_play_sessions(state: &AppState) {
    let completed = std::mem::take(&mut *state.server_play_completed.lock());
    for session in completed {
        persist_server_play_session(state, session);
    }
}

/// 开启新的自治统计会话（load 成功 / 曲终接力加载成功时调用；切曲先结算上一曲）
pub(crate) fn begin_server_play_session(
    state: &AppState,
    track_id: &str,
    source: &str,
    track_json: &str,
) {
    finalize_server_play_session(state);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    *state.server_play_session.lock() = Some(crate::state::ServerPlaySession {
        track_id: track_id.to_string(),
        source: source.to_string(),
        track_json: track_json.to_string(),
        started_at: now_ms,
        playing_since: Some(now_ms),
        listened_ms: 0,
    });
}

/// 启动输出恢复看门狗（服务启动时调用一次）。
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
        // 曲终无候选广播去抖：同一曲终片段只广播一次，避免 WS 刷屏
        let mut no_candidate_announced = false;
        // 曲终接力候选加载失败的退避状态（见 AdvanceBackoff）
        let mut advance_backoff: Option<AdvanceBackoff> = None;
        // 跨格式、PCM↔DSD 与 stream 在线源走曲终完整重连；缩短其调度等待。
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;

            // 服务端自治统计：曲终/音源失败结算请求消费（写库）
            if state
                .server_play_finalize_requested
                .swap(false, std::sync::atomic::Ordering::AcqRel)
            {
                flush_completed_server_play_sessions(&state);
            }

            // Direct boundary 自治消费：无缝边界发生后的引擎簿记提交、队列推进
            // 与再下一曲预载调度（移植自 atom playbackSession 的 boundary 消费，
            // 不依赖浏览器；事件回调线程禁止锁 player，故经标志位转交本线程）
            let boundary_generation = state.direct_boundary_event.lock().take();
            if let Some(generation) = boundary_generation {
                super::direct_preloader::consume_boundary(&state, generation);
            }

            // B 层自动连播：曲终（Ended 置位）且有候选 → 服务端直接加载播放，
            // 浏览器（遥控器）离场不影响接续。加载即消费候选
            if state
                .auto_advance_requested
                .swap(false, std::sync::atomic::Ordering::AcqRel)
            {
                // 曲终自动连播的重放由边界消费兜底：gapless 边界切换时已把
                // pending_next 清空（state.rs），候选不可能是刚无缝播完的曲子
                let still_ended = {
                    let snap = state.snapshot();
                    matches!(snap.state, audio_engine_core::PlayerState::Stopped)
                        && snap.is_finished
                };
                let candidate = if still_ended {
                    // 队列注册时以队列权威推导（手动切歌后旧接力候选不复活）；
                    // 未注册队列回退旧单槽候选
                    let queue_snapshot = state.queue.lock().clone();
                    let legacy = state.pending_next.lock().take();
                    let current_source = state.player.lock().current_source().map(String::from);
                    auto_advance_candidate(queue_snapshot, current_source.as_deref(), legacy)
                } else {
                    // 用户已在曲终后手动接管（换曲/重播/暂停）：放弃自动接续
                    advance_backoff = None;
                    None
                };
                if let Some(next) = candidate {
                    // 失败退避门控：同一失败候选未到重试时刻则保持置位等下一 tick；
                    // 换了候选（直链重解析后的新 URL）立即重试
                    if !advance_backoff
                        .as_ref()
                        .is_none_or(|backoff| backoff.gate_open(&next.source))
                    {
                        state
                            .auto_advance_requested
                            .store(true, std::sync::atomic::Ordering::Release);
                        continue;
                    }
                    let attempts = match &advance_backoff {
                        Some(backoff) if backoff.source == next.source => backoff.attempts,
                        _ => 0,
                    };
                    if attempts >= AUTO_ADVANCE_MAX_ATTEMPTS {
                        // 同一候选反复失败：放弃重试（不置位），等队列/候选
                        // 重新注册（source 变化即重置 attempts）后自动恢复
                        if !no_candidate_announced {
                            no_candidate_announced = true;
                            tracing::warn!(
                                source = %next.source,
                                attempts,
                                "曲终接力候选连续加载失败，放弃重试；候选更新后将自动接续"
                            );
                            let _ = state.ws_tx.send(serde_json::json!({
                                "type": "autoAdvanceFailed",
                                "data": {
                                    "source": next.source,
                                    "attempts": attempts,
                                    "final": true,
                                },
                            }));
                        }
                        continue;
                    }
                    no_candidate_announced = false;
                    tracing::info!(source = %next.source, "曲终自动连播：加载下一曲候选");
                    let load_result = load_handler(
                        State(state.clone()),
                        Query(LoadQuery {}),
                        Json(LoadRequest {
                            source: next.source.clone(),
                            auto_play: Some(true),
                            // 时长提示进 meta：stream 模式的 duration 仅来自
                            // 前端 meta，接力加载丢弃它会导致时长归零
                            meta: duration_hint_meta(next.duration_hint),
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
                    // 自治统计：接力加载成功的会话开启由 load 路径的
                    // update_now_playing → begin_server_play_session 统一完成
                    if let Some(detail) = failure {
                        let new_attempts = attempts + 1;
                        // 指数退避：5s 起倍增至上限；置位等冷却后自动重试
                        let delay = AUTO_ADVANCE_RETRY_BASE
                            .checked_mul(1 << (new_attempts - 1).min(4))
                            .unwrap_or(AUTO_ADVANCE_RETRY_MAX)
                            .min(AUTO_ADVANCE_RETRY_MAX);
                        tracing::warn!(
                            source = %next.source,
                            error = %detail,
                            attempt = new_attempts,
                            retry_in = ?delay,
                            "自动连播加载失败，退避后重试"
                        );
                        advance_backoff = Some(AdvanceBackoff {
                            source: next.source.clone(),
                            attempts: new_attempts,
                            next_at: std::time::Instant::now() + delay,
                        });
                        state
                            .auto_advance_requested
                            .store(true, std::sync::atomic::Ordering::Release);
                        let _ = state.ws_tx.send(serde_json::json!({
                            "type": "autoAdvanceFailed",
                            "data": {
                                "source": next.source,
                                "error": detail,
                                "attempts": new_attempts,
                            },
                        }));
                    } else {
                        advance_backoff = None;
                    }
                } else if still_ended {
                    // 无候选兜底（停播根因①）：重新置位等待客户端补注册候选，
                    // 注册后下一 tick 自动接续；同时广播一次让在线客户端可感知。
                    // 此前此处为静默跳过——浏览器离场时曲终即永久停播
                    state
                        .auto_advance_requested
                        .store(true, std::sync::atomic::Ordering::Release);
                    if !no_candidate_announced {
                        no_candidate_announced = true;
                        tracing::warn!(
                            "曲终自动连播：无注册候选（遥控器离场或未重注册），保持停止态；候选注册后将自动接续"
                        );
                        let _ = state.ws_tx.send(serde_json::json!({
                            "type": "autoAdvanceNoCandidate",
                            "data": {},
                        }));
                    }
                }
            } else {
                no_candidate_announced = false;
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
                    // 与曲终自动连播同源：队列注册时按队列权威推导（当前曲的下一曲）
                    let queue_snapshot = state.queue.lock().clone();
                    let legacy = state.pending_next.lock().take();
                    auto_advance_candidate(queue_snapshot, Some(&source), legacy)
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
                                meta: duration_hint_meta(next.duration_hint),
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

#[cfg(test)]
mod tests {
    use super::auto_advance_candidate;
    use crate::state::{PendingNext, QueueItem, QueueRepeat, QueueSnapshot};

    fn queue_of(sources: &[&str]) -> QueueSnapshot {
        QueueSnapshot::new(
            sources
                .iter()
                .map(|s| QueueItem {
                    source: s.to_string(),
                    duration_ms: None,
                    title: None,
                    artist: None,
                    album: None,
                    cover: None,
                    track: None,
                })
                .collect(),
            0,
            QueueRepeat::Off,
            false,
        )
    }

    fn legacy(source: &str) -> Option<PendingNext> {
        Some(PendingNext {
            source: source.to_string(),
            duration_hint: None,
        })
    }

    #[test]
    fn queue_authoritative_next_ignores_stale_legacy_candidate() {
        // 手动切歌场景：旧接力候选（/stale）不得复活，队列权威给出当前曲的下一曲
        let got = auto_advance_candidate(
            Some(queue_of(&["/a", "/b", "/c"])),
            Some("/b"),
            legacy("/stale"),
        );
        assert_eq!(got.unwrap().source, "/c");
    }

    #[test]
    fn queue_end_with_repeat_off_yields_none_even_with_legacy() {
        let got =
            auto_advance_candidate(Some(queue_of(&["/a", "/b"])), Some("/b"), legacy("/stale"));
        assert!(got.is_none());
    }

    #[test]
    fn repeat_one_replays_current_track_for_server_autonomy() {
        // repeat=one：接力候选 = 当前曲自身（浏览器离场时服务端自治重播；
        // 在场时前端 seek(0)+play 先到先得，看门狗检测到状态离开曲终即放弃）
        let mut q = queue_of(&["/a", "/b"]);
        q.repeat = QueueRepeat::One;
        let got = auto_advance_candidate(Some(q), Some("/a"), None);
        assert_eq!(got.unwrap().source, "/a");
    }

    #[test]
    fn aligns_by_current_source_after_manual_switch() {
        // 切歌后按 source 对齐：当前曲是 /c 时（repeat=all）下一曲回卷到 /a
        let mut q = queue_of(&["/a", "/b", "/c"]);
        q.repeat = QueueRepeat::All;
        let got = auto_advance_candidate(Some(q), Some("/c"), None);
        assert_eq!(got.unwrap().source, "/a");
    }

    #[test]
    fn no_queue_falls_back_to_legacy_candidate() {
        let got = auto_advance_candidate(None, Some("/x"), legacy("/legacy-next"));
        assert_eq!(got.unwrap().source, "/legacy-next");
        assert!(auto_advance_candidate(None, None, None).is_none());
    }

    #[test]
    fn skips_unresolved_queue_entries_when_advancing() {
        let got = auto_advance_candidate(
            Some(queue_of(&["/a", "", "https://example.test/next.flac"])),
            Some("/a"),
            None,
        );
        assert_eq!(got.unwrap().source, "https://example.test/next.flac");
    }
}
