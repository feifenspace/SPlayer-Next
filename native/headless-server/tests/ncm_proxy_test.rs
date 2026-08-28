//! 在线音乐平台代理与 ncm-api-rs 集成测试

use axum::body::Body;
use axum::http::{Request, StatusCode};
use headless_server::api::routes::build_router;
use headless_server::config::Config;
use headless_server::state::AppState;
use serde_json::Value;
use tower::ServiceExt;

/// 创建测试用 AppState
async fn create_test_app_state() -> (AppState, tempfile::TempDir) {
    let temp_dir = tempfile::tempdir().expect("Failed to create tempdir");
    let db_path = temp_dir.path().join("data").join("library.db");
    let cover_dir = temp_dir.path().join("data").join("covers");

    let config = Config {
        listen_addr: "127.0.0.1:14558".to_string(),
        cors_origins: Some("*".to_string()),
        api_token: None,
        cover_cache_dir: Some(cover_dir),
        database_path: Some(db_path),
        web_root: None,
        diretta_target: None,
    };

    let state = AppState::new(&config).expect("Failed to create AppState");
    (state, temp_dir)
}

#[tokio::test]
async fn test_ncm_router_mounted_at_api_ncm() {
    let (state, _temp) = create_test_app_state().await;
    let app = build_router(state);

    // 访问 /api/ncm
    let req = Request::builder()
        .uri("/api/ncm")
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["code"], 200);
}

#[tokio::test]
async fn test_proxy_apis_call_netease() {
    let (state, _temp) = create_test_app_state().await;
    let app = build_router(state);

    let req = Request::builder()
        .uri("/api/v1/proxy/apis/call")
        .method("POST")
        .header("Content-Type", "application/json")
        .body(Body::from(
            r#"{"platform": "netease", "name": "search_default", "params": {}}"#,
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["ok"].as_bool().unwrap_or(false));
}

#[tokio::test]
async fn test_proxy_apis_call_invalid_platform() {
    let (state, _temp) = create_test_app_state().await;
    let app = build_router(state);

    let req = Request::builder()
        .uri("/api/v1/proxy/apis/call")
        .method("POST")
        .header("Content-Type", "application/json")
        .body(Body::from(
            r#"{"platform": "unknown_platform", "name": "test", "params": {}}"#,
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["ok"], false);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("Unsupported platform"));
}
