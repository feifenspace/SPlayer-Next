//! 静态 Web UI 托管（D.13.1）：显式/探测到的磁盘目录优先（开发模式），
//! 否则回退到编译期内嵌资源（首屏零磁盘 IO）。

use std::path::PathBuf;

use axum::Router;
use rust_embed::RustEmbed;
use tower_http::services::{ServeDir, ServeFile};

use crate::state::AppState;

/// 编译期内嵌的 Web 资源（native/headless-server/web/，打包期由真实
/// 前端构建产物填充；源码树内为占位页）
#[derive(RustEmbed)]
#[folder = "web"]
struct EmbeddedWeb;

fn embedded_content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" | "webmanifest" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// 服务内嵌资源的 axum fallback：SPA 未知路径回退 index.html
async fn embedded_fallback(uri: axum::http::Uri) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;

    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match EmbeddedWeb::get(path).or_else(|| EmbeddedWeb::get("index.html")) {
        Some(asset) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, embedded_content_type(path))],
            asset.data,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// 探测 web 根目录：显式配置优先；未配置且磁盘无可用目录时挂内嵌资源
pub(crate) fn mount_static_web_ui(
    mut router: Router<AppState>,
    state: &AppState,
) -> Router<AppState> {
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
            return router;
        }
    }

    tracing::info!("磁盘无 Web UI 目录，挂载内嵌资源（D.13.1）");
    router.fallback_service(axum::routing::any(embedded_fallback))
}
