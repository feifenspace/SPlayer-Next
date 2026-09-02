//! SQLite 媒体库持久化模块
//!
//! 提供本地音乐库元数据（曲目、专辑、歌手、扫描目录）的持久化存储与查询支持。

use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use audio_engine_core::scanner::{FileRecord, ScannedTrack};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// 数据库曲目结构（对齐前端 Track 数据格式）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbTrack {
    pub id: String,
    pub source: String,
    pub path: String,
    pub cue_path: Option<String>,
    pub cue_audio_path: Option<String>,
    pub cue_start_ms: Option<u64>,
    pub cue_end_ms: Option<u64>,
    pub title: String,
    pub track: Option<u16>,
    pub artist: Option<String>,
    pub artists: Vec<DbArtist>,
    pub album: Option<DbAlbum>,
    pub duration: u64,
    pub cover: Option<String>,
    pub codec: Option<String>,
    pub sample_rate: Option<u32>,
    pub bit_rate: Option<i64>,
    pub channels: Option<u32>,
    pub bits_per_sample: Option<u32>,
    pub file_size: u64,
    pub file_mtime: Option<u64>,
    pub file_ctime: Option<u64>,
    pub scanned_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbArtist {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbAlbum {
    pub name: String,
    pub cover: Option<String>,
    pub artist: Option<String>,
}

/// 专辑聚合摘要
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumSummary {
    pub name: String,
    pub cover: Option<String>,
    pub artist: Option<String>,
    pub track_count: u32,
}

/// 歌手聚合摘要
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistSummary {
    pub name: String,
    pub track_count: u32,
}

/// 初始化数据库并建表
pub fn init_db(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create database directory: {:?}", parent))?;
    }

    let conn = Connection::open(db_path)
        .with_context(|| format!("Failed to open SQLite database at {:?}", db_path))?;

    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;

    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS tracks (
            id TEXT PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            title TEXT NOT NULL,
            track INTEGER,
            artist TEXT,
            album TEXT,
            duration INTEGER NOT NULL,
            cover TEXT,
            codec TEXT,
            sample_rate INTEGER,
            bit_rate INTEGER,
            channels INTEGER,
            bits_per_sample INTEGER,
            file_size INTEGER NOT NULL,
            file_mtime INTEGER,
            file_ctime INTEGER,
            scanned_at INTEGER NOT NULL,
            cue_path TEXT,
            cue_audio_path TEXT,
            cue_start_ms INTEGER,
            cue_end_ms INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_tracks_title ON tracks(title);
        CREATE INDEX IF NOT EXISTS idx_tracks_album ON tracks(album);
        CREATE INDEX IF NOT EXISTS idx_tracks_artist ON tracks(artist);

        CREATE TABLE IF NOT EXISTS scan_dirs (
            path TEXT PRIMARY KEY,
            added_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS playlists (
            id TEXT PRIMARY KEY,
            type TEXT NOT NULL DEFAULT 'local',
            title TEXT NOT NULL,
            description TEXT,
            cover TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_playlists_updated ON playlists(updated_at DESC);

        CREATE TABLE IF NOT EXISTS playlist_tracks (
            playlist_id TEXT NOT NULL,
            track_id TEXT NOT NULL,
            position INTEGER NOT NULL,
            added_at INTEGER NOT NULL,
            PRIMARY KEY (playlist_id, track_id)
        );
        CREATE INDEX IF NOT EXISTS idx_playlist_tracks_pos ON playlist_tracks(playlist_id, position);

        CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS play_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            track_id TEXT NOT NULL,
            source TEXT NOT NULL,
            started_at INTEGER NOT NULL,
            listened_ms INTEGER NOT NULL,
            track_json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_play_history_started ON play_history(started_at DESC);

        CREATE TABLE IF NOT EXISTS account_sessions (
            platform TEXT PRIMARY KEY,
            cookies TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );
        "#,
    )?;

    // 自动为已有数据库升级添加 CUE 字段
    let _ = conn.execute("ALTER TABLE tracks ADD COLUMN cue_path TEXT", []);
    let _ = conn.execute("ALTER TABLE tracks ADD COLUMN cue_audio_path TEXT", []);
    let _ = conn.execute("ALTER TABLE tracks ADD COLUMN cue_start_ms INTEGER", []);
    let _ = conn.execute("ALTER TABLE tracks ADD COLUMN cue_end_ms INTEGER", []);
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_tracks_cue_audio ON tracks(cue_audio_path)", []);

    Ok(conn)
}

/// 读取某平台的 session cookies（转换为键值对 Map）
pub fn get_account_cookies(conn: &Connection, platform: &str) -> std::collections::HashMap<String, String> {
    let query = "SELECT cookies FROM account_sessions WHERE platform = ?";
    let cookies_json: Option<String> = conn
        .query_row(query, [platform], |row| row.get(0))
        .optional()
        .unwrap_or(None);
    if let Some(json_str) = cookies_json {
        serde_json::from_str(&json_str).unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    }
}

/// 保存某平台的 session cookies
pub fn save_account_cookies(
    conn: &Connection,
    platform: &str,
    cookies: &std::collections::HashMap<String, String>,
) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let json_str = serde_json::to_string(cookies)?;
    conn.execute(
        r#"
        INSERT INTO account_sessions (platform, cookies, updated_at) VALUES (?, ?, ?)
        ON CONFLICT(platform) DO UPDATE SET
            cookies = excluded.cookies,
            updated_at = excluded.updated_at
        "#,
        params![platform, json_str, now],
    )?;
    Ok(())
}

/// 清除某平台的 session cookies
pub fn clear_account_cookies(conn: &Connection, platform: &str) -> Result<()> {
    conn.execute("DELETE FROM account_sessions WHERE platform = ?", [platform])?;
    Ok(())
}

/// 获取全部扫描目录列表
pub fn get_scan_dirs(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT path FROM scan_dirs ORDER BY added_at ASC")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut dirs = Vec::new();
    for row in rows {
        dirs.push(row?);
    }
    Ok(dirs)
}

/// 添加扫描目录
pub fn add_scan_dir(conn: &Connection, path: &str) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    conn.execute(
        "INSERT OR IGNORE INTO scan_dirs (path, added_at) VALUES (?1, ?2)",
        params![path, now],
    )?;
    Ok(())
}

/// 删除扫描目录及目录下所有关联歌曲
pub fn remove_scan_dir(conn: &Connection, path: &str) -> Result<()> {
    conn.execute("DELETE FROM scan_dirs WHERE path = ?1", params![path])?;
    let pattern = format!("{}%", path);
    conn.execute(
        "DELETE FROM tracks WHERE path LIKE ?1 OR path = ?2",
        params![pattern, path],
    )?;
    Ok(())
}

/// 获取增量对比所需的已有文件记录
pub fn get_file_records(conn: &Connection) -> Result<Vec<FileRecord>> {
    let mut stmt = conn.prepare("SELECT path, file_mtime, file_size, cover FROM tracks")?;
    let rows = stmt.query_map([], |row| {
        Ok(FileRecord {
            path: row.get(0)?,
            mtime: row.get::<_, Option<u64>>(1)?.unwrap_or(0),
            size: row.get(2)?,
            cover_path: row.get(3)?,
        })
    })?;

    let mut records = Vec::new();
    for row in rows {
        records.push(row?);
    }
    Ok(records)
}

/// 批量写入或更新扫描到的曲目
pub fn upsert_scanned_tracks(conn: &mut Connection, tracks: &[ScannedTrack]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            r#"
            INSERT INTO tracks (
                id, path, title, track, artist, album, duration,
                cover, codec, sample_rate, bit_rate, channels,
                bits_per_sample, file_size, file_mtime, file_ctime, scanned_at
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                ?8, ?9, ?10, ?11, ?12,
                ?13, ?14, ?15, ?16, ?17
            )
            ON CONFLICT(path) DO UPDATE SET
                title = excluded.title,
                track = excluded.track,
                artist = excluded.artist,
                album = excluded.album,
                duration = excluded.duration,
                cover = excluded.cover,
                codec = excluded.codec,
                sample_rate = excluded.sample_rate,
                bit_rate = excluded.bit_rate,
                channels = excluded.channels,
                bits_per_sample = excluded.bits_per_sample,
                file_size = excluded.file_size,
                file_mtime = excluded.file_mtime,
                file_ctime = excluded.file_ctime,
                scanned_at = excluded.scanned_at
            "#,
        )?;

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        for track in tracks {
            let id = format!("local:{:x}", md5_hash(&track.path));
            let title = track
                .title
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| {
                    Path::new(&track.path)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("Unknown Title")
                });

            let duration_ms = (track.duration * 1000.0) as u64;

            stmt.execute(params![
                id,
                track.path,
                title,
                track.track,
                track.artist.as_deref(),
                track.album.as_deref(),
                duration_ms,
                track.cover.as_deref(),
                track.codec,
                track.sample_rate,
                track.bit_rate,
                track.channels,
                track.bits_per_sample,
                track.file_size,
                track.mtime,
                track.ctime,
                now,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// 删除指定路径列表的曲目
pub fn delete_tracks_by_paths(conn: &mut Connection, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached("DELETE FROM tracks WHERE path = ?1")?;
        for p in paths {
            stmt.execute(params![p])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// 格式化封面 URL 为 Web 规范路径 (/api/v1/covers/xxx)
pub fn normalize_cover_url(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    let raw_trimmed = raw.trim();
    if raw_trimmed.is_empty() {
        return None;
    }
    if raw_trimmed.starts_with("http://") || raw_trimmed.starts_with("https://") || raw_trimmed.starts_with("/api/v1/covers/") {
        return Some(raw_trimmed.to_string());
    }
    if let Some(stripped) = raw_trimmed.strip_prefix("cache://covers/") {
        return Some(format!("/api/v1/covers/{}", stripped));
    }
    if let Some(stripped) = raw_trimmed.strip_prefix("cache://") {
        return Some(format!("/api/v1/covers/{}", stripped));
    }
    let p = std::path::Path::new(raw_trimmed);
    if let Some(fname) = p.file_name().and_then(|f| f.to_str()) {
        return Some(format!("/api/v1/covers/{}", fname));
    }
    Some(format!("/api/v1/covers/{}", raw_trimmed))
}

/// 同步解析 CUE 文件并向 tracks 写入虚拟分轨记录
pub fn sync_cue_tracks(conn: &mut Connection, cue_files: &[String], cover_cache_dir: Option<&Path>) -> Result<usize> {
    if cue_files.is_empty() {
        return Ok(0);
    }

    let tx = conn.transaction()?;
    let mut total_synced = 0;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let cache_dir_str = cover_cache_dir
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "data/covers".to_string());

    {
        let mut insert_stmt = tx.prepare_cached(
            r#"
            INSERT INTO tracks (
                id, path, title, track, artist, album, duration,
                cover, codec, sample_rate, bit_rate, channels,
                bits_per_sample, file_size, file_mtime, file_ctime, scanned_at,
                cue_path, cue_audio_path, cue_start_ms, cue_end_ms
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                ?8, ?9, ?10, ?11, ?12,
                ?13, ?14, ?15, ?16, ?17,
                ?18, ?19, ?20, ?21
            )
            ON CONFLICT(path) DO UPDATE SET
                title = excluded.title,
                track = excluded.track,
                artist = excluded.artist,
                album = excluded.album,
                duration = excluded.duration,
                cover = excluded.cover,
                codec = excluded.codec,
                sample_rate = excluded.sample_rate,
                bit_rate = excluded.bit_rate,
                channels = excluded.channels,
                bits_per_sample = excluded.bits_per_sample,
                file_size = excluded.file_size,
                file_mtime = excluded.file_mtime,
                file_ctime = excluded.file_ctime,
                scanned_at = excluded.scanned_at,
                cue_path = excluded.cue_path,
                cue_audio_path = excluded.cue_audio_path,
                cue_start_ms = excluded.cue_start_ms,
                cue_end_ms = excluded.cue_end_ms
            "#,
        )?;

        for cue_file in cue_files {
            let cue_path_obj = Path::new(cue_file);
            if !cue_path_obj.is_file() {
                continue;
            }

            let cue_sheet = match audio_engine_core::cue::CueSheet::parse_file(cue_file) {
                Ok(sheet) => sheet,
                Err(err) => {
                    tracing::warn!("解析 CUE 文件失败 [{}]: {}", cue_file, err);
                    continue;
                }
            };

            let (cue_mtime, cue_ctime) = audio_engine_core::scanner::file_stat(cue_path_obj)
                .map(|(m, c, _)| (m, c))
                .unwrap_or((now, now));

            // 从 CUE 文件所在目录智能提取封面
            let folder_cover_from_cue = audio_engine_core::metadata::extract_folder_cover_thumbnail(
                cue_file,
                &cache_dir_str,
            );

            if cue_sheet.tracks.is_empty() {
                continue;
            }

            for cue_track in &cue_sheet.tracks {
                let physical_str = cue_track.physical_path.to_string_lossy().to_string();

                // 查库获取母版音频参数
                let parent_meta: Option<(u64, Option<String>, Option<String>, Option<u32>, Option<i64>, Option<u32>, Option<u32>, u64)> = tx
                    .query_row(
                        "SELECT duration, cover, codec, sample_rate, bit_rate, channels, bits_per_sample, file_size FROM tracks WHERE path = ?1",
                        [&physical_str],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
                    )
                    .optional()
                    .unwrap_or(None);

                let (parent_dur_ms, cover, codec, sample_rate, bit_rate, channels, bits_per_sample, file_size) = match parent_meta {
                    Some(m) => m,
                    None => {
                        if let Some(scanned) = audio_engine_core::scanner::probe_fast(&physical_str, Some(&cache_dir_str)) {
                            (
                                (scanned.duration * 1000.0) as u64,
                                scanned.cover,
                                Some(scanned.codec),
                                Some(scanned.sample_rate),
                                Some(scanned.bit_rate),
                                Some(scanned.channels),
                                Some(scanned.bits_per_sample),
                                scanned.file_size,
                            )
                        } else {
                            (0, None, Some("wav".to_string()), Some(44100), Some(1411200), Some(2), Some(16), 0)
                        }
                    }
                };

                let effective_cover = cover
                    .or_else(|| folder_cover_from_cue.clone())
                    .or_else(|| audio_engine_core::metadata::extract_folder_cover_thumbnail(&physical_str, &cache_dir_str));
                let cover_url = normalize_cover_url(effective_cover);

                let cue_start_ms = (cue_track.start_time * 1000.0) as u64;
                let duration_ms = if let Some(dur_sec) = cue_track.duration {
                    (dur_sec * 1000.0) as u64
                } else if parent_dur_ms > cue_start_ms {
                    parent_dur_ms - cue_start_ms
                } else {
                    0
                };
                let cue_end_ms = cue_start_ms + duration_ms;

                let track_virtual_path = format!("cue://{}#track={:02}", cue_file, cue_track.track_num);
                let id = format!("local:{:x}", md5_hash(&track_virtual_path));

                let title = cue_track
                    .title
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| format!("Track {:02}", cue_track.track_num));

                let artist = cue_track
                    .artist
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| cue_sheet.global_performer.clone());

                let album = cue_sheet
                    .global_title
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| {
                        cue_path_obj
                            .parent()
                            .and_then(|p| p.file_name())
                            .and_then(|s| s.to_str())
                            .map(String::from)
                    });

                let _ = insert_stmt.execute(params![
                    id,
                    track_virtual_path,
                    title,
                    Some(cue_track.track_num),
                    artist,
                    album,
                    duration_ms,
                    cover_url,
                    codec,
                    sample_rate,
                    bit_rate,
                    channels,
                    bits_per_sample,
                    file_size,
                    cue_mtime,
                    cue_ctime,
                    now,
                    Some(cue_file.clone()),
                    Some(physical_str),
                    Some(cue_start_ms),
                    Some(cue_end_ms),
                ]);

                total_synced += 1;
            }
        }
    }
    tx.commit()?;
    tracing::info!("成功同步 CUE 分轨数: {}", total_synced);
    Ok(total_synced)
}

/// 同步解析 SACD ISO 文件并向 tracks 写入虚拟分轨记录
pub fn sync_sacd_tracks(
    conn: &mut Connection,
    iso_files: &[String],
    cover_cache_dir: Option<&Path>,
) -> Result<usize> {
    if iso_files.is_empty() {
        return Ok(0);
    }

    let tx = conn.transaction()?;
    let mut total_synced = 0;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let cache_dir_str = cover_cache_dir
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "data/covers".to_string());

    {
        let mut insert_stmt = tx.prepare_cached(
            r#"
            INSERT INTO tracks (
                id, path, title, track, artist, album, duration,
                cover, codec, sample_rate, bit_rate, channels,
                bits_per_sample, file_size, file_mtime, file_ctime, scanned_at
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                ?8, ?9, ?10, ?11, ?12,
                ?13, ?14, ?15, ?16, ?17
            )
            ON CONFLICT(path) DO UPDATE SET
                title = excluded.title,
                track = excluded.track,
                artist = excluded.artist,
                album = excluded.album,
                duration = excluded.duration,
                cover = excluded.cover,
                codec = excluded.codec,
                sample_rate = excluded.sample_rate,
                bit_rate = excluded.bit_rate,
                channels = excluded.channels,
                bits_per_sample = excluded.bits_per_sample,
                file_size = excluded.file_size,
                file_mtime = excluded.file_mtime,
                file_ctime = excluded.file_ctime,
                scanned_at = excluded.scanned_at
            "#,
        )?;

        for iso_file in iso_files {
            let iso_path_obj = Path::new(iso_file);
            if !iso_path_obj.is_file() {
                continue;
            }

            // 1. 先清理该 ISO 可能残留的物理整轨记录或旧虚拟分轨
            let prefix_pattern = format!("{}|%", iso_file);
            let _ = tx.execute(
                "DELETE FROM tracks WHERE path LIKE ?1 OR path = ?2",
                params![prefix_pattern, iso_file],
            );

            // 2. 解析 SACD ISO 展开分轨并自动提取同目录封面
            let tracks = audio_engine_core::scanner::probe_sacd_tracks(
                iso_file,
                Some(&cache_dir_str),
            );

            if tracks.is_empty() {
                continue;
            }

            for t in tracks {
                let id = format!("local:{:x}", md5_hash(&t.path));
                let duration_ms = (t.duration * 1000.0) as u64;
                let cover_url = normalize_cover_url(t.cover);

                let _ = insert_stmt.execute(params![
                    id,
                    t.path,
                    t.title.unwrap_or_else(|| "Unknown Track".to_string()),
                    t.track,
                    t.artist,
                    t.album,
                    duration_ms,
                    cover_url,
                    t.codec,
                    t.sample_rate,
                    t.bit_rate,
                    t.channels,
                    t.bits_per_sample,
                    t.file_size,
                    t.mtime,
                    t.ctime,
                    now,
                ]);

                total_synced += 1;
            }
        }
    }
    tx.commit()?;
    tracing::info!("成功同步 SACD ISO 分轨数: {}", total_synced);
    Ok(total_synced)
}

/// 根据 path 获取单首曲目详情
pub fn get_track_by_path(conn: &Connection, path: &str) -> Result<Option<DbTrack>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT
            id, path, title, track, artist, album, duration,
            cover, codec, sample_rate, bit_rate, channels,
            bits_per_sample, file_size, file_mtime, file_ctime, scanned_at,
            cue_path, cue_audio_path, cue_start_ms, cue_end_ms
        FROM tracks
        WHERE path = ?1
        "#,
    )?;

    let track = stmt.query_row(params![path], row_to_track).optional()?;
    Ok(track)
}

/// 获取全部曲目（自动排除被 CUE 分轨引用的容器整轨）
pub fn get_all_tracks(conn: &Connection) -> Result<Vec<DbTrack>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT
            id, path, title, track, artist, album, duration,
            cover, codec, sample_rate, bit_rate, channels,
            bits_per_sample, file_size, file_mtime, file_ctime, scanned_at,
            cue_path, cue_audio_path, cue_start_ms, cue_end_ms
        FROM tracks
        WHERE path NOT IN (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)
        ORDER BY album ASC, CAST(track AS INTEGER) ASC, cue_start_ms ASC, path ASC
        "#,
    )?;

    let rows = stmt.query_map([], row_to_track)?;
    let mut list = Vec::new();
    for row in rows {
        list.push(row?);
    }
    Ok(list)
}

/// 按专辑获取曲目（自动排除容器整轨）
pub fn get_tracks_by_album(conn: &Connection, album_name: &str) -> Result<Vec<DbTrack>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT
            id, path, title, track, artist, album, duration,
            cover, codec, sample_rate, bit_rate, channels,
            bits_per_sample, file_size, file_mtime, file_ctime, scanned_at,
            cue_path, cue_audio_path, cue_start_ms, cue_end_ms
        FROM tracks
        WHERE album = ?1
          AND path NOT IN (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)
        ORDER BY CAST(track AS INTEGER) ASC, cue_start_ms ASC, path ASC
        "#,
    )?;

    let rows = stmt.query_map(params![album_name], row_to_track)?;
    let mut list = Vec::new();
    for row in rows {
        list.push(row?);
    }
    Ok(list)
}

/// 按歌手获取曲目（自动排除容器整轨）
pub fn get_tracks_by_artist(conn: &Connection, artist_name: &str) -> Result<Vec<DbTrack>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT
            id, path, title, track, artist, album, duration,
            cover, codec, sample_rate, bit_rate, channels,
            bits_per_sample, file_size, file_mtime, file_ctime, scanned_at,
            cue_path, cue_audio_path, cue_start_ms, cue_end_ms
        FROM tracks
        WHERE (artist = ?1 OR artist LIKE ?2)
          AND path NOT IN (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)
        ORDER BY album ASC, CAST(track AS INTEGER) ASC, cue_start_ms ASC, title ASC
        "#,
    )?;

    let pattern = format!("%{}%", artist_name);
    let rows = stmt.query_map(params![artist_name, pattern], row_to_track)?;
    let mut list = Vec::new();
    for row in rows {
        list.push(row?);
    }
    Ok(list)
}

/// 聚合获取专辑列表（自动排除容器整轨）
pub fn get_album_list(conn: &Connection) -> Result<Vec<AlbumSummary>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT
            album,
            MAX(cover) as cover,
            MAX(artist) as artist,
            COUNT(*) as track_count
        FROM tracks
        WHERE album IS NOT NULL AND TRIM(album) != ''
          AND path NOT IN (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)
        GROUP BY album
        ORDER BY album ASC
        "#,
    )?;

    let rows = stmt.query_map([], |row| {
        let raw_cover: Option<String> = row.get(1)?;
        Ok(AlbumSummary {
            name: row.get(0)?,
            cover: normalize_cover_url(raw_cover),
            artist: row.get(2)?,
            track_count: row.get(3)?,
        })
    })?;

    let mut list = Vec::new();
    for row in rows {
        list.push(row?);
    }
    Ok(list)
}

/// 聚合获取歌手列表（自动排除容器整轨）
pub fn get_artist_list(conn: &Connection) -> Result<Vec<ArtistSummary>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT
            artist,
            COUNT(*) as track_count
        FROM tracks
        WHERE artist IS NOT NULL AND TRIM(artist) != ''
          AND path NOT IN (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)
        GROUP BY artist
        ORDER BY artist ASC
        "#,
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(ArtistSummary {
            name: row.get(0)?,
            track_count: row.get(1)?,
        })
    })?;

    let mut list = Vec::new();
    for row in rows {
        list.push(row?);
    }
    Ok(list)
}

/// 歌单摘要
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbPlaylist {
    pub id: String,
    pub r#type: String,
    pub title: String,
    pub description: Option<String>,
    pub cover: Option<String>,
    pub track_count: u32,
    pub created_at: u64,
    pub updated_at: u64,
}

/// 歌单详情（含按顺序排序的曲目列表）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbPlaylistDetail {
    pub id: String,
    pub r#type: String,
    pub title: String,
    pub description: Option<String>,
    pub cover: Option<String>,
    pub tracks: Vec<DbTrack>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// 媒体库统计概览
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbLibraryStats {
    pub total_tracks: u32,
    pub total_duration: u64,
    pub total_artists: u32,
    pub total_albums: u32,
}

// -------------------------------------------------------------------
// 歌单操作
// -------------------------------------------------------------------

/// 获取全部歌单列表
pub fn get_all_playlists(conn: &Connection) -> Result<Vec<DbPlaylist>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT
            p.id, p.type, p.title, p.description, p.cover,
            COUNT(pt.track_id) as track_count,
            p.created_at, p.updated_at
        FROM playlists p
        LEFT JOIN playlist_tracks pt ON p.id = pt.playlist_id
        GROUP BY p.id
        ORDER BY p.updated_at DESC
        "#,
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(DbPlaylist {
            id: row.get(0)?,
            r#type: row.get(1)?,
            title: row.get(2)?,
            description: row.get(3)?,
            cover: row.get(4)?,
            track_count: row.get(5)?,
            created_at: row.get(6)?,
            updated_at: row.get(7)?,
        })
    })?;

    let mut list = Vec::new();
    for row in rows {
        list.push(row?);
    }
    Ok(list)
}

/// 获取歌单详情（含曲目列表）
pub fn get_playlist_detail(conn: &Connection, id: &str) -> Result<Option<DbPlaylistDetail>> {
    let mut stmt = conn.prepare(
        "SELECT id, type, title, description, cover, created_at, updated_at FROM playlists WHERE id = ?1",
    )?;

    let playlist_meta = stmt
        .query_row(params![id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, u64>(5)?,
                row.get::<_, u64>(6)?,
            ))
        })
        .optional()?;

    let Some((id, ptype, title, description, cover, created_at, updated_at)) = playlist_meta else {
        return Ok(None);
    };

    let mut track_stmt = conn.prepare(
        r#"
        SELECT
            t.id, t.path, t.title, t.track, t.artist, t.album, t.duration,
            t.cover, t.codec, t.sample_rate, t.bit_rate, t.channels,
            t.bits_per_sample, t.file_size, t.file_mtime, t.file_ctime, t.scanned_at,
            t.cue_path, t.cue_audio_path, t.cue_start_ms, t.cue_end_ms
        FROM playlist_tracks pt
        JOIN tracks t ON pt.track_id = t.id
        WHERE pt.playlist_id = ?1
        ORDER BY pt.position ASC
        "#,
    )?;

    let rows = track_stmt.query_map(params![id], row_to_track)?;
    let mut tracks = Vec::new();
    for row in rows {
        tracks.push(row?);
    }

    Ok(Some(DbPlaylistDetail {
        id,
        r#type: ptype,
        title,
        description,
        cover,
        tracks,
        created_at,
        updated_at,
    }))
}

/// 创建歌单
pub fn create_playlist(
    conn: &Connection,
    id: &str,
    title: &str,
    description: Option<&str>,
    cover: Option<&str>,
) -> Result<DbPlaylist> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    conn.execute(
        r#"
        INSERT INTO playlists (id, type, title, description, cover, created_at, updated_at)
        VALUES (?1, 'local', ?2, ?3, ?4, ?5, ?6)
        "#,
        params![id, title, description, cover, now, now],
    )?;

    Ok(DbPlaylist {
        id: id.to_string(),
        r#type: "local".to_string(),
        title: title.to_string(),
        description: description.map(String::from),
        cover: cover.map(String::from),
        track_count: 0,
        created_at: now,
        updated_at: now,
    })
}

/// 更新歌单元信息
pub fn update_playlist(
    conn: &Connection,
    id: &str,
    title: Option<&str>,
    description: Option<&str>,
    cover: Option<&str>,
) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let mut sets = Vec::new();
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(t) = title {
        sets.push(format!("title = ?{}", params_vec.len() + 1));
        params_vec.push(Box::new(t.to_string()));
    }
    if let Some(d) = description {
        sets.push(format!("description = ?{}", params_vec.len() + 1));
        params_vec.push(Box::new(d.to_string()));
    }
    if let Some(c) = cover {
        sets.push(format!("cover = ?{}", params_vec.len() + 1));
        params_vec.push(Box::new(c.to_string()));
    }

    sets.push(format!("updated_at = ?{}", params_vec.len() + 1));
    params_vec.push(Box::new(now));

    let query = format!(
        "UPDATE playlists SET {} WHERE id = ?{}",
        sets.join(", "),
        params_vec.len() + 1
    );
    params_vec.push(Box::new(id.to_string()));

    let slice_params: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
    conn.execute(&query, rusqlite::params_from_iter(slice_params))?;
    Ok(())
}

/// 删除歌单及关联关系
pub fn delete_playlist(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM playlist_tracks WHERE playlist_id = ?1",
        params![id],
    )?;
    conn.execute("DELETE FROM playlists WHERE id = ?1", params![id])?;
    Ok(())
}

/// 向歌单追加曲目
pub fn add_playlist_tracks(
    conn: &mut Connection,
    playlist_id: &str,
    track_ids: &[String],
) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let tx = conn.transaction()?;
    {
        // 查找当前最大 position
        let mut max_pos: u32 = tx
            .query_row(
                "SELECT COALESCE(MAX(position), 0) FROM playlist_tracks WHERE playlist_id = ?1",
                params![playlist_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let mut stmt = tx.prepare_cached(
            r#"
            INSERT OR IGNORE INTO playlist_tracks (playlist_id, track_id, position, added_at)
            VALUES (?1, ?2, ?3, ?4)
            "#,
        )?;

        for tid in track_ids {
            max_pos += 1;
            stmt.execute(params![playlist_id, tid, max_pos, now])?;
        }

        tx.execute(
            "UPDATE playlists SET updated_at = ?1 WHERE id = ?2",
            params![now, playlist_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// 从歌单移除曲目
pub fn remove_playlist_tracks(
    conn: &mut Connection,
    playlist_id: &str,
    track_ids: &[String],
) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "DELETE FROM playlist_tracks WHERE playlist_id = ?1 AND track_id = ?2",
        )?;
        for tid in track_ids {
            stmt.execute(params![playlist_id, tid])?;
        }

        tx.execute(
            "UPDATE playlists SET updated_at = ?1 WHERE id = ?2",
            params![now, playlist_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

// -------------------------------------------------------------------
// 设置持久化
// -------------------------------------------------------------------

/// 获取全部设置项字典
pub fn get_all_settings(conn: &Connection) -> Result<serde_json::Value> {
    let mut stmt = conn.prepare("SELECT key, value FROM settings")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut map = serde_json::Map::new();
    for row in rows {
        let (k, v) = row?;
        let val: serde_json::Value =
            serde_json::from_str(&v).unwrap_or(serde_json::Value::String(v));
        map.insert(k, val);
    }
    Ok(serde_json::Value::Object(map))
}

/// 获取单个设置项
pub fn get_setting(conn: &Connection, key: &str) -> Result<Option<serde_json::Value>> {
    let mut stmt = conn.prepare("SELECT value FROM settings WHERE key = ?1")?;
    let val_str: Option<String> = stmt
        .query_row(params![key], |r| r.get::<_, String>(0))
        .optional()?;
    match val_str {
        Some(s) => {
            let parsed = serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s));
            Ok(Some(parsed))
        }
        None => Ok(None),
    }
}

/// 设置配置项
pub fn set_setting(conn: &Connection, key: &str, val: &serde_json::Value) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let v_str = serde_json::to_string(val)?;
    conn.execute(
        r#"
        INSERT INTO settings (key, value, updated_at)
        VALUES (?1, ?2, ?3)
        ON CONFLICT(key) DO UPDATE SET
            value = excluded.value,
            updated_at = excluded.updated_at
        "#,
        params![key, v_str, now],
    )?;
    Ok(())
}

/// 批量设置配置项
pub fn set_all_settings(
    conn: &mut Connection,
    settings: &serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            r#"
            INSERT INTO settings (key, value, updated_at)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = excluded.updated_at
            "#,
        )?;
        for (k, v) in settings {
            let v_str = serde_json::to_string(v)?;
            stmt.execute(params![k, v_str, now])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// 重置所有配置项
pub fn reset_settings(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM settings", [])?;
    Ok(())
}

// -------------------------------------------------------------------
// 播放统计与历史
// -------------------------------------------------------------------

/// 记录播放历史
pub fn record_play_history(
    conn: &Connection,
    track_id: &str,
    source: &str,
    started_at: u64,
    listened_ms: u64,
    track_json: &str,
) -> Result<()> {
    conn.execute(
        r#"
        INSERT INTO play_history (track_id, source, started_at, listened_ms, track_json)
        VALUES (?1, ?2, ?3, ?4, ?5)
        "#,
        params![track_id, source, started_at, listened_ms, track_json],
    )?;
    Ok(())
}

/// 获取最近播放历史
pub fn get_play_history(conn: &Connection, limit: u32) -> Result<Vec<serde_json::Value>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT track_id, source, started_at, listened_ms, track_json
        FROM play_history
        ORDER BY started_at DESC
        LIMIT ?1
        "#,
    )?;

    let rows = stmt.query_map(params![limit], |row| {
        let track_json: String = row.get(4)?;
        let mut obj: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&track_json).unwrap_or_default();
        obj.insert("startedAt".to_string(), row.get::<_, u64>(2)?.into());
        obj.insert("listenedMs".to_string(), row.get::<_, u64>(3)?.into());
        Ok(serde_json::Value::Object(obj))
    })?;

    let mut list = Vec::new();
    for row in rows {
        list.push(row?);
    }
    Ok(list)
}

/// 获取媒体库统计（自动排除容器整轨）
pub fn get_library_stats(conn: &Connection) -> Result<DbLibraryStats> {
    let total_tracks: u32 = conn
        .query_row(
            "SELECT COUNT(*) FROM tracks WHERE path NOT IN (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let total_duration_ms: u64 = conn
        .query_row(
            "SELECT COALESCE(SUM(duration), 0) FROM tracks WHERE path NOT IN (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let total_artists: u32 = conn
        .query_row(
            "SELECT COUNT(DISTINCT artist) FROM tracks WHERE artist IS NOT NULL AND TRIM(artist) != '' AND path NOT IN (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let total_albums: u32 = conn
        .query_row(
            "SELECT COUNT(DISTINCT album) FROM tracks WHERE album IS NOT NULL AND TRIM(album) != '' AND path NOT IN (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    Ok(DbLibraryStats {
        total_tracks,
        total_duration: total_duration_ms,
        total_artists,
        total_albums,
    })
}

/// 辅助行转换
fn row_to_track(row: &rusqlite::Row) -> rusqlite::Result<DbTrack> {
    let artist_str: Option<String> = row.get(4)?;
    let album_str: Option<String> = row.get(5)?;
    let raw_cover: Option<String> = row.get(7)?;
    let cover_str = normalize_cover_url(raw_cover);

    let artists = if let Some(ref name) = artist_str {
        vec![DbArtist { name: name.clone() }]
    } else {
        vec![]
    };

    let album = album_str.map(|name| DbAlbum {
        name,
        cover: cover_str.clone(),
        artist: artist_str.clone(),
    });

    Ok(DbTrack {
        id: row.get(0)?,
        source: "local".to_string(),
        path: row.get(1)?,
        cue_path: row.get(17)?,
        cue_audio_path: row.get(18)?,
        cue_start_ms: row.get(19)?,
        cue_end_ms: row.get(20)?,
        title: row.get(2)?,
        track: row.get(3)?,
        artist: artist_str,
        artists,
        album,
        duration: row.get(6)?,
        cover: cover_str,
        codec: row.get(8)?,
        sample_rate: row.get(9)?,
        bit_rate: row.get(10)?,
        channels: row.get(11)?,
        bits_per_sample: row.get(12)?,
        file_size: row.get(13)?,
        file_mtime: row.get(14)?,
        file_ctime: row.get(15)?,
        scanned_at: row.get(16)?,
    })
}

/// 快速路径 MD5
fn md5_hash(text: &str) -> u128 {
    let digest = md5::compute(text.as_bytes());
    u128::from_be_bytes(digest.0)
}
