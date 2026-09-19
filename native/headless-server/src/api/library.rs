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
pub struct TrackIdsRequest {
    pub ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ScanDirRequest {
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct LibraryTracksPageQuery {
    pub limit: Option<u32>,
    pub offset: Option<u64>,
    pub cursor: Option<String>,
    pub q: Option<String>,
    pub codec: Option<String>,
    #[serde(rename = "sampleRate")]
    pub sample_rate: Option<u32>,
    pub sort: Option<String>,
    pub order: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LibraryAggregatePageQuery {
    pub limit: Option<u32>,
    pub offset: Option<u64>,
    pub cursor: Option<String>,
    pub q: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LibraryFolderTracksQuery {
    pub path: String,
    pub limit: Option<u32>,
    pub offset: Option<u64>,
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

/// 按 ID 批量读取曲目，供收藏和已保存队列按需加载。
pub(crate) async fn library_tracks_by_ids_handler(
    State(state): State<AppState>,
    Json(payload): Json<TrackIdsRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let ids: Vec<String> = payload.ids.into_iter().filter(|id| !id.is_empty()).take(10_000).collect();
    let conn = state.db.lock();
    let tracks = crate::db::get_tracks_by_ids(&conn, &ids)?;
    Ok(Json(PlayerResponse::ok(serde_json::to_value(tracks).unwrap_or_default())))
}

/// 获取目录下的分页曲目。
pub(crate) async fn library_folder_tracks_handler(
    State(state): State<AppState>,
    Query(query): Query<LibraryFolderTracksQuery>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let limit = query.limit.unwrap_or(200).clamp(1, 1000);
    let offset = query.offset.unwrap_or(0);
    let conn = state.db.lock();
    let (items, total) = crate::db::get_folder_tracks_page(&conn, &query.path, limit, offset)?;
    Ok(Json(PlayerResponse::ok(json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
        "hasMore": offset.saturating_add(items.len() as u64) < total,
    }))))
}

/// 获取轻量目录索引，避免前端为构建目录树加载全部曲目。
pub(crate) async fn library_folders_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let version = crate::db::get_folder_index_version(&conn)?;
    if let Ok(Some(cached)) = crate::db::get_setting(&conn, "library.folder_index_cache") {
        if cached.get("version").and_then(|v| v.as_u64()) == Some(version) {
            if let Some(items) = cached.get("items") {
                if let Ok(folders) = serde_json::from_value::<Vec<crate::db::LibraryFolderSummary>>(items.clone()) {
                    return Ok(Json(PlayerResponse::ok(json!(folders))));
                }
            }
        }
    }
    let folders = crate::db::get_folder_summaries(&conn)?;
    let cache = json!({ "version": version, "items": folders });
    if let Err(error) = crate::db::set_setting(&conn, "library.folder_index_cache", &cache) {
        tracing::debug!(%error, "写入目录索引缓存失败");
    }
    let items = cache.get("items").cloned().unwrap_or_else(|| json!([]));
    Ok(Json(PlayerResponse::ok(items)))
}

/// 分页查询音乐库曲目；兼容接口 /library/tracks 仍保留。
pub(crate) async fn library_tracks_page_handler(
    State(state): State<AppState>,
    Query(query): Query<LibraryTracksPageQuery>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let limit = query.limit.unwrap_or(200).clamp(1, 1000);
    let offset = query.offset.unwrap_or(0);
    let conn = state.db.lock();
    let (items, total, next_cursor) = crate::db::get_tracks_page_advanced(
        &conn,
        query.q.as_deref(),
        query.codec.as_deref(),
        query.sample_rate,
        limit,
        offset,
        query.sort.as_deref(),
        query.order.as_deref(),
        query.cursor.as_deref(),
    )?;
    Ok(Json(PlayerResponse::ok(json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
        "nextCursor": next_cursor,
        "hasMore": next_cursor.is_some(),
    }))))
}

/// 分页获取专辑聚合。
pub(crate) async fn library_albums_page_handler(
    State(state): State<AppState>,
    Query(query): Query<LibraryAggregatePageQuery>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let limit = query.limit.unwrap_or(200).clamp(1, 1000);
    let offset = query.offset.unwrap_or(0);
    let conn = state.db.lock();
    // 游标分页作为唯一分页协议；首请求 cursor 为空，后续请求使用上一页游标。
    let (items, total, next_cursor) =
        crate::db::get_album_page_cursor(&conn, query.q.as_deref(), limit, query.cursor.as_deref())?;
    Ok(Json(PlayerResponse::ok(json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
        "nextCursor": next_cursor,
        "hasMore": next_cursor.is_some(),
    }))))
}

/// 分页获取歌手聚合。
pub(crate) async fn library_artists_page_handler(
    State(state): State<AppState>,
    Query(query): Query<LibraryAggregatePageQuery>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let limit = query.limit.unwrap_or(200).clamp(1, 1000);
    let offset = query.offset.unwrap_or(0);
    let conn = state.db.lock();
    // 与专辑列表保持一致，游标是唯一分页位置。
    let (items, total, next_cursor) =
        crate::db::get_artist_page_cursor(&conn, query.q.as_deref(), limit, query.cursor.as_deref())?;
    Ok(Json(PlayerResponse::ok(json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
        "nextCursor": next_cursor,
        "hasMore": next_cursor.is_some(),
    }))))
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
pub(crate) async fn library_clear_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let mut conn = state.db.lock();
    let deleted = crate::db::clear_library_tracks(&mut conn)?;
    Ok(Json(PlayerResponse::ok(json!({ "deleted": deleted }))))
}

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
                succeeded: 0,
                failed: 0,
                removed: 0,
                cue_files: 0,
                iso_files: 0,
            });
            return;
        }

        {
            let conn = state_clone.db.lock();
            let checkpoint = json!({
                "phase": "running",
                "incremental": incremental,
                "directories": dirs,
                "scanned": 0,
                "total": 0,
                "succeeded": 0,
                "failed": 0,
            });
            if let Err(error) = crate::db::set_setting(&conn, "library.scan_checkpoint", &checkpoint) {
                tracing::warn!(%error, "写入媒体库扫描检查点失败");
            }
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
        let succeeded_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let failed_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let succeeded_for_cb = succeeded_count.clone();
        let failed_for_cb = failed_count.clone();

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
                    skipped,
                    failed: batch_failed,
                    tracks,
                } => {
                    let batch_succeeded = skipped + tracks.len() as u32;
                    let succeeded = succeeded_for_cb.fetch_add(
                        batch_succeeded,
                        std::sync::atomic::Ordering::SeqCst,
                    ) + batch_succeeded;
                    let failed = failed_for_cb.fetch_add(
                        batch_failed,
                        std::sync::atomic::Ordering::SeqCst,
                    ) + batch_failed;
                    if !tracks.is_empty() {
                        let mut conn = state_for_cb.db.lock();
                        if let Err(error) = crate::db::upsert_scanned_tracks(&mut conn, &tracks) {
                            tracing::error!(%error, "写入扫描到的媒体库记录失败");
                        }
                    }
                    {
                        let conn = state_for_cb.db.lock();
                        let checkpoint = json!({
                            "phase": "running",
                            "incremental": incremental,
                            "scanned": scanned,
                            "total": total,
                            "succeeded": succeeded,
                            "failed": failed,
                        });
                        if let Err(error) = crate::db::set_setting(&conn, "library.scan_checkpoint", &checkpoint) {
                            tracing::debug!(%error, "更新媒体库扫描检查点失败");
                        }
                    }
                    let _ = state_for_cb
                        .scan_tx
                        .send(crate::state::ScanProgressMessage {
                            r#type: "progress".to_string(),
                            phase: "scanning".to_string(),
                            scanned,
                            total,
                            current,
                            succeeded,
                            failed,
                            removed: 0,
                            cue_files: 0,
                            iso_files: 0,
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
                    let succeeded = succeeded_for_cb.load(
                        std::sync::atomic::Ordering::SeqCst,
                    );
                    let failed = failed_for_cb.load(
                        std::sync::atomic::Ordering::SeqCst,
                    );
                    {
                        let mut conn = state_for_cb.db.lock();
                        if !removed_paths.is_empty() {
                            if let Err(error) = crate::db::delete_tracks_by_paths(&mut conn, &removed_paths) {
                                tracing::error!(%error, "删除媒体库失效记录失败");
                            }
                        }
                        if !cue_files.is_empty() {
                            let cover_cache_dir = state_for_cb.config.resolved_cover_cache_dir();
                            if let Err(error) = crate::db::sync_cue_tracks(
                                &mut conn,
                                &cue_files,
                                Some(&cover_cache_dir),
                            ) {
                                tracing::error!(%error, "同步 CUE 虚拟分轨失败");
                            }
                        }
                        if !iso_files.is_empty() {
                            let cover_cache_dir = state_for_cb.config.resolved_cover_cache_dir();
                            if let Err(error) = crate::db::sync_sacd_tracks(
                                &mut conn,
                                &iso_files,
                                Some(&cover_cache_dir),
                            ) {
                                tracing::error!(%error, "同步 SACD ISO 虚拟分轨失败");
                            }
                        }
                    }
                    {
                        let conn = state_for_cb.db.lock();
                        let checkpoint = json!({
                            "phase": "done",
                            "incremental": incremental,
                            "scanned": scanned,
                            "total": total,
                            "succeeded": succeeded,
                            "failed": failed,
                            "removed": removed_paths.len(),
                            "cue_files": cue_files.len(),
                            "iso_files": iso_files.len(),
                        });
                        if let Err(error) = crate::db::set_setting(&conn, "library.scan_checkpoint", &checkpoint) {
                            tracing::debug!(%error, "保存媒体库扫描检查点失败");
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
                            succeeded,
                            failed,
                            removed: removed_paths.len() as u32,
                            cue_files: cue_files.len() as u32,
                            iso_files: iso_files.len() as u32,
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
    // 保持 is_scanning 直到后台扫描线程真正退出。
    Json(PlayerResponse::ok(json!({ "status": "scan_cancel_requested" })))
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
