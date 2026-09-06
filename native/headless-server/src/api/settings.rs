//! 系统配置与播放统计端点。

//! REST API 控制器
//!
//! 基于 Axum 0.8 的路由定义，提供播放控制、状态查询、扫描和 WebSocket 端点。

use axum::{
    extract::{Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::PlayerResponse;
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct SetConfigRequest {
    pub key: Option<String>,
    pub value: Option<Value>,
    pub settings: Option<serde_json::Map<String, Value>>,
}

/// 获取全部用户设置
pub(crate) async fn config_get_all_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let settings = crate::db::get_all_settings(&conn)?;
    Ok(Json(PlayerResponse::ok(settings)))
}

/// 获取单个设置项
pub(crate) async fn config_get_handler(
    State(state): State<AppState>,
    axum::extract::Path(key): axum::extract::Path<String>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let conn = state.db.lock();
    let val = crate::db::get_setting(&conn, &key)?.unwrap_or(Value::Null);
    Ok(Json(PlayerResponse::ok(val)))
}

/// 保存设置项（支持单个 key/value 或整个 settings 字典）
pub(crate) async fn config_set_handler(
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
pub(crate) async fn config_reset_handler(
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
pub(crate) async fn stats_record_handler(
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
pub(crate) async fn stats_history_handler(
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
pub(crate) async fn stats_summary_handler(
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
