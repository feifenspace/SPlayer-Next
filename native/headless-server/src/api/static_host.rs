//! 静态 Web UI 托管：SPA 静态资源 + 404 fallback 到 index.html。

use std::path::PathBuf;

use tower_http::services::{ServeDir, ServeFile};

use crate::state::AppState;

/// 探测 web 根目录（显式配置优先），命中则挂为 fallback service。
pub(crate) fn mount_static_web_ui(
    mut router: axum::Router<AppState>,
    state: &AppState,
) -> axum::Router<AppState> {
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
    router
}
