//! Source Direct 端点：stage/cancel/commit 与在线源 memfd 物化。

//! REST API 控制器
//!
//! 基于 Axum 0.8 的路由定义，提供播放控制、状态查询、扫描和 WebSocket 端点。

use std::path::PathBuf;

use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::json;

use super::{spawn_isolated_blocking, PlayerResponse};
use crate::error::ApiError;
use crate::state::AppState;

/// 获取流媒体音频 RAM 内存缓冲目录（优先 Linux /dev/shm 内存文件系统，彻底规避磁盘写入磨损与物理磁盘空间占用）
pub(crate) fn get_stream_cache_dir() -> std::path::PathBuf {
    let candidate_dirs = [
        PathBuf::from("/dev/shm/splayer-headless-ram/streams"),
        std::env::temp_dir().join("splayer-stream-cache"),
        PathBuf::from("/opt/splayer-headless/data/cache/streams"),
        PathBuf::from("data/cache/streams"),
    ];

    for dir in &candidate_dirs {
        if std::fs::create_dir_all(dir).is_ok() {
            let test_file = dir.join(".write_test");
            if std::fs::write(&test_file, b"ok").is_ok() {
                let _ = std::fs::remove_file(test_file);
                return dir.clone();
            }
        }
    }

    std::env::temp_dir().join("splayer-stream-cache")
}

/// 自动清理过期的流媒体内存缓存，只保留当前播放曲目与下一首预载曲目（最多保留 2 首，其余立刻从内存释放）
pub(crate) fn clean_old_stream_cache(cache_dir: &std::path::Path) {
    if let Ok(entries) = std::fs::read_dir(cache_dir) {
        let mut files: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if path.to_string_lossy().ends_with(".part") {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                if let Ok(meta) = entry.metadata() {
                    let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                    files.push((path, mtime));
                }
            }
        }
        // 纯内存模式：严格限制最多保留 2 首，防止物理 RAM 溢出
        if files.len() > 2 {
            files.sort_by_key(|(_, mtime)| *mtime);
            let to_remove = files.len() - 2;
            for (p, _) in files.into_iter().take(to_remove) {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

/// 读取在线源播放模式设置：`preload`（默认，全量下载后播放，可无缝 stage）
/// 或 `stream`（边下边播，起播快但切曲重建连接）
pub(crate) fn online_source_mode(state: &AppState) -> String {
    let conn = state.db.lock();
    // 前端设置绑定路径为 system.player.onlineSourceMode，DB 键与绑定路径逐字对应；
    // 兼容无前缀写法
    for key in ["system.player.onlineSourceMode", "player.onlineSourceMode"] {
        if let Ok(Some(v)) = crate::db::get_setting(&conn, key) {
            if let Some(mode) = v.as_str() {
                return mode.to_string();
            }
        }
    }
    "preload".to_string()
}

/// memfd 物化产物：`file` 是 fd N 的唯一持有者——`/proc/self/fd/N` 路径的解析
/// 依赖该 fd 存活，调用方必须在通过路径打开 FFmpeg 的整个期间持有本值
/// （fd 关闭后编号可能被复用，路径会静默指向别的文件）；FFmpeg 打开成功后
/// 解码器持有自己的文件描述符，本值即可释放
pub(crate) enum DirectInput {
    /// `file` 从不被读取——它是 fd N 的存活锚点（Drop 即关闭 fd），
    /// 路径 `/proc/self/fd/N` 的解析依赖它存活，dead_code 为有意误报
    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    Memfd {
        file: std::fs::File,
        path: String,
    },
    Path(String),
}

impl DirectInput {
    pub(crate) fn path(&self) -> &str {
        match self {
            #[cfg(target_os = "linux")]
            Self::Memfd { path, .. } => path,
            Self::Path(path) => path,
        }
    }
}

/// preload 物化大小上限：防失控响应打爆内存（memfd 映射的就是 RAM），
/// 覆盖 DSD128 整轨约 2.5GB/h 的量级
pub(crate) const DIRECT_PRELOAD_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// 创建 memfd 匿名内存文件（仅创建、不读取响应；失败即 memfd 不可用，响应可安全回退磁盘）
#[cfg(target_os = "linux")]
pub(crate) fn create_memfd_file() -> anyhow::Result<(std::fs::File, String)> {
    use std::os::fd::FromRawFd;

    let name = c"splayer-stream-cache";
    let fd = unsafe { libc::memfd_create(name.as_ptr() as *const _, libc::MFD_CLOEXEC) };
    if fd < 0 {
        anyhow::bail!("memfd_create 失败: {}", std::io::Error::last_os_error());
    }
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    // memfd 无需落盘 sync；路径必须在 File 移入注册表前用 fd 值构造
    Ok((file, format!("/proc/self/fd/{fd}")))
}

/// 把已建立的响应写入 memfd 文件，超过 DIRECT_PRELOAD_MAX_BYTES 即失败。
/// 响应体自此被消费，调用方不得再将其用于磁盘回退
#[cfg(target_os = "linux")]
pub(crate) fn write_response_to_memfd(
    file: &mut std::fs::File,
    response: &mut reqwest::blocking::Response,
) -> anyhow::Result<()> {
    use std::io::{Read, Write};

    let mut limited = response.take(DIRECT_PRELOAD_MAX_BYTES);
    std::io::copy(&mut limited, file)?;
    anyhow::ensure!(
        limited.limit() > 0,
        "在线音源超过 preload 大小上限 {DIRECT_PRELOAD_MAX_BYTES} 字节"
    );
    file.flush()?;
    Ok(())
}

/// 将远端 HTTP(S) 音频流物化到本地可解码输入（preload 模式：优先 memfd 纯内存，
/// memfd 不可用时回退磁盘缓存），以便 Diretta Source Direct 模式进行精确解码与传输
pub(crate) fn materialize_direct_input(url: &str) -> anyhow::Result<DirectInput> {
    use std::fs::{self, File};
    use std::io::Read;

    let cache_dir = get_stream_cache_dir();
    clean_old_stream_cache(&cache_dir);

    let hash = format!("{:x}", md5::compute(url.as_bytes()));
    let mut ext = if url.contains(".flac") {
        "flac"
    } else if url.contains(".mp3") {
        "mp3"
    } else if url.contains(".m4a") || url.contains(".aac") {
        "m4a"
    } else if url.contains(".wav") {
        "wav"
    } else if url.contains(".dsf") {
        "dsf"
    } else if url.contains(".dff") {
        "dff"
    } else {
        ""
    };

    if !ext.is_empty() {
        let target_file = cache_dir.join(format!("{}.{}", hash, ext));
        if target_file.exists()
            && fs::metadata(&target_file)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        {
            return Ok(DirectInput::Path(target_file.to_string_lossy().to_string()));
        }
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    let mut response = client
        .get(url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        )
        .header("Accept", "*/*")
        .header("Accept-Encoding", "identity")
        .send()?;

    if !response.status().is_success() {
        anyhow::bail!("下载在线流媒体音频失败: HTTP {}", response.status());
    }

    if ext.is_empty() {
        if let Some(ct) = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
        {
            let ct = ct.to_lowercase();
            if ct.contains("flac") {
                ext = "flac";
            } else if ct.contains("mpeg") || ct.contains("mp3") {
                ext = "mp3";
            } else if ct.contains("mp4") || ct.contains("m4a") || ct.contains("aac") {
                ext = "m4a";
            } else if ct.contains("wav") {
                ext = "wav";
            } else if ct.contains("dsf") {
                ext = "dsf";
            }
        }
    }
    if ext.is_empty() {
        ext = "audio";
    }

    let target_file = cache_dir.join(format!("{}.{}", hash, ext));
    if target_file.exists()
        && fs::metadata(&target_file)
            .map(|m| m.len() > 0)
            .unwrap_or(false)
    {
        return Ok(DirectInput::Path(target_file.to_string_lossy().to_string()));
    }

    // 磁盘缓存未命中：优先下载到 memfd 纯内存缓存（零磁盘 IO，与 header 嗅探共用
    // 同一次 GET）。仅 memfd 不可用（创建失败/非 Linux）时回退磁盘——一旦开始写
    // memfd，响应体已被消费，磁盘回退只能拿到不完整内容，中途失败直接报错
    #[cfg(target_os = "linux")]
    {
        match create_memfd_file() {
            Ok((mut file, path)) => {
                write_response_to_memfd(&mut file, &mut response)?;
                tracing::info!(url = %url, path = %path, "在线音源已下载至 memfd 纯内存缓存");
                return Ok(DirectInput::Memfd { file, path });
            }
            Err(error) => {
                tracing::warn!(url = %url, %error, "memfd 不可用，回退磁盘缓存");
            }
        }
    }

    let part_file = cache_dir.join(format!("{}.{}.part", hash, ext));
    let mut file = File::create(&part_file)?;
    let mut limited = response.take(DIRECT_PRELOAD_MAX_BYTES);
    std::io::copy(&mut limited, &mut file)?;
    let exceeded = limited.limit() == 0;
    file.sync_all()?;
    drop(file);

    if exceeded {
        let _ = fs::remove_file(&part_file);
        anyhow::bail!("在线音源超过 preload 大小上限 {DIRECT_PRELOAD_MAX_BYTES} 字节");
    }

    fs::rename(&part_file, &target_file)?;
    Ok(DirectInput::Path(target_file.to_string_lossy().to_string()))
}

/// Direct 载入成功后的统一响应体
pub(crate) fn direct_load_response(
    source: &str,
    auto_play: bool,
    meta: audio_engine_core::AudioMetadata,
) -> Json<PlayerResponse> {
    let has_cover = meta.cover_raw.is_some() || meta.cover.is_some();
    Json(PlayerResponse::ok(json!({
        "status": if auto_play { "playing" } else { "paused" },
        "source": source,
        "title": meta.title,
        "artist": meta.artist,
        "album": meta.album,
        "duration": meta.duration_secs,
        "sample_rate": meta.sample_rate,
        "original_sample_rate": meta.original_sample_rate,
        "channels": meta.channels,
        "bits_per_sample": meta.bits_per_sample,
        "bit_rate": meta.bit_rate,
        "codec": meta.codec,
        "cover": meta.cover,
        "has_cover": has_cover,
        "has_embedded_lyric": meta.embedded_lyric.is_some(),
    })))
}

/// 播放中同格式 handoff 编排已下沉 core（InnerPlayer::try_direct_handoff），
/// headless 与桌面 NAPI 共用同一实现

/// Direct 预加载请求体
#[derive(Debug, Deserialize)]
pub struct DirectStageRequest {
    pub source: String,
    pub duration_secs: Option<f64>,
    pub generation: Option<u64>,
    /// 候选曲元数据由前端直传（H1.5）：probe_fast 无法解析管道/CUE 虚拟格式
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub cover: Option<String>,
}

/// Direct 提交切歌边界请求体
#[derive(Debug, Deserialize)]
pub struct DirectCommitBoundaryRequest {
    pub source: String,
    pub duration_secs: f64,
}

/// Diretta 预加载下一曲（支持远程流媒体预先下载至 RAM）
pub(crate) async fn direct_stage_next_handler(
    State(state): State<AppState>,
    Json(payload): Json<DirectStageRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let source = payload.source;
    let duration = payload.duration_secs.unwrap_or(0.0);
    let generation = payload.generation.unwrap_or(0);

    let stage_handle = state.player.lock().direct_stage_handle();

    let Some(handle) = stage_handle else {
        return Ok(Json(PlayerResponse::ok(json!({
            "staged": false,
            "reason": "Direct runtime inactive",
        }))));
    };

    let is_http = source.starts_with("http://") || source.starts_with("https://");
    // stream 模式下在线源不做全量下载 stage（与用户选择的流式策略冲突）
    if is_http && online_source_mode(&state) == "stream" {
        return Ok(Json(PlayerResponse::ok(json!({
            "staged": false,
            "reason": "onlineSourceMode=stream skips online preloading",
        }))));
    }

    let physical_source = if is_http {
        let src_clone = source.clone();
        spawn_isolated_blocking("direct-stage-preload", move || {
            materialize_direct_input(&src_clone)
        })
        .await
        .map_err(|e| ApiError::internal(format!("Stage preload error: {e}")))?
        .map_err(|e| ApiError::bad_request(format!("Failed to preload stream to RAM: {e}")))?
    } else {
        DirectInput::Path(source.clone())
    };

    // 候选元数据直传存 staged_meta：boundary 无缝切换后 now-playing 快照立即
    // 显示新曲信息。source 用请求原始串，与 boundary commit 后的 current_source 一致
    *state.staged_meta.lock() = Some((
        generation,
        json!({
            "source": source.clone(),
            "title": payload.title,
            "artist": payload.artist,
            "album": payload.album,
            "cover": payload.cover,
            "duration_secs": duration,
        }),
    ));

    let result = spawn_isolated_blocking("direct-stage-next-worker", move || {
        // DirectInput（含 memfd 的 File）随闭包存活整个 stage 过程：
        // producer 在此期间同步打开解码器，路径解析始终有 fd 支撑
        handle.stage_local(physical_source.path(), duration, generation)
    })
    .await
    .map_err(|e| ApiError::internal(format!("Stage next worker error: {e}")))?;

    match result {
        Ok(()) => {
            tracing::info!(source = %source, generation, "无缝候选已 stage");
            Ok(Json(PlayerResponse::ok(json!({
                "staged": true,
                "source": source,
                "generation": generation,
            }))))
        }
        Err(err) => {
            // 常见拒绝原因：wire format 与当前连接不一致（采样率/位深跳变，
            // 引擎拒绝跨格式无缝）——这条日志用于与曲终自动连播、输出停滞关联
            tracing::info!(source = %source, error = %err, "无缝 stage 被拒，回退曲终接力");
            Err(ApiError::bad_request(format!("Direct stage failed: {err}")))
        }
    }
}

/// 取消已暂存的 Direct 下一曲预加载
pub(crate) async fn direct_cancel_next_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    if let Some(handle) = state.player.lock().direct_stage_handle() {
        handle.cancel();
    }
    Ok(Json(PlayerResponse::ok(json!({ "cancelled": true }))))
}

/// 提交已完成的 Direct Gapless 边界切换
pub(crate) async fn direct_commit_boundary_handler(
    State(state): State<AppState>,
    Json(payload): Json<DirectCommitBoundaryRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    state
        .player
        .lock()
        .commit_direct_gapless_boundary(&payload.source, payload.duration_secs)
        .map_err(|e| ApiError::bad_request(format!("Commit boundary failed: {e}")))?;
    Ok(Json(PlayerResponse::ok(json!({
        "committed": true,
        "source": payload.source,
        "duration": payload.duration_secs,
    }))))
}
