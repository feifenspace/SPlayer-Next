//! 文件访问端点：封面、歌词文件、目录浏览。

//! REST API 控制器
//!
//! 基于 Axum 0.8 的路由定义，提供播放控制、状态查询、扫描和 WebSocket 端点。

use super::spawn_isolated_blocking;
use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::PlayerResponse;
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct FilePathQuery {
    pub path: Option<String>,
}

/// 封面 id 成分校验：id 会被 `join` 进封面目录，含路径分隔符或 `..` 即可
/// 穿越（`GET /api/v1/covers/..%2F..%2Fetc%2Fpasswd` 已实测可回读任意文件）。
/// 合法 id 形如 `local:{md5:x}` 或缓存文件名，均不含分隔符。
fn is_cover_id_safe(id: &str) -> bool {
    !id.is_empty() && !id.contains('/') && !id.contains('\\') && !id.contains("..")
}

/// 查询 path 成分校验：库内文件路径（绝对路径）合法，但拒绝 `..` 跳转成分
fn path_has_traversal(path: &str) -> bool {
    path.contains("..")
}

/// 根据 ID 或缓存文件名获取封面流（文件读取为阻塞 IO，隔离到独立线程）
pub(crate) async fn cover_get_handler(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    if !is_cover_id_safe(&id) {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }
    let cover_dir = state.config.resolved_cover_cache_dir();
    spawn_isolated_blocking("cover-file-read", move || {
        let possible_paths = [
            cover_dir.join(&id),
            cover_dir.join(format!("{}.jpg", id)),
            cover_dir.join(format!("{}.png", id)),
        ];

        for p in &possible_paths {
            if p.is_file() {
                if let Ok(bytes) = std::fs::read(p) {
                    let content_type = if p.extension().map_or(false, |ext| ext == "png") {
                        "image/png"
                    } else {
                        "image/jpeg"
                    };
                    return (
                        axum::http::StatusCode::OK,
                        [
                            (axum::http::header::CONTENT_TYPE, content_type),
                            (
                                axum::http::header::CACHE_CONTROL,
                                "public, max-age=31536000, immutable",
                            ),
                        ],
                        bytes,
                    )
                        .into_response();
                }
            }
        }

        axum::http::StatusCode::NOT_FOUND.into_response()
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(%e, "封面读取任务失败");
        axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })
}

/// 动态从本地音频文件提取内嵌封面流（FFmpeg 探测为阻塞 IO，隔离到独立线程）
pub(crate) async fn cover_file_handler(
    Query(query): Query<FilePathQuery>,
) -> axum::response::Response {
    let Some(path) = query.path else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };
    if path_has_traversal(&path) {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    spawn_isolated_blocking("cover-embed-read", move || {
        let p = std::path::Path::new(&path);
        if !p.is_file() {
            return axum::http::StatusCode::NOT_FOUND.into_response();
        }

        if let Ok(file) = std::fs::File::open(&path) {
            if let Ok(reader) = audio_engine_core::ffmpeg_audio::AudioReader::new(file) {
                if let Some(pic_bytes) = audio_engine_core::metadata::read_attached_pic(&reader) {
                    let content_type = if pic_bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
                        "image/png"
                    } else {
                        "image/jpeg"
                    };
                    return (
                        axum::http::StatusCode::OK,
                        [
                            (axum::http::header::CONTENT_TYPE, content_type),
                            (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
                        ],
                        pic_bytes,
                    )
                        .into_response();
                }
            }
        }

        axum::http::StatusCode::NOT_FOUND.into_response()
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(%e, "内嵌封面提取任务失败");
        axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })
}

/// 获取本地音频的内嵌歌词及同目录外部 .lrc 歌词（标签读取为阻塞 IO，隔离到独立线程）
pub(crate) async fn lyric_file_handler(
    Query(query): Query<FilePathQuery>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let path = query
        .path
        .ok_or_else(|| ApiError::bad_request("Missing path parameter"))?;
    if path_has_traversal(&path) {
        return Err(ApiError::bad_request(
            "path must not contain '..' components",
        ));
    }

    spawn_isolated_blocking("lyric-file-read", move || {
        let p = std::path::Path::new(&path);
        if !p.is_file() {
            return Err(ApiError::not_found(&format!("File at {}", path)));
        }

        let embedded = audio_engine_core::metadata::read_tags(&path)
            .ok()
            .and_then(|t| t.lyrics);

        let external = audio_engine_core::metadata::find_all_external_lyrics(&path);
        let external_json: Vec<Value> = external
            .into_iter()
            .map(|l| {
                let content = std::fs::read_to_string(&l.path).unwrap_or_default();
                json!({
                    "format": l.format,
                    "path": l.path,
                    "content": content,
                })
            })
            .collect();

        Ok(Json(PlayerResponse::ok(json!({
            "embedded": embedded,
            "external": external_json,
        }))))
    })
    .await
    .map_err(|e| ApiError::internal(e))?
}

#[derive(Debug, Deserialize)]
pub struct FsBrowseQuery {
    pub path: Option<String>,
}

/// 服务端文件目录浏览器（专为 Headless Web UI 选歌及添加曲库目录设计）
pub(crate) async fn fs_browse_handler(
    Query(query): Query<FsBrowseQuery>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let raw_path = query.path.unwrap_or_default().trim().to_string();

    // 如果没有传 path 或者为根路径 "/"，列出常用根节点与挂载点
    if raw_path.is_empty() || raw_path == "/" {
        let mut roots = Vec::new();
        // 1. 用户 Home 目录
        if let Ok(home) = std::env::var("HOME") {
            let p = std::path::PathBuf::from(&home);
            if p.is_dir() {
                roots.push(json!({
                    "name": format!("🏠 家目录 ({})", home),
                    "path": home,
                    "is_dir": true,
                }));
                let music_dir = p.join("Music");
                if music_dir.is_dir() {
                    roots.push(json!({
                        "name": "🎵 音乐目录 (~/Music)",
                        "path": music_dir.to_string_lossy().to_string(),
                        "is_dir": true,
                    }));
                }
            }
        }
        // 2. 常见挂载点与存储目录
        for mount in &["/media", "/mnt", "/data", "/home", "/opt", "/var", "/"] {
            let p = std::path::Path::new(mount);
            if p.is_dir() && !roots.iter().any(|r| r["path"] == *mount) {
                roots.push(json!({
                    "name": format!("📁 {}", mount),
                    "path": mount.to_string(),
                    "is_dir": true,
                }));
            }
        }

        return Ok(Json(PlayerResponse::ok(json!({
            "current_path": "/",
            "parent_path": null,
            "dirs": roots,
            "audio_count": 0,
        }))));
    }

    let p = std::path::Path::new(&raw_path);
    if !p.exists() || !p.is_dir() {
        return Err(ApiError::not_found(&format!(
            "Directory not found: {}",
            raw_path
        )));
    }

    let parent_path = p.parent().map(|parent| {
        let s = parent.to_string_lossy().to_string();
        if s.is_empty() {
            "/".to_string()
        } else {
            s
        }
    });

    let mut dirs = Vec::new();
    let mut audio_count = 0usize;

    static AUDIO_EXTENSIONS: &[&str] = &[
        "flac", "wav", "mp3", "m4a", "dsf", "dff", "ape", "mac", "ogg", "wma", "alac", "aac",
        "iso", "dts", "ac3", "wv", "mid", "midi", "mqa",
    ];

    if let Ok(entries) = std::fs::read_dir(p) {
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            // 忽略隐藏文件与系统临时目录
            if file_name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                let has_children = std::fs::read_dir(&path)
                    .map(|mut it| it.next().is_some())
                    .unwrap_or(false);
                dirs.push(json!({
                    "name": file_name.to_string(),
                    "path": path.to_string_lossy().to_string(),
                    "has_children": has_children,
                    "is_dir": true,
                }));
            } else if path.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    let ext_lower = ext.to_ascii_lowercase();
                    if AUDIO_EXTENSIONS.contains(&ext_lower.as_str()) || ext_lower == "cue" {
                        audio_count += 1;
                    }
                }
            }
        }
    }

    dirs.sort_by(|a, b| {
        let name_a = a["name"].as_str().unwrap_or_default().to_lowercase();
        let name_b = b["name"].as_str().unwrap_or_default().to_lowercase();
        name_a.cmp(&name_b)
    });

    Ok(Json(PlayerResponse::ok(json!({
        "current_path": raw_path,
        "parent_path": parent_path,
        "dirs": dirs,
        "audio_count": audio_count,
    }))))
}

// -------------------------------------------------------------------
// 测试辅助
// -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_response_ok_shape() {
        let resp = PlayerResponse::ok(json!({ "status": "playing" }));
        assert!(resp.success);
        assert!(resp.data.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn player_response_err_shape() {
        let resp = PlayerResponse::err(ApiError::bad_request("boom"));
        assert!(!resp.success);
        assert!(resp.data.is_none());
        assert!(resp.error.is_some());
    }

    #[test]
    fn cover_id_traversal_rejected() {
        // 合法 id：local:{md5:x} / 缓存文件名（含扩展名）
        assert!(is_cover_id_safe("local:1a2b3c"));
        assert!(is_cover_id_safe("d41d8cd98f00b204e9800998ecf8427e.jpg"));
        // 穿越：分隔符与相对成分
        assert!(!is_cover_id_safe("../etc/passwd"));
        assert!(!is_cover_id_safe("..%2Fetc%2Fpasswd")); // 解码后含 '/'
        assert!(!is_cover_id_safe("a\\b"));
        assert!(!is_cover_id_safe(".."));
        assert!(!is_cover_id_safe(""));
    }

    #[test]
    fn query_path_traversal_rejected() {
        assert!(!path_has_traversal("/music/album/01.flac"));
        assert!(!path_has_traversal("cue://abc|0|120000|1"));
        assert!(path_has_traversal("/music/../secret.txt"));
        assert!(path_has_traversal(".."));
    }
}
