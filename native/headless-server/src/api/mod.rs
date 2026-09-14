//! REST API 路由组装与共享基础设施（控制器层）。

mod direct;
mod direct_preloader;
mod queue_source_resolver;
pub(crate) mod diretta_api;
mod fs;
mod library;
pub mod player;
mod playlist;
mod settings;
mod static_host;
mod watchdog;
mod ws;

pub mod online_apis;
pub mod routes;

pub use watchdog::spawn_output_recovery_watchdog;

use std::time::Duration;

use axum::{
    extract::State,
    http::{header, Request},
    middleware::{self, Next},
    Json, Router,
};
use serde_json::Value;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::error::ApiError;
use crate::state::AppState;
use direct::*;
use direct_preloader::*;
use diretta_api::*;
use fs::*;
use library::*;
use player::*;
pub use player::{AlsaDsdHandle, LoadMeta, LoadRequest};
use playlist::*;
use settings::*;
use ws::*;

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

/// 构建带 CORS、静态资源托管、SPA 回退和 Token 校验的完整 Router
pub fn build_router(state: AppState) -> Router {
    let cors = build_cors_layer(&state);

    // 受 Token 保护的控制路由
    let protected = Router::new()
        .route("/api/v1/player/play", axum::routing::post(play_handler))
        .route("/api/v1/player/pause", axum::routing::post(pause_handler))
        .route("/api/v1/player/stop", axum::routing::post(stop_handler))
        .route("/api/v1/player/volume", axum::routing::post(volume_handler))
        .route(
            "/api/v1/player/devices",
            axum::routing::get(player::devices_handler),
        )
        .route("/api/v1/player/load", axum::routing::post(load_handler))
        .route("/api/v1/player/seek", axum::routing::post(seek_handler))
        // 服务端权威“正在播放”快照（重开页面/无浏览器恢复曲目显示）
        .route(
            "/api/v1/player/now-playing",
            axum::routing::get(now_playing_handler),
        )
        // 下一曲候选预注册（B 层自动连播：浏览器关闭后服务端仍可接续）
        .route(
            "/api/v1/player/queue/next-candidate",
            axum::routing::post(queue_next_candidate_handler)
                .delete(queue_next_candidate_cancel_handler),
        )
        // 服务端播放队列快照（Direct 无缝预载与 boundary 自治推进的队列权威）
        .route(
            "/api/v1/player/queue",
            axum::routing::get(get_queue_handler)
                .put(queue_snapshot_handler)
                .delete(queue_clear_handler),
        )
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
        // 播放统计聚合与收藏事件（Web/headless 前端统计页与首页卡片）
        .route(
            "/api/v1/stats/play_summary",
            axum::routing::get(stats_play_summary_handler),
        )
        .route(
            "/api/v1/stats/top_tracks",
            axum::routing::get(stats_top_tracks_handler),
        )
        .route(
            "/api/v1/stats/top_albums",
            axum::routing::get(stats_top_albums_handler),
        )
        .route(
            "/api/v1/stats/top_artists",
            axum::routing::get(stats_top_artists_handler),
        )
        .route(
            "/api/v1/stats/daily",
            axum::routing::get(stats_daily_handler),
        )
        .route(
            "/api/v1/stats/hourly",
            axum::routing::get(stats_hourly_handler),
        )
        .route(
            "/api/v1/stats/favorite",
            axum::routing::post(stats_favorite_handler),
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

    router = static_host::mount_static_web_ui(router, &state);
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
        let mut valid_origins = Vec::new();
        for origin in origins {
            match origin.parse() {
                Ok(parsed) => valid_origins.push(parsed),
                Err(_) => tracing::warn!(origin = %origin, "CORS 配置含非法 origin，已忽略"),
            }
        }
        AllowOrigin::list(valid_origins)
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

/// 统一在线音源调用 Handler
async fn apis_call_handler(
    State(state): State<AppState>,
    Json(payload): Json<crate::api::online_apis::ApiCallRequest>,
) -> Json<crate::api::online_apis::ApiCallResponse> {
    let resp = crate::api::online_apis::dispatch_api_call(payload, &state.db).await;
    Json(resp)
}
