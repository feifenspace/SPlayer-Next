//! 播放控制与加载/seek 状态机端点。

//! REST API 控制器
//!
//! 基于 Axum 0.8 的路由定义，提供播放控制、状态查询、扫描和 WebSocket 端点。

use std::sync::Arc;

use super::spawn_isolated_blocking;
use axum::{
    extract::{Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::direct::{
    direct_load_response, materialize_direct_input, materialize_local_to_ram, online_source_mode,
    DirectInput,
};
use super::PlayerResponse;
use crate::error::ApiError;
use crate::state::{AppState, PlayerSnapshot};
use anyhow::Context as _;
use audio_engine_core::direct_runtime::{
    is_native_dsd_source, DirectLoadOutcome, DIRECT_FADE_DRAIN_MIN_BLOCKS,
    DIRECT_FULL_RECONNECT_STABILIZATION,
};
use audio_engine_core::ram_buffer::RamTrackBuffer;
use audio_engine_core::LoadSuperseded;
use std::io::Read as _;

/// 查询参数占位：历史上承载可选 cancel_handle_id，现为空结构（保留以兼容既有请求）
#[derive(Debug, Deserialize)]
pub struct LoadQuery {}

/// 音量控制请求体
#[derive(Debug, Deserialize)]
pub struct VolumeRequest {
    volume: f64,
}

/// seek 请求体
#[derive(Debug, Deserialize)]
pub struct SeekRequest {
    pub(crate) position_secs: f64,
}

/// 加载请求体
#[derive(Debug, Deserialize)]
pub struct LoadRequest {
    /// 音轨源路径或 URL
    pub source: String,
    /// 是否自动播放（默认 true）
    pub auto_play: Option<bool>,
    /// 伴随元数据（用于前端传递 CUE 分轨信息）
    pub meta: Option<LoadMeta>,
}

/// 前端传递的音轨元数据
#[derive(Debug, Deserialize)]
pub struct LoadMeta {
    pub id: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<u64>,
    pub track: Option<u16>,
    pub cue_path: Option<String>,
    pub cue_audio_path: Option<String>,
    pub cue_start_ms: Option<u64>,
    pub cue_end_ms: Option<u64>,
}

// -------------------------------------------------------------------
// REST 路由入口
// -------------------------------------------------------------------

/// 服务端控制协议版本（A2.6）：客户端启动时校验 range，不兼容报结构化错误。
/// 语义化破坏时 +1；v2 为当前版本（v1 裸格式兼容层退役后唯一版本）
pub const PROTOCOL_VERSION: u32 = 2;

/// 健康/状态查询
pub(crate) async fn status_handler(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let snapshot: PlayerSnapshot = state.snapshot();
    Ok(Json(json!({
        "state": format!("{:?}", snapshot.state),
        "position": snapshot.position,
        "duration": snapshot.duration,
        "volume": snapshot.volume,
        "speed": snapshot.speed,
        "is_finished": snapshot.is_finished,
        "current_source": snapshot.current_source,
        // 构建元数据（G.2 runtime.json 契约的运行期部分）
        "version": env!("CARGO_PKG_VERSION"),
        "commit": env!("SPLAYER_BUILD_COMMIT"),
        "source_dirty": env!("SPLAYER_BUILD_DIRTY") == "true",
        "protocol": { "version": PROTOCOL_VERSION },
    })))
}

/// 输出设备列表（D.3 前置落地）：包装引擎 list_output_devices，
/// ALSA MMAP 后端（B9）落地后在此追加 mmap 能力标志，响应形状不变
pub(crate) async fn devices_handler() -> Result<Json<PlayerResponse>, ApiError> {
    let devices = spawn_isolated_blocking("player-devices-worker", move || {
        let mut devices = audio_engine_core::audio_output::list_output_devices();
        // ALSA MMAP 直出设备并入同一列表：id 带 alsammap: 前缀即可直接选用，
        // UI 无需改版（B9 后追加的能力可见性）
        for (name, desc) in audio_engine_core::alsa_mmap_sink::list_hw_devices() {
            devices.push((
                format!("alsammap:{name}"),
                format!("{desc} · ALSA MMAP 直出"),
                false,
            ));
        }
        devices
    })
    .await
    .map_err(|e| ApiError::internal(e))?;
    Ok(Json(PlayerResponse::ok(json!({
        "devices": devices
            .into_iter()
            .map(|(id, name, is_default)| json!({
                "id": id,
                "name": name,
                "is_default": is_default,
                "mmap": id.starts_with("alsammap:"),
            }))
            .collect::<Vec<_>>(),
    }))))
}

/// 播放
pub(crate) async fn play_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let revival_source = spawn_isolated_blocking("player-play-worker", move || {
        let mut player = state.player.lock();
        player
            .play()
            .map_err(|e| ApiError::bad_request(e.to_string()))
    })
    .await
    .map_err(|e| ApiError::internal(e))??;

    match revival_source {
        None => Ok(Json(PlayerResponse::ok(json!({ "status": "playing" })))),
        Some(source) => Ok(Json(PlayerResponse::ok(json!({
            "status": "needs_load",
            "source": source,
        })))),
    }
}

/// 暂停
pub(crate) async fn pause_handler(State(state): State<AppState>) -> Json<PlayerResponse> {
    let _ = spawn_isolated_blocking("player-pause-worker", move || {
        let mut player = state.player.lock();
        let _ = player.pause();
    })
    .await;
    Json(PlayerResponse::ok(json!({ "status": "paused" })))
}

/// 停止
pub(crate) async fn stop_handler(State(state): State<AppState>) -> Json<PlayerResponse> {
    let _ = spawn_isolated_blocking("player-stop-worker", move || {
        // 停止即作废在途/已就绪的无缝预载（下一曲 staging 不属于新会话）
        super::direct_preloader::invalidate();
        // 中止在途的 probe 物化下载（若有）：停止后继续下载属于纯浪费
        if let Some(download) = state.load_download_cancel.lock().take() {
            download.cancel();
        }
        let mut player = state.player.lock();
        player.stop();
        state.note_source_change(None);
    })
    .await;
    Json(PlayerResponse::ok(json!({ "status": "stopped" })))
}

/// 音量控制
pub(crate) async fn volume_handler(
    State(state): State<AppState>,
    Json(payload): Json<VolumeRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let volume = (payload.volume as f32).clamp(0.0, 1.0);
    // 与 play/pause/stop 同款：player 锁操作走隔离线程，HTTP 线程零持锁（A2.2）
    spawn_isolated_blocking("player-volume-worker", move || {
        let mut player = state.player.lock();
        let _ = player.set_volume(volume);
        state.note_volume(volume);
    })
    .await
    .map_err(|e| ApiError::internal(e))?;
    Ok(Json(PlayerResponse::ok(json!({ "volume": volume }))))
}

#[derive(Debug, Deserialize)]
pub struct NextCandidateRequest {
    /// 下一曲音源（本地路径 / 物化后的 URL）
    pub source: String,
    /// 时长提示（秒，可选）
    pub duration_hint: Option<f64>,
}

/// 服务端可加载的候选 source：HTTP(S) 直链、绝对路径、CUE 虚拟轨、SACD ISO 虚拟轨。
/// 裸 track id / 相对路径 / 裸 .iso 路径（无虚拟轨字段）解码器打不开，
/// 注册了也只会在曲终加载失败
pub(crate) fn is_loadable_candidate_source(source: &str) -> bool {
    if source.to_lowercase().ends_with(".iso") && !source.contains('|') {
        return false;
    }
    source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("cue://")
        || source.starts_with('/')
}

/// 注册下一曲候选（B 层自动连播）：浏览器关闭后服务端仍能在曲终自动接续。
/// 单槽后写覆盖；曲终加载完成后候选即被消费
pub(crate) async fn queue_next_candidate_handler(
    State(state): State<AppState>,
    Json(payload): Json<NextCandidateRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    if !is_loadable_candidate_source(&payload.source) {
        return Err(ApiError::bad_request(format!(
            "候选 source 无法被服务端加载（需为 URL/绝对路径/cue://）：{}",
            payload.source
        )));
    }
    // cue:// 候选必须查库验证存在，防止曲库重扫/删轨后曲终接力加载失败（H1.3）
    if payload.source.starts_with("cue://") {
        let conn = state.db.lock();
        match crate::db::get_track_by_path(&conn, &payload.source) {
            Ok(Some(track)) if track.cue_audio_path.is_some() => {}
            Ok(_) => {
                return Err(ApiError::bad_request(format!(
                    "cue:// 候选不在曲库中（或缺少母版路径）：{}",
                    payload.source
                )));
            }
            Err(e) => return Err(ApiError::internal(format!("cue:// 候选查库失败: {e}"))),
        }
    }
    *state.pending_next.lock() = Some(crate::state::PendingNext {
        source: payload.source.clone(),
        duration_hint: payload.duration_hint,
    });
    let _ = state.ws_tx.send(serde_json::json!({
        "type": "nextCandidateChanged",
        "data": { "source": payload.source },
    }));
    Ok(Json(PlayerResponse::ok(json!({
        "registered": true,
        "source": payload.source,
    }))))
}

/// 取消下一曲候选
pub(crate) async fn queue_next_candidate_cancel_handler(
    State(state): State<AppState>,
) -> Json<PlayerResponse> {
    *state.pending_next.lock() = None;
    let _ = state.ws_tx.send(serde_json::json!({
        "type": "nextCandidateChanged",
        "data": null,
    }));
    Json(PlayerResponse::ok(json!({ "registered": false })))
}

/// 加载音轨（完整三段式异步 IO 闭环）
/// 整曲物化进 mlock RAM 缓冲（L2 纯内存播放，蓝图 §3.1）。
/// `Ok(None)` = 超出上限或空文件，调用方回退路径模式（带日志，非静默）
fn materialize_ram_buffer(path: &str, max_bytes: usize) -> anyhow::Result<Option<RamTrackBuffer>> {
    let file = std::fs::File::open(path).with_context(|| format!("打开待物化文件失败: {path}"))?;
    let len = file.metadata()?.len() as usize;
    if len == 0 || len > max_bytes {
        tracing::info!(path, len, max_bytes, "曲目超出 RAM 物化上限，回退路径模式");
        return Ok(None);
    }
    let buf = RamTrackBuffer::with_capacity(len, max_bytes);
    let mut reader = std::io::BufReader::with_capacity(512 * 1024, file);
    let mut chunk = vec![0u8; 512 * 1024];
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        let written = buf.append(&chunk[..n]);
        if written != n {
            anyhow::bail!("RAM 物化写入不完整（{written}/{n}），文件在读取期间增长?");
        }
    }
    buf.mark_fully_loaded();
    // mlock 失败（EPERM）降级为普通内存，lock_memory 内部已带 warn 日志
    let _ = buf.lock_memory();
    tracing::info!(path, len, "曲目已物化进 RAM，播放期零磁盘 IO");
    Ok(Some(buf))
}

/// CUE 虚拟轨必须先经曲库解析为物理分段格式。查库失败/缺母版路径直接报错，
/// 禁止把 cue:// 当普通路径下传解码器（fail loud，H1.4）。
/// 放在 player take 之前：本段只依赖 DB，且早退不会丢弃已 take 的播放状态
fn resolve_cue_source(
    state: &AppState,
    source: &str,
    meta: Option<&LoadMeta>,
) -> Result<String, ApiError> {
    let mut source_for_decoder = source.to_owned();
    if source.starts_with("cue://") {
        let resolved = {
            let conn = state.db.lock();
            match crate::db::get_track_by_path(&conn, source) {
                Ok(Some(track)) => track
                    .cue_audio_path
                    .clone()
                    .map(|audio_path| (track, audio_path)),
                Ok(None) => None,
                Err(e) => return Err(ApiError::internal(format!("cue:// 曲目查库失败: {e}"))),
            }
        };
        let Some((track, audio_path)) = resolved else {
            return Err(ApiError::bad_request(format!(
                "cue:// 曲目不在曲库中（或缺少母版路径），拒绝按普通路径打开: {source}"
            )));
        };
        let start_sec = track.cue_start_ms.unwrap_or(0) as f64 / 1000.0;
        let dur_sec = track.duration as f64 / 1000.0;
        let track_num = track.track.unwrap_or(1);
        source_for_decoder = format!(
            "{}|{:.3}|{:.3}|{}",
            audio_path, start_sec, dur_sec, track_num
        );
        tracing::info!(
            "CUE virtual track resolved to physical source: {}",
            source_for_decoder
        );
    } else if let Some(meta) = meta {
        if let Some(start_ms) = meta.cue_start_ms {
            let audio_path = meta.cue_audio_path.as_deref().unwrap_or(source);
            let start_sec = start_ms as f64 / 1000.0;
            let dur_sec = meta.duration.unwrap_or(0) as f64 / 1000.0;
            let track_num = meta.track.unwrap_or(1);
            source_for_decoder = format!(
                "{}|{:.3}|{:.3}|{}",
                audio_path, start_sec, dur_sec, track_num
            );
            tracing::info!(
                "CUE metadata resolved to physical source: {}",
                source_for_decoder
            );
        }
    }
    Ok(source_for_decoder)
}

/// 单次 load 在进入异步 worker 前从 player 锁内预留的全部状态
struct LoadReservation {
    handle: audio_engine_core::HttpCancelHandle,
    /// probe 物化下载专用取消句柄：已注册进 state.load_download_cancel，
    /// 下一个 load/stop 会 cancel 它以中止本请求仍在途的全量下载。
    /// 与主 handle 分离——播放中源的 HttpAudioSource 持有主 handle，
    /// 若复用会把"取消下载"误伤成"打断播放中的连接"
    download_cancel: audio_engine_core::HttpCancelHandle,
    direct_initial_take: Option<audio_engine_core::player::OldThreads>,
    direct_active: bool,
    current_direct_format: Option<audio_engine_core::direct_runtime::DirectFormat>,
    token: u64,
    load_token: Arc<std::sync::atomic::AtomicU64>,
    cover_dir: Option<String>,
    normalization_enabled: bool,
    device_name: Option<String>,
    direct_selector: Option<String>,
    output_generation: u64,
    failure_callback: audio_engine_core::audio_output::OutputFailureCallback,
    equalizer: Arc<parking_lot::Mutex<audio_engine_core::equalizer::Equalizer>>,
    tempo: Arc<parking_lot::Mutex<audio_engine_core::tempo::StretchProcessor>>,
}

/// handoff 尝试：Direct 连接存活且新请求仍指向 Diretta 时只登记 load token，
/// 保留连接——旧曲目在 probe / 淡出期间继续出声；失败后在任务内做全量回收。
/// 设备已切离 Diretta（direct_selector 为 None）时必须全量拆线，否则旧连接泄漏
fn reserve_player_for_load(
    state: &AppState,
    handle: audio_engine_core::HttpCancelHandle,
    source: &str,
    keep_engine_staged: bool,
) -> Result<LoadReservation, ApiError> {
    let mut player = state.player.lock();
    let mut device_name = player.selected_device().map(String::from);
    // B1.1 位纯真门槛：alsammap 选择下音量≠100%/DSP 开启或源为 DSD 时自动降级
    // cpal（文档 B9.4）；音量恢复 100% 且源为 PCM 后下一次 load 自动回到 MMAP。
    if let Some(ref selector) = device_name {
        if selector.starts_with("alsammap:") {
            let downgrade_reason = if is_native_dsd_source(source) {
                Some("DSD 源需 DSD→PCM 转换，alsammap 位纯直出不支持".to_string())
            } else {
                player
                    .validate_alsammap_entry()
                    .err()
                    .map(|e| e.to_string())
            };
            if let Some(reason) = downgrade_reason {
                // 降级目标优先选同一物理声卡的 plughw 兄弟设备（ALSA 为每个 hw 设备
                // 自动定义 plughw 并做格式/采样率转换），声音仍从用户所选声卡输出
                let sibling = selector.strip_prefix("alsammap:").and_then(|alsa| {
                    let rest = alsa.strip_prefix("hw:").unwrap_or(alsa);
                    (!rest.is_empty()).then(|| format!("alsa:plughw:{rest}"))
                });
                tracing::warn!(selector = %selector, sibling = ?sibling, reason = %reason, "alsammap 降级 cpal");
                device_name = sibling;
            }
        }
    }
    let direct_selector = device_name
        .as_deref()
        .filter(|v| audio_engine_core::diretta::selector_target(v).is_some())
        .map(String::from);
    if direct_selector.is_some() {
        if let Err(e) = player.validate_direct_entry() {
            return Err(ApiError::bad_request(e.to_string()));
        }
        // 手动切歌意图已明确：立即作废无缝预载缓存（含在途 prepare，epoch 推进）。
        // 否则 probe/淡出窗口（数百 ms）内当前曲 EOF 会装填缓存曲目"抢播"，
        // 用户先听到错曲片段再进入目标曲——这是切歌"杂音/错乱感"的来源之一。
        // 预载在 load 提交后会按新当前曲重新调度，此处作废无副作用。
        // v12-3：点播命中预载缓存时保留 staged（直通接力装填使用）
        if !keep_engine_staged {
            if let Some(handle) = player.direct_stage_handle() {
                handle.cancel();
            }
        }
    }
    let direct_active = direct_selector.is_some() && player.direct_active();
    let current_direct_format = player.direct_format();
    let (direct_initial_take, token) = if direct_active {
        (None, player.reserve_direct_handoff_token(handle.clone()))
    } else {
        let (old_threads, token) = player.take_for_async_load(handle.clone());
        (Some(old_threads), token)
    };
    // 轮换注册下载取消句柄：cancel 上一请求仍在途的物化下载（probe 阶段
    // 长循环不受 load token 校验中断，无此机制则连续切歌会并发多个僵尸下载）
    let download_cancel = audio_engine_core::HttpCancelHandle::new();
    let previous_download = state
        .load_download_cancel
        .lock()
        .replace(download_cancel.clone());
    if let Some(previous) = previous_download {
        previous.cancel();
    }
    let output_generation = player.reserve_output_generation();
    let failure_callback = player.make_failure_callback(output_generation);
    Ok(LoadReservation {
        handle,
        download_cancel,
        direct_initial_take,
        direct_active,
        current_direct_format,
        token,
        load_token: player.load_token_handle(),
        cover_dir: player.cover_cache_dir().map(String::from),
        normalization_enabled: player.is_normalization_enabled(),
        device_name,
        direct_selector,
        output_generation,
        failure_callback,
        equalizer: player.equalizer_handle(),
        tempo: player.tempo_handle(),
    })
}

pub(crate) async fn load_handler(
    State(state): State<AppState>,
    Query(_query): Query<LoadQuery>,
    Json(payload): Json<LoadRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let auto_play = payload.auto_play.unwrap_or(true);
    let source = payload.source;

    // 手动/遥控加载即接管播放：作废可能残留的曲终自动接力标志（上一曲 Ended
    // 置位、当时无候选被看门狗重新武装）。否则新曲若走 handoff 提交（服务端
    // 快照要等首个位置事件才离开曲终态），看门狗 still_ended 判定会在注册
    // 候选出现后触发接力，按队列快照级联跳曲（表现为切歌后曲目自己连跳）
    state
        .auto_advance_requested
        .store(false, std::sync::atomic::Ordering::Release);

    // 若为后台冷启动恢复请求（auto_play: false），且当前播放器已处于活跃播放或暂停状态，直接返回现有状态，不打断后台音频流
    if !auto_play {
        let snap = state.snapshot();
        if matches!(
            snap.state,
            audio_engine_core::PlayerState::Playing | audio_engine_core::PlayerState::Paused
        ) {
            return Ok(Json(PlayerResponse::ok(json!({
                "status": "active",
                "source": source,
                "duration": snap.duration,
            }))));
        }
    }

    let handle = audio_engine_core::HttpCancelHandle::new();
    let source_for_decoder = resolve_cue_source(&state, &source, payload.meta.as_ref())?;
    // v12-3 点播命中预载缓存：必须在 reserve 作废 stage 之前查询命中；
    // 命中则注册直通代数并保留引擎 staged（reserve 跳过 cancel），
    // handoff 排空窗口内直接接力预载候选，跳过并行开源
    let prestaged_generation =
        super::direct_preloader::take_staged_for_source(&state, &source);
    audio_engine_core::player::register_prestaged_handoff(prestaged_generation);
    let reservation = reserve_player_for_load(
        &state,
        handle,
        &source_for_decoder,
        prestaged_generation.is_some(),
    )?;
    let meta_duration_secs = payload
        .meta
        .as_ref()
        .and_then(|m| m.duration)
        .map(|ms| ms as f64 / 1000.0);

    if reservation.direct_selector.is_some() {
        return run_direct_load(
            &state,
            source,
            auto_play,
            source_for_decoder,
            meta_duration_secs,
            reservation,
        )
        .await;
    }
    finish_regular_load(
        &state,
        source,
        auto_play,
        source_for_decoder,
        meta_duration_secs,
        reservation,
    )
    .await
}

/// 把 Direct 连接的实际格式折算进元数据（handoff 与全量重连共用）
fn fold_direct_format_into(
    metadata: &mut audio_engine_core::AudioMetadata,
    format: audio_engine_core::direct_runtime::DirectFormat,
) {
    match format {
        audio_engine_core::direct_runtime::DirectFormat::Pcm(format) => {
            metadata.sample_rate = format.sample_rate;
            metadata.original_sample_rate = format.sample_rate;
            metadata.channels = format.channels;
            metadata.bits_per_sample = u32::from(format.valid_bits);
        }
        audio_engine_core::direct_runtime::DirectFormat::Dsd(format) => {
            metadata.sample_rate = format.bit_rate;
            metadata.original_sample_rate = format.bit_rate;
            metadata.channels = format.channels;
            metadata.bits_per_sample = 1;
        }
    }
}

/// stream 模式元数据轻嗅探：Range 拉取在线源首 64KB，按容器/编码魔数判定
/// 编码格式（仅用于前端展示；播放线格式以 Diretta 连接建立后的实测为准）。
/// 任何失败返回 None（调用方回退 "stream" 占位），绝不阻塞起播主路径
fn sniff_http_codec(url: &str) -> Option<String> {
    const SNIFF_BYTES: usize = 64 * 1024;
    use std::io::Read as _;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;
    let response = client
        .get(url)
        .header("Range", format!("bytes=0-{}", SNIFF_BYTES - 1))
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        )
        .header("Accept", "*/*")
        .header("Accept-Encoding", "identity")
        .send()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut buf = Vec::with_capacity(SNIFF_BYTES);
    response.take(SNIFF_BYTES as u64).read_to_end(&mut buf).ok()?;
    Some(sniff_codec_magic(&buf).to_string())
}

/// 按魔数判定编码格式。未知内容返回 "stream"（与旧行为一致）
fn sniff_codec_magic(buf: &[u8]) -> &'static str {
    if buf.starts_with(b"fLaC") {
        return "flac";
    }
    // ID3v2 头或 MPEG 帧同步（0xFFEx，11 位同步）
    if buf.starts_with(b"ID3") || (buf.len() >= 2 && buf[0] == 0xFF && (buf[1] & 0xE0) == 0xE0) {
        return "mp3";
    }
    if buf.starts_with(b"OggS") {
        return "ogg";
    }
    if buf.starts_with(b"FRM8") {
        return "dff";
    }
    if buf.starts_with(b"DSD ") {
        return "dsf";
    }
    // MP4/M4A 容器：在 moov 范围内找 stsd 编解码四字符标识
    if buf.len() >= 12 && &buf[4..8] == b"ftyp" {
        for (needle, codec) in [
            (&b"alac"[..], "alac"),
            (&b"fLaC"[..], "flac"),
            (&b"mp4a"[..], "aac"),
            (&b"opus"[..], "opus"),
        ] {
            if buf.windows(needle.len()).any(|w| w == needle) {
                return codec;
            }
        }
        return "m4a";
    }
    if buf.len() >= 12 && buf.starts_with(b"RIFF") && &buf[8..12] == b"WAVE" {
        return "wav";
    }
    "stream"
}

/// 阶段 1：探测新源元数据（handoff 粗检需要 sample_rate/channels）。
/// stream 模式无法廉价探测：占位元数据 + 换源时的权威校验兜底
#[allow(clippy::type_complexity)]
fn probe_direct_source(
    source_for_direct: &str,
    stream_mode: bool,
    is_dsd: bool,
    use_dop: bool,
    ram_preload: bool,
    ram_max_bytes: usize,
    meta_duration_secs: Option<f64>,
    cover_dir: Option<&str>,
    handle: &audio_engine_core::HttpCancelHandle,
    download_cancel: &audio_engine_core::HttpCancelHandle,
    load_token: &std::sync::atomic::AtomicU64,
    token: u64,
) -> Result<
    (
        Option<DirectInput>,
        audio_engine_core::AudioMetadata,
        Option<RamTrackBuffer>,
    ),
    anyhow::Error,
> {
    let is_http =
        source_for_direct.starts_with("http://") || source_for_direct.starts_with("https://");
    // B6.2 DoP：dsd_transport=dop 时 DSD 源整曲转 DoP WAV 进 RAM，
    // 经 PCM Direct 播放（绕开 SDK 原生 DSD 通道）；否则 DSD 走原生直通。
    // DoP 产物保持 RamTrackBuffer（mlock）通道，full_reconnect 经
    // open_reader_verified 消费——DSD 家族从不 handoff，无需 memfd 路径语义
    let ram = if use_dop && !is_http {
        Some(audio_engine_core::dsd::dop_wav::convert_dsd_to_dop_ram(
            source_for_direct,
            ram_max_bytes,
        )?)
    } else {
        None
    };
    // L2 纯内存（本地 PCM preload）：整曲物化进 memfd 纯内存缓存，probe /
    // handoff / full_reconnect 统一从 RAM 打开（此前 RamTrackBuffer 方案仅
    // full_reconnect 消费，handoff 成功时整曲物化被浪费并双读 NAS）。CUE
    // 管道源必须先剥离出物理母版再物化——此前把管道串直传 File::open 必然
    // ENOENT（ram_preload 开启时 CUE 轨加载直接 500，已实测复现）；超上限
    // /物化失败回退路径模式。设备无 swap，memfd 页面即常驻 RAM
    let mut local_preload: Option<DirectInput> = None;
    if ram.is_none() && !is_http && !is_dsd && ram_preload {
        let preload_physical = audio_engine_core::cue::parse_cue_virtual_path(source_for_direct)
            .map(|cue| cue.physical_path)
            .unwrap_or_else(|| source_for_direct.to_owned());
        local_preload = materialize_local_to_ram(&preload_physical, ram_max_bytes as u64, || {
            load_token.load(std::sync::atomic::Ordering::Acquire) != token
        })
        .unwrap_or_else(|error| {
            tracing::warn!(
                source = %source_for_direct,
                %error,
                "本地源 memfd 物化失败，回退路径模式"
            );
            None
        });
    }
    let physical_source = if stream_mode {
        None
    } else if is_http {
        // 物化下载用独立取消句柄：被新 load/stop 掐断时本请求整体失败，
        // token 校验随后把它归类为让位（而非故障）
        let abort_download = || download_cancel.is_cancelled();
        Some(materialize_direct_input(
            source_for_direct,
            download_cancel,
            abort_download,
        )?)
    } else if let Some(input) = local_preload {
        Some(input)
    } else {
        Some(DirectInput::Path(source_for_direct.to_owned()))
    };
    let metadata = match physical_source.as_ref().map(DirectInput::path) {
        Some(path) => {
            // 探测路径：本地源（含 CUE/SACD 管道）一律从原始 source 探测——
            // 轨级元数据、路径派生的艺术家/专辑标签与目录封面均依赖真实路径，
            // 从 /proc/self/fd/N 探测会产生垃圾标签（已实测）；memfd 仅用于
            // 播放期打开。在线源维持从物化产物探测（原行为）
            let probe_path = if is_http {
                path
            } else {
                source_for_direct
            };
            let meta = audio_engine_core::decoder::probe_metadata(probe_path, cover_dir, handle.clone())?;
            if load_token.load(std::sync::atomic::Ordering::Acquire) != token {
                anyhow::bail!(LoadSuperseded);
            }
            meta
        }
        None if ram.is_some() => {
            // RAM 源的元数据仍从原始路径一次性探测（载入期磁盘读，播放期零 IO）
            let meta = audio_engine_core::decoder::probe_metadata(
                source_for_direct,
                cover_dir,
                handle.clone(),
            )?;
            if load_token.load(std::sync::atomic::Ordering::Acquire) != token {
                anyhow::bail!(LoadSuperseded);
            }
            meta
        }
        None => audio_engine_core::AudioMetadata {
            duration_secs: meta_duration_secs.unwrap_or(0.0),
            // stream 模式不做完整探测（在线流探测代价高），但前端"音质详情"
            // 不能显示占位符：Range 拉首 64KB 魔数轻嗅真实容器/编码，失败
            // 才回退 "stream"。采样率/位深/声道仍由连接建立后的
            // fold_direct_format_into 用 Diretta 实际线格式折算
            codec: sniff_http_codec(source_for_direct)
                .unwrap_or_else(|| "stream".to_string()),
            ..Default::default()
        },
    };
    Ok((physical_source, metadata, ram))
}

/// 阶段 2：handoff-first——保留 Diretta 连接，块边界原子换源。
/// Ok(true) = handoff 已提交；Ok(false) = 格式不符需回退全量重连；Err = Superseded/真错
#[allow(clippy::too_many_arguments)]
fn try_handoff_to_new_source(
    state: &AppState,
    token: u64,
    source_for_direct: &str,
    open_path: Option<&str>,
    auto_play: bool,
    current_format: audio_engine_core::direct_runtime::DirectFormat,
    metadata: &mut audio_engine_core::AudioMetadata,
    is_dsd: bool,
) -> Result<bool, anyhow::Error> {
    match audio_engine_core::InnerPlayer::try_direct_handoff(
        &state.player,
        token,
        source_for_direct,
        open_path,
        metadata.duration_secs,
        auto_play,
        current_format,
        metadata,
        is_dsd,
    ) {
        Ok(Some(format)) => {
            fold_direct_format_into(metadata, format);
            tracing::info!(
                target: "diretta_handoff",
                phase = "load_handoff_ok",
                source = %source_for_direct,
                "handoff 提交成功，Diretta 连接已复用"
            );
            Ok(true)
        }
        Ok(None) => anyhow::bail!(LoadSuperseded),
        Err(err) => {
            if err.is::<LoadSuperseded>() {
                return Err(err);
            }
            // v11-3: 热重配实验分支——handoff 预检失败（典型为跨采样率）时，
            // 先尝试不拆连接的热重配；失败/未开开关再回退全量重连。
            // 排空语义与 handoff 一致（try_direct_hot_reconfigure 内部自处理）。
            // v12-4 tinyLMS 模式跳过：tinyLMS 结论是格式变更必须 Hard Reset
            // （SDK setSinkConfigure 不向 Target 外发 SinkConfigure，在线重配
            // 时钟不跟随），预静音后全量重连才是久经验证路径
            if !audio_engine_core::direct_runtime::tiny_lms_switch_enabled() {
                match audio_engine_core::InnerPlayer::try_direct_hot_reconfigure(
                &state.player,
                token,
                source_for_direct,
                open_path,
                metadata.duration_secs,
                auto_play,
                current_format,
                metadata,
                is_dsd,
            ) {
                Ok(Some(format)) => {
                    fold_direct_format_into(metadata, format);
                    tracing::info!(
                        target: "diretta_handoff",
                        phase = "load_hot_reconfigure_ok",
                        source = %source_for_direct,
                        "热重配提交成功，Diretta 连接已复用"
                    );
                    return Ok(true);
                }
                Ok(None) => anyhow::bail!(LoadSuperseded),
                Err(hot_err) => {
                    if hot_err.is::<LoadSuperseded>() {
                        return Err(hot_err);
                    }
                    tracing::debug!(
                        target: "diretta_handoff",
                        phase = "load_hot_reconfigure_skip",
                        error = %hot_err,
                        "热重配不可用，回退全量重连"
                    );
                }
                }
            }
            tracing::warn!(
                target: "diretta_handoff",
                phase = "load_handoff_fallback",
                error = %err,
                "handoff 失败，回退全量重连"
            );
            Ok(false)
        }
    }
}

/// 阶段 3：全量重连（淡出拆旧连接 → 重新协商 Diretta endpoint → 打开并验证新源）。
/// 返回（连接，生效 token）——handoff 失败重拆时 token 会推进
#[allow(clippy::too_many_arguments)]
fn full_reconnect_load(
    state: &AppState,
    selector: &str,
    source_for_direct: &str,
    auto_play: bool,
    stream_mode: bool,
    physical_source: Option<DirectInput>,
    ram_source: Option<RamTrackBuffer>,
    metadata: &mut audio_engine_core::AudioMetadata,
    direct_initial_take: Option<audio_engine_core::player::OldThreads>,
    handle: &audio_engine_core::HttpCancelHandle,
    load_token: &std::sync::atomic::AtomicU64,
    token: u64,
    task_final_token: &std::sync::atomic::AtomicU64,
    meta_duration_secs: Option<f64>,
) -> Result<(audio_engine_core::direct_runtime::DirectPlayback, u64), anyhow::Error> {
    let (old_threads, token) = match direct_initial_take {
        Some(threads) => (threads, token),
        None => {
            // v12-4 tinyLMS Hard Reset（默认）：拆连接前 SDK 回调层预静音——
            // PCM 连续交付 8 周期数字静音 / DSD 置 0x69 静音垫并同步短等，
            // 对齐 tinyLMS TriggerPreMute+WaitPreMuteDone。此后在途数据尾部
            // 必为零电平，disconnect 清空 Target 缓冲无咔哒
            if audio_engine_core::direct_runtime::tiny_lms_switch_enabled() {
                let premute = state.player.lock().begin_direct_pre_mute();
                if premute {
                    // 锁外事件驱动等待倒计时消耗完（连接暂停/无拉流时由超时兜底）
                    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(120);
                    while state.player.lock().direct_pre_mute_pending()
                        && std::time::Instant::now() < deadline
                    {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    tracing::debug!(
                        target: "diretta_handoff",
                        phase = "reconnect_pre_mute_done",
                        "Hard Reset 预静音完成，开始拆连接"
                    );
                }
            } else {
                // legacy：handoff 尝试失败，确保旧源排空后再拆（sequence 中可能已淡出）。
                // 事件驱动等待，无固定 sleep；暂停态/无连接瞬时通过
                {
                    let mut player = state.player.lock();
                    let _ = player.begin_direct_fade_out();
                }
                // 排空等待最长 600ms：短锁取句柄，在锁外等待不占全局 player 锁
                let drain = state.player.lock().direct_drain_handle();
                if let Some(monitor) = &drain {
                    // 超时与动态排空目标联动（drain_target + EXTRA）
                    if !monitor.wait_fade_drained(
                        DIRECT_FADE_DRAIN_MIN_BLOCKS,
                        std::time::Duration::from_micros(monitor.drain_target_micros())
                            + audio_engine_core::direct_runtime::DIRECT_FADE_DRAIN_EXTRA,
                    ) {
                        tracing::warn!(
                            target: "diretta_handoff",
                            phase = "reconnect_fade_drain_timeout",
                            "全量重连前排空等待超时，仍继续拆连接"
                        );
                    }
                }
            }
            // 校验 + 拆连接必须同一把锁内完成，防止与更新的 load 竞态抢跑。
            // 【防爆音决策】此处不做 pause/sync->stop 软停止：真机 A/B 实测
            // sync->stop 本身产生低频咚声（暂停路径同源），而直接从"静音垫
            // 交付中"disconnect（与 stop 路径同序列）无咚——咚源在新会话
            // 建立（setSink playback-rejection 门控），见 bridge setSink 注释
            // direct_initial_take=None 意味着本请求在 reserve 阶段已通过
            // reserve_direct_handoff_token 注册过 handle（handoff 失败回退）。
            // 必须用 retake 变体：take_for_async_load 会把 pending_load_handle
            // 里的本请求 handle 当作"上一请求"取消掉（自取消 bug，切歌报错
            // "Operation cancelled by user" 且播放器停在 Stopped）
            let (threads, token) = {
                let mut player = state.player.lock();
                if !player.is_load_token_current(token) {
                    anyhow::bail!(LoadSuperseded);
                }
                player.retake_for_async_load_after_handoff_reserve(handle.clone())
            };
            task_final_token.store(token, std::sync::atomic::Ordering::Release);
            (threads, token)
        }
    };
    let replacing_direct_playback = old_threads.direct_playback.is_some();
    if let Some(h) = old_threads.join_aux() {
        let _ = h.join();
    }
    if replacing_direct_playback {
        // 替换现存连接后给 Target/DAC 一个格式稳定窗口（非 stream 模式的启动验证也依赖它）
        std::thread::sleep(DIRECT_FULL_RECONNECT_STABILIZATION);
    }

    let is_http_source =
        source_for_direct.starts_with("http://") || source_for_direct.starts_with("https://");
    let playback = if stream_mode {
        let http = audio_engine_core::ffmpeg_audio::HttpAudioSource::new_with_cancel_handle(
            source_for_direct,
            handle,
        )?;
        audio_engine_core::direct_runtime::DirectPlayback::open_stream(
            selector,
            source_for_direct,
            Box::new(http),
            meta_duration_secs.unwrap_or(0.0),
            auto_play,
        )?
    } else if let Some(ram) = ram_source {
        // L2 纯内存播放：RAM 源经 ReadSeek 打开并做与 open_verified_local
        // 同款的启动验证；CUE 轨起点作为轨内坐标基准
        let start_offset = audio_engine_core::cue::parse_cue_virtual_path(source_for_direct)
            .map(|cue| cue.start_time)
            .unwrap_or(0.0);
        audio_engine_core::direct_runtime::DirectPlayback::open_reader_verified(
            selector,
            source_for_direct,
            Box::new(ram),
            metadata.duration_secs,
            start_offset,
            auto_play,
            load_token,
            token,
        )?
    } else if !is_http_source
        && matches!(
            physical_source.as_ref(),
            Some(DirectInput::Memfd { .. })
        )
    {
        // 本地 memfd 物化：经路径重新打开（全新 fd，读位置从 0 开始；try_clone
        // 会与物化写侧共享 file offset——写完停在 EOF，FFmpeg 首读即空报
        // Invalid data，已实测复现）+ Reader 打开（播放期零 CIFS IO）。
        // 不走 open_verified_local——它从路径参数解析虚拟轨信息，memfd 路径
        // 无 CUE 管道后缀会丢失轨起点；Reader 通道显式传轨内偏移，轨级时长
        // （probe 已按 CUE 折算）经 set_duration 驱动虚拟 EOF（与 DoP 通道
        // 同机制）。在线 memfd 仍走 open_verified_local（无虚拟轨，原行为）
        let fd_path = match physical_source.as_ref().expect("分支已匹配 Memfd") {
            DirectInput::Memfd { path, .. } => path.clone(),
            _ => unreachable!("matches! 已收窄"),
        };
        let file = std::fs::File::open(&fd_path)
            .with_context(|| format!("重开 memfd 物化产物失败: {fd_path}"))?;
        let start_offset = audio_engine_core::cue::parse_cue_virtual_path(source_for_direct)
            .map(|cue| cue.start_time)
            .unwrap_or(0.0);
        audio_engine_core::direct_runtime::DirectPlayback::open_reader_verified(
            selector,
            source_for_direct,
            Box::new(file),
            metadata.duration_secs,
            start_offset,
            auto_play,
            load_token,
            token,
        )?
    } else {
        let physical = physical_source
            .as_ref()
            .map(DirectInput::path)
            .expect("非 stream 模式必然已有物理源路径");
        audio_engine_core::direct_runtime::DirectPlayback::open_verified_local(
            selector,
            physical,
            metadata.duration_secs,
            auto_play,
            load_token,
            token,
        )?
    };
    fold_direct_format_into(metadata, playback.format());
    metadata.duration_secs = playback.duration();
    if load_token.load(std::sync::atomic::Ordering::Acquire) != token {
        anyhow::bail!(LoadSuperseded);
    }
    Ok((playback, token))
}

/// Diretta 选择器路径：worker 内 probe → handoff-first → 全量重连，按结果提交/让位
async fn run_direct_load(
    state: &AppState,
    source: String,
    auto_play: bool,
    source_for_decoder: String,
    meta_duration_secs: Option<f64>,
    reservation: LoadReservation,
) -> Result<Json<PlayerResponse>, ApiError> {
    let LoadReservation {
        handle,
        download_cancel,
        direct_initial_take,
        direct_active,
        current_direct_format: direct_format_snapshot,
        token,
        load_token: load_token_for_direct,
        cover_dir,
        direct_selector,
        ..
    } = reservation;
    let selector = direct_selector.expect("run_direct_load 仅在 direct_selector 存在时进入");
    let source_for_direct = source_for_decoder;
    let source_mode = online_source_mode(state);
    // 任务结束前本请求最后持有的 token（handoff 失败回退重拆时会推进一次）
    let task_final_token = Arc::new(std::sync::atomic::AtomicU64::new(token));
    let task_final_token_reader = Arc::clone(&task_final_token);
    // worker 闭包用克隆：state 在本 handler 后续（commit / 错误清理）仍需使用
    let state_for_task = state.clone();

    let result = spawn_isolated_blocking("player-direct-load-worker", move || {
        let is_http =
            source_for_direct.starts_with("http://") || source_for_direct.starts_with("https://");
        // DSD 原生流（DSF/DFF/SACD ISO）需要 seekable 输入做 chunk 定位，
        // stream 模式下强制走 preload（memfd/磁盘缓存）
        let is_dsd = is_native_dsd_source(&source_for_direct);
        let stream_mode = is_http && source_mode == "stream" && !is_dsd;
        let ram_preload = state_for_task.config.playback.ram_preload;
        let ram_max_bytes = state_for_task.config.resolved_ram_preload_max_bytes();
        let use_dop = is_dsd && state_for_task.config.playback.dsd_transport == "dop";

        let (physical_source, mut metadata, ram_source) = probe_direct_source(
            &source_for_direct,
            stream_mode,
            is_dsd,
            use_dop,
            ram_preload,
            ram_max_bytes,
            meta_duration_secs,
            cover_dir.as_deref(),
            &handle,
            &download_cancel,
            &load_token_for_direct,
            token,
        )?;

        // handoff 打开用物化路径：preload 物化产物（在线源 memfd/磁盘缓存、
        // 本地源 memfd）本地 seekable，handoff 阶段直接从物化产物打开——
        // 在线源避免绕过缓存重开网络流（整曲双下载），本地源消除换源期
        // CIFS 重读（此前 ram_source 物化在 handoff 成功时被整个浪费）。
        // 规则：物化产物路径 ≠ 逻辑 source 时传入；本地未物化（Path(source)）
        // 不传——cue/sacd 解析必须留在引擎内基于 source 进行（物化路径无
        // 虚拟轨信息）；stream 模式无物化（None），按 URL 流式打开（原行为）
        let handoff_open_path = physical_source
            .as_ref()
            .filter(|input| input.path() != source_for_direct)
            .map(|input| input.path().to_owned());

        if direct_active {
            let current_format =
                direct_format_snapshot.expect("direct_active 时必须携带当前连接格式快照");
            match try_handoff_to_new_source(
                &state_for_task,
                token,
                &source_for_direct,
                handoff_open_path.as_deref(),
                auto_play,
                current_format,
                &mut metadata,
                is_dsd,
            ) {
                Ok(true) => {
                    return Ok(DirectLoadOutcome::Handoff(Box::new(metadata)));
                }
                Ok(false) => {}
                Err(err) => return Err(err),
            }
        }

        let (playback, token) = full_reconnect_load(
            &state_for_task,
            &selector,
            &source_for_direct,
            auto_play,
            stream_mode,
            physical_source,
            ram_source,
            &mut metadata,
            direct_initial_take,
            &handle,
            &load_token_for_direct,
            token,
            &task_final_token,
            meta_duration_secs,
        )?;
        Ok(DirectLoadOutcome::FullReconnect {
            metadata: Box::new(metadata),
            playback,
            token,
        })
    })
    .await
    .map_err(|e| ApiError::internal(format!("Direct load task join error: {e}")))?;

    commit_direct_outcome(state, &source, auto_play, task_final_token_reader, result).await
}

/// Diretta worker 结果提交：handoff 直接收尾；全量重连持 token 提交；
/// 错误仅在仍持最后 token 时上报清理，否则静默让位给更新的 load
async fn commit_direct_outcome(
    state: &AppState,
    source: &str,
    auto_play: bool,
    task_final_token: Arc<std::sync::atomic::AtomicU64>,
    result: anyhow::Result<
        audio_engine_core::direct_runtime::DirectLoadOutcome<Box<audio_engine_core::AudioMetadata>>,
    >,
) -> Result<Json<PlayerResponse>, ApiError> {
    match result {
        Ok(DirectLoadOutcome::Handoff(meta)) => {
            update_now_playing(state, source, &meta);
            state.note_source_change(Some(source));
            // 新曲已生效：调度下一曲无缝预载（队列未注册时为无害 no-op）
            super::direct_preloader::schedule_next_preload(state);
            Ok(direct_load_response(source, auto_play, *meta))
        }
        Ok(DirectLoadOutcome::FullReconnect {
            metadata,
            playback,
            token,
        }) => {
            let committed_meta = {
                let mut player = state.player.lock();
                player
                    .commit_direct_loaded(token, source, auto_play, *metadata, playback)
                    .map_err(|e| ApiError::internal(e.to_string()))?
            };
            match committed_meta {
                Some(meta) => {
                    update_now_playing(state, source, &meta);
                    state.note_source_change(Some(source));
                    // 新曲已生效：调度下一曲无缝预载（队列未注册时为无害 no-op）
                    super::direct_preloader::schedule_next_preload(state);
                    Ok(direct_load_response(source, auto_play, meta))
                }
                None => Ok(Json(PlayerResponse::ok(json!({
                    "status": "superseded",
                    "source": source,
                })))),
            }
        }
        Err(err) => {
            if err.is::<LoadSuperseded>() {
                return Ok(Json(PlayerResponse::ok(json!({
                    "status": "superseded",
                    "source": source,
                }))));
            }
            let err_text = format!("{err:#}");
            let mut player = state.player.lock();
            // 仅当本请求仍持有最后登记的 token 时才报错并清理；
            // 否则已被更新的 load 抢占，静默让位（连接归新请求管理）
            if player
                .is_load_token_current(task_final_token.load(std::sync::atomic::Ordering::Acquire))
            {
                player.stop();
                state.note_source_change(None);
                return Err(ApiError::bad_request(err_text));
            }
            Ok(Json(PlayerResponse::ok(json!({
                "status": "superseded",
                "source": source,
            }))))
        }
    }
}

/// 非 Direct 选择器路径：拆旧线程后 prepare → 协商输出 → 启动解码
#[allow(clippy::type_complexity)]
fn regular_load_worker(
    source_for_decoder: String,
    cover_dir: Option<String>,
    handle: audio_engine_core::HttpCancelHandle,
    ram_preload: bool,
    ram_max_bytes: usize,
    load_token: Arc<std::sync::atomic::AtomicU64>,
    token: u64,
    device_name: Option<String>,
    output_generation: u64,
    failure_callback: audio_engine_core::audio_output::OutputFailureCallback,
    normalization_enabled: bool,
    equalizer: Arc<parking_lot::Mutex<audio_engine_core::equalizer::Equalizer>>,
    tempo: Arc<parking_lot::Mutex<audio_engine_core::tempo::StretchProcessor>>,
    direct_initial_take: Option<audio_engine_core::player::OldThreads>,
) -> Result<
    (
        audio_engine_core::AudioMetadata,
        std::thread::JoinHandle<audio_engine_core::decoder::DecoderData>,
        Arc<audio_engine_core::shared::Shared>,
        audio_engine_core::audio_output::AudioOutput,
        Option<audio_engine_core::HttpCancelHandle>,
    ),
    anyhow::Error,
> {
    // 非 Direct 选择器路径：direct_initial_take 必为 Some（进入时已在锁内拆线）
    if let Some(threads) = direct_initial_take {
        if let Some(h) = threads.join_aux() {
            let _ = h.join();
        }
    }
    // L2 纯内存播放：本地 PCM/CUE 源整曲物化进 RAM 后交解码器
    // （SACD ISO 虚拟轨与在线源维持既有通道；超上限回退路径模式）
    let is_http =
        source_for_decoder.starts_with("http://") || source_for_decoder.starts_with("https://");
    let is_sacd_virtual =
        audio_engine_core::sacd::parse_sacd_virtual_path(&source_for_decoder).is_some();
    let prepared = if ram_preload && !is_http && !is_sacd_virtual {
        let physical = audio_engine_core::cue::parse_cue_virtual_path(&source_for_decoder)
            .map(|cue| cue.physical_path)
            .unwrap_or_else(|| source_for_decoder.clone());
        match materialize_ram_buffer(&physical, ram_max_bytes)? {
            Some(ram) => audio_engine_core::decoder::prepare_decode_from_ram(
                ram,
                &source_for_decoder,
                cover_dir.as_deref(),
            )?,
            None => audio_engine_core::decoder::prepare_decode(
                &source_for_decoder,
                cover_dir.as_deref(),
                handle,
            )?,
        }
    } else {
        audio_engine_core::decoder::prepare_decode(
            &source_for_decoder,
            cover_dir.as_deref(),
            handle,
        )?
    };
    if load_token.load(std::sync::atomic::Ordering::Acquire) != token {
        anyhow::bail!(LoadSuperseded);
    }
    // 输出采样率协商：音源原始采样率被设备支持时按精确采样率打开
    let output = audio_engine_core::audio_output::AudioOutput::new(
        device_name.as_deref(),
        Some(prepared.original_sample_rate()),
        output_generation,
        failure_callback,
    )?;
    let shared = audio_engine_core::shared::Shared::new(output.sample_rate(), output.channels());
    shared.set_normalization_enabled(normalization_enabled);
    equalizer
        .lock()
        .set_output_format(output.sample_rate(), output.channels());
    equalizer.lock().reset_state();
    tempo
        .lock()
        .set_output_format(output.sample_rate(), output.channels());
    tempo.lock().reset();
    let (metadata, decode_handle, cancel) = audio_engine_core::decoder::start_prepared_decode(
        prepared,
        std::sync::Arc::clone(&shared),
        equalizer,
        tempo,
    )?;
    Ok((metadata, decode_handle, shared, output, cancel))
}

/// 非 Direct 路径收尾：worker 结果错误处理 → 提交 → now-playing/观测日志/响应
async fn finish_regular_load(
    state: &AppState,
    source: String,
    auto_play: bool,
    source_for_decoder: String,
    meta_duration_secs: Option<f64>,
    reservation: LoadReservation,
) -> Result<Json<PlayerResponse>, ApiError> {
    let _ = meta_duration_secs;
    let LoadReservation {
        handle,
        direct_initial_take,
        token,
        load_token,
        cover_dir,
        normalization_enabled,
        device_name,
        output_generation,
        failure_callback,
        equalizer,
        tempo,
        ..
    } = reservation;
    let ram_preload = state.config.playback.ram_preload;
    let ram_max_bytes = state.config.resolved_ram_preload_max_bytes();

    let result = spawn_isolated_blocking("player-load-worker", move || {
        regular_load_worker(
            source_for_decoder,
            cover_dir,
            handle,
            ram_preload,
            ram_max_bytes,
            load_token,
            token,
            device_name,
            output_generation,
            failure_callback,
            normalization_enabled,
            equalizer,
            tempo,
            direct_initial_take,
        )
    })
    .await
    .map_err(|e| ApiError::internal(format!("Load task join error: {e}")))?;

    let (metadata, decode_handle, shared, output, cancel) = match result {
        Ok(res) => res,
        Err(err) => {
            let mut player = state.player.lock();
            if !player.is_load_token_current(token) {
                return Ok(Json(PlayerResponse::ok(json!({
                    "status": "superseded",
                    "source": source,
                }))));
            }
            player.clear_pending_load(token);
            let is_remote = source.starts_with("http://") || source.starts_with("https://");
            if is_remote {
                player.emit_source_error();
            }
            // 保留完整错误链：仅 to_string() 只显示最外层 kind，丢失具体原因
            return Err(ApiError::bad_request(format!("{err:#}")));
        }
    };

    let committed_meta = {
        let mut player = state.player.lock();
        player
            .commit_loaded(
                token,
                &source,
                auto_play,
                audio_engine_core::player::LoadedPlayback {
                    metadata,
                    decode_handle,
                    shared,
                    output,
                    cancel,
                },
            )
            .map_err(|e| ApiError::internal(e.to_string()))?
    };

    match committed_meta {
        Some(meta) => {
            update_now_playing(state, &source, &meta);
            state.note_source_change(Some(&source));
            // 开流格式观测：把每次 load 的采样率/位深/编解码留在日志里，
            // 用于与输出停滞的相关性分析（Target 对特定格式拒收的定位）
            tracing::info!(
                source = %source,
                sample_rate = meta.sample_rate,
                original_sample_rate = meta.original_sample_rate,
                channels = meta.channels,
                bits_per_sample = meta.bits_per_sample,
                codec = ?meta.codec,
                "load 已提交"
            );
            Ok(direct_load_response(&source, auto_play, meta))
        }
        None => Ok(Json(PlayerResponse::ok(json!({
            "status": "superseded",
            "source": source,
        })))),
    }
}

/// 更新服务端 now-playing 元数据快照（load 成功时调用，重开页面/无浏览器恢复用）
pub(crate) fn update_now_playing(
    state: &AppState,
    source: &str,
    meta: &audio_engine_core::AudioMetadata,
) {
    *state.now_playing.lock() = Some(json!({
        "source": source,
        "title": meta.title,
        "artist": meta.artist,
        "album": meta.album,
        "cover": meta.cover,
        "duration_secs": meta.duration_secs,
        "sample_rate": meta.sample_rate,
        "original_sample_rate": meta.original_sample_rate,
        "channels": meta.channels,
        "bits_per_sample": meta.bits_per_sample,
        "bit_rate": meta.bit_rate,
        "codec": meta.codec,
    }));
}

/// 服务端权威“正在播放”快照：重开页面/无浏览器场景恢复曲目显示用。
/// metadata 为 None 表示当前无已加载曲目（或 load 失败后的清理态）
pub(crate) async fn now_playing_handler(State(state): State<AppState>) -> Json<PlayerResponse> {
    let snap = state.snapshot();
    let meta = state.now_playing.lock().clone();
    Json(PlayerResponse::ok(json!({
        "source": snap.current_source,
        "metadata": meta,
        "state": format!("{:?}", snap.state),
        "position": snap.position,
        "duration": snap.duration,
        "playing": snap.state == audio_engine_core::PlayerState::Playing,
    })))
}

pub(crate) enum SeekOutcome {
    Resumed {
        shared: std::sync::Arc<audio_engine_core::shared::Shared>,
        handle: std::thread::JoinHandle<audio_engine_core::decoder::DecoderData>,
    },
    Fallback,
}

/// seek（三段式异步恢复）
pub(crate) async fn seek_handler(
    State(state): State<AppState>,
    Json(payload): Json<SeekRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let position = payload.position_secs.max(0.0);

    let direct_take = {
        let mut player = state.player.lock();
        player
            .take_for_async_direct_seek()
            .map_err(|e| ApiError::internal(e.to_string()))?
    };
    if let Some(take) = direct_take {
        let token = take.token;
        let (playback, seek_result) =
            spawn_isolated_blocking("player-direct-seek-worker", move || {
                let mut playback = take.playback;
                let result = playback.seek_while_paused(position);
                (playback, result)
            })
            .await
            .map_err(|e| ApiError::internal(format!("Direct seek task join error: {e}")))?;

        let mut player = state.player.lock();
        if !player.is_load_token_current(token) {
            return Ok(Json(PlayerResponse::ok(json!({
                "status": "superseded",
                "position": position,
            }))));
        }
        if let Err(error) = seek_result {
            player.enter_paused_for_recovery();
            let _ = player.commit_direct_seeked(token, playback);
            return Err(ApiError::bad_request(format!("{error:#}")));
        }
        let committed = player
            .commit_direct_seeked(token, playback)
            .map_err(|e| ApiError::internal(e.to_string()))?;
        if committed {
            state.note_position(position);
        }
        return Ok(Json(PlayerResponse::ok(json!({
            "status": if committed { "seeked" } else { "superseded" },
            "position": position,
        }))));
    }

    let (take, was_playing, current_source) = {
        let mut player = state.player.lock();
        let was_playing = player.state() == audio_engine_core::PlayerState::Playing;
        let current_source = player.current_source().map(String::from);
        let take = player.take_for_async_seek();
        (take, was_playing, current_source)
    };

    let Some(take) = take else {
        return Ok(Json(PlayerResponse::ok(json!({
            "status": "no_active_track",
            "position": position,
        }))));
    };

    let audio_engine_core::player::SeekTake {
        old_threads,
        normalization_enabled,
        normalization_gain,
        current_source: _,
        was_playing: _,
        output_sample_rate,
        output_channels,
        token,
        equalizer,
        tempo,
        original_sample_rate: _,
    } = take;

    let outcome: SeekOutcome = spawn_isolated_blocking("player-seek-worker", move || {
        let decoder_data = old_threads.join_aux().and_then(|h| h.join().ok());
        let mut decoder_data = match decoder_data {
            Some(d) => d,
            None => return SeekOutcome::Fallback,
        };
        if !decoder_data.seek(position) {
            return SeekOutcome::Fallback;
        }
        // 沿用实际输出流采样率，与复用的 DecoderData 重采样器目标一致
        let shared = audio_engine_core::shared::Shared::new(output_sample_rate, output_channels);
        shared.set_normalization_enabled(normalization_enabled);
        shared.set_normalization_gain(normalization_gain);
        equalizer
            .lock()
            .set_output_format(output_sample_rate, output_channels);
        equalizer.lock().reset_state();
        tempo
            .lock()
            .set_output_format(output_sample_rate, output_channels);
        tempo.lock().reset();
        let handle = match audio_engine_core::decoder::resume_decode(
            decoder_data,
            std::sync::Arc::clone(&shared),
            equalizer,
            tempo,
        ) {
            Ok(handle) => handle,
            Err(_) => return SeekOutcome::Fallback,
        };
        SeekOutcome::Resumed { shared, handle }
    })
    .await
    .map_err(|e| ApiError::internal(format!("Seek task join error: {e}")))?;

    match outcome {
        SeekOutcome::Resumed { shared, handle } => {
            let mut player = state.player.lock();
            let committed = player
                .commit_seeked(token, position, shared, handle, None)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            if committed {
                state.note_position(position);
            }
            Ok(Json(PlayerResponse::ok(json!({
                "status": if committed { "seeked" } else { "superseded" },
                "position": position,
            }))))
        }
        SeekOutcome::Fallback => {
            if let Some(source) = current_source {
                // 回退到重新 load
                let load_req = LoadRequest {
                    source,
                    auto_play: Some(was_playing),
                    meta: None,
                };
                let load_query = LoadQuery {};
                load_handler(State(state), Query(load_query), Json(load_req)).await
            } else {
                Ok(Json(PlayerResponse::ok(json!({
                    "status": "fallback_failed",
                    "position": position,
                }))))
            }
        }
    }
}
