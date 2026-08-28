//! REST API 控制器
//!
//! 基于 Axum 0.8 的路由定义，提供播放控制、状态查询、扫描和 WebSocket 端点。

use std::path::PathBuf;
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
    source: String,
    /// 是否自动播放（默认 true）
    auto_play: Option<bool>,
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
    let revival_source = {
        let mut player = state.player.lock();
        player
            .play()
            .map_err(|e| ApiError::bad_request(e.to_string()))?
    };

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
    {
        let mut player = state.player.lock();
        player.pause();
    }
    Json(PlayerResponse::ok(json!({ "status": "paused" })))
}

/// 停止
async fn stop_handler(State(state): State<AppState>) -> Json<PlayerResponse> {
    {
        let mut player = state.player.lock();
        player.stop();
    }
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
        player.set_volume(volume);
    }
    Json(PlayerResponse::ok(json!({ "volume": volume })))
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
    let handle = audio_engine_core::HttpCancelHandle::new();

    let (
        old_threads,
        token,
        load_token,
        cover_dir,
        normalization_enabled,
        device_name,
        output_generation,
        failure_callback,
        equalizer,
        tempo,
    ) = {
        let mut player = state.player.lock();
        let (old_threads, token) = player.take_for_async_load(handle.clone());
        let output_generation = player.reserve_output_generation();
        let failure_callback = player.make_failure_callback(output_generation);
        (
            old_threads,
            token,
            player.load_token_handle(),
            player.cover_cache_dir().map(String::from),
            player.is_normalization_enabled(),
            player.selected_device().map(String::from),
            output_generation,
            failure_callback,
            player.equalizer_handle(),
            player.tempo_handle(),
        )
    };

    let source_for_decoder = source.clone();
    let result = tokio::task::spawn_blocking(move || {
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
            return Err(ApiError::bad_request(err.to_string()));
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
    } = take;

    let outcome: SeekOutcome = tokio::task::spawn_blocking(move || {
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
                    ..
                } => {
                    if !removed_paths.is_empty() {
                        let mut conn = state_for_cb.db.lock();
                        let _ = crate::db::delete_tracks_by_paths(&mut conn, &removed_paths);
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
    Json(payload): Json<crate::api::online_apis::ApiCallRequest>,
) -> Json<crate::api::online_apis::ApiCallResponse> {
    let resp = crate::api::online_apis::dispatch_api_call(payload).await;
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
    let targets =
        tokio::task::spawn_blocking(|| match audio_engine_core::diretta::DirettaFinder::new() {
            Ok(finder) => finder.scan(5),
            Err(e) => {
                tracing::warn!("DirettaFinder open failed: {e}");
                Vec::new()
            }
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
    let (selected_device, runtime) = {
        let player = state.player.lock();
        (
            player.selected_device().map(String::from),
            audio_engine_core::diretta::runtime_state(),
        )
    };

    Ok(Json(PlayerResponse::ok(json!({
        "selected_device": selected_device,
        "is_diretta_active": runtime.is_diretta_active,
        "is_online": runtime.is_online,
        "is_playing": runtime.is_playing,
        "target_address": runtime.target_addr,
        "sample_rate": runtime.sample_rate,
        "channels": runtime.channels,
        "is_dsd": runtime.is_dsd,
        "underrun_count": runtime.underrun_count,
        "last_error": runtime.last_error,
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
        if trimmed.is_empty() {
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

/// 查询指定 Diretta 目标 DAC 的硬件解码能力与网络参数
///
/// 通过临时建立 DirettaSync 连接读取 SDK 内部 SinkInfo 缓存（真实设备能力）。
/// 若连接失败则返回明确的 `available: false` 与错误原因，不再伪造能力数据。
async fn diretta_target_info_handler(
    Json(payload): Json<DirettaTargetInfoRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let target = payload.target.trim().to_string();
    let info = tokio::task::spawn_blocking(move || query_target_info(&target))
        .await
        .map_err(|e| ApiError::internal(format!("Diretta target info task failed: {e}")))?;

    Ok(Json(PlayerResponse::ok(info)))
}

/// 阻塞式查询目标设备真实能力
fn query_target_info(target: &str) -> serde_json::Value {
    let target_clean = target
        .strip_prefix("diretta:")
        .or_else(|| target.strip_prefix("diretta@"))
        .unwrap_or(target);

    // 解析 ip%ifno,port 形式的地址
    let mut addr = target_clean.to_string();
    let mut port = 19644u16;
    if let Some((ip, p)) = addr.split_once(',') {
        if let Ok(p) = p.parse::<u16>() {
            port = p;
        }
        addr = ip.to_string();
    }
    let mut ifno = 0i32;
    if let Some((ip, i)) = addr.split_once('%') {
        ifno = i.parse::<i32>().unwrap_or(0);
        addr = ip.to_string();
    }

    let caps = match audio_engine_core::diretta::query_sink_info(
        &format!("{}%{},{}", addr, ifno, port),
        ifno,
        0,
    ) {
        Ok(caps) => caps,
        Err(e) => {
            return json!({
                "target_address": target_clean,
                "available": false,
                "error": format!("connect failed: {e}"),
            });
        }
    };

    let pcm_format_desc = if caps.supports_pcm {
        format!(
            "最高 {}kHz / {}-bit / {} 声道",
            caps.pcm_max_sample_rate / 1000,
            caps.pcm_max_bits,
            caps.pcm_max_channels
        )
    } else {
        "不支持 PCM".to_string()
    };

    let dsd_format_desc = if caps.supports_dsd {
        let dsd_max_mult = caps.dsd_max_sample_rate / 44100;
        let dsd_name = match dsd_max_mult {
            64 => "DSD64 (2.8MHz)",
            128 => "DSD128 (5.6MHz)",
            256 => "DSD256 (11.2MHz)",
            512 | 557 => "DSD512 (22.5MHz/24.5MHz)",
            1024 => "DSD1024",
            _ => "Native DSD",
        };
        let order = if caps.supports_dsd_msb { "MSB" } else { "LSB" };
        format!("Native {} {}", dsd_name, order)
    } else {
        "不支持 DSD".to_string()
    };

    let mtu = if caps.req_mtu > 0 {
        caps.req_mtu
    } else if caps.min_mtu > 0 {
        caps.min_mtu
    } else {
        1500
    };

    let transmission_mode = "Mode 3 (MicroSecond Sync)".to_string();

    json!({
        "target_address": target_clean,
        "available": true,
        "pcm_format_desc": pcm_format_desc,
        "dsd_format_desc": dsd_format_desc,
        "transmission_mode": transmission_mode,
        "mtu": mtu,
        "supports_pcm": caps.supports_pcm,
        "supports_dsd": caps.supports_dsd,
        "supports_dsd_lsb": caps.supports_dsd_lsb,
        "supports_dsd_msb": caps.supports_dsd_msb,
        "pcm_min_sample_rate": caps.pcm_min_sample_rate,
        "pcm_max_sample_rate": caps.pcm_max_sample_rate,
        "pcm_min_bits": caps.pcm_min_bits,
        "pcm_max_bits": caps.pcm_max_bits,
        "pcm_min_channels": caps.pcm_min_channels,
        "pcm_max_channels": caps.pcm_max_channels,
        "dsd_min_sample_rate": caps.dsd_min_sample_rate,
        "dsd_max_sample_rate": caps.dsd_max_sample_rate,
        "dsd_min_channels": caps.dsd_min_channels,
        "dsd_max_channels": caps.dsd_max_channels,
        "dsd_supports_lsb_byte_order": caps.dsd_supports_lsb_byte_order,
        "dsd_supports_msb_byte_order": caps.dsd_supports_msb_byte_order,
        "dsd_supports_little_endian": caps.dsd_supports_little_endian,
        "dsd_supports_big_endian": caps.dsd_supports_big_endian,
        "dsd_supports_32bit_block": caps.dsd_supports_32bit_block,
        "latency_buffer_ms": caps.latency_buffer,
        "latency_max_ms": caps.latency_max,
        "latency_hw_ms": caps.latency_hw,
        "min_mtu": caps.min_mtu,
        "req_mtu": caps.req_mtu,
        "max_mtu": caps.max_mtu,
        "support_ms_mode": caps.support_ms_mode,
    })
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
