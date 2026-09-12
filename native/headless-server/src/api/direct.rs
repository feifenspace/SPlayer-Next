//! Source Direct 端点：stage/cancel/commit 与在线源 memfd 物化。

//! REST API 控制器
//!
//! 基于 Axum 0.8 的路由定义，提供播放控制、状态查询、扫描和 WebSocket 端点。

use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};

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

/// 本地音源整曲物化进 memfd 纯内存缓存（L2 纯内存：handoff/stage/full-reconnect
/// 均从 RAM 打开，播放期零 CIFS/磁盘 IO；设备无 swap，memfd 页面即常驻内存，
/// 无需 mlock 兜底）。`Ok(None)` = 空文件/超上限/memfd 不可用，调用方回退
/// 路径模式（行为不变，仅多一次源读取）。`abort` 在每个 chunk 边界检查
/// （预载失效/新 load 抢占时即时退出，不占线程读完全程）
pub(crate) fn materialize_local_to_ram(
    path: &str,
    max_bytes: u64,
    abort: impl Fn() -> bool,
) -> anyhow::Result<Option<DirectInput>> {
    use anyhow::Context as _;
    use std::io::{Read, Write};

    let mut source =
        std::fs::File::open(path).with_context(|| format!("打开待物化文件失败: {path}"))?;
    let len = source.metadata()?.len();
    if len == 0 || len > max_bytes {
        tracing::info!(path, len, max_bytes, "本地音源超出 RAM 物化上限，回退路径模式");
        return Ok(None);
    }

    #[cfg(target_os = "linux")]
    match create_memfd_file() {
        Ok((mut file, fd_path)) => {
            let mut chunk = vec![0u8; 512 * 1024];
            loop {
                if abort() {
                    anyhow::bail!("本地音源物化已中止（被新请求取代/预载失效）");
                }
                let n = source.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                file.write_all(&chunk[..n])?;
            }
            file.flush()?;
            tracing::info!(path, len, fd_path = %fd_path, "本地音源已物化进 memfd 纯内存缓存");
            return Ok(Some(DirectInput::Memfd { file, path: fd_path }));
        }
        Err(error) => {
            tracing::warn!(%error, "memfd 不可用，本地音源回退路径模式");
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = max_bytes;
    }
    Ok(None)
}

/// 本地源是否为原生 DSD（DSF/DFF/SACD ISO，含 CUE 指向 DSD 母版的情况）。
/// 原生 DSD 不做 memfd 物化：DSD 家族 stage/handoff 有独立通道（direct_dsd），
/// 且 DSD 源不经 PCM preload 语义
fn local_source_is_native_dsd(source: &str) -> bool {
    let physical = audio_engine_core::cue::parse_cue_virtual_path(source)
        .map(|cue| cue.physical_path)
        .unwrap_or_else(|| source.to_owned());
    let ext = Path::new(&physical)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(ext.as_str(), "dsf" | "dff" | "dsdiff" | "iso")
        || physical.contains(".iso|")
        || physical.contains(".ISO|")
        || physical.to_ascii_lowercase().contains(".iso#")
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

/// 把 reader 内容写入 writer，超过 DIRECT_PRELOAD_MAX_BYTES 即超限返回；
/// 每个 chunk 边界检查 abort 谓词（新 load/预载失效时即时中止，不占线程
/// 与带宽跑满全程）。返回（已写入字节数, 是否超限）
fn copy_with_abort(
    reader: &mut impl std::io::Read,
    writer: &mut impl std::io::Write,
    expected_bytes: Option<u64>,
    abort: impl Fn() -> bool,
) -> anyhow::Result<(u64, bool)> {
    if let Some(expected) = expected_bytes {
        if expected > DIRECT_PRELOAD_MAX_BYTES {
            return Ok((expected, true));
        }
    }

    let mut chunk = vec![0u8; 256 * 1024];
    let mut written: u64 = 0;
    loop {
        if abort() {
            anyhow::bail!("在线音源下载已中止（被新请求取代/预载失效）");
        }
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            if let Some(expected) = expected_bytes {
                if written != expected {
                    anyhow::bail!(
                        "在线音源下载不完整：收到 {written} 字节，HTTP 声明应为 {expected} 字节"
                    );
                }
            }
            return Ok((written, false));
        }
        writer.write_all(&chunk[..n])?;
        written += n as u64;
        if written > DIRECT_PRELOAD_MAX_BYTES {
            return Ok((written, true));
        }
    }
}

/// 将远端 HTTP(S) 音频流物化到本地可解码输入（preload 模式：优先 memfd 纯内存，
/// memfd 不可用时回退磁盘缓存），以便 Diretta Source Direct 模式进行精确解码与传输。
///
/// 下载通道：优先 Range 流式读取器（HttpAudioSource：单次读空闲 10s 超时、
/// 断流指数退避重连、cancel 即时中断），无整体时长上限——慢速大文件
/// （DSD 整轨数百 MB）合法耗用任意时长；打开失败（典型：服务器不支持
/// Range）回退一次性 GET（60s 整体超时，小文件兜底）。`abort` 在每个
/// chunk 边界检查；`cancel` 供 Range 通道即时掐断在途读/重连
pub(crate) fn materialize_direct_input(
    url: &str,
    cancel: &audio_engine_core::HttpCancelHandle,
    abort: impl Fn() -> bool,
) -> anyhow::Result<DirectInput> {
    use std::fs::{self, File};
    use std::io::Write;

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

    let (mut reader, expected_bytes): (Box<dyn std::io::Read>, Option<u64>) = {
        match audio_engine_core::ffmpeg_audio::HttpAudioSource::new_with_cancel_handle(
            url,
            cancel.clone(),
        ) {
            Ok(mut source) => {
                // HttpAudioSource 只把总长度保存在私有字段中。通过 SeekFrom::End(0)
                // 取出已由 Content-Range 验证的长度，再回到起点；末尾 seek 不会发
                // 网络请求，回到 0 会建立新的、可从头读取的 Range 响应。
                let expected = source.seek(SeekFrom::End(0))?;
                source.seek(SeekFrom::Start(0))?;
                (Box::new(source), Some(expected))
            }
            Err(error) => {
                tracing::warn!(url = %url, %error, "Range 流式下载通道不可用，回退一次性 GET");
                let client = reqwest::blocking::Client::builder()
                    .connect_timeout(std::time::Duration::from_secs(10))
                    .timeout(std::time::Duration::from_secs(60))
                    .build()?;
                let response = client
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
                // 扩展名嗅探仅此分支可行：Range 通道不暴露响应头
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
                let expected = response.content_length();
                (Box::new(response), expected)
            }
        }
    };
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

    // 磁盘缓存未命中：优先下载到 memfd 纯内存缓存（零磁盘 IO）。仅 memfd
    // 不可用（创建失败/非 Linux）时回退磁盘——一旦开始写 memfd，响应体已被
    // 消费，磁盘回退只能拿到不完整内容，中途失败直接报错
    #[cfg(target_os = "linux")]
    {
        match create_memfd_file() {
            Ok((mut file, path)) => {
                let (written, exceeded) =
                    copy_with_abort(&mut reader, &mut file, expected_bytes, &abort)?;
                if exceeded {
                    anyhow::bail!("在线音源超过 preload 大小上限 {DIRECT_PRELOAD_MAX_BYTES} 字节");
                }
                file.flush()?;
                tracing::info!(url = %url, path = %path, written, "在线音源已下载至 memfd 纯内存缓存");
                return Ok(DirectInput::Memfd { file, path });
            }
            Err(error) => {
                tracing::warn!(url = %url, %error, "memfd 不可用，回退磁盘缓存");
            }
        }
    }

    let part_file = cache_dir.join(format!("{}.{}.part", hash, ext));
    let mut file = File::create(&part_file)?;
    let (written, exceeded) =
        copy_with_abort(&mut reader, &mut file, expected_bytes, &abort)?;
    file.sync_all()?;
    drop(file);

    if exceeded {
        let _ = fs::remove_file(&part_file);
        anyhow::bail!("在线音源超过 preload 大小上限 {DIRECT_PRELOAD_MAX_BYTES} 字节");
    }

    fs::rename(&part_file, &target_file)?;
    tracing::info!(url = %url, path = %target_file.to_string_lossy(), written, "在线音源已下载至磁盘缓存");
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
    /// 前端曲目 id：boundary 转正后随 WS 带回，前端按 id 采纳新曲
    #[serde(default)]
    pub track_id: Option<String>,
}

/// Direct 提交切歌边界请求体
#[derive(Debug, Deserialize)]
pub struct DirectCommitBoundaryRequest {
    pub source: String,
    pub duration_secs: f64,
}

/// stage 核心入参：REST handler 与 direct_preloader 共用
pub(crate) struct DirectStageInput {
    pub source: String,
    pub duration_secs: f64,
    pub generation: u64,
    /// 候选曲元数据（None = 不更新 staged_meta）
    pub meta: Option<serde_json::Value>,
}

/// stage 核心结果：
/// - `Ok(None)`：已 stage
/// - `Ok(Some(reason))`：未 stage 且非错误（runtime 不活跃 / stream 模式跳过）
/// - `Err`：stage 被拒（典型 wire format 不一致）
///
/// 同步阻塞（内部含 HTTP 物化与 producer 握手等待），调用方需在
/// `spawn_isolated_blocking` 或专用阻塞线程上执行。
/// `abort`：物化下载的中止谓词（每个 chunk 边界检查）；预载链路传
/// preload token 失效检查，重新调度时旧下载即时退出
pub(crate) fn stage_direct_core(
    state: &AppState,
    input: DirectStageInput,
    abort: impl Fn() -> bool,
) -> anyhow::Result<Option<&'static str>> {
    let DirectStageInput {
        source,
        duration_secs: duration,
        generation,
        meta,
    } = input;

    let Some(handle) = state.player.lock().direct_stage_handle() else {
        return Ok(Some("Direct runtime inactive"));
    };

    let is_http = source.starts_with("http://") || source.starts_with("https://");
    // stream 模式下在线源不做全量下载 stage（与用户选择的流式策略冲突）
    if is_http && online_source_mode(state) == "stream" {
        return Ok(Some("onlineSourceMode=stream skips online preloading"));
    }

    // cue:// 候选必须先查库重写为「物理路径|start|dur|track」管道格式：
    // 引擎 stage/handoff 只认管道格式，原始 cue:// 下传 ffmpeg 会报
    // Protocol not found（与 load 路径的 resolve_cue_source 同语义）
    let source = if !is_http && source.starts_with("cue://") {
        let resolved = {
            let conn = state.db.lock();
            crate::db::get_track_by_path(&conn, &source)
                .ok()
                .flatten()
                .and_then(|track| {
                    let audio_path = track.cue_audio_path.clone()?;
                    let start_sec = track.cue_start_ms.unwrap_or(0) as f64 / 1000.0;
                    let dur_sec = track.duration as f64 / 1000.0;
                    let track_num = track.track.unwrap_or(1);
                    Some(format!("{audio_path}|{start_sec:.3}|{dur_sec:.3}|{track_num}"))
                })
        };
        match resolved {
            Some(v) => v,
            None => anyhow::bail!(
                "cue:// 候选不在曲库中（或缺少母版路径），无法 stage: {source}"
            ),
        }
    } else {
        source
    };

    // HTTP 源预先物化为本地可 seek 输入（native stage 仅支持本地源）。
    // cancel 用一次性句柄：本链路的中止由 abort 谓词（预载失效令牌）承担。
    // HTTP 物化产物路径兼作 stage 逻辑源（原行为）：扩展名/DSD 家族嗅探依赖
    // 真实扩展名，URL 不可靠
    //
    // 本地源 + ram_preload：整曲物化进 memfd 后 stage（gapless 预载解码器从
    // RAM 供数，消除 stage/边界切换期的 CIFS 流式读）。CUE 管道格式解析仍由
    // 引擎基于 source 完成，打开走 memfd（stage_local 的 open_path 分离）；
    // 原生 DSD 不物化（独立 direct_dsd 通道）；物化超限/失败回退路径模式
    let (stage_source, physical_source, stage_open_path) = if is_http {
        let input = materialize_direct_input(
            &source,
            &audio_engine_core::HttpCancelHandle::new(),
            abort,
        )
        .map_err(|e| anyhow::anyhow!("Failed to preload stream to RAM: {e}"))?;
        // 保留 URL 作为逻辑源以识别 DSF/DFF/SACD；实际打开使用 memfd/缓存路径。
        // /proc/self/fd/N 没有扩展名，不能作为 Native DSD 家族判断依据。
        let open_path = input.path().to_owned();
        (source.clone(), input, Some(open_path))
    } else {
        let ram_preload = state.config.playback.ram_preload;
        let ram_max_bytes = state.config.resolved_ram_preload_max_bytes();
        if ram_preload && !local_source_is_native_dsd(&source) {
            let preload_physical = audio_engine_core::cue::parse_cue_virtual_path(&source)
                .map(|cue| cue.physical_path)
                .unwrap_or_else(|| source.clone());
            match materialize_local_to_ram(&preload_physical, ram_max_bytes as u64, &abort) {
                Ok(Some(input)) => {
                    let open_path = input.path().to_owned();
                    (source.clone(), input, Some(open_path))
                }
                Ok(None) => (source.clone(), DirectInput::Path(source.clone()), None),
                Err(error) => {
                    tracing::warn!(
                        source = %source,
                        %error,
                        "本地源 memfd 物化失败，回退路径模式 stage"
                    );
                    (source.clone(), DirectInput::Path(source.clone()), None)
                }
            }
        } else {
            (source.clone(), DirectInput::Path(source.clone()), None)
        }
    };

    // 候选元数据直传存 staged_meta：boundary 无缝切换后 now-playing 快照立即
    // 显示新曲信息。source 用请求原始串，与 boundary commit 后的 current_source 一致
    if let Some(meta) = meta {
        *state.staged_meta.lock() = Some((generation, meta));
    }

    // DirectInput（含 memfd 的 File）随闭包存活整个 stage 过程：
    // producer 在此期间同步完成解码器打开（prepare 线程在 stage_local 返回前
    // 已完成），路径解析始终有 fd 支撑；解码器打开后持自身 fd，锚点可释放
    let _ = &physical_source; // fd 存活锚点（Drop 即关 fd），显式引用消除 unused 警告
    match handle.stage_local(&stage_source, stage_open_path.as_deref(), duration, generation) {
        Ok(()) => {
            tracing::info!(source = %source, generation, "无缝候选已 stage");
            Ok(None)
        }
        Err(err) => {
            // 常见拒绝原因：wire format 与当前连接不一致（采样率/位深跳变，
            // 引擎拒绝跨格式无缝）——这条日志用于与曲终自动连播、输出停滞关联
            tracing::info!(source = %source, error = %err, "无缝 stage 被拒，回退曲终接力");
            Err(err)
        }
    }
}

/// Diretta 预加载下一曲（支持远程流媒体预先下载至 RAM）
pub(crate) async fn direct_stage_next_handler(
    State(state): State<AppState>,
    Json(payload): Json<DirectStageRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let source = payload.source.clone();
    let duration = payload.duration_secs.unwrap_or(0.0);
    let generation = payload.generation.unwrap_or(0);
    let meta = json!({
        "source": source.clone(),
        "title": payload.title,
        "artist": payload.artist,
        "album": payload.album,
        "cover": payload.cover,
        "duration_secs": duration,
        "track_id": payload.track_id,
    });

    let state_for_worker = state.clone();
    let outcome = spawn_isolated_blocking("direct-stage-next-worker", move || {
        stage_direct_core(
            &state_for_worker,
            DirectStageInput {
                source,
                duration_secs: duration,
                generation,
                meta: Some(meta),
            },
            // 旧前端驱动链路无预载失效令牌：物化下载不做中途取消
            || false,
        )
    })
    .await
    .map_err(|e| ApiError::internal(format!("Stage next worker error: {e}")))?
    .map_err(|err| ApiError::bad_request(format!("Direct stage failed: {err}")))?;

    match outcome {
        None => Ok(Json(PlayerResponse::ok(json!({
            "staged": true,
            "source": payload.source,
            "generation": generation,
        })))),
        Some(reason) => Ok(Json(PlayerResponse::ok(json!({
            "staged": false,
            "reason": reason,
        })))),
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

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::copy_with_abort;

    #[test]
    fn preload_rejects_truncated_content_with_known_length() {
        let mut reader = Cursor::new(b"partial".to_vec());
        let mut output = Vec::new();
        let error = copy_with_abort(&mut reader, &mut output, Some(8), || false)
            .expect_err("truncated HTTP response must not be accepted");

        assert!(error.to_string().contains("下载不完整"));
        assert_eq!(output, b"partial");
    }

    #[test]
    fn preload_accepts_complete_content_with_known_length() {
        let mut reader = Cursor::new(b"complete".to_vec());
        let mut output = Vec::new();
        let (written, exceeded) =
            copy_with_abort(&mut reader, &mut output, Some(8), || false).expect("complete data");

        assert_eq!(written, 8);
        assert!(!exceeded);
        assert_eq!(output, b"complete");
    }
}
