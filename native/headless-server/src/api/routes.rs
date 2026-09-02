//! REST API 控制器
//!
//! 基于 Axum 0.8 的路由定义，提供播放控制、状态查询、扫描和 WebSocket 端点。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{header, Request},
    middleware::{self, Next},
    response::IntoResponse,
    Json, Router,
};
use futures_util::sink::SinkExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};

use crate::error::ApiError;
use crate::state::{AppState, PlayerSnapshot};

/// 查询参数：加载音轨所需的可选 cancel_handle_id
#[derive(Debug, Deserialize)]
pub struct LoadQuery {
    #[serde(rename = "cancel_handle_id")]
    cancel_handle_id: Option<String>,
}

/// 扫描查询参数
#[derive(Debug, Deserialize)]
pub struct ScanQuery {
    path: Option<String>,
}

/// WebSocket 查询参数
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    token: Option<String>,
}

/// 播放器控制响应
#[derive(serde::Serialize)]
pub struct PlayerResponse {
    pub success: bool,
    pub data: Option<Value>,
    pub error: Option<ApiError>,
}

/// 在独立 OS 线程中运行阻塞操作，彻底与 Tokio 运行时上下文隔离。
/// 避免 C/C++ FFI（Diretta）与内部包含单线程 Runtime 的组件（如 HttpAudioSource）
/// 在 Tokio 工作线程中被 Drop 时触发 "Cannot drop a runtime in a context where blocking is not allowed"。
pub async fn spawn_isolated_blocking<F, T>(name: &'static str, f: F) -> Result<T, String>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let res = f();
            let _ = tx.send(res);
        })
        .map_err(|e| format!("Failed to spawn OS thread {name}: {e}"))?;
    rx.await
        .map_err(|e| format!("OS thread {name} panicked or dropped sender: {e}"))
}

impl PlayerResponse {
    pub fn ok(data: Value) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn err(error: ApiError) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(error),
        }
    }
}

/// 音量控制请求体
#[derive(Debug, Deserialize)]
pub struct VolumeRequest {
    volume: f64,
}

/// seek 请求体
#[derive(Debug, Deserialize)]
pub struct SeekRequest {
    position_secs: f64,
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

/// 构建带 CORS、静态资源托管、SPA 回退和 Token 校验的完整 Router
pub fn build_router(state: AppState) -> Router {
    let cors = build_cors_layer(&state);

    // 受 Token 保护的控制路由
    let protected = Router::new()
        .route("/api/v1/player/play", axum::routing::post(play_handler))
        .route("/api/v1/player/pause", axum::routing::post(pause_handler))
        .route("/api/v1/player/stop", axum::routing::post(stop_handler))
        .route("/api/v1/player/volume", axum::routing::post(volume_handler))
        .route("/api/v1/player/load", axum::routing::post(load_handler))
        .route("/api/v1/player/seek", axum::routing::post(seek_handler))
        // 媒体库操作
        .route(
            "/api/v1/library/tracks",
            axum::routing::get(library_tracks_handler),
        )
        .route(
            "/api/v1/library/albums",
            axum::routing::get(library_albums_handler),
        )
        .route(
            "/api/v1/library/artists",
            axum::routing::get(library_artists_handler),
        )
        .route(
            "/api/v1/library/albums/{name}/tracks",
            axum::routing::get(library_album_tracks_handler),
        )
        .route(
            "/api/v1/library/artists/{name}/tracks",
            axum::routing::get(library_artist_tracks_handler),
        )
        .route(
            "/api/v1/library/scan_dirs",
            axum::routing::get(library_scan_dirs_get_handler)
                .post(library_scan_dirs_add_handler)
                .delete(library_scan_dirs_remove_handler),
        )
        .route(
            "/api/v1/library/scan",
            axum::routing::post(library_scan_handler),
        )
        .route(
            "/api/v1/library/cancel_scan",
            axum::routing::post(library_cancel_scan_handler),
        )
        .route(
            "/api/v1/library/scan/status",
            axum::routing::get(library_scan_status_handler),
        )
        // 歌单操作
        .route(
            "/api/v1/playlist/list",
            axum::routing::get(playlist_list_handler),
        )
        .route(
            "/api/v1/playlist/all",
            axum::routing::get(playlist_list_handler),
        )
        .route(
            "/api/v1/playlist/create",
            axum::routing::post(playlist_create_handler),
        )
        .route(
            "/api/v1/playlist/{id}",
            axum::routing::get(playlist_get_handler)
                .put(playlist_update_handler)
                .delete(playlist_delete_handler),
        )
        .route(
            "/api/v1/playlist/{id}/update",
            axum::routing::post(playlist_update_handler),
        )
        .route(
            "/api/v1/playlist/{id}/tracks",
            axum::routing::post(playlist_add_tracks_handler).delete(playlist_remove_tracks_handler),
        )
        // 用户配置
        .route(
            "/api/v1/config/all",
            axum::routing::get(config_get_all_handler),
        )
        .route(
            "/api/v1/config/set",
            axum::routing::post(config_set_handler),
        )
        .route(
            "/api/v1/config/reset",
            axum::routing::post(config_reset_handler),
        )
        .route(
            "/api/v1/config/{key}",
            axum::routing::get(config_get_handler),
        )
        // 播放统计与历史
        .route(
            "/api/v1/stats/record",
            axum::routing::post(stats_record_handler),
        )
        .route(
            "/api/v1/stats/history",
            axum::routing::get(stats_history_handler),
        )
        .route(
            "/api/v1/stats/summary",
            axum::routing::get(stats_summary_handler),
        )
        // 在线音源统一调用接口
        .route(
            "/api/v1/proxy/apis/call",
            axum::routing::post(apis_call_handler),
        )
        // Diretta Audio-over-IP 控制
        .route(
            "/api/v1/diretta/scan",
            axum::routing::get(diretta_scan_handler),
        )
        .route(
            "/api/v1/diretta/status",
            axum::routing::get(diretta_status_handler),
        )
        .route(
            "/api/v1/diretta/select",
            axum::routing::post(diretta_select_handler),
        )
        .route(
            "/api/v1/diretta/target_info",
            axum::routing::post(diretta_target_info_handler),
        )
        // Diretta Source Direct Gapless 预加载与边界切换 (纯内存流式无缝切换)
        .route(
            "/api/v1/player/direct/stage_next",
            axum::routing::post(direct_stage_next_handler),
        )
        .route(
            "/api/v1/player/direct/cancel_next",
            axum::routing::post(direct_cancel_next_handler),
        )
        .route(
            "/api/v1/player/direct/commit_boundary",
            axum::routing::post(direct_commit_boundary_handler),
        )
        // 服务端目录文件浏览（Web UI 选择曲库目录）
        .route("/api/v1/fs/browse", axum::routing::get(fs_browse_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            token_middleware,
        ));

    let mut router = Router::new()
        // 健康检查（无需鉴权）
        .route("/api/status", axum::routing::get(status_handler))
        // 扫描探测（无需鉴权）
        .route("/api/v1/scan/probe", axum::routing::get(scan_probe_handler))
        // 封面与歌词服务（无需鉴权，支持缓存与静态访问）
        .route(
            "/api/v1/covers/file",
            axum::routing::get(cover_file_handler),
        )
        .route("/api/v1/covers/{id}", axum::routing::get(cover_get_handler))
        .route(
            "/api/v1/lyrics/file",
            axum::routing::get(lyric_file_handler),
        )
        // 音频串流代理转发（支持 Range 分片与防盗链）
        .route(
            "/api/v1/proxy/stream",
            axum::routing::get(crate::api::online_apis::stream_proxy_handler),
        )
        // 远程图片代理转发（解决客户端无法直连 static.qobuz.com / resources.tidal.com）
        .route(
            "/api/proxy/image",
            axum::routing::get(crate::api::online_apis::image_proxy_handler),
        )
        .route(
            "/api/v1/proxy/image",
            axum::routing::get(crate::api::online_apis::image_proxy_handler),
        )
        // WebSocket（单独鉴权）
        .route("/ws", axum::routing::get(ws_handler))
        // 受保护路由
        .merge(protected);

    // 静态 Web UI 托管（带 SPA 404 fallback 到 index.html）
    let web_root_candidate = state.config.web_root.clone().or_else(|| {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()));
        let mut candidates = vec![
            PathBuf::from("/opt/splayer-headless/web"),
            PathBuf::from("./web"),
            PathBuf::from("./out/renderer"),
        ];
        if let Some(dir) = exe_dir {
            candidates.push(dir.join("web"));
        }
        candidates
            .into_iter()
            .find(|p| p.exists() && p.join("index.html").exists())
    });

    if let Some(web_root) = web_root_candidate {
        if web_root.exists() {
            let index_file = web_root.join("index.html");
            tracing::info!(?web_root, ?index_file, "Mounting static Web UI");
            let serve_dir = ServeDir::new(&web_root).fallback(ServeFile::new(index_file));
            router = router.fallback_service(serve_dir);
        }
    }

    let ncm_client = ncm_api_rs::ApiClient::new(None);
    let ncm_router = ncm_api_rs::server::build_app(ncm_client);

    router
        .layer(CompressionLayer::new())
        .layer(cors)
        .with_state(state)
        .nest("/api/ncm", ncm_router)
}

/// 构建 CORS 层
fn build_cors_layer(state: &AppState) -> CorsLayer {
    let origins = state.config.cors_origins();
    let allow_origin = if origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(origins.into_iter().map(|o| o.parse().unwrap()))
    };
    CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, header::ACCEPT])
        .max_age(Duration::from_secs(86400))
}

/// Token 校验中间件
async fn token_middleware(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Result<axum::response::Response, ApiError> {
    if let Some(expected_token) = state.config.api_token.as_ref() {
        let auth_header = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");

        let valid = auth_header
            .strip_prefix("Bearer ")
            .map(|t| t == expected_token.as_str())
            .unwrap_or(false);

        if !valid {
            return Err(ApiError::unauthorized());
        }
    }
    Ok(next.run(req).await)
}

// -------------------------------------------------------------------
// REST 处理函数
// -------------------------------------------------------------------

/// 健康/状态查询
async fn status_handler(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let snapshot: PlayerSnapshot = state.snapshot();
    Ok(Json(json!({
        "state": format!("{:?}", snapshot.state),
        "position": snapshot.position,
        "duration": snapshot.duration,
        "volume": snapshot.volume,
        "speed": snapshot.speed,
        "is_finished": snapshot.is_finished,
        "current_source": snapshot.current_source,
    })))
}

/// 播放
async fn play_handler(State(state): State<AppState>) -> Result<Json<PlayerResponse>, ApiError> {
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
async fn pause_handler(State(state): State<AppState>) -> Json<PlayerResponse> {
    let _ = spawn_isolated_blocking("player-pause-worker", move || {
        let mut player = state.player.lock();
        let _ = player.pause();
    })
    .await;
    Json(PlayerResponse::ok(json!({ "status": "paused" })))
}

/// 停止
async fn stop_handler(State(state): State<AppState>) -> Json<PlayerResponse> {
    let _ = spawn_isolated_blocking("player-stop-worker", move || {
        let mut player = state.player.lock();
        player.stop();
    })
    .await;
    Json(PlayerResponse::ok(json!({ "status": "stopped" })))
}

/// 音量控制
async fn volume_handler(
    State(state): State<AppState>,
    Json(payload): Json<VolumeRequest>,
) -> Json<PlayerResponse> {
    let volume = (payload.volume as f32).clamp(0.0, 1.0);
    {
        let mut player = state.player.lock();
        let _ = player.set_volume(volume);
    }
    Json(PlayerResponse::ok(json!({ "volume": volume })))
}

/// 获取流媒体音频 RAM 内存缓冲目录（优先 Linux /dev/shm 内存文件系统，彻底规避磁盘写入磨损与物理磁盘空间占用）
fn get_stream_cache_dir() -> std::path::PathBuf {
    use std::path::PathBuf;

    let candidate_dirs = [
        PathBuf::from("/dev/shm/splayer-headless-ram/streams"),
        std::env::temp_dir().join("splayer-stream-cache"),
        PathBuf::from("/opt/splayer-headless/data/cache/streams"),
        PathBuf::from("data/cache/streams"),
    ];

    for dir in &candidate_dirs {
        if std::fs::create_dir_all(dir).is_ok() {
            let test_file = dir.join(".write_test");
            if std::fs::write(&test_file, b"ok").is_ok() {
                let _ = std::fs::remove_file(test_file);
                return dir.clone();
            }
        }
    }

    std::env::temp_dir().join("splayer-stream-cache")
}

/// 自动清理过期的流媒体内存缓存，只保留当前播放曲目与下一首预载曲目（最多保留 2 首，其余立刻从内存释放）
fn clean_old_stream_cache(cache_dir: &std::path::Path) {
    if let Ok(entries) = std::fs::read_dir(cache_dir) {
        let mut files: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if path.to_string_lossy().ends_with(".part") {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                if let Ok(meta) = entry.metadata() {
                    let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                    files.push((path, mtime));
                }
            }
        }
        // 纯内存模式：严格限制最多保留 2 首，防止物理 RAM 溢出
        if files.len() > 2 {
            files.sort_by_key(|(_, mtime)| *mtime);
            let to_remove = files.len() - 2;
            for (p, _) in files.into_iter().take(to_remove) {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

/// 将远端 HTTP(S) 音频流下载并固化到本地 RAM 缓冲文件中，以便 Diretta Source Direct 模式进行精确解码与传输
fn materialize_direct_input(url: &str) -> anyhow::Result<String> {
    use std::fs::{self, File};

    let cache_dir = get_stream_cache_dir();
    let _ = fs::create_dir_all(&cache_dir);
    clean_old_stream_cache(&cache_dir);

    let hash = format!("{:x}", md5::compute(url.as_bytes()));
    let mut ext = if url.contains(".flac") {
        "flac"
    } else if url.contains(".mp3") {
        "mp3"
    } else if url.contains(".m4a") || url.contains(".aac") {
        "m4a"
    } else if url.contains(".wav") {
        "wav"
    } else if url.contains(".dsf") {
        "dsf"
    } else if url.contains(".dff") {
        "dff"
    } else {
        ""
    };

    if !ext.is_empty() {
        let target_file = cache_dir.join(format!("{}.{}", hash, ext));
        if target_file.exists() && fs::metadata(&target_file).map(|m| m.len() > 0).unwrap_or(false) {
            return Ok(target_file.to_string_lossy().to_string());
        }
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    let mut response = client
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .header("Accept", "*/*")
        .header("Accept-Encoding", "identity")
        .send()?;

    if !response.status().is_success() {
        anyhow::bail!("下载在线流媒体音频失败: HTTP {}", response.status());
    }

    if ext.is_empty() {
        if let Some(ct) = response.headers().get("content-type").and_then(|v| v.to_str().ok()) {
            let ct = ct.to_lowercase();
            if ct.contains("flac") {
                ext = "flac";
            } else if ct.contains("mpeg") || ct.contains("mp3") {
                ext = "mp3";
            } else if ct.contains("mp4") || ct.contains("m4a") || ct.contains("aac") {
                ext = "m4a";
            } else if ct.contains("wav") {
                ext = "wav";
            } else if ct.contains("dsf") {
                ext = "dsf";
            }
        }
    }
    if ext.is_empty() {
        ext = "audio";
    }

    let target_file = cache_dir.join(format!("{}.{}", hash, ext));
    if target_file.exists() && fs::metadata(&target_file).map(|m| m.len() > 0).unwrap_or(false) {
        return Ok(target_file.to_string_lossy().to_string());
    }

    let part_file = cache_dir.join(format!("{}.{}.part", hash, ext));
    let mut file = File::create(&part_file)?;
    std::io::copy(&mut response, &mut file)?;
    file.sync_all()?;
    drop(file);

    fs::rename(&part_file, &target_file)?;
    Ok(target_file.to_string_lossy().to_string())
}

const LOAD_SUPERSEDED_REASON: &str = "Load superseded by a newer request";

/// 加载音轨（完整三段式异步 IO 闭环）
async fn load_handler(
    State(state): State<AppState>,
    Query(query): Query<LoadQuery>,
    Json(payload): Json<LoadRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let _ = query.cancel_handle_id;
    let auto_play = payload.auto_play.unwrap_or(true);
    let source = payload.source;

    // 若为后台冷启动恢复请求（auto_play: false），且当前播放器已处于活跃播放或暂停状态，直接返回现有状态，不打断后台音频流
    if !auto_play {
        let snap = state.snapshot();
        if matches!(snap.state, audio_engine_core::PlayerState::Playing | audio_engine_core::PlayerState::Paused) {
            return Ok(Json(PlayerResponse::ok(json!({
                "status": "active",
                "source": source,
                "duration": snap.duration,
            }))));
        }
    }

    let handle = audio_engine_core::HttpCancelHandle::new();

    let (
        old_threads,
        token,
        load_token,
        cover_dir,
        normalization_enabled,
        device_name,
        direct_selector,
        output_generation,
        failure_callback,
        equalizer,
        tempo,
    ) = {
        let mut player = state.player.lock();
        let device_name = player.selected_device().map(String::from);
        let direct_selector = device_name
            .as_deref()
            .filter(|v| audio_engine_core::diretta::selector_target(v).is_some())
            .map(String::from);
        if direct_selector.is_some() {
            if let Err(e) = player.validate_direct_entry() {
                return Err(ApiError::bad_request(e.to_string()));
            }
        }
        let (old_threads, token) = player.take_for_async_load(handle.clone());
        let output_generation = player.reserve_output_generation();
        let failure_callback = player.make_failure_callback(output_generation);
        (
            old_threads,
            token,
            player.load_token_handle(),
            player.cover_cache_dir().map(String::from),
            player.is_normalization_enabled(),
            device_name,
            direct_selector,
            output_generation,
            failure_callback,
            player.equalizer_handle(),
            player.tempo_handle(),
        )
    };

    let mut source_for_decoder = source.clone();
    if source.starts_with("cue://") {
        let conn = state.db.lock();
        if let Ok(Some(track)) = crate::db::get_track_by_path(&conn, &source) {
            if let Some(audio_path) = track.cue_audio_path {
                let start_sec = track.cue_start_ms.unwrap_or(0) as f64 / 1000.0;
                let dur_sec = track.duration as f64 / 1000.0;
                let track_num = track.track.unwrap_or(1);
                source_for_decoder = format!("{}|{:.3}|{:.3}|{}", audio_path, start_sec, dur_sec, track_num);
                tracing::info!("CUE virtual track resolved to physical source: {}", source_for_decoder);
            }
        }
    } else if let Some(ref meta) = payload.meta {
        if let Some(start_ms) = meta.cue_start_ms {
            let audio_path = meta.cue_audio_path.as_deref().unwrap_or(&source);
            let start_sec = start_ms as f64 / 1000.0;
            let dur_sec = meta.duration.unwrap_or(0) as f64 / 1000.0;
            let track_num = meta.track.unwrap_or(1);
            source_for_decoder = format!("{}|{:.3}|{:.3}|{}", audio_path, start_sec, dur_sec, track_num);
            tracing::info!("CUE metadata resolved to physical source: {}", source_for_decoder);
        }
    }

    if let Some(selector) = direct_selector {
        let source_for_direct = source_for_decoder.clone();
        let load_token_for_direct = Arc::clone(&load_token);
        let result = spawn_isolated_blocking("player-direct-load-worker", move || {
            let replacing_direct_playback = old_threads.direct_playback.is_some();
            if let Some(h) = old_threads.join_aux() {
                let _ = h.join();
            }
            if replacing_direct_playback {
                std::thread::sleep(std::time::Duration::from_millis(800));
            }
            let physical_source = if source_for_direct.starts_with("http://") || source_for_direct.starts_with("https://") {
                materialize_direct_input(&source_for_direct)?
            } else {
                source_for_direct
            };
            let mut metadata = audio_engine_core::decoder::probe_metadata(
                &physical_source,
                cover_dir.as_deref(),
                handle,
            )?;
            if load_token_for_direct.load(std::sync::atomic::Ordering::Acquire) != token {
                anyhow::bail!(LOAD_SUPERSEDED_REASON);
            }
            let playback = audio_engine_core::direct_runtime::DirectPlayback::open_local(
                &selector,
                &physical_source,
                metadata.duration_secs,
                0.0,
                auto_play,
            )?;
            match playback.format() {
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
            metadata.duration_secs = playback.duration();
            if load_token_for_direct.load(std::sync::atomic::Ordering::Acquire) != token {
                anyhow::bail!(LOAD_SUPERSEDED_REASON);
            }
            Ok::<_, anyhow::Error>((metadata, playback))
        })
        .await
        .map_err(|e| ApiError::internal(format!("Direct load task join error: {e}")))?;

        let (metadata, playback) = match result {
            Ok(val) => val,
            Err(err) => {
                let mut player = state.player.lock();
                if !player.is_load_token_current(token) {
                    return Ok(Json(PlayerResponse::ok(json!({
                        "status": "superseded",
                        "source": source,
                    }))));
                }
                player.stop();
                return Err(ApiError::bad_request(format!("{err:#}")));
            }
        };

        let committed_meta = {
            let mut player = state.player.lock();
            player
                .commit_direct_loaded(token, &source, auto_play, metadata, playback)
                .map_err(|e| ApiError::internal(e.to_string()))?
        };

        return match committed_meta {
            Some(meta) => Ok(Json(PlayerResponse::ok(json!({
                "status": if auto_play { "playing" } else { "paused" },
                "source": source,
                "title": meta.title,
                "artist": meta.artist,
                "album": meta.album,
                "duration": meta.duration_secs,
                "sample_rate": meta.sample_rate,
                "original_sample_rate": meta.original_sample_rate,
                "channels": meta.channels,
                "bits_per_sample": meta.bits_per_sample,
                "bit_rate": meta.bit_rate,
                "codec": meta.codec,
                "cover": meta.cover,
                "has_cover": meta.cover_raw.is_some() || meta.cover.is_some(),
                "has_embedded_lyric": meta.embedded_lyric.is_some(),
            })))),
            None => Ok(Json(PlayerResponse::ok(json!({
                "status": "superseded",
                "source": source,
            })))),
        };
    }

    let result = spawn_isolated_blocking("player-load-worker", move || {
        if let Some(h) = old_threads.join_aux() {
            let _ = h.join();
        }
        let prepared = audio_engine_core::decoder::prepare_decode(
            &source_for_decoder,
            cover_dir.as_deref(),
            handle,
        )?;
        if load_token.load(std::sync::atomic::Ordering::Acquire) != token {
            anyhow::bail!(LOAD_SUPERSEDED_REASON);
        }
        // 输出采样率协商：音源原始采样率被设备支持时按精确采样率打开
        let output = audio_engine_core::audio_output::AudioOutput::new(
            device_name.as_deref(),
            Some(prepared.original_sample_rate()),
            output_generation,
            failure_callback,
        )?;
        let shared =
            audio_engine_core::shared::Shared::new(output.sample_rate(), output.channels());
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
        Ok::<_, anyhow::Error>((metadata, decode_handle, shared, output, cancel))
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
        Some(meta) => Ok(Json(PlayerResponse::ok(json!({
            "status": if auto_play { "playing" } else { "paused" },
            "source": source,
            "title": meta.title,
            "artist": meta.artist,
            "album": meta.album,
            "duration": meta.duration_secs,
            "sample_rate": meta.sample_rate,
            "original_sample_rate": meta.original_sample_rate,
            "channels": meta.channels,
            "bits_per_sample": meta.bits_per_sample,
            "bit_rate": meta.bit_rate,
            "codec": meta.codec,
            "cover": meta.cover,
            "has_cover": meta.cover_raw.is_some() || meta.cover.is_some(),
            "has_embedded_lyric": meta.embedded_lyric.is_some(),
        })))),
        None => Ok(Json(PlayerResponse::ok(json!({
            "status": "superseded",
            "source": source,
        })))),
    }
}

enum SeekOutcome {
    Resumed {
        shared: std::sync::Arc<audio_engine_core::shared::Shared>,
        handle: std::thread::JoinHandle<audio_engine_core::decoder::DecoderData>,
    },
    Fallback,
}

/// seek（三段式异步恢复）
async fn seek_handler(
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
                let load_query = LoadQuery {
                    cancel_handle_id: None,
                };
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

/// Direct 预加载请求体
#[derive(Debug, Deserialize)]
pub struct DirectStageRequest {
    pub source: String,
    pub duration_secs: Option<f64>,
    pub generation: Option<u64>,
}

/// Direct 提交切歌边界请求体
#[derive(Debug, Deserialize)]
pub struct DirectCommitBoundaryRequest {
    pub source: String,
    pub duration_secs: f64,
}

/// Diretta 预加载下一曲（支持远程流媒体预先下载至 RAM）
async fn direct_stage_next_handler(
    State(state): State<AppState>,
    Json(payload): Json<DirectStageRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let source = payload.source;
    let duration = payload.duration_secs.unwrap_or(0.0);
    let generation = payload.generation.unwrap_or(0);

    let stage_handle = state.player.lock().direct_stage_handle();

    let Some(handle) = stage_handle else {
        return Ok(Json(PlayerResponse::ok(json!({
            "staged": false,
            "reason": "Direct runtime inactive",
        }))));
    };

    let physical_source = if source.starts_with("http://") || source.starts_with("https://") {
        let src_clone = source.clone();
        spawn_isolated_blocking("direct-stage-preload", move || {
            materialize_direct_input(&src_clone)
        })
        .await
        .map_err(|e| ApiError::internal(format!("Stage preload error: {e}")))?
        .map_err(|e| ApiError::bad_request(format!("Failed to preload stream to RAM: {e}")))?
    } else {
        source.clone()
    };

    let result = spawn_isolated_blocking("direct-stage-next-worker", move || {
        handle.stage_local(&physical_source, duration, generation)
    })
    .await
    .map_err(|e| ApiError::internal(format!("Stage next worker error: {e}")))?;

    match result {
        Ok(()) => Ok(Json(PlayerResponse::ok(json!({
            "staged": true,
            "source": source,
            "generation": generation,
        })))),
        Err(err) => Err(ApiError::bad_request(format!("Direct stage failed: {err}"))),
    }
}

/// 取消已暂存的 Direct 下一曲预加载
async fn direct_cancel_next_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    if let Some(handle) = state.player.lock().direct_stage_handle() {
        handle.cancel();
    }
    Ok(Json(PlayerResponse::ok(json!({ "cancelled": true }))))
}

/// 提交已完成的 Direct Gapless 边界切换
async fn direct_commit_boundary_handler(
    State(state): State<AppState>,
    Json(payload): Json<DirectCommitBoundaryRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    state
        .player
        .lock()
        .commit_direct_gapless_boundary(&payload.source, payload.duration_secs)
        .map_err(|e| ApiError::bad_request(format!("Commit boundary failed: {e}")))?;
    Ok(Json(PlayerResponse::ok(json!({
        "committed": true,
        "source": payload.source,
        "duration": payload.duration_secs,
    }))))
}

#[derive(Debug, Deserialize)]
pub struct ScanRequest {
    pub incremental: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ScanDirRequest {
    pub path: String,
}

/// 单文件快速探测
async fn scan_probe_handler(
    State(state): State<AppState>,
    Query(query): Query<ScanQuery>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let path = query
        .path
        .ok_or_else(|| ApiError::bad_request("Missing path parameter"))?;
    let cover_dir = state.config.resolved_cover_cache_dir();
    let cover_dir_str = cover_dir.to_str();

    let scanned = audio_engine_core::scanner::probe_fast(&path, cover_dir_str)
        .ok_or_else(|| ApiError::not_found(&format!("Audio file at {}", path)))?;

    Ok(Json(PlayerResponse::ok(json!({
        "path": scanned.path,
        "title": scanned.title,
        "artist": scanned.artist,
        "album": scanned.album,
        "track": scanned.track,
        "duration": scanned.duration,
        "codec": scanned.codec,
        "sample_rate": scanned.sample_rate,
        "bit_rate": scanned.bit_rate,
        "channels": scanned.channels,
        "bits_per_sample": scanned.bits_per_sample,
        "cover": scanned.cover,
    }))))
}

/// 获取音乐库全部曲目
async fn library_tracks_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let tracks = crate::db::get_all_tracks(&conn)?;
    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(tracks).unwrap_or_default(),
    )))
}

/// 获取音乐库全部专辑聚合
async fn library_albums_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let albums = crate::db::get_album_list(&conn)?;
    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(albums).unwrap_or_default(),
    )))
}

/// 获取音乐库全部歌手聚合
async fn library_artists_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let artists = crate::db::get_artist_list(&conn)?;
    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(artists).unwrap_or_default(),
    )))
}

/// 获取指定专辑下的所有曲目
async fn library_album_tracks_handler(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let tracks = crate::db::get_tracks_by_album(&conn, &name)?;
    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(tracks).unwrap_or_default(),
    )))
}

/// 获取指定歌手下的所有曲目
async fn library_artist_tracks_handler(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let tracks = crate::db::get_tracks_by_artist(&conn, &name)?;
    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(tracks).unwrap_or_default(),
    )))
}

/// 获取已配置的扫描目录列表
async fn library_scan_dirs_get_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let dirs = crate::db::get_scan_dirs(&conn)?;
    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(dirs).unwrap_or_default(),
    )))
}

/// 添加扫描目录
async fn library_scan_dirs_add_handler(
    State(state): State<AppState>,
    Json(payload): Json<ScanDirRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    crate::db::add_scan_dir(&conn, &payload.path)?;
    Ok(Json(PlayerResponse::ok(json!({ "added": payload.path }))))
}

/// 删除扫描目录
async fn library_scan_dirs_remove_handler(
    State(state): State<AppState>,
    Json(payload): Json<ScanDirRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    crate::db::remove_scan_dir(&conn, &payload.path)?;
    Ok(Json(PlayerResponse::ok(json!({ "removed": payload.path }))))
}

/// 启动后台扫描
async fn library_scan_handler(
    State(state): State<AppState>,
    Json(payload): Json<Option<ScanRequest>>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let incremental = payload.and_then(|p| p.incremental).unwrap_or(true);

    if state
        .is_scanning
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return Ok(Json(PlayerResponse::ok(json!({
            "status": "already_scanning",
        }))));
    }

    state
        .scan_cancel
        .store(false, std::sync::atomic::Ordering::SeqCst);

    let state_clone = state.clone();
    tokio::task::spawn_blocking(move || {
        let dirs = {
            let conn = state_clone.db.lock();
            crate::db::get_scan_dirs(&conn).unwrap_or_default()
        };

        if dirs.is_empty() {
            state_clone
                .is_scanning
                .store(false, std::sync::atomic::Ordering::SeqCst);
            let _ = state_clone.scan_tx.send(crate::state::ScanProgressMessage {
                r#type: "done".to_string(),
                phase: "done".to_string(),
                scanned: 0,
                total: 0,
                current: None,
            });
            return;
        }

        let file_records = if incremental {
            let conn = state_clone.db.lock();
            crate::db::get_file_records(&conn).ok()
        } else {
            None
        };

        let cover_dir = state_clone.config.resolved_cover_cache_dir();
        let cover_dir_str = cover_dir.to_str();
        let state_for_cb = state_clone.clone();

        audio_engine_core::scanner::scan_directories(
            &dirs,
            cover_dir_str,
            file_records.as_deref(),
            &state_clone.scan_cancel,
            &move |event| match event {
                audio_engine_core::scanner::ScanEvent::Progress {
                    scanned,
                    total,
                    current,
                    tracks,
                } => {
                    if !tracks.is_empty() {
                        let mut conn = state_for_cb.db.lock();
                        let _ = crate::db::upsert_scanned_tracks(&mut conn, &tracks);
                    }
                    let _ = state_for_cb
                        .scan_tx
                        .send(crate::state::ScanProgressMessage {
                            r#type: "progress".to_string(),
                            phase: "scanning".to_string(),
                            scanned,
                            total,
                            current,
                        });
                }
                audio_engine_core::scanner::ScanEvent::Done {
                    scanned,
                    total,
                    removed_paths,
                    cue_files,
                    iso_files,
                    ..
                } => {
                    {
                        let mut conn = state_for_cb.db.lock();
                        if !removed_paths.is_empty() {
                            let _ = crate::db::delete_tracks_by_paths(&mut conn, &removed_paths);
                        }
                        if !cue_files.is_empty() {
                            let cover_cache_dir = state_for_cb.config.resolved_cover_cache_dir();
                            let _ = crate::db::sync_cue_tracks(&mut conn, &cue_files, Some(&cover_cache_dir));
                        }
                        if !iso_files.is_empty() {
                            let cover_cache_dir = state_for_cb.config.resolved_cover_cache_dir();
                            let _ = crate::db::sync_sacd_tracks(&mut conn, &iso_files, Some(&cover_cache_dir));
                        }
                    }
                    let _ = state_for_cb
                        .scan_tx
                        .send(crate::state::ScanProgressMessage {
                            r#type: "done".to_string(),
                            phase: "done".to_string(),
                            scanned,
                            total,
                            current: None,
                        });
                }
            },
        );

        state_clone
            .is_scanning
            .store(false, std::sync::atomic::Ordering::SeqCst);
    });

    Ok(Json(PlayerResponse::ok(json!({
        "status": "scan_started",
        "incremental": incremental,
    }))))
}

/// 取消后台扫描
async fn library_cancel_scan_handler(State(state): State<AppState>) -> Json<PlayerResponse> {
    state
        .scan_cancel
        .store(true, std::sync::atomic::Ordering::SeqCst);
    state
        .is_scanning
        .store(false, std::sync::atomic::Ordering::SeqCst);
    Json(PlayerResponse::ok(json!({ "status": "scan_cancelled" })))
}

/// 获取当前扫描状态
async fn library_scan_status_handler(State(state): State<AppState>) -> Json<PlayerResponse> {
    let is_scanning = state.is_scanning.load(std::sync::atomic::Ordering::SeqCst);
    Json(PlayerResponse::ok(json!({ "is_scanning": is_scanning })))
}

// -------------------------------------------------------------------
// 歌单 Request Payload 与 Handlers
// -------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreatePlaylistRequest {
    pub id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub cover: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePlaylistRequest {
    pub title: Option<String>,
    pub description: Option<String>,
    pub cover: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PlaylistTracksRequest {
    pub track_ids: Option<Vec<String>>,
    pub tracks: Option<Vec<Value>>,
}

/// 获取歌单列表
async fn playlist_list_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let playlists = crate::db::get_all_playlists(&conn)?;
    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(playlists).unwrap_or_default(),
    )))
}

/// 获取单个歌单详情
async fn playlist_get_handler(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let detail = crate::db::get_playlist_detail(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(&format!("Playlist {}", id)))?;
    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(detail).unwrap_or_default(),
    )))
}

/// 创建歌单
async fn playlist_create_handler(
    State(state): State<AppState>,
    Json(payload): Json<CreatePlaylistRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let id = payload.id.unwrap_or_else(|| {
        format!(
            "pl-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        )
    });

    let conn = state.db.lock();
    let playlist = crate::db::create_playlist(
        &conn,
        &id,
        &payload.title,
        payload.description.as_deref(),
        payload.cover.as_deref(),
    )?;

    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(playlist).unwrap_or_default(),
    )))
}

/// 更新歌单
async fn playlist_update_handler(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(payload): Json<UpdatePlaylistRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    crate::db::update_playlist(
        &conn,
        &id,
        payload.title.as_deref(),
        payload.description.as_deref(),
        payload.cover.as_deref(),
    )?;
    Ok(Json(PlayerResponse::ok(json!({ "updated": id }))))
}

/// 删除歌单
async fn playlist_delete_handler(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    crate::db::delete_playlist(&conn, &id)?;
    Ok(Json(PlayerResponse::ok(json!({ "deleted": id }))))
}

/// 向歌单添加曲目
async fn playlist_add_tracks_handler(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(payload): Json<PlaylistTracksRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let mut track_ids = payload.track_ids.unwrap_or_default();
    if track_ids.is_empty() {
        if let Some(tracks) = payload.tracks {
            for t in tracks {
                if let Some(tid) = t.get("id").and_then(|v| v.as_str()) {
                    track_ids.push(tid.to_string());
                }
            }
        }
    }

    let mut conn = state.db.lock();
    crate::db::add_playlist_tracks(&mut conn, &id, &track_ids)?;
    Ok(Json(PlayerResponse::ok(json!({
        "playlist_id": id,
        "added_count": track_ids.len()
    }))))
}

/// 从歌单移除曲目
async fn playlist_remove_tracks_handler(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(payload): Json<PlaylistTracksRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let mut track_ids = payload.track_ids.unwrap_or_default();
    if track_ids.is_empty() {
        if let Some(tracks) = payload.tracks {
            for t in tracks {
                if let Some(tid) = t.get("id").and_then(|v| v.as_str()) {
                    track_ids.push(tid.to_string());
                }
            }
        }
    }

    let mut conn = state.db.lock();
    crate::db::remove_playlist_tracks(&mut conn, &id, &track_ids)?;
    Ok(Json(PlayerResponse::ok(json!({
        "playlist_id": id,
        "removed_count": track_ids.len()
    }))))
}

// -------------------------------------------------------------------
// 用户配置 Handlers
// -------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SetConfigRequest {
    pub key: Option<String>,
    pub value: Option<Value>,
    pub settings: Option<serde_json::Map<String, Value>>,
}

/// 获取全部用户设置
async fn config_get_all_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let settings = crate::db::get_all_settings(&conn)?;
    Ok(Json(PlayerResponse::ok(settings)))
}

/// 获取单个设置项
async fn config_get_handler(
    State(state): State<AppState>,
    axum::extract::Path(key): axum::extract::Path<String>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let val = crate::db::get_setting(&conn, &key)?.unwrap_or(Value::Null);
    Ok(Json(PlayerResponse::ok(val)))
}

/// 保存设置项（支持单个 key/value 或整个 settings 字典）
async fn config_set_handler(
    State(state): State<AppState>,
    Json(payload): Json<SetConfigRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    if let Some(settings) = &payload.settings {
        let mut conn = state.db.lock();
        crate::db::set_all_settings(&mut conn, settings)?;
    } else if let (Some(k), Some(v)) = (&payload.key, &payload.value) {
        let conn = state.db.lock();
        crate::db::set_setting(&conn, k, v)?;
    } else {
        return Err(ApiError::bad_request(
            "Missing key/value or settings object in body",
        ));
    }
    Ok(Json(PlayerResponse::ok(json!({ "status": "saved" }))))
}

/// 重置所有配置项
async fn config_reset_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    crate::db::reset_settings(&conn)?;
    Ok(Json(PlayerResponse::ok(json!({ "status": "reset" }))))
}

// -------------------------------------------------------------------
// 播放统计与历史 Handlers
// -------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RecordHistoryRequest {
    #[serde(rename = "trackId")]
    pub track_id: Option<String>,
    pub source: Option<String>,
    #[serde(rename = "startedAt")]
    pub started_at: Option<u64>,
    #[serde(rename = "listenedMs")]
    pub listened_ms: Option<u64>,
    pub track: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub limit: Option<u32>,
}

/// 记录播放历史
async fn stats_record_handler(
    State(state): State<AppState>,
    Json(payload): Json<RecordHistoryRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let track_id = payload
        .track_id
        .or_else(|| {
            payload
                .track
                .as_ref()
                .and_then(|t| t.get("id").and_then(|v| v.as_str()).map(String::from))
        })
        .unwrap_or_else(|| "unknown".to_string());

    let source = payload.source.unwrap_or_else(|| "local".to_string());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let started_at = payload.started_at.unwrap_or(now);
    let listened_ms = payload.listened_ms.unwrap_or(0);
    let track_json = serde_json::to_string(&payload.track.unwrap_or_default())
        .unwrap_or_else(|_| "{}".to_string());

    let conn = state.db.lock();
    crate::db::record_play_history(
        &conn,
        &track_id,
        &source,
        started_at,
        listened_ms,
        &track_json,
    )?;

    Ok(Json(PlayerResponse::ok(json!({ "recorded": true }))))
}

/// 查询最近播放历史
async fn stats_history_handler(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let limit = query.limit.unwrap_or(100);
    let conn = state.db.lock();
    let history = crate::db::get_play_history(&conn, limit)?;
    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(history).unwrap_or_default(),
    )))
}

/// 查询媒体库统计概览
async fn stats_summary_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let stats = crate::db::get_library_stats(&conn)?;
    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(stats).unwrap_or_default(),
    )))
}

// -------------------------------------------------------------------
// 封面与歌词服务 Handlers
// -------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct FilePathQuery {
    pub path: Option<String>,
}

/// 根据 ID 或缓存文件名获取封面流
async fn cover_get_handler(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    let cover_dir = state.config.resolved_cover_cache_dir();
    let possible_paths = [
        cover_dir.join(&id),
        cover_dir.join(format!("{}.jpg", id)),
        cover_dir.join(format!("{}.png", id)),
    ];

    for p in &possible_paths {
        if p.is_file() {
            if let Ok(bytes) = std::fs::read(p) {
                let content_type = if p.extension().map_or(false, |ext| ext == "png") {
                    "image/png"
                } else {
                    "image/jpeg"
                };
                return (
                    axum::http::StatusCode::OK,
                    [
                        (axum::http::header::CONTENT_TYPE, content_type),
                        (
                            axum::http::header::CACHE_CONTROL,
                            "public, max-age=31536000, immutable",
                        ),
                    ],
                    bytes,
                )
                    .into_response();
            }
        }
    }

    axum::http::StatusCode::NOT_FOUND.into_response()
}

/// 动态从本地音频文件提取内嵌封面流
async fn cover_file_handler(Query(query): Query<FilePathQuery>) -> axum::response::Response {
    let Some(path) = query.path else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };

    let p = std::path::Path::new(&path);
    if !p.is_file() {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }

    if let Ok(file) = std::fs::File::open(&path) {
        if let Ok(reader) = audio_engine_core::ffmpeg_audio::AudioReader::new(file) {
            if let Some(pic_bytes) = audio_engine_core::metadata::read_attached_pic(&reader) {
                let content_type = if pic_bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
                    "image/png"
                } else {
                    "image/jpeg"
                };
                return (
                    axum::http::StatusCode::OK,
                    [
                        (axum::http::header::CONTENT_TYPE, content_type),
                        (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
                    ],
                    pic_bytes,
                )
                    .into_response();
            }
        }
    }

    axum::http::StatusCode::NOT_FOUND.into_response()
}

/// 获取本地音频的内嵌歌词及同目录外部 .lrc 歌词
async fn lyric_file_handler(
    Query(query): Query<FilePathQuery>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let path = query
        .path
        .ok_or_else(|| ApiError::bad_request("Missing path parameter"))?;

    let p = std::path::Path::new(&path);
    if !p.is_file() {
        return Err(ApiError::not_found(&format!("File at {}", path)));
    }

    let embedded = audio_engine_core::metadata::read_tags(&path)
        .ok()
        .and_then(|t| t.lyrics);

    let external = audio_engine_core::metadata::find_all_external_lyrics(&path);
    let external_json: Vec<Value> = external
        .into_iter()
        .map(|l| {
            let content = std::fs::read_to_string(&l.path).unwrap_or_default();
            json!({
                "format": l.format,
                "path": l.path,
                "content": content,
            })
        })
        .collect();

    Ok(Json(PlayerResponse::ok(json!({
        "embedded": embedded,
        "external": external_json,
    }))))
}

/// 统一在线音源调用 Handler
async fn apis_call_handler(
    State(state): State<AppState>,
    Json(payload): Json<crate::api::online_apis::ApiCallRequest>,
) -> Json<crate::api::online_apis::ApiCallResponse> {
    let resp = crate::api::online_apis::dispatch_api_call(payload, &state.db).await;
    Json(resp)
}

/// WebSocket 端点：实时推送播放器状态与扫描进度（支持 ?token=xxx 参数校验）
async fn ws_handler(
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
async fn ws_run(mut socket: WebSocket, state: AppState) {
    let mut rx = state.ws_tx.subscribe();
    let mut rx_scan = state.scan_tx.subscribe();
    let mut interval = tokio::time::interval(Duration::from_millis(500));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // 定时推送当前播放器快照
                let snapshot = state.snapshot();
                let payload = serde_json::to_string(&snapshot).unwrap_or_else(|_| "{}".into());
                if socket.send(Message::Text(payload.into())).await.is_err() {
                    break;
                }
            }
            Ok(msg) = rx.recv() => {
                // 推送事件触发的状态更新
                let payload = serde_json::to_string(&msg).unwrap_or_else(|_| "{}".into());
                if socket.send(Message::Text(payload.into())).await.is_err() {
                    break;
                }
            }
            Ok(scan_msg) = rx_scan.recv() => {
                // 推送扫描进度与完成事件
                let payload = serde_json::to_string(&json!({
                    "event": "scan_progress",
                    "data": scan_msg,
                })).unwrap_or_else(|_| "{}".into());
                if socket.send(Message::Text(payload.into())).await.is_err() {
                    break;
                }
            }
            res = socket.recv() => {
                match res {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }
    let _ = socket.close().await;
}

// -------------------------------------------------------------------
// Diretta Audio-over-IP Handlers
// -------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DirettaSelectRequest {
    pub target: Option<String>,
}

/// 扫描局域网内的 Diretta 目标设备
async fn diretta_scan_handler() -> Result<Json<PlayerResponse>, ApiError> {
    let targets = spawn_isolated_blocking("diretta-scan-worker", || {
        audio_engine_core::diretta::scan_devices().unwrap_or_default()
    })
    .await
    .map_err(|e| ApiError::internal(format!("Diretta scan task failed: {e}")))?;

    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(targets).unwrap_or_default(),
    )))
}

/// 获取当前 Diretta 输出状态与选中的设备
async fn diretta_status_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let player = state.player.lock();
    let selected_device = player.selected_device().map(String::from);
    let is_direct_active = player.direct_active();
    let is_playing = player.state() == audio_engine_core::PlayerState::Playing;

    Ok(Json(PlayerResponse::ok(json!({
        "selected_device": selected_device,
        "is_diretta_active": is_direct_active,
        "is_online": is_direct_active,
        "is_playing": is_playing,
        "target_address": selected_device.as_deref().and_then(audio_engine_core::diretta::selector_target).unwrap_or(""),
    }))))
}

/// 切换音频输出到指定的 Diretta Target 设备（或传入 null/空 恢复默认声卡）
async fn diretta_select_handler(
    State(state): State<AppState>,
    Json(payload): Json<DirettaSelectRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let mut player = state.player.lock();
    let dev_name = payload.target.as_ref().and_then(|t| {
        let trimmed = t.trim();
        if trimmed.is_empty()
            || trimmed == "undefined"
            || trimmed == "diretta:undefined"
            || trimmed == "null"
            || trimmed == "diretta:null"
            || trimmed == "system-default"
        {
            None
        } else if trimmed.starts_with("diretta:") || trimmed.starts_with("diretta@") {
            Some(trimmed.to_string())
        } else {
            Some(format!("diretta:{}", trimmed))
        }
    });

    player.set_output_device(dev_name);

    Ok(Json(PlayerResponse::ok(json!({
        "status": "output_device_updated",
        "selected_device": player.selected_device(),
    }))))
}

#[derive(Debug, Deserialize)]
pub struct DirettaTargetInfoRequest {
    pub target: String,
}

/// 查询指定 Diretta 目标 DAC 的信息
async fn diretta_target_info_handler(
    Json(payload): Json<DirettaTargetInfoRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let target = payload.target.trim().to_string();
    let target_clean = target
        .strip_prefix("diretta:")
        .or_else(|| target.strip_prefix("diretta@"))
        .unwrap_or(&target)
        .to_string();

    let devices = spawn_isolated_blocking("diretta-info-worker", move || {
        audio_engine_core::diretta::scan_devices().unwrap_or_default()
    })
    .await
    .map_err(|e| ApiError::internal(format!("Diretta target info task failed: {e}")))?;

    let dev = devices.into_iter().find(|d| {
        d.id == target
            || d.full_addr == target_clean
            || d.ipv6_addr == target_clean
            || d.id == format!("diretta:{target_clean}")
            || d.name == target
    });

    let mtu = dev.as_ref().map(|d| d.mtu).unwrap_or(1500);
    let target_display = if !target_clean.is_empty() && target_clean != "undefined" {
        target_clean
    } else if let Some(ref d) = dev {
        d.full_addr.clone()
    } else {
        "Diretta Target".to_string()
    };

    Ok(Json(PlayerResponse::ok(json!({
        "target_address": target_display,
        "pcm_format_desc": "最高 768kHz / 32-bit / 2-8 声道",
        "dsd_format_desc": "Native DSD512 (22.5MHz/24.5MHz) MSB/LSB",
        "transmission_mode": "Mode 3 (MicroSecond Sync)",
        "mtu": mtu,
        "supports_pcm": true,
        "supports_dsd": true,
        "supports_native_dsd": true,
        "pcm_max_sample_rate": 768000,
        "pcm_max_bits": 32,
        "pcm_channels": 2,
        "dsd_max_sample_rate": 22579200,
        "bit_perfect_supported": true,
        "available": true,
    }))))
}

#[derive(Debug, Deserialize)]
pub struct FsBrowseQuery {
    pub path: Option<String>,
}

/// 服务端文件目录浏览器（专为 Headless Web UI 选歌及添加曲库目录设计）
async fn fs_browse_handler(
    Query(query): Query<FsBrowseQuery>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let raw_path = query.path.unwrap_or_default().trim().to_string();

    // 如果没有传 path 或者为根路径 "/"，列出常用根节点与挂载点
    if raw_path.is_empty() || raw_path == "/" {
        let mut roots = Vec::new();
        // 1. 用户 Home 目录
        if let Ok(home) = std::env::var("HOME") {
            let p = std::path::PathBuf::from(&home);
            if p.is_dir() {
                roots.push(json!({
                    "name": format!("🏠 家目录 ({})", home),
                    "path": home,
                    "is_dir": true,
                }));
                let music_dir = p.join("Music");
                if music_dir.is_dir() {
                    roots.push(json!({
                        "name": "🎵 音乐目录 (~/Music)",
                        "path": music_dir.to_string_lossy().to_string(),
                        "is_dir": true,
                    }));
                }
            }
        }
        // 2. 常见挂载点与存储目录
        for mount in &["/media", "/mnt", "/data", "/home", "/opt", "/var", "/"] {
            let p = std::path::Path::new(mount);
            if p.is_dir() && !roots.iter().any(|r| r["path"] == *mount) {
                roots.push(json!({
                    "name": format!("📁 {}", mount),
                    "path": mount.to_string(),
                    "is_dir": true,
                }));
            }
        }

        return Ok(Json(PlayerResponse::ok(json!({
            "current_path": "/",
            "parent_path": null,
            "dirs": roots,
            "audio_count": 0,
        }))));
    }

    let p = std::path::Path::new(&raw_path);
    if !p.exists() || !p.is_dir() {
        return Err(ApiError::not_found(&format!(
            "Directory not found: {}",
            raw_path
        )));
    }

    let parent_path = p.parent().map(|parent| {
        let s = parent.to_string_lossy().to_string();
        if s.is_empty() {
            "/".to_string()
        } else {
            s
        }
    });

    let mut dirs = Vec::new();
    let mut audio_count = 0usize;

    static AUDIO_EXTENSIONS: &[&str] = &[
        "flac", "wav", "mp3", "m4a", "dsf", "dff", "ape", "mac", "ogg", "wma", "alac", "aac",
        "iso", "dts", "ac3", "wv", "mid", "midi", "mqa",
    ];

    if let Ok(entries) = std::fs::read_dir(p) {
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            // 忽略隐藏文件与系统临时目录
            if file_name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                let has_children = std::fs::read_dir(&path)
                    .map(|mut it| it.next().is_some())
                    .unwrap_or(false);
                dirs.push(json!({
                    "name": file_name.to_string(),
                    "path": path.to_string_lossy().to_string(),
                    "has_children": has_children,
                    "is_dir": true,
                }));
            } else if path.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    let ext_lower = ext.to_ascii_lowercase();
                    if AUDIO_EXTENSIONS.contains(&ext_lower.as_str()) || ext_lower == "cue" {
                        audio_count += 1;
                    }
                }
            }
        }
    }

    dirs.sort_by(|a, b| {
        let name_a = a["name"].as_str().unwrap_or_default().to_lowercase();
        let name_b = b["name"].as_str().unwrap_or_default().to_lowercase();
        name_a.cmp(&name_b)
    });

    Ok(Json(PlayerResponse::ok(json!({
        "current_path": raw_path,
        "parent_path": parent_path,
        "dirs": dirs,
        "audio_count": audio_count,
    }))))
}

// -------------------------------------------------------------------
// 测试辅助
// -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_response_ok_shape() {
        let resp = PlayerResponse::ok(json!({ "status": "playing" }));
        assert!(resp.success);
        assert!(resp.data.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn player_response_err_shape() {
        let resp = PlayerResponse::err(ApiError::bad_request("boom"));
        assert!(!resp.success);
        assert!(resp.data.is_none());
        assert!(resp.error.is_some());
    }
}
