//! 音乐库扫描与曲库查询端点。

//! REST API 控制器
//!
//! 基于 Axum 0.8 的路由定义，提供播放控制、状态查询、扫描和 WebSocket 端点。

use axum::{
    extract::{Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use super::PlayerResponse;
use crate::error::ApiError;
use crate::state::AppState;

/// 扫描查询参数
#[derive(Debug, Deserialize)]
pub struct ScanQuery {
    path: Option<String>,
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
pub(crate) async fn scan_probe_handler(
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
pub(crate) async fn library_tracks_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let tracks = crate::db::get_all_tracks(&conn)?;
    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(tracks).unwrap_or_default(),
    )))
}

/// 获取音乐库全部专辑聚合
pub(crate) async fn library_albums_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let albums = crate::db::get_album_list(&conn)?;
    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(albums).unwrap_or_default(),
    )))
}

/// 获取音乐库全部歌手聚合
pub(crate) async fn library_artists_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let artists = crate::db::get_artist_list(&conn)?;
    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(artists).unwrap_or_default(),
    )))
}

/// 获取指定专辑下的所有曲目
pub(crate) async fn library_album_tracks_handler(
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
pub(crate) async fn library_artist_tracks_handler(
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
pub(crate) async fn library_scan_dirs_get_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let dirs = crate::db::get_scan_dirs(&conn)?;
    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(dirs).unwrap_or_default(),
    )))
}

/// 添加扫描目录
pub(crate) async fn library_scan_dirs_add_handler(
    State(state): State<AppState>,
    Json(payload): Json<ScanDirRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    crate::db::add_scan_dir(&conn, &payload.path)?;
    Ok(Json(PlayerResponse::ok(json!({ "added": payload.path }))))
}

/// 删除扫描目录
pub(crate) async fn library_scan_dirs_remove_handler(
    State(state): State<AppState>,
    Json(payload): Json<ScanDirRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    crate::db::remove_scan_dir(&conn, &payload.path)?;
    Ok(Json(PlayerResponse::ok(json!({ "removed": payload.path }))))
}

/// 启动后台扫描
pub(crate) async fn library_scan_handler(
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
                            let _ = crate::db::sync_cue_tracks(
                                &mut conn,
                                &cue_files,
                                Some(&cover_cache_dir),
                            );
                        }
                        if !iso_files.is_empty() {
                            let cover_cache_dir = state_for_cb.config.resolved_cover_cache_dir();
                            let _ = crate::db::sync_sacd_tracks(
                                &mut conn,
                                &iso_files,
                                Some(&cover_cache_dir),
                            );
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
pub(crate) async fn library_cancel_scan_handler(
    State(state): State<AppState>,
) -> Json<PlayerResponse> {
    state
        .scan_cancel
        .store(true, std::sync::atomic::Ordering::SeqCst);
    state
        .is_scanning
        .store(false, std::sync::atomic::Ordering::SeqCst);
    Json(PlayerResponse::ok(json!({ "status": "scan_cancelled" })))
}

/// 获取当前扫描状态
pub(crate) async fn library_scan_status_handler(
    State(state): State<AppState>,
) -> Json<PlayerResponse> {
    let is_scanning = state.is_scanning.load(std::sync::atomic::Ordering::SeqCst);
    Json(PlayerResponse::ok(json!({ "is_scanning": is_scanning })))
}

// -------------------------------------------------------------------
// 歌单 Request Payload 与 Handlers
// -------------------------------------------------------------------
