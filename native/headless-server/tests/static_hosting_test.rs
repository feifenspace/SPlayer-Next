//! 静态资源托管与 SPA 回退集成测试

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use headless_server::api::routes::build_router;
use headless_server::config::Config;
use headless_server::state::AppState;
use tower::ServiceExt;

/// 创建临时静态 Web 目录测试桩
struct TempWebRoot {
    path: std::path::PathBuf,
}

impl TempWebRoot {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("splayer_web_test_{}", nanos));
        fs::create_dir_all(path.join("assets")).expect("Failed to create test assets dir");

        fs::write(
            path.join("index.html"),
            "<!DOCTYPE html><html><body><h1>SPlayer Web UI</h1></body></html>",
        )
        .expect("Failed to write index.html");

        fs::write(
            path.join("assets/style.css"),
            "body { background-color: #121212; color: #fff; }",
        )
        .expect("Failed to write style.css");

        fs::write(
            path.join("assets/app.js"),
            "console.log('SPlayer initialized');",
        )
        .expect("Failed to write app.js");

        Self { path }
    }
}

impl Drop for TempWebRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn create_app_with_web_root(web_root: std::path::PathBuf) -> (axum::Router, AppState) {
    let config = Config {
        listen_addr: "127.0.0.1:14558".to_string(),
        cors_origins: Some("*".to_string()),
        api_token: None,
        cover_cache_dir: None,
        database_path: None,
        web_root: Some(web_root),
        diretta_target: None,
        proxy: None,
    };
    let state = AppState::new(&config).expect("Failed to create AppState");
    let router = build_router(state.clone());
    (router, state)
}

#[tokio::test]
async fn test_serve_index_html_at_root() {
    let temp = TempWebRoot::new();
    let (app, _state) = create_app_with_web_root(temp.path.clone());

    let req = Request::builder()
        .uri("/")
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(content_type.contains("text/html"));

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("SPlayer Web UI"));
}

#[tokio::test]
async fn test_serve_static_asset() {
    let temp = TempWebRoot::new();
    let (app, _state) = create_app_with_web_root(temp.path.clone());

    let req = Request::builder()
        .uri("/assets/style.css")
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(content_type.contains("text/css"));

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("background-color: #121212"));
}

#[tokio::test]
async fn test_spa_history_fallback() {
    let temp = TempWebRoot::new();
    let (app, _state) = create_app_with_web_root(temp.path.clone());

    // 请求一个不存在的前端虚拟路由（如 /playlist/favorites/123）
    let req = Request::builder()
        .uri("/playlist/favorites/123")
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    // SPA 回退机制必须返回 200 OK，并且内容为 index.html
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("SPlayer Web UI"));
}

#[tokio::test]
async fn test_api_precedence_over_static_files() {
    let temp = TempWebRoot::new();
    let (app, _state) = create_app_with_web_root(temp.path.clone());

    // 请求 API 路由，确保优先命中 API 而不是走 fallback
    let req = Request::builder()
        .uri("/api/status")
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.get("state").is_some());
}
