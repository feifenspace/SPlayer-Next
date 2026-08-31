use axum::body::Body;
use axum::http::{Request, StatusCode};
use headless_server::api::routes::build_router;
use headless_server::config::Config;
use headless_server::state::AppState;
use serde_json::Value;
use tower::ServiceExt;

async fn create_test_app_state() -> AppState {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test_diretta.db");

    let config = Config {
        listen_addr: "127.0.0.1:0".to_string(),
        cors_origins: None,
        api_token: None,
        cover_cache_dir: Some(temp_dir.path().join("covers")),
        database_path: Some(db_path),
        web_root: None,
        diretta_target: None,
        proxy: None,
    };

    AppState::new(&config).expect("Failed to create test AppState")
}

#[tokio::test]
async fn test_diretta_status_endpoint() {
    let state = create_test_app_state().await;
    let app = build_router(state);

    let request = Request::builder()
        .uri("/api/v1/diretta/status")
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(body["success"].as_bool().unwrap());
    assert_eq!(body["data"]["is_diretta_active"], false);
}

#[tokio::test]
async fn test_diretta_select_endpoint() {
    let state = create_test_app_state().await;
    let app = build_router(state);

    // 1. 选择 Diretta 设备
    let request = Request::builder()
        .uri("/api/v1/diretta/select")
        .method("POST")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"target": "fe80::5241:b9ff:fe70:f9d2%2"}"#))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(body["success"].as_bool().unwrap());
    assert_eq!(body["data"]["status"], "output_device_updated");
    assert_eq!(
        body["data"]["selected_device"],
        "diretta:fe80::5241:b9ff:fe70:f9d2%2"
    );

    // 2. 验证 status 反映了变化。
    //    注意：is_diretta_active 来自 Diretta 会话运行时状态，需要真实
    //    Endpoint 连接后才会变为 true，测试环境无硬件，不应在此断言。
    //    select 接口的契约是登记目标设备，故断言 selected_device 已生效。
    let status_req = Request::builder()
        .uri("/api/v1/diretta/status")
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let status_resp = app.clone().oneshot(status_req).await.unwrap();
    let body_bytes = axum::body::to_bytes(status_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(
        body["data"]["selected_device"],
        "diretta:fe80::5241:b9ff:fe70:f9d2%2"
    );

    // 3. 恢复默认设备 (null)
    let reset_req = Request::builder()
        .uri("/api/v1/diretta/select")
        .method("POST")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"target": null}"#))
        .unwrap();

    let reset_resp = app.oneshot(reset_req).await.unwrap();
    assert_eq!(reset_resp.status(), StatusCode::OK);
}
