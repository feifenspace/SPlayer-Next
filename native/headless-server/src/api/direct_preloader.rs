//! Direct 无缝预载编排器（移植自 splayer-atom electron/main/services/playbackPreloader.ts）
//!
//! 服务端自治的 gapless 编排：队列感知的"下一曲选取 → HTTP 物化 → stage →
//! boundary 消费 → 再预载"闭环，不依赖浏览器在线。浏览器离场时无缝播放与
//! 曲终接力照常工作；stage 被拒（跨 wire 格式）时自动登记接力候选，
//! 由输出恢复看门狗的 auto_advance 分支在曲终加载。
//!
//! 与旧前端驱动链路的兼容：本模块未注册队列（PUT /api/v1/player/queue）时
//! 不做任何事；旧 REST 端点（queue/next-candidate、direct/stage_next、
//! direct/commit_boundary）保持可用，两条链路以 generation 空间隔离
//! （本模块 generation 从 1_000_000 起步）。

use std::sync::atomic::{AtomicU64, Ordering};

use axum::{extract::State, Json};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, info, warn};

use super::direct::{stage_direct_core, DirectStageInput};
use super::player::is_loadable_candidate_source;
use super::PlayerResponse;
use crate::state::{AppState, PendingNext, QueueItem, QueueRepeat, QueueSnapshot};

/// 预载 generation 单调计数：从大基数起步，与旧版前端的小整数 generation
/// 空间隔离，避免 staged_meta 转正匹配串台
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1_000_000);
/// 预载失效令牌：任何新调度/失效操作使在途预载结果作废
static PRELOAD_TOKEN: AtomicU64 = AtomicU64::new(0);

/// 已成功 stage 的下一曲（单槽：同一时刻只有一个 stage 在途/就绪）
#[derive(Debug, Clone)]
struct StagedNext {
    generation: u64,
    /// 实际 stage 并将出现在 boundary commit 里的 source（原始串）
    source: String,
    duration_secs: f64,
}

static STAGED_NEXT: Mutex<Option<StagedNext>> = Mutex::new(None);

/// 最近一次已完成簿记提交的边界代际。边界事件可能重复投递（引擎/回调层
/// 竞态），小于等于该值的再次投递属于已提交代际的重复边界，直接忽略——
/// 此前每次重复都会打"generation 不匹配"WARN，实测功能无损、纯日志噪音
static LAST_COMMITTED_GENERATION: AtomicU64 = AtomicU64::new(0);

/// 使在途/已就绪的预载作废：load 提交、stop、新一轮调度前调用
pub(crate) fn invalidate() {
    PRELOAD_TOKEN.fetch_add(1, Ordering::AcqRel);
    *STAGED_NEXT.lock() = None;
}

/// 下一曲无缝预载调度（fire-and-forget，非阻塞）：
/// 队列对齐 → 下一曲选取 → 专用线程上解析/物化/stage。
/// 在 Direct load 提交成功与 boundary 自治提交后调用
pub(crate) fn schedule_next_preload(state: &AppState) {
    invalidate();

    // 仅 Diretta 选择器需要 staging；其他输出走既有链路
    let is_direct = {
        let player = state.player.lock();
        player
            .selected_device()
            .is_some_and(|dev| audio_engine_core::diretta::selector_target(&dev).is_some())
    };
    if !is_direct {
        return;
    }

    // 队列对齐 + 下一曲选取（短锁内克隆，锁外使用）
    let current_source = state.player.lock().current_source().map(String::from);
    let item = {
        let Some(snapshot) = state.queue.lock().clone() else {
            return; // 未注册队列：保持前端驱动旧链路
        };
        let mut snapshot = snapshot;
        snapshot.align_by_source(current_source.as_deref());
        let Some((_next_pos, item)) = snapshot.next() else {
            info!("无缝预载：队列无下一曲（repeat/队尾），跳过");
            return;
        };
        item.clone()
    };
    if !is_loadable_candidate_source(&item.source) {
        warn!(source = %item.source, "无缝预载：下一曲 source 不可加载，跳过");
        return;
    }

    let token = PRELOAD_TOKEN.fetch_add(1, Ordering::AcqRel) + 1;
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::AcqRel);
    let state_for_worker = state.clone();
    let spawn_result = std::thread::Builder::new()
        .name("direct-preload".into())
        .spawn(move || stage_next_worker(state_for_worker, token, generation, item));
    if let Err(error) = spawn_result {
        warn!(error = %error, "无缝预载线程启动失败");
    }
}

/// boundary 自治消费（看门狗线程调用）：无缝边界已发生，
/// 提交引擎簿记 + 队列推进 + 立即为再下一曲调度预载
pub(crate) fn consume_boundary(state: &AppState, generation: u64) {
    // 重复边界闸门：簿记按代际单调提交，已提交代际的重复投递（引擎事件
    // 竞态导致）直接忽略，降级 debug
    let committed = LAST_COMMITTED_GENERATION.load(Ordering::Acquire);
    if generation != 0 && generation <= committed {
        debug!(generation, committed, "忽略已提交代际的重复无缝边界");
        return;
    }
    let staged = STAGED_NEXT.lock().take();
    let Some(staged) = staged else {
        // 孤儿 boundary 兜底：stage 已被引擎接受，worker 登记前被 invalidate
        // 清场（快照重推/手动切歌的竞态窗口）。staged_meta 与边界同代即同曲，
        // 用它完成簿记——否则队列游标与引擎播放脱节，后续接力选曲错位（跳曲）
        let fallback = {
            let mut guard = state.staged_meta.lock();
            match guard.as_ref() {
                Some((g, meta)) if *g == generation => {
                    let source = meta
                        .get("source")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let duration = meta
                        .get("duration_secs")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    // 同代元数据已消费，取走防止迟到复用
                    guard.take();
                    source.map(|source| (source, duration))
                }
                _ => None,
            }
        };
        let Some((source, duration_secs)) = fallback else {
            debug!(generation, "无缝边界无对应 stage 记录（非预载切换），忽略");
            return;
        };
        warn!(source = %source, generation, "孤儿无缝边界：以 staged_meta 兜底簿记");
        commit_boundary_bookkeeping(state, &source, duration_secs, generation);
        return;
    };
    if staged.generation != generation {
        if staged.generation > generation {
            // 迟到的旧边界：新 stage 已取代旧 stage（预载被重新调度），被取代
            // 舞台的边界无需簿记；队列游标由下一个真实边界的 align_by_source
            // 自愈，功能无损 → 降级 debug（此前一律 WARN，属日志噪音）
            debug!(
                expected = staged.generation,
                actual = generation,
                "迟到的无缝边界属于已被取代的预载，忽略"
            );
            return;
        }
        // generation 超前于已登记 stage：真实异常（不应出现），保留 WARN
        warn!(
            expected = staged.generation,
            actual = generation,
            "无缝边界 generation 超前于已登记 stage，簿记跳过"
        );
        return;
    }
    commit_boundary_bookkeeping(state, &staged.source, staged.duration_secs, generation);
}

/// 无缝边界的簿记提交：引擎 current_source/时长同步 + 队列游标推进 + 再预载调度
fn commit_boundary_bookkeeping(
    state: &AppState,
    source: &str,
    duration_secs: f64,
    generation: u64,
) {
    // 引擎簿记提交（快速锁操作；连接已被边界切换，仅同步 current_source/时长/坐标）
    {
        let mut player = state.player.lock();
        if let Err(error) = player.commit_direct_gapless_boundary(source, duration_secs) {
            warn!(error = %error, "无缝边界簿记提交失败（runtime 可能已被接管）");
            return;
        }
    }
    LAST_COMMITTED_GENERATION.store(generation, Ordering::Release);

    // 队列游标推进：按 staged source 对齐（队列被前端重排后自愈）
    {
        let mut guard = state.queue.lock();
        if let Some(snapshot) = guard.as_mut() {
            snapshot.align_by_source(Some(source));
        }
    }

    info!(source = %source, generation, "无缝边界已自治提交，调度再下一曲");
    schedule_next_preload(state);
}

/// stage 结果处理：成功登记单槽；被拒/跳过则登记曲终接力候选
/// （跨 wire 格式的下一曲由输出恢复看门狗在曲终全量重连加载，浏览器无关）
fn stage_next_worker(state: AppState, token: u64, generation: u64, item: QueueItem) {
    if token != PRELOAD_TOKEN.load(Ordering::Acquire) {
        return;
    }
    let duration_secs = item.duration_ms.map_or(0.0, |ms| ms as f64 / 1000.0);
    let track_id = item
        .track
        .as_ref()
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let meta = json!({
        "source": item.source.clone(),
        "title": item.title.clone(),
        "artist": item.artist.clone(),
        "album": item.album.clone(),
        "cover": item.cover.clone(),
        "duration_secs": duration_secs,
        // 前端曲目 id：boundary 转正/WS 事件据此带回，前端按 id 采纳新曲
        "track_id": track_id,
    });

    let outcome = stage_direct_core(
        &state,
        DirectStageInput {
            source: item.source.clone(),
            duration_secs,
            generation,
            meta: Some(meta),
        },
        // 预载失效令牌：被新一轮调度/手动切歌取代时，在途物化下载即时中止
        || token != PRELOAD_TOKEN.load(Ordering::Acquire),
    );

    if token != PRELOAD_TOKEN.load(Ordering::Acquire) {
        return; // 预载已被取代：结果作废
    }

    match outcome {
        Ok(None) => {
            *STAGED_NEXT.lock() = Some(StagedNext {
                generation,
                source: item.source.clone(),
                duration_secs,
            });
            info!(source = %item.source, generation, "无缝预载就绪");
            let _ = state.ws_tx.send(json!({
                "type": "directPreloadReady",
                "data": { "source": item.source, "generation": generation },
            }));
        }
        Ok(Some(reason)) => {
            info!(source = %item.source, reason, "无缝预载跳过，登记曲终接力候选");
            register_relay_candidate(&state, &item);
        }
        Err(error) => {
            warn!(source = %item.source, error = %error, "无缝预载被拒，登记曲终接力候选");
            register_relay_candidate(&state, &item);
        }
    }
}

/// 登记曲终接力候选（与前端 queue/next-candidate 同一单槽，后写覆盖）
fn register_relay_candidate(state: &AppState, item: &QueueItem) {
    if !is_loadable_candidate_source(&item.source) {
        return;
    }
    *state.pending_next.lock() = Some(PendingNext {
        source: item.source.clone(),
        duration_hint: item.duration_ms.map(|ms| ms as f64 / 1000.0),
    });
    let _ = state.ws_tx.send(json!({
        "type": "nextCandidateChanged",
        "data": { "source": item.source },
    }));
}

// ============================================================================
// 队列快照 API
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct QueueItemPayload {
    pub source: String,
    pub duration_ms: Option<u64>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub cover: Option<String>,
    /// 前端完整曲目快照（透传给 GET，供前端恢复平台身份；旧版前端不推）
    #[serde(default)]
    pub track: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct QueueSnapshotPayload {
    pub items: Vec<QueueItemPayload>,
    /// 当前播放条目下标（items 下标）
    pub index: Option<usize>,
    pub repeat: Option<String>,
    pub shuffle: Option<bool>,
}

/// 注册/更新服务端播放队列快照（整表推送）。无缝预载与 boundary 自治推进
/// 以此为队列权威；未注册时保持前端驱动旧链路
pub(crate) async fn queue_snapshot_handler(
    State(state): State<AppState>,
    Json(payload): Json<QueueSnapshotPayload>,
) -> Json<PlayerResponse> {
    let items = payload
        .items
        .into_iter()
        .map(|it| QueueItem {
            source: it.source,
            duration_ms: it.duration_ms,
            title: it.title,
            artist: it.artist,
            album: it.album,
            cover: it.cover,
            track: it.track,
        })
        .collect::<Vec<_>>();
    let repeat = QueueRepeat::parse(payload.repeat.as_deref());
    let snapshot = QueueSnapshot::new(
        items,
        payload.index.unwrap_or(0),
        repeat,
        payload.shuffle.unwrap_or(false),
    );
    let count = snapshot.items.len();
    *state.queue.lock() = Some(snapshot);
    // 快照更新即重新调度无缝预载：前端在下一曲直链解析落定后会重推快照，
    // 此处以最新 source 重 stage（invalidate + 幂等重调度）
    schedule_next_preload(&state);
    // 曲终等待态的自愈置位：此前接力因候选不可加载/反复失败而放弃时，
    // auto_advance_requested 已消费不再置位——浏览器回来重推快照（直链
    // 已重新解析）后若无此置位，曲终接力永远不会自动恢复
    {
        let snap = state.snapshot();
        if matches!(snap.state, audio_engine_core::PlayerState::Stopped) && snap.is_finished {
            state
                .auto_advance_requested
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
    Json(PlayerResponse::ok(json!({
        "registered": true,
        "items": count,
    })))
}

/// 读取服务端播放队列快照
pub(crate) async fn get_queue_handler(State(state): State<AppState>) -> Json<PlayerResponse> {
    let guard = state.queue.lock();
    match guard.as_ref() {
        Some(snapshot) => {
            let current_index = snapshot.order.get(snapshot.pos).copied().unwrap_or(0);
            Json(PlayerResponse::ok(json!({
                "registered": true,
                "items": snapshot.items,
                "index": current_index,
                "pos": snapshot.pos,
                "repeat": snapshot.repeat,
                "total": snapshot.items.len(),
            })))
        }
        None => Json(PlayerResponse::ok(json!({
            "registered": false,
            "items": [],
        }))),
    }
}

/// 清除服务端队列快照：回到前端驱动旧链路。
/// 同时作废无缝预载与旧接力候选（队列权威移除后旧候选不得复活）
pub(crate) async fn queue_clear_handler(State(state): State<AppState>) -> Json<PlayerResponse> {
    *state.queue.lock() = None;
    invalidate();
    *state.pending_next.lock() = None;
    Json(PlayerResponse::ok(json!({ "registered": false })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(source: &str) -> QueueItem {
        QueueItem {
            source: source.to_string(),
            duration_ms: None,
            title: None,
            artist: None,
            album: None,
            cover: None,
            track: None,
        }
    }

    #[test]
    fn next_follows_order_and_repeat_semantics() {
        // repeat=off：队尾即无下一曲
        let q = QueueSnapshot::new(
            vec![item("/a"), item("/b"), item("/c")],
            0,
            QueueRepeat::Off,
            false,
        );
        assert_eq!(q.next().unwrap().1.source, "/b");

        // repeat=all：队尾回卷
        let q = QueueSnapshot::new(
            vec![item("/a"), item("/b"), item("/c")],
            2,
            QueueRepeat::All,
            false,
        );
        assert_eq!(q.next().unwrap().1.source, "/a");

        // repeat=one：永不预载（重播走既有接力）
        let q = QueueSnapshot::new(vec![item("/a"), item("/b")], 0, QueueRepeat::One, false);
        assert!(q.next().is_none());
    }

    #[test]
    fn align_by_source_self_heals_after_queue_reorder() {
        let mut q = QueueSnapshot::new(
            vec![item("/a"), item("/b"), item("/c")],
            0,
            QueueRepeat::All,
            false,
        );
        assert_eq!(q.pos, 0);
        // 前端重排后（或 boundary 消费时）按 source 对齐
        q.align_by_source(Some("/c"));
        assert_eq!(q.order[q.pos], 2);
        assert_eq!(q.next().unwrap().1.source, "/a");
        // 未知 source 保持原位
        q.align_by_source(Some("/missing"));
        assert_eq!(q.order[q.pos], 2);
    }

    #[test]
    fn shuffle_preserves_membership_and_current_first_alignment() {
        let q = QueueSnapshot::new(
            vec![item("/a"), item("/b"), item("/c"), item("/d")],
            2,
            QueueRepeat::Off,
            true,
        );
        let mut sources: Vec<&str> = q
            .order
            .iter()
            .map(|&i| q.items[i].source.as_str())
            .collect();
        sources.sort_unstable();
        assert_eq!(sources, vec!["/a", "/b", "/c", "/d"]);
        // 洗牌物化：当前播放条目（index=2 → "/c"）固定在 order 首位，pos=0
        assert_eq!(q.items[q.order[0]].source, "/c");
        assert_eq!(q.pos, 0);
        // repeat=off：从当前曲起其余曲目全部可播到（此前排列中段截断会跳曲）
        let mut played = vec!["/c".to_string()];
        let mut cursor = q.clone();
        while let Some((source, _pos)) = cursor.next().map(|(_p, it)| (it.source.clone(), _p)) {
            played.push(source.clone());
            cursor.align_by_source(Some(&source));
        }
        let mut seen = played.clone();
        seen.sort();
        assert_eq!(seen, vec!["/a", "/b", "/c", "/d"]);
        // 推进顺序确定性（同输入同排列）
        let q2 = QueueSnapshot::new(
            vec![item("/a"), item("/b"), item("/c"), item("/d")],
            2,
            QueueRepeat::Off,
            true,
        );
        let order2: Vec<usize> = q2.order.clone();
        assert_eq!(order2, q.order);
        // repeat=all：推进一整圈后回绕，周期确定性（回绕头 = order[0] 当前曲）
        let mut qc = QueueSnapshot::new(
            vec![item("/a"), item("/b"), item("/c"), item("/d")],
            2,
            QueueRepeat::All,
            true,
        );
        let mut cycle: Vec<String> = Vec::new();
        for _ in 0..4 {
            let (source, _pos) = qc
                .next()
                .map(|(_p, it)| (it.source.clone(), _p))
                .expect("repeat=all 永有下一曲");
            qc.align_by_source(Some(&source));
            cycle.push(source);
        }
        // 一圈恰好回绕：第 5 次推进与第 1 次相同（确定性周期）
        let wrap_head = qc.next().map(|(_p, it)| it.source.clone()).unwrap();
        assert_eq!(wrap_head, cycle[0]);
        assert_eq!(cycle[3], "/c"); // 圈尾即当前曲（order[0]）
    }
}
