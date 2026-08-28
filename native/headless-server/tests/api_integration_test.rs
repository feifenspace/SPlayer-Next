//! 集成测试：HTTP API 接口逻辑
//!
//! 使用 axum-test + tower-http 来 mock 服务器，测试路由和状态管理。

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use headless_server::api::routes::build_router;
use headless_server::config::Config;
use headless_server::state::AppState;
use serde_json::Value;
use tower::ServiceExt; // for `oneshot`

/// 创建测试用 AppState
async fn create_test_app_state() -> AppState {
    let config = Config {
        listen_addr: "127.0.0.1:14558".to_string(),
        cors_origins: Some("*".to_string()),
        api_token: None, // 测试时不启用 token 校验
        cover_cache_dir: None,
        database_path: None,
        web_root: None,
        diretta_target: None,
    };
    AppState::new(&config).expect("Failed to create AppState")
}

/// 测试健康检查端点
#[tokio::test]
async fn test_status_handler() {
    let state = create_test_app_state().await;
    let app = build_router(state);

    let request = Request::builder()
        .uri("/api/status")
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();

    // 验证响应结构
    assert!(body.get("state").is_some());
    assert!(body.get("position").is_some());
    assert!(body.get("duration").is_some());
    assert!(body.get("volume").is_some());
    assert!(body.get("is_finished").is_some());
    assert!(body.get("current_source").is_some());
}

/// 测试播放控制端点
#[tokio::test]
async fn test_player_control_handlers() {
    let state = create_test_app_state().await;
    let app = build_router(state);

    // 测试 play
    let play_request = Request::builder()
        .uri("/api/v1/player/play")
        .method("POST")
        .body(Body::empty())
        .unwrap();

    let play_response = app.clone().oneshot(play_request).await.unwrap();
    assert_eq!(play_response.status(), StatusCode::OK);

    let play_body_bytes = axum::body::to_bytes(play_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let play_body: Value = serde_json::from_slice(&play_body_bytes).unwrap();
    assert!(play_body["success"].as_bool().unwrap());
    assert_eq!(play_body["data"]["status"], "playing");

    // 测试 pause
    let pause_request = Request::builder()
        .uri("/api/v1/player/pause")
        .method("POST")
        .body(Body::empty())
        .unwrap();

    let pause_response = app.clone().oneshot(pause_request).await.unwrap();
    assert_eq!(pause_response.status(), StatusCode::OK);

    let pause_body_bytes = axum::body::to_bytes(pause_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let pause_body: Value = serde_json::from_slice(&pause_body_bytes).unwrap();
    assert!(pause_body["success"].as_bool().unwrap());
    assert_eq!(pause_body["data"]["status"], "paused");

    // 测试 stop
    let stop_request = Request::builder()
        .uri("/api/v1/player/stop")
        .method("POST")
        .body(Body::empty())
        .unwrap();

    let stop_response = app.oneshot(stop_request).await.unwrap();
    assert_eq!(stop_response.status(), StatusCode::OK);

    let stop_body_bytes = axum::body::to_bytes(stop_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let stop_body: Value = serde_json::from_slice(&stop_body_bytes).unwrap();
    assert!(stop_body["success"].as_bool().unwrap());
    assert_eq!(stop_body["data"]["status"], "stopped");
}

/// 测试音量控制端点
#[tokio::test]
async fn test_volume_handler() {
    let state = create_test_app_state().await;
    let app = build_router(state);

    let volume_request = Request::builder()
        .uri("/api/v1/player/volume")
        .method("POST")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"volume": 0.5}"#))
        .unwrap();

    let response = app.oneshot(volume_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(body["success"].as_bool().unwrap());
    assert_eq!(body["data"]["volume"], 0.5);
}

/// 测试非法 JSON body（语法错误）
#[tokio::test]
async fn test_invalid_json_body() {
    let state = create_test_app_state().await;
    let app = build_router(state);

    let request = Request::builder()
        .uri("/api/v1/player/volume")
        .method("POST")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"volume": }"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    // 语法错误的 JSON 返回 400 BAD_REQUEST
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// 测试 JSON 缺失必需字段（volume 缺失）
#[tokio::test]
async fn test_missing_volume_field() {
    let state = create_test_app_state().await;
    let app = build_router(state);

    let request = Request::builder()
        .uri("/api/v1/player/volume")
        .method("POST")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    // 合法 JSON 结构但缺必需字段，返回 422 Unprocessable Entity
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// 测试 load 缺失必需的 source 字段
#[tokio::test]
async fn test_missing_load_source_field() {
    let state = create_test_app_state().await;
    let app = build_router(state);

    let request = Request::builder()
        .uri("/api/v1/player/load")
        .method("POST")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"auto_play": true}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    // 合法 JSON 结构但缺必需字段（source），返回 422 Unprocessable Entity
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// 测试 Token 校验中间件
#[tokio::test]
async fn test_token_middleware() {
    // 创建启用 token 校验的配置
    let config = Config {
        listen_addr: "127.0.0.1:14558".to_string(),
        cors_origins: Some("*".to_string()),
        api_token: Some("test-token-123".to_string()),
        cover_cache_dir: None,
        database_path: None,
        web_root: None,
        diretta_target: None,
    };
    let state = AppState::new(&config).expect("Failed to create AppState");
    let app = build_router(state);

    // 测试无 token 访问受保护端点
    let request = Request::builder()
        .uri("/api/v1/player/play")
        .method("POST")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // 测试正确 token
    let request_with_token = Request::builder()
        .uri("/api/v1/player/play")
        .method("POST")
        .header("Authorization", "Bearer test-token-123")
        .body(Body::empty())
        .unwrap();

    let response_with_token = app.oneshot(request_with_token).await.unwrap();
    assert_eq!(response_with_token.status(), StatusCode::OK);
}

/// 测试扫描探测端点（headless 模式下不可用）
#[tokio::test]
async fn test_scan_probe_handler() {
    let state = create_test_app_state().await;
    let app = build_router(state);

    let request = Request::builder()
        .uri("/api/v1/scan/probe")
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// 测试 WebSocket 连接
#[tokio::test]
async fn test_websocket_connection() {
    use futures_util::StreamExt;
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    // 启动测试服务器
    let state = create_test_app_state().await;
    let app = build_router(state);

    // 使用 spawn 启动服务器
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("Server failed");
    });

    // 给服务器启动时间
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 连接 WebSocket
    let url = format!("ws://{}/ws", addr);
    let (mut ws_stream, _) = connect_async(&url)
        .await
        .expect("Failed to connect to WebSocket");

    // 等待接收到状态消息
    let msg = tokio::time::timeout(Duration::from_secs(1), ws_stream.next())
        .await
        .expect("Timeout waiting for WebSocket message")
        .expect("WebSocket connection closed");

    match msg {
        Ok(Message::Text(text)) => {
            let _: Value = serde_json::from_str(&text).expect("Invalid JSON from WebSocket");
            // 成功接收到状态消息
        }
        Ok(_) => panic!("Expected text message from WebSocket"),
        Err(e) => panic!("WebSocket error: {}", e),
    }
}

/// 测试没有激活音轨时的 seek 接口
#[tokio::test]
async fn test_seek_no_active_track() {
    let state = create_test_app_state().await;
    let app = build_router(state);

    let request = Request::builder()
        .uri("/api/v1/player/seek")
        .method("POST")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"position_secs": 15.0}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(body["success"].as_bool().unwrap());
    assert_eq!(body["data"]["status"], "no_active_track");
}

/// 测试加载不存在的文件返回 400 Bad Request
#[tokio::test]
async fn test_load_nonexistent_file() {
    let state = create_test_app_state().await;
    let app = build_router(state);

    let request = Request::builder()
        .uri("/api/v1/player/load")
        .method("POST")
        .header("Content-Type", "application/json")
        .body(Body::from(
            r#"{"source": "/path/to/nonexistent/music.mp3", "auto_play": false}"#,
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["code"], "BadRequest");
}
