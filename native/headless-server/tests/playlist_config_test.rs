//! 歌单、设置、统计与本地媒体服务集成测试

use axum::body::Body;
use axum::http::{Request, StatusCode};
use headless_server::api::routes::build_router;
use headless_server::config::Config;
use headless_server::db;
use headless_server::state::AppState;
use serde_json::Value;
use tower::ServiceExt;

/// 创建带临时 DB 的测试用 AppState
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
async fn test_db_playlist_lifecycle() {
    let (state, _temp) = create_test_app_state().await;
    let mut conn = state.db.lock();

    // 先插入测试曲目
    let tracks = vec![
        audio_engine_core::scanner::ScannedTrack {
            path: "/music/track1.mp3".to_string(),
            title: Some("Track 1".to_string()),
            artist: Some("Artist 1".to_string()),
            album: Some("Album 1".to_string()),
            track: Some(1),
            duration: 180.0,
            codec: "mp3".to_string(),
            sample_rate: 44100,
            bit_rate: 320000,
            channels: 2,
            bits_per_sample: 16,
            cover: None,
            file_size: 5000000,
            mtime: 1000,
            ctime: 1000,
        },
        audio_engine_core::scanner::ScannedTrack {
            path: "/music/track2.mp3".to_string(),
            title: Some("Track 2".to_string()),
            artist: Some("Artist 2".to_string()),
            album: Some("Album 2".to_string()),
            track: Some(2),
            duration: 200.0,
            codec: "mp3".to_string(),
            sample_rate: 44100,
            bit_rate: 320000,
            channels: 2,
            bits_per_sample: 16,
            cover: None,
            file_size: 6000000,
            mtime: 1001,
            ctime: 1001,
        },
    ];
    db::upsert_scanned_tracks(&mut conn, &tracks).unwrap();

    let all_tracks = db::get_all_tracks(&conn).unwrap();
    assert_eq!(all_tracks.len(), 2);
    let t1_id = &all_tracks[0].id;
    let t2_id = &all_tracks[1].id;

    // 1. 创建歌单
    let pl = db::create_playlist(&conn, "pl-test-1", "我的最爱", Some("测试歌单"), None).unwrap();
    assert_eq!(pl.title, "我的最爱");
    assert_eq!(pl.track_count, 0);

    // 2. 添加歌曲
    db::add_playlist_tracks(&mut conn, "pl-test-1", &[t1_id.clone(), t2_id.clone()]).unwrap();

    // 3. 查询详情
    let detail = db::get_playlist_detail(&conn, "pl-test-1")
        .unwrap()
        .expect("Playlist not found");
    assert_eq!(detail.tracks.len(), 2);
    assert_eq!(detail.tracks[0].title, "Track 1");

    // 4. 更新歌单
    db::update_playlist(&conn, "pl-test-1", Some("新标题"), Some("新简介"), None).unwrap();
    let detail = db::get_playlist_detail(&conn, "pl-test-1")
        .unwrap()
        .expect("Playlist not found");
    assert_eq!(detail.title, "新标题");
    assert_eq!(detail.description.as_deref(), Some("新简介"));

    // 5. 移除单曲
    db::remove_playlist_tracks(&mut conn, "pl-test-1", &[t1_id.clone()]).unwrap();
    let detail = db::get_playlist_detail(&conn, "pl-test-1")
        .unwrap()
        .expect("Playlist not found");
    assert_eq!(detail.tracks.len(), 1);
    assert_eq!(detail.tracks[0].title, "Track 2");

    // 6. 删除歌单
    db::delete_playlist(&conn, "pl-test-1").unwrap();
    let detail = db::get_playlist_detail(&conn, "pl-test-1").unwrap();
    assert!(detail.is_none());
}

#[tokio::test]
async fn test_db_settings_and_stats() {
    let (state, _temp) = create_test_app_state().await;
    let conn = state.db.lock();

    // 设置测试
    db::set_setting(&conn, "audio.volume", &serde_json::json!({ "value": 0.85 })).unwrap();
    let val = db::get_setting(&conn, "audio.volume").unwrap();
    assert_eq!(val, Some(serde_json::json!({ "value": 0.85 })));

    let all_settings = db::get_all_settings(&conn).unwrap();
    assert!(all_settings.get("audio.volume").is_some());

    db::reset_settings(&conn).unwrap();
    let val = db::get_setting(&conn, "audio.volume").unwrap();
    assert!(val.is_none());

    // 播放历史与统计测试
    db::record_play_history(
        &conn,
        "local:track1",
        "local",
        1700000000,
        150000,
        r#"{"id":"local:track1","title":"Test"}"#,
    )
    .unwrap();

    let history = db::get_play_history(&conn, 10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["title"], "Test");
    assert_eq!(history[0]["listenedMs"], 150000);

    let stats = db::get_library_stats(&conn).unwrap();
    assert_eq!(stats.total_tracks, 0); // 没有入库 tracks
}

#[tokio::test]
async fn test_playlist_and_config_rest_api() {
    let (state, _temp) = create_test_app_state().await;
    let app = build_router(state);

    // 1. 创建歌单 POST /api/v1/playlist/create
    let req = Request::builder()
        .uri("/api/v1/playlist/create")
        .method("POST")
        .header("Content-Type", "application/json")
        .body(Body::from(
            r#"{"id": "pl-api-1", "title": "API Playlist", "description": "Desc"}"#,
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["data"]["title"], "API Playlist");

    // 2. 获取歌单详情 GET /api/v1/playlist/pl-api-1
    let req = Request::builder()
        .uri("/api/v1/playlist/pl-api-1")
        .method("GET")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 3. 配置保存 POST /api/v1/config/set
    let req = Request::builder()
        .uri("/api/v1/config/set")
        .method("POST")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"key": "theme.dark", "value": true}"#))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 4. 配置读取 GET /api/v1/config/theme.dark
    let req = Request::builder()
        .uri("/api/v1/config/theme.dark")
        .method("GET")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["data"], true);

    // 5. 统计与历史 POST /api/v1/stats/record & GET /api/v1/stats/history
    let req = Request::builder()
        .uri("/api/v1/stats/record")
        .method("POST")
        .header("Content-Type", "application/json")
        .body(Body::from(
            r#"{"trackId": "tr-1", "source": "local", "listenedMs": 30000, "track": {"id": "tr-1", "title": "History Track"}}"#,
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = Request::builder()
        .uri("/api/v1/stats/history")
        .method("GET")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
