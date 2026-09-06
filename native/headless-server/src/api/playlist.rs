//! 歌单 CRUD 端点。

//! REST API 控制器
//!
//! 基于 Axum 0.8 的路由定义，提供播放控制、状态查询、扫描和 WebSocket 端点。

use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use super::PlayerResponse;
use crate::error::ApiError;
use crate::state::AppState;

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
pub(crate) async fn playlist_list_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let playlists = crate::db::get_all_playlists(&conn)?;
    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(playlists).unwrap_or_default(),
    )))
}

/// 获取单个歌单详情
pub(crate) async fn playlist_get_handler(
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
pub(crate) async fn playlist_create_handler(
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
pub(crate) async fn playlist_update_handler(
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
pub(crate) async fn playlist_delete_handler(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    crate::db::delete_playlist(&conn, &id)?;
    Ok(Json(PlayerResponse::ok(json!({ "deleted": id }))))
}

/// 向歌单添加曲目
pub(crate) async fn playlist_add_tracks_handler(
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
pub(crate) async fn playlist_remove_tracks_handler(
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
