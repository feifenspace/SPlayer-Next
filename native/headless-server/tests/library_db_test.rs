//! 媒体库持久化与 API 集成测试

use axum::body::Body;
use axum::http::{Request, StatusCode};
use headless_server::api::routes::build_router;
use headless_server::config::Config;
use headless_server::db;
use headless_server::state::AppState;
use serde_json::Value;
use tower::ServiceExt;

/// 创建带临时 DB 的测试用 AppState
async fn create_test_library_app_state() -> (AppState, tempfile::TempDir) {
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
        proxy: None,
    };

    let state = AppState::new(&config).expect("Failed to create AppState");
    (state, temp_dir)
}

#[tokio::test]
async fn test_db_scan_dirs_crud() {
    let (state, _temp) = create_test_library_app_state().await;
    let conn = state.db.lock();

    // 初始应为空
    let dirs = db::get_scan_dirs(&conn).unwrap();
    assert!(dirs.is_empty());

    // 添加目录
    db::add_scan_dir(&conn, "/music/pop").unwrap();
    db::add_scan_dir(&conn, "/music/rock").unwrap();

    let dirs = db::get_scan_dirs(&conn).unwrap();
    assert_eq!(dirs.len(), 2);
    assert!(dirs.contains(&"/music/pop".to_string()));
    assert!(dirs.contains(&"/music/rock".to_string()));

    // 移除目录
    db::remove_scan_dir(&conn, "/music/pop").unwrap();
    let dirs = db::get_scan_dirs(&conn).unwrap();
    assert_eq!(dirs.len(), 1);
    assert_eq!(dirs[0], "/music/rock");
}

#[tokio::test]
async fn test_db_tracks_upsert_and_queries() {
    let (state, _temp) = create_test_library_app_state().await;
    let mut conn = state.db.lock();

    let tracks = vec![
        audio_engine_core::scanner::ScannedTrack {
            path: "/music/rock/song1.flac".to_string(),
            title: Some("Song One".to_string()),
            artist: Some("Artist A".to_string()),
            album: Some("Album X".to_string()),
            track: Some(1),
            duration: 200.5,
            codec: "flac".to_string(),
            sample_rate: 44100,
            bit_rate: 1000000,
            channels: 2,
            bits_per_sample: 16,
            cover: Some("/covers/c1.jpg".to_string()),
            file_size: 20000000,
            mtime: 1000,
            ctime: 1000,
        },
        audio_engine_core::scanner::ScannedTrack {
            path: "/music/rock/song2.flac".to_string(),
            title: Some("Song Two".to_string()),
            artist: Some("Artist A".to_string()),
            album: Some("Album X".to_string()),
            track: Some(2),
            duration: 180.0,
            codec: "flac".to_string(),
            sample_rate: 44100,
            bit_rate: 1000000,
            channels: 2,
            bits_per_sample: 16,
            cover: Some("/covers/c1.jpg".to_string()),
            file_size: 18000000,
            mtime: 1001,
            ctime: 1001,
        },
        audio_engine_core::scanner::ScannedTrack {
            path: "/music/pop/song3.mp3".to_string(),
            title: Some("Song Three".to_string()),
            artist: Some("Artist B".to_string()),
            album: Some("Album Y".to_string()),
            track: Some(1),
            duration: 210.0,
            codec: "mp3".to_string(),
            sample_rate: 44100,
            bit_rate: 320000,
            channels: 2,
            bits_per_sample: 16,
            cover: None,
            file_size: 8000000,
            mtime: 1002,
            ctime: 1002,
        },
    ];

    db::upsert_scanned_tracks(&mut conn, &tracks).unwrap();

    // 查询全部曲目
    let all_tracks = db::get_all_tracks(&conn).unwrap();
    assert_eq!(all_tracks.len(), 3);

    // 查询专辑聚合
    let albums = db::get_album_list(&conn).unwrap();
    assert_eq!(albums.len(), 2);
    let album_x = albums.iter().find(|a| a.name == "Album X").unwrap();
    assert_eq!(album_x.track_count, 2);

    // 查询歌手聚合
    let artists = db::get_artist_list(&conn).unwrap();
    assert_eq!(artists.len(), 2);
    let artist_a = artists.iter().find(|a| a.name == "Artist A").unwrap();
    assert_eq!(artist_a.track_count, 2);

    // 按专辑查询
    let album_tracks = db::get_tracks_by_album(&conn, "Album X").unwrap();
    assert_eq!(album_tracks.len(), 2);

    // 按歌手查询
    let artist_tracks = db::get_tracks_by_artist(&conn, "Artist B").unwrap();
    assert_eq!(artist_tracks.len(), 1);

    // 增量文件记录
    let records = db::get_file_records(&conn).unwrap();
    assert_eq!(records.len(), 3);

    // 删除路径
    db::delete_tracks_by_paths(&mut conn, &["/music/pop/song3.mp3".to_string()]).unwrap();
    let all_tracks = db::get_all_tracks(&conn).unwrap();
    assert_eq!(all_tracks.len(), 2);
}

#[tokio::test]
async fn test_library_rest_endpoints() {
    let (state, _temp) = create_test_library_app_state().await;

    // 先插入测试曲目和扫描目录
    {
        let mut conn = state.db.lock();
        db::add_scan_dir(&conn, "/music/rock").unwrap();
        let tracks = vec![audio_engine_core::scanner::ScannedTrack {
            path: "/music/rock/track1.flac".to_string(),
            title: Some("Track 1".to_string()),
            artist: Some("Rock Band".to_string()),
            album: Some("Greatest Hits".to_string()),
            track: Some(1),
            duration: 240.0,
            codec: "flac".to_string(),
            sample_rate: 48000,
            bit_rate: 1500000,
            channels: 2,
            bits_per_sample: 24,
            cover: None,
            file_size: 30000000,
            mtime: 2000,
            ctime: 2000,
        }];
        db::upsert_scanned_tracks(&mut conn, &tracks).unwrap();
    }

    let app = build_router(state);

    // 1. GET /api/v1/library/tracks
    let req = Request::builder()
        .uri("/api/v1/library/tracks")
        .method("GET")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["success"].as_bool().unwrap());
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"][0]["title"], "Track 1");

    // 2. GET /api/v1/library/albums
    let req = Request::builder()
        .uri("/api/v1/library/albums")
        .method("GET")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 3. GET /api/v1/library/artists
    let req = Request::builder()
        .uri("/api/v1/library/artists")
        .method("GET")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 4. GET /api/v1/library/scan_dirs
    let req = Request::builder()
        .uri("/api/v1/library/scan_dirs")
        .method("GET")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 1);

    // 5. POST /api/v1/library/scan_dirs (添加)
    let req = Request::builder()
        .uri("/api/v1/library/scan_dirs")
        .method("POST")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"path": "/music/jazz"}"#))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 6. GET /api/v1/library/scan/status
    let req = Request::builder()
        .uri("/api/v1/library/scan/status")
        .method("GET")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["data"]["is_scanning"], false);

    // 7. POST /api/v1/library/cancel_scan
    let req = Request::builder()
        .uri("/api/v1/library/cancel_scan")
        .method("POST")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_cue_sheet_sync_and_virtual_tracks() {
    let (state, temp) = create_test_library_app_state().await;
    let cue_dir = temp.path().join("cue_album");
    std::fs::create_dir_all(&cue_dir).unwrap();

    let wav_path = cue_dir.join("CDImage.wav");
    std::fs::write(&wav_path, b"RIFF dummy wav file").unwrap();

    let cue_content = r#"
TITLE "Test CUE Album"
PERFORMER "Various Artists"
FILE "CDImage.wav" WAVE
  TRACK 01 AUDIO
    TITLE "Track 1 Title"
    PERFORMER "Artist One"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE "Track 2 Title"
    PERFORMER "Artist Two"
    INDEX 01 03:20:00
"#;
    let cue_path = cue_dir.join("CDImage.cue");
    std::fs::write(&cue_path, cue_content).unwrap();

    // 1. 先模拟 scanner 扫描出母版音频并插入 tracks
    {
        let mut conn = state.db.lock();
        let scanned = vec![audio_engine_core::scanner::ScannedTrack {
            path: wav_path.to_string_lossy().to_string(),
            title: Some("CDImage".to_string()),
            artist: None,
            album: None,
            track: None,
            duration: 420.0, // 7 分钟
            codec: "wav".to_string(),
            sample_rate: 44100,
            bit_rate: 1411200,
            channels: 2,
            bits_per_sample: 16,
            cover: None,
            file_size: 10000000,
            mtime: 1000,
            ctime: 1000,
        }];
        db::upsert_scanned_tracks(&mut conn, &scanned).unwrap();
    }

    // 2. 执行 CUE 同步
    {
        let mut conn = state.db.lock();
        let count = db::sync_cue_tracks(&mut conn, &[cue_path.to_string_lossy().to_string()], None).unwrap();
        assert_eq!(count, 2);
    }

    // 3. 验证查询：母版 CDImage.wav 被自动隐藏，展示 2 首虚拟分轨
    {
        let conn = state.db.lock();
        let all_tracks = db::get_all_tracks(&conn).unwrap();
        assert_eq!(all_tracks.len(), 2);
        assert_eq!(all_tracks[0].title, "Track 1 Title");
        assert_eq!(all_tracks[0].artist.as_deref(), Some("Artist One"));
        assert_eq!(all_tracks[0].album.as_ref().map(|a| a.name.as_str()), Some("Test CUE Album"));
        assert_eq!(all_tracks[0].duration, 200000); // 3分20秒 = 200s = 200000ms

        assert_eq!(all_tracks[1].title, "Track 2 Title");
        assert_eq!(all_tracks[1].artist.as_deref(), Some("Artist Two"));
        assert_eq!(all_tracks[1].duration, 220000); // 420s - 200s = 220s = 220000ms

        // 专辑聚合
        let albums = db::get_album_list(&conn).unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].name, "Test CUE Album");
        assert_eq!(albums[0].track_count, 2);

        // 单轨获取
        let t1 = db::get_track_by_path(&conn, &format!("cue://{}#track=01", cue_path.to_string_lossy())).unwrap().unwrap();
        assert_eq!(t1.cue_audio_path.as_deref(), Some(wav_path.to_str().unwrap()));
        assert_eq!(t1.cue_start_ms, Some(0));
        assert_eq!(t1.cue_end_ms, Some(200000));
    }
}
