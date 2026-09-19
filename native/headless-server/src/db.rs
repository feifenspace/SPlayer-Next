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
/// 当前二进制支持的库 schema 版本。破坏性升级时 +1 并在 init_db 补迁移逻辑；
/// 库版本高于此值（用户回滚了程序）时拒绝启动，防止降级读坏（G.2 迁移 preflight）
pub const SCHEMA_VERSION: i32 = 2;

/// 迁移 preflight：库 schema 版本新于二进制支持版本时拒绝启动；
/// 需要升级的旧库先 VACUUM INTO 备份到 <db目录>/backups/migration-<版本>-<时间戳>/
fn schema_preflight(conn: &Connection, db_path: &Path) -> Result<()> {
    let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    match version.cmp(&SCHEMA_VERSION) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Greater => Err(anyhow::anyhow!(
            "数据库 schema 版本 ({version}) 高于本程序支持版本 ({SCHEMA_VERSION})，拒绝以旧程序读新数据。请升级程序或恢复备份：{:?}",
            db_path
        )),
        std::cmp::Ordering::Less => {
            let has_tables: bool = conn.query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table'",
                [],
                |row| row.get(0),
            )?;
            if !has_tables {
                return Ok(()); // 全新空库，无需备份
            }
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let backup_dir = db_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("backups")
                .join(format!("migration-v{version}-{ts}"));
            std::fs::create_dir_all(&backup_dir)
                .with_context(|| format!("创建迁移备份目录失败: {:?}", backup_dir))?;
            let backup_file = backup_dir.join("library.db");
            conn.execute("VACUUM INTO ?1", [backup_file.to_string_lossy().as_ref()])?;
            let manifest = serde_json::json!({
                "schema_version_before": version,
                "schema_version_target": SCHEMA_VERSION,
                "timestamp_unix": ts,
                "source": db_path,
                "backup": backup_file,
            });
            std::fs::write(
                backup_dir.join("manifest.json"),
                serde_json::to_vec_pretty(&manifest)?,
            )?;
            tracing::info!(
                version_before = version,
                backup = %backup_file.display(),
                "旧版库已备份，开始 schema 迁移"
            );
            Ok(())
        }
    }
}

pub fn init_db(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create database directory: {:?}", parent))?;
    }

    let conn = Connection::open(db_path)
        .with_context(|| format!("Failed to open SQLite database at {:?}", db_path))?;

    schema_preflight(&conn, db_path)?;

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
        CREATE INDEX IF NOT EXISTS idx_playlist_tracks_track ON playlist_tracks(track_id);
        CREATE INDEX IF NOT EXISTS idx_tracks_title_sort ON tracks(title, id);
        CREATE INDEX IF NOT EXISTS idx_tracks_artist_sort ON tracks(artist, title, id);
        CREATE INDEX IF NOT EXISTS idx_tracks_album_sort ON tracks(album, title, id);
        CREATE INDEX IF NOT EXISTS idx_tracks_sample_rate ON tracks(sample_rate);
        CREATE INDEX IF NOT EXISTS idx_tracks_codec ON tracks(codec);
        CREATE INDEX IF NOT EXISTS idx_tracks_album ON tracks(album);
        CREATE INDEX IF NOT EXISTS idx_tracks_artist ON tracks(artist);
        CREATE INDEX IF NOT EXISTS idx_tracks_cue_path ON tracks(cue_path);
        CREATE INDEX IF NOT EXISTS idx_tracks_cue_audio_path ON tracks(cue_audio_path);

        CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );

        -- 服务端内部运行状态（输出设备选择等）：独立于前端 settings，
        -- 不随 get_all_settings 下发、不被 reset_settings 清空
        CREATE TABLE IF NOT EXISTS server_state (
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

        CREATE TABLE IF NOT EXISTS favorite_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            track_id TEXT NOT NULL,
            source TEXT NOT NULL,
            action TEXT NOT NULL,
            at INTEGER NOT NULL,
            track_json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_favorite_history_at ON favorite_history(at DESC);

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
    let _ = conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_tracks_cue_audio ON tracks(cue_audio_path)",
        [],
    );

    // 迁移完成后盖版本戳（下次启动 preflight 即为 Equal 短路）
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;

    Ok(conn)
}

/// 读取某平台的 session cookies（转换为键值对 Map）
pub fn get_account_cookies(
    conn: &Connection,
    platform: &str,
) -> std::collections::HashMap<String, String> {
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
    conn.execute(
        "DELETE FROM account_sessions WHERE platform = ?",
        [platform],
    )?;
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
    // LIKE 通配符转义：路径中的 %/_ 按字面匹配，防止越界删除其它目录的曲目
    let escaped = path
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let pattern = format!("{escaped}%");
    conn.execute(
        "DELETE FROM playlist_tracks WHERE track_id IN (
             SELECT id FROM tracks
             WHERE path LIKE ?1 ESCAPE '\\' OR path = ?2 OR cue_path LIKE ?1 ESCAPE '\\'
         )",
        params![pattern, path],
    )?;
    conn.execute(
        "DELETE FROM tracks
         WHERE path LIKE ?1 ESCAPE '\\' OR path = ?2 OR cue_path LIKE ?1 ESCAPE '\\'",
        params![pattern, path],
    )?;
    Ok(())
}

/// 获取增量对比所需的已有文件记录
pub fn get_file_records(conn: &Connection) -> Result<Vec<FileRecord>> {
    let mut stmt = conn.prepare(
        "SELECT path, file_mtime, file_size, cover FROM tracks
         WHERE cue_path IS NULL
           AND path NOT LIKE 'cue://%'
           AND path NOT LIKE '%.iso|%'",
    )?;
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

    // CUE/ISO 是容器源文件，虚拟分轨记录中的 mtime 代表容器本身。
    // 把源文件加入增量索引，避免每次扫描都重新解析整张 CUE/ISO。
    let mut container_stmt = conn.prepare(
        "SELECT cue_path, MAX(file_mtime), 0, NULL
         FROM tracks
         WHERE cue_path IS NOT NULL
         GROUP BY cue_path
         UNION
         SELECT substr(path, 1, instr(path, '|') - 1), MAX(file_mtime), 0, NULL
         FROM tracks
         WHERE path LIKE '%.iso|%'
         GROUP BY substr(path, 1, instr(path, '|') - 1)",
    )?;
    let container_rows = container_stmt.query_map([], |row| {
        Ok(FileRecord {
            path: row.get(0)?,
            mtime: row.get::<_, Option<u64>>(1)?.unwrap_or(0),
            size: 0,
            cover_path: None,
        })
    })?;
    for row in container_rows {
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
                .filter(|s| !s.trim().is_empty() && !s.contains('\u{fffd}'))
                .unwrap_or_else(|| {
                    Path::new(&track.path)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("Unknown Title")
                });

            let artist = track
                .artist
                .as_deref()
                .filter(|s| !s.trim().is_empty() && !s.contains('\u{fffd}'));

            let album = track
                .album
                .as_deref()
                .filter(|s| !s.trim().is_empty() && !s.contains('\u{fffd}'));

            let duration_ms = (track.duration * 1000.0) as u64;

            stmt.execute(params![
                id,
                track.path,
                title,
                track.track,
                artist,
                album,
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
pub fn clear_library_tracks(conn: &mut Connection) -> Result<u64> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM playlist_tracks", [])?;
    let deleted = tx.execute("DELETE FROM tracks", [])? as u64;
    tx.execute(
        "DELETE FROM settings WHERE key IN ('library.folder_index_cache', 'library.scan_checkpoint')",
        [],
    )?;
    tx.commit()?;
    Ok(deleted)
}

pub fn delete_tracks_by_paths(conn: &mut Connection, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut links = tx.prepare_cached(
            "DELETE FROM playlist_tracks
             WHERE track_id IN (
                 SELECT id FROM tracks
                 WHERE path = ?1 OR cue_path = ?1 OR path LIKE ?2
             )",
        )?;
        let mut tracks_stmt = tx.prepare_cached(
            "DELETE FROM tracks
             WHERE path = ?1 OR cue_path = ?1 OR path LIKE ?2",
        )?;
        for p in paths {
            let iso_pattern = format!("{p}|%");
            links.execute(params![p, iso_pattern])?;
            tracks_stmt.execute(params![p, iso_pattern])?;
        }
        tx.execute(
            "DELETE FROM playlist_tracks
             WHERE track_id NOT IN (SELECT id FROM tracks)
                OR playlist_id NOT IN (SELECT id FROM playlists)",
            [],
        )?;
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
    if raw_trimmed.starts_with("http://")
        || raw_trimmed.starts_with("https://")
        || raw_trimmed.starts_with("/api/v1/covers/")
    {
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
pub fn sync_cue_tracks(
    conn: &mut Connection,
    cue_files: &[String],
    cover_cache_dir: Option<&Path>,
) -> Result<usize> {
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

            // 只有在 CUE 成功解析后才删除旧分轨，避免临时解析失败导致媒体库丢失旧数据。
            // 保留 playlist_tracks 关系：相同虚拟曲目 id 重新写入后仍应留在用户歌单中；
            // 已被 CUE 删除的曲目关系在本事务结束前统一清理。
            tx.execute("DELETE FROM tracks WHERE cue_path = ?1", params![cue_file])?;

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
                if !Path::new(&physical_str).is_file() {
                    tracing::warn!(
                        cue = %cue_file,
                        audio = %physical_str,
                        "跳过引用缺失音频的 CUE 分轨"
                    );
                    continue;
                }

                // 查库获取母版音频参数
                let parent_meta: Option<(u64, Option<String>, Option<String>, Option<u32>, Option<i64>, Option<u32>, Option<u32>, u64)> = tx
                    .query_row(
                        "SELECT duration, cover, codec, sample_rate, bit_rate, channels, bits_per_sample, file_size FROM tracks WHERE path = ?1",
                        [&physical_str],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
                    )
                    .optional()
                    .unwrap_or(None);

                let (
                    parent_dur_ms,
                    cover,
                    codec,
                    sample_rate,
                    bit_rate,
                    channels,
                    bits_per_sample,
                    file_size,
                ) = match parent_meta {
                    Some(m) => m,
                    None => {
                        if let Some(scanned) = audio_engine_core::scanner::probe_fast(
                            &physical_str,
                            Some(&cache_dir_str),
                        ) {
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
                            (
                                0,
                                None,
                                Some("wav".to_string()),
                                Some(44100),
                                Some(1411200),
                                Some(2),
                                Some(16),
                                0,
                            )
                        }
                    }
                };

                let effective_cover =
                    cover.or_else(|| folder_cover_from_cue.clone()).or_else(|| {
                        audio_engine_core::metadata::extract_folder_cover_thumbnail(
                            &physical_str,
                            &cache_dir_str,
                        )
                    });
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

                let track_virtual_path =
                    format!("cue://{}#track={:02}", cue_file, cue_track.track_num);
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

                insert_stmt.execute(params![
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
                ])?;

                total_synced += 1;
            }
        }
    }
    tx.execute(
        "DELETE FROM playlist_tracks
         WHERE track_id NOT IN (SELECT id FROM tracks)
            OR playlist_id NOT IN (SELECT id FROM playlists)",
        [],
    )?;
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
            let tracks =
                audio_engine_core::scanner::probe_sacd_tracks(iso_file, Some(&cache_dir_str));

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

/// 分页查询媒体库曲目。旧 get_all_tracks 保留给兼容接口，
/// 新调用方应使用此方法，避免一次性把大曲库加载进内存。
pub fn get_tracks_page(
    conn: &Connection,
    query: Option<&str>,
    limit: u32,
    offset: u64,
) -> Result<(Vec<DbTrack>, u64)> {
    let q = query.unwrap_or("").trim();
    let total: u64 = conn.query_row(
        "SELECT COUNT(*) FROM tracks
         WHERE path NOT IN (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)
           AND (?1 = '' OR title LIKE '%' || ?1 || '%'
                OR artist LIKE '%' || ?1 || '%'
                OR album LIKE '%' || ?1 || '%')",
        params![q],
        |row| row.get(0),
    )?;
    let mut stmt = conn.prepare(
        "SELECT
            id, path, title, track, artist, album, duration,
            cover, codec, sample_rate, bit_rate, channels,
            bits_per_sample, file_size, file_mtime, file_ctime, scanned_at,
            cue_path, cue_audio_path, cue_start_ms, cue_end_ms
         FROM tracks
         WHERE path NOT IN (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)
           AND (?1 = '' OR title LIKE '%' || ?1 || '%'
                OR artist LIKE '%' || ?1 || '%'
                OR album LIKE '%' || ?1 || '%')
         ORDER BY album ASC, CAST(track AS INTEGER) ASC, cue_start_ms ASC, path ASC, id ASC
         LIMIT ?2 OFFSET ?3",
    )?;
    let rows = stmt.query_map(params![q, limit, offset], row_to_track)?;
    let mut items = Vec::new();
    for row in rows {
        items.push(row?);
    }
    Ok((items, total))
}


/// 带筛选、排序和可选 id cursor 的曲目分页查询。
pub fn get_tracks_page_advanced(
    conn: &Connection,
    query: Option<&str>,
    codec: Option<&str>,
    sample_rate: Option<u32>,
    limit: u32,
    offset: u64,
    sort: Option<&str>,
    order: Option<&str>,
    cursor: Option<&str>,
) -> Result<(Vec<DbTrack>, u64, Option<String>)> {
    #[derive(Serialize, Deserialize)]
    struct TrackPageCursor {
        key: String,
        id: String,
    }

    let q = query.unwrap_or("").trim();
    let codec = codec.unwrap_or("").trim();
    let sort_column = match sort.unwrap_or("album") {
        "id" => "id",
        "title" => "title",
        "artist" => "artist",
        "album" => "album",
        "codec" => "codec",
        "sampleRate" | "sample_rate" => "sample_rate",
        _ => "album",
    };
    let descending = matches!(order.unwrap_or("asc"), "desc" | "DESC");
    let direction = if descending { "DESC" } else { "ASC" };
    let comparison = if descending { "<" } else { ">" };

    let decoded_cursor = cursor
        .filter(|value| !value.trim().is_empty())
        .map(|value| -> Result<TrackPageCursor> {
            let decoded = urlencoding::decode(value)
                .map_err(|error| anyhow::anyhow!("invalid track cursor: {error}"))?;
            Ok(serde_json::from_str(decoded.as_ref())?)
        })
        .transpose()?;

    let base_where = "path NOT IN (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)
         AND (?1 = '' OR title LIKE '%' || ?1 || '%' OR artist LIKE '%' || ?1 || '%' OR album LIKE '%' || ?1 || '%')
         AND (?2 = '' OR codec = ?2)
         AND (?3 IS NULL OR sample_rate = ?3)";
    let total: u64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM tracks WHERE {base_where}"),
        params![q, codec, sample_rate],
        |row| row.get(0),
    )?;

    let order_sql = if sort_column == "sample_rate" {
        format!("COALESCE(sample_rate, 0) {direction}, id {direction}")
    } else {
        format!("COALESCE({sort_column}, '') {direction}, id {direction}")
    };

    let fetch_limit = limit.saturating_add(1);
    let mut items = Vec::new();
    if let Some(ref page_cursor) = decoded_cursor {
        let cursor_condition = if sort_column == "sample_rate" {
            format!(
                " AND (COALESCE(sample_rate, 0) {comparison} ?4
                   OR (COALESCE(sample_rate, 0) = ?4 AND id {comparison} ?5))"
            )
        } else {
            format!(
                " AND (COALESCE({sort_column}, '') {comparison} ?4
                   OR (COALESCE({sort_column}, '') = ?4 AND id {comparison} ?5))"
            )
        };
        let sql = format!(
            "SELECT id, path, title, track, artist, album, duration,
                    cover, codec, sample_rate, bit_rate, channels,
                    bits_per_sample, file_size, file_mtime, file_ctime, scanned_at,
                    cue_path, cue_audio_path, cue_start_ms, cue_end_ms
             FROM tracks
             WHERE {base_where}{cursor_condition}
             ORDER BY {order_sql}
             LIMIT ?6 OFFSET ?7"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = if sort_column == "sample_rate" {
            let key = page_cursor.key.parse::<i64>().unwrap_or(0);
            stmt.query_map(
                params![q, codec, sample_rate, key, page_cursor.id, fetch_limit, 0],
                row_to_track,
            )?
        } else {
            stmt.query_map(
                params![q, codec, sample_rate, page_cursor.key, page_cursor.id, fetch_limit, 0],
                row_to_track,
            )?
        };
        for row in rows {
            items.push(row?);
        }
    } else {
        let sql = format!(
            "SELECT id, path, title, track, artist, album, duration,
                    cover, codec, sample_rate, bit_rate, channels,
                    bits_per_sample, file_size, file_mtime, file_ctime, scanned_at,
                    cue_path, cue_audio_path, cue_start_ms, cue_end_ms
             FROM tracks
             WHERE {base_where}
             ORDER BY {order_sql}
             LIMIT ?4 OFFSET ?5"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![q, codec, sample_rate, fetch_limit, offset], row_to_track)?;
        for row in rows {
            items.push(row?);
        }
    }

    let has_more = items.len() as u32 > limit;
    if has_more {
        items.truncate(limit as usize);
    }
    let next_cursor = if has_more {
        items.last().map(|item| {
            let key = match sort_column {
                "sample_rate" => item.sample_rate.unwrap_or(0).to_string(),
                "title" => item.title.clone(),
                "artist" => item.artist.clone().unwrap_or_default(),
                "album" => item.album.as_ref().map(|album| album.name.clone()).unwrap_or_default(),
                "codec" => item.codec.clone().unwrap_or_default(),
                _ => item.id.clone(),
            };
            let payload = TrackPageCursor {
                key,
                id: item.id.clone(),
            };
            urlencoding::encode(&serde_json::to_string(&payload).unwrap_or_default()).into_owned()
        })
    } else {
        None
    };
    Ok((items, total, next_cursor))
}

/// 使用名称游标分页获取专辑，避免大 offset 扫描。
pub fn get_album_page_cursor(
    conn: &Connection, query: Option<&str>, limit: u32, cursor: Option<&str>,
) -> Result<(Vec<AlbumSummary>, u64, Option<String>)> {
    let q = query.unwrap_or("").trim();
    let base = "album IS NOT NULL AND TRIM(album) != '' AND path NOT IN
        (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)
        AND (?1 = '' OR album LIKE '%' || ?1 || '%')";
    let total: u64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM (SELECT album FROM tracks WHERE {base} GROUP BY album)"),
        params![q], |row| row.get(0))?;
    let condition = if cursor.is_some() { " AND album > ?2" } else { "" };
    let fetch_limit = limit.saturating_add(1);
    let limit_param = if cursor.is_some() { "?3" } else { "?2" };
    let sql = format!("SELECT album, MAX(cover), MAX(artist), COUNT(*) FROM tracks WHERE {base}{condition} GROUP BY album ORDER BY album ASC LIMIT {limit_param}");
    let mut stmt = conn.prepare(&sql)?;
    let mut items = Vec::new();
    if let Some(c) = cursor {
        let rows = stmt.query_map(params![q, c, fetch_limit], |row| Ok(AlbumSummary {
            name: row.get(0)?, cover: normalize_cover_url(row.get(1)?), artist: row.get(2)?, track_count: row.get(3)?,
        }))?;
        for row in rows { items.push(row?); }
    } else {
        let rows = stmt.query_map(params![q, fetch_limit], |row| Ok(AlbumSummary {
            name: row.get(0)?, cover: normalize_cover_url(row.get(1)?), artist: row.get(2)?, track_count: row.get(3)?,
        }))?;
        for row in rows { items.push(row?); }
    }
    let has_more = items.len() as u32 > limit; if has_more { items.truncate(limit as usize); }
    let next = if has_more { items.last().map(|v| v.name.clone()) } else { None };
    Ok((items, total, next))
}

/// 使用名称游标分页获取歌手，避免大 offset 扫描。
pub fn get_artist_page_cursor(
    conn: &Connection, query: Option<&str>, limit: u32, cursor: Option<&str>,
) -> Result<(Vec<ArtistSummary>, u64, Option<String>)> {
    let q = query.unwrap_or("").trim();
    let base = "artist IS NOT NULL AND TRIM(artist) != '' AND path NOT IN
        (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)
        AND (?1 = '' OR artist LIKE '%' || ?1 || '%')";
    let total: u64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM (SELECT artist FROM tracks WHERE {base} GROUP BY artist)"),
        params![q], |row| row.get(0))?;
    let condition = if cursor.is_some() { " AND artist > ?2" } else { "" };
    let fetch_limit = limit.saturating_add(1);
    let limit_param = if cursor.is_some() { "?3" } else { "?2" };
    let sql = format!("SELECT artist, COUNT(*) FROM tracks WHERE {base}{condition} GROUP BY artist ORDER BY artist ASC LIMIT {limit_param}");
    let mut stmt = conn.prepare(&sql)?;
    let mut items = Vec::new();
    if let Some(c) = cursor {
        let rows = stmt.query_map(params![q, c, fetch_limit], |row| Ok(ArtistSummary {
            name: row.get(0)?, track_count: row.get(1)?,
        }))?;
        for row in rows { items.push(row?); }
    } else {
        let rows = stmt.query_map(params![q, fetch_limit], |row| Ok(ArtistSummary {
            name: row.get(0)?, track_count: row.get(1)?,
        }))?;
        for row in rows { items.push(row?); }
    }
    let has_more = items.len() as u32 > limit; if has_more { items.truncate(limit as usize); }
    let next = if has_more { items.last().map(|v| v.name.clone()) } else { None };
    Ok((items, total, next))
}

/// 分页获取专辑聚合。
pub fn get_album_page(
    conn: &Connection,
    query: Option<&str>,
    limit: u32,
    offset: u64,
) -> Result<(Vec<AlbumSummary>, u64)> {
    let q = query.unwrap_or("").trim();
    let where_sql = "album IS NOT NULL AND TRIM(album) != '' AND path NOT IN
        (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)
        AND (?1 = '' OR album LIKE '%' || ?1 || '%')";
    let total: u64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM (SELECT album FROM tracks WHERE {} GROUP BY album)", where_sql),
        params![q],
        |row| row.get(0),
    )?;
    let mut stmt = conn.prepare(&format!(
        "SELECT album, MAX(cover), MAX(artist), COUNT(*)
         FROM tracks WHERE {} GROUP BY album ORDER BY album ASC LIMIT ?2 OFFSET ?3",
        where_sql
    ))?;
    let rows = stmt.query_map(params![q, limit, offset], |row| {
        Ok(AlbumSummary {
            name: row.get(0)?,
            cover: normalize_cover_url(row.get(1)?),
            artist: row.get(2)?,
            track_count: row.get(3)?,
        })
    })?;
    let mut items = Vec::new();
    for row in rows { items.push(row?); }
    Ok((items, total))
}

/// 分页获取歌手聚合。
pub fn get_artist_page(
    conn: &Connection,
    query: Option<&str>,
    limit: u32,
    offset: u64,
) -> Result<(Vec<ArtistSummary>, u64)> {
    let q = query.unwrap_or("").trim();
    let where_sql = "artist IS NOT NULL AND TRIM(artist) != '' AND path NOT IN
        (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)
        AND (?1 = '' OR artist LIKE '%' || ?1 || '%')";
    let total: u64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM (SELECT artist FROM tracks WHERE {} GROUP BY artist)", where_sql),
        params![q],
        |row| row.get(0),
    )?;
    let mut stmt = conn.prepare(&format!(
        "SELECT artist, COUNT(*)
         FROM tracks WHERE {} GROUP BY artist ORDER BY artist ASC LIMIT ?2 OFFSET ?3",
        where_sql
    ))?;
    let rows = stmt.query_map(params![q, limit, offset], |row| {
        Ok(ArtistSummary { name: row.get(0)?, track_count: row.get(1)? })
    })?;
    let mut items = Vec::new();
    for row in rows { items.push(row?); }
    Ok((items, total))
}

/// 按 ID 批量获取曲目，结果顺序由调用方按请求 ID 重建。
pub fn get_tracks_by_ids(conn: &Connection, ids: &[String]) -> Result<Vec<DbTrack>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = (1..=ids.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT id, path, title, track, artist, album, duration,
                cover, codec, sample_rate, bit_rate, channels,
                bits_per_sample, file_size, file_mtime, file_ctime, scanned_at,
                cue_path, cue_audio_path, cue_start_ms, cue_end_ms
         FROM tracks WHERE id IN ({placeholders})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let values: Vec<&dyn rusqlite::ToSql> =
        ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
    let rows = stmt.query_map(rusqlite::params_from_iter(values), row_to_track)?;
    let mut tracks = Vec::new();
    for row in rows { tracks.push(row?); }
    Ok(tracks)
}

/// 按目录分页读取曲目，包含目录下的子目录。
pub fn get_folder_tracks_page(
    conn: &Connection,
    folder: &str,
    limit: u32,
    offset: u64,
) -> Result<(Vec<DbTrack>, u64)> {
    let prefix = format!("{}/%", folder.trim_end_matches('/').replace('\\', "/"));
    let where_sql = "COALESCE(REPLACE(cue_audio_path, '\\', '/'), REPLACE(path, '\\', '/')) LIKE ?1
        AND path NOT IN (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)";
    let total: u64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM tracks WHERE {where_sql}"),
        rusqlite::params![prefix],
        |row| row.get(0),
    )?;
    let sql = format!(
        "SELECT id, path, title, track, artist, album, duration,
                cover, codec, sample_rate, bit_rate, channels,
                bits_per_sample, file_size, file_mtime, file_ctime, scanned_at,
                cue_path, cue_audio_path, cue_start_ms, cue_end_ms
         FROM tracks WHERE {where_sql}
         ORDER BY COALESCE(album, ''), COALESCE(track, 0), title, id
         LIMIT ?2 OFFSET ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![prefix, limit, offset], row_to_track)?;
    let mut items = Vec::new();
    for row in rows { items.push(row?); }
    Ok((items, total))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LibraryFolderSummary {
    pub name: String,
    pub path: String,
    pub track_count: u64,
}

/// 目录索引版本：扫描时间、文件时间或记录数变化都会使缓存失效。
pub fn get_folder_index_version(conn: &Connection) -> Result<u64> {
    let version: u64 = conn.query_row(
        "SELECT COUNT(*) + COALESCE(MAX(scanned_at), 0) + COALESCE(MAX(file_mtime), 0) FROM tracks",
        [],
        |row| row.get(0),
    )?;
    Ok(version)
}

/// 返回轻量目录索引；曲目详情仍通过分页接口按目录读取。
pub fn get_folder_summaries(conn: &Connection) -> Result<Vec<LibraryFolderSummary>> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(cue_audio_path, path)
         FROM tracks
         WHERE path NOT IN (SELECT cue_audio_path FROM tracks WHERE cue_audio_path IS NOT NULL)"
    )?;
    let paths = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for path in paths {
        let path = path?;
        let normalized = path.replace('\\', "/");
        let mut current = normalized.rsplit_once('/').map(|(parent, _)| parent.to_string());
        while let Some(folder) = current {
            *counts.entry(folder.clone()).or_default() += 1;
            current = folder.rsplit_once('/').map(|(parent, _)| parent.to_string());
        }
    }
    let mut result = counts.into_iter().map(|(path, track_count)| {
        let name = path.rsplit('/').next().filter(|v| !v.is_empty()).unwrap_or(&path).to_string();
        LibraryFolderSummary { name, path, track_count }
    }).collect::<Vec<_>>();
    result.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
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
        if tx.query_row(
            "SELECT 1 FROM playlists WHERE id = ?1",
            params![playlist_id],
            |_| Ok(()),
        ).optional()?.is_none() {
            return Err(anyhow::anyhow!("playlist not found: {playlist_id}"));
        }
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

        let mut seen = std::collections::HashSet::new();
        for tid in track_ids {
            if !seen.insert(tid) {
                continue;
            }
            let exists: Option<String> = tx
                .query_row("SELECT id FROM tracks WHERE id = ?1", params![tid], |row| row.get(0))
                .optional()?;
            if exists.is_none() {
                return Err(anyhow::anyhow!("track not found: {tid}"));
            }
            let already: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM playlist_tracks WHERE playlist_id = ?1 AND track_id = ?2)",
                params![playlist_id, tid],
                |row| row.get(0),
            )?;
            if already {
                continue;
            }
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
// 服务端内部状态（server_state 表）
// -------------------------------------------------------------------

/// 读取服务端内部状态值
pub fn get_server_state(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT value FROM server_state WHERE key = ?1")?;
    let val: Option<String> = stmt.query_row(params![key], |r| r.get(0)).optional()?;
    Ok(val)
}

/// 写入服务端内部状态值（UPSERT）
pub fn set_server_state(conn: &Connection, key: &str, value: &str) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    conn.execute(
        r#"
        INSERT INTO server_state (key, value, updated_at)
        VALUES (?1, ?2, ?3)
        ON CONFLICT(key) DO UPDATE SET
            value = excluded.value,
            updated_at = excluded.updated_at
        "#,
        params![key, value, now],
    )?;
    Ok(())
}

// -------------------------------------------------------------------
// 播放统计与历史
// -------------------------------------------------------------------

/// 记录播放历史
/// 查询某曲目最近一次播放会话：(行 id, started_at, listened_ms)。
/// 统计会话合并（G.2）用：同曲 reload 在窗口内续写同一行而非新增重复计数
pub fn latest_play_session(conn: &Connection, track_id: &str) -> Result<Option<(i64, u64, u64)>> {
    conn.query_row(
        "SELECT rowid, started_at, listened_ms FROM play_history \
         WHERE track_id = ?1 ORDER BY started_at DESC, rowid DESC LIMIT 1",
        [track_id],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, i64>(2)? as u64,
            ))
        },
    )
    .optional()
    .map_err(anyhow::Error::from)
}

/// 向既有会话行追加收听时长
pub fn extend_play_history(conn: &Connection, rowid: i64, add_ms: u64) -> Result<()> {
    conn.execute(
        "UPDATE play_history SET listened_ms = listened_ms + ?1 WHERE rowid = ?2",
        params![add_ms as i64, rowid],
    )?;
    Ok(())
}

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
    // 7×24 常驻防膨胀：每次插入后裁剪，只保留最近 5000 条
    conn.execute(
        "DELETE FROM play_history WHERE rowid NOT IN \
         (SELECT rowid FROM play_history ORDER BY started_at DESC LIMIT 5000)",
        [],
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

// -------------------------------------------------------------------
// 播放统计聚合（首页卡片 / Stats 页图表，对齐 electron playStats.ts 语义）
// -------------------------------------------------------------------

/// 今日 00:00 的 unix ms（服务器本地时区）
fn day_start_ms(now: u64) -> u64 {
    use chrono::{Local, TimeZone};
    let dt = Local
        .timestamp_millis_opt(now as i64)
        .single()
        .unwrap_or_else(Local::now);
    let mid = dt.date_naive().and_hms_opt(0, 0, 0).unwrap();
    Local
        .from_local_datetime(&mid)
        .single()
        .map(|t| t.timestamp_millis() as u64)
        .unwrap_or(now)
}

/// 本周一 00:00 的 unix ms（本地时区，周一为一周起点）
fn week_start_ms(now: u64) -> u64 {
    use chrono::{Datelike, Local, TimeZone};
    let dt = Local
        .timestamp_millis_opt(now as i64)
        .single()
        .unwrap_or_else(Local::now);
    let days_from_monday = i64::from(dt.weekday().num_days_from_monday());
    let monday = dt.date_naive() - chrono::Duration::days(days_from_monday);
    let mid = monday.and_hms_opt(0, 0, 0).unwrap();
    Local
        .from_local_datetime(&mid)
        .single()
        .map(|t| t.timestamp_millis() as u64)
        .unwrap_or(now)
}

/// 连续收听天数：从今天（或昨天）向前数连续有播放记录的天数
fn compute_streak(conn: &Connection) -> u64 {
    let days: Vec<String> = match conn
        .prepare(
            "SELECT DISTINCT date(started_at / 1000, 'unixepoch', 'localtime') AS day \
             FROM play_history ORDER BY day DESC",
        )
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        }) {
        Ok(days) => days,
        Err(_) => return 0,
    };
    if days.is_empty() {
        return 0;
    }
    use chrono::{Datelike, Local, TimeZone};
    let now_ms = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let today = Local
        .timestamp_millis_opt(now_ms as i64)
        .single()
        .unwrap_or_else(Local::now);
    let key_of = |d: chrono::DateTime<chrono::Local>| {
        format!(
            "{:04}-{:02}-{:02}",
            d.year(),
            d.month(),
            d.day()
        )
    };
    let key_today = key_of(today);
    let yest = today - chrono::Duration::days(1);
    let key_yesterday = key_of(yest);
    if days[0] != key_today && days[0] != key_yesterday {
        return 0;
    }
    let present: std::collections::HashSet<&str> = days.iter().map(|s| s.as_str()).collect();
    let mut cursor = if days[0] == key_today { today } else { yest };
    let mut streak = 0u64;
    loop {
        let key = key_of(cursor);
        if !present.contains(key.as_str()) {
            break;
        }
        streak += 1;
        cursor -= chrono::Duration::days(1);
    }
    streak
}

/// 播放统计汇总（PlayStatsSummary 形状，camelCase 直接给前端）
pub fn get_play_stats_summary(conn: &Connection) -> Result<serde_json::Value> {
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let day_start = day_start_ms(now);
    let week_start = week_start_ms(now);
    let last_week_start = week_start.saturating_sub(7 * 24 * 3600 * 1000);

    let scalar = |sql: &str, params: &[&dyn rusqlite::ToSql]| -> i64 {
        conn.query_row(sql, params, |r| r.get::<_, i64>(0)).unwrap_or(0)
    };
    let empty: &[&dyn rusqlite::ToSql] = &[];
    let today_listened = scalar(
        "SELECT COALESCE(SUM(listened_ms),0) FROM play_history WHERE started_at >= ?1",
        &[&day_start],
    );
    let week_listened = scalar(
        "SELECT COALESCE(SUM(listened_ms),0) FROM play_history WHERE started_at >= ?1",
        &[&week_start],
    );
    let last_week_listened = scalar(
        "SELECT COALESCE(SUM(listened_ms),0) FROM play_history WHERE started_at >= ?1 AND started_at < ?2",
        &[&last_week_start, &week_start],
    );
    let total_listened = scalar("SELECT COALESCE(SUM(listened_ms),0) FROM play_history", empty);
    let week_plays = scalar(
        "SELECT COUNT(*) FROM play_history WHERE started_at >= ?1",
        &[&week_start],
    );
    let total_plays = scalar("SELECT COUNT(*) FROM play_history", empty);
    let week_fav_adds = scalar(
        "SELECT COUNT(*) FROM favorite_history WHERE action='add' AND at >= ?1",
        &[&week_start],
    );
    let streak = compute_streak(conn);

    Ok(serde_json::json!({
        "todayListenedMs": today_listened,
        "weekListenedMs": week_listened,
        "lastWeekListenedMs": last_week_listened,
        "totalListenedMs": total_listened,
        "weekPlayCount": week_plays,
        "totalPlayCount": total_plays,
        "weekFavoriteAdds": week_fav_adds,
        "streakDays": streak,
    }))
}

/// 最常播放曲目（排除 streaming 源；对齐 electron 版 SQL）
pub fn get_top_tracks(conn: &Connection, limit: u32) -> Result<Vec<serde_json::Value>> {
    let mut stmt = conn.prepare(
        r#"SELECT track_json, COUNT(*) AS plays FROM play_history
           WHERE source != 'streaming'
           GROUP BY source, track_id
           ORDER BY plays DESC, MAX(started_at) DESC LIMIT ?1"#,
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut list = Vec::new();
    for r in rows {
        let (track_json, plays) = r?;
        let track = serde_json::from_str::<serde_json::Value>(&track_json).unwrap_or_default();
        list.push(serde_json::json!({ "track": track, "playCount": plays }));
    }
    Ok(list)
}

/// 最常播放专辑（按 album.id/name 聚合，对齐 electron 版 SQL）
pub fn get_top_albums(conn: &Connection, limit: u32) -> Result<Vec<serde_json::Value>> {
    let mut stmt = conn.prepare(
        r#"SELECT track_json, COUNT(*) AS plays FROM play_history
           WHERE source != 'streaming'
             AND TRIM(COALESCE(json_extract(track_json,'$.album.name'),'')) != ''
           GROUP BY source, COALESCE(json_extract(track_json,'$.album.id'),
                                     json_extract(track_json,'$.album.name'))
           ORDER BY plays DESC, MAX(started_at) DESC LIMIT ?1"#,
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut list = Vec::new();
    for r in rows {
        let (track_json, plays) = r?;
        let track = serde_json::from_str::<serde_json::Value>(&track_json).unwrap_or_default();
        list.push(serde_json::json!({ "track": track, "playCount": plays }));
    }
    Ok(list)
}

/// 最常播放歌手（json_each 展开多歌手，对齐 electron 版 SQL）
pub fn get_top_artists(conn: &Connection, limit: u32) -> Result<Vec<serde_json::Value>> {
    let mut stmt = conn.prepare(
        r#"SELECT track_json, artist.value AS artist_json, COUNT(*) AS plays
           FROM play_history, json_each(play_history.track_json, '$.artists') artist
           WHERE play_history.source != 'streaming'
             AND TRIM(COALESCE(json_extract(artist.value,'$.name'),'')) != ''
           GROUP BY play_history.source,
                    COALESCE(json_extract(artist.value,'$.id'),
                             LOWER(json_extract(artist.value,'$.name')))
           ORDER BY plays DESC, MAX(started_at) DESC LIMIT ?1"#,
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut list = Vec::new();
    for r in rows {
        let (track_json, artist_json, plays) = r?;
        let track = serde_json::from_str::<serde_json::Value>(&track_json).unwrap_or_default();
        let artist = serde_json::from_str::<serde_json::Value>(&artist_json).unwrap_or_default();
        list.push(serde_json::json!({
            "artist": artist,
            "track": track,
            "playCount": plays,
        }));
    }
    Ok(list)
}

/// 最近 N 天每日播放次数（升序；缺日由前端图表补零）
pub fn get_daily_play_stats(conn: &Connection, days: u32) -> Result<Vec<serde_json::Value>> {
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let start = day_start_ms(now).saturating_sub(u64::from(days).saturating_sub(1) * 86400_000);
    let mut stmt = conn.prepare(
        "SELECT date(started_at/1000,'unixepoch','localtime') AS day, COUNT(*) AS c \
         FROM play_history WHERE started_at >= ?1 GROUP BY day ORDER BY day ASC",
    )?;
    let rows = stmt.query_map(params![start], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut list = Vec::new();
    for r in rows {
        let (day, c) = r?;
        list.push(serde_json::json!({ "day": day, "playCount": c }));
    }
    Ok(list)
}

/// 各小时累计播放次数（0-23 全量补零）
pub fn get_hourly_play_stats(conn: &Connection) -> Result<Vec<serde_json::Value>> {
    let mut stmt = conn.prepare(
        "SELECT CAST(strftime('%H', started_at/1000,'unixepoch','localtime') AS INTEGER) AS h, \
         COUNT(*) AS c FROM play_history GROUP BY h",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut map = std::collections::HashMap::new();
    for r in rows {
        let (h, c) = r?;
        map.insert(h, c);
    }
    let list: Vec<_> = (0..24)
        .map(|h| {
            serde_json::json!({
                "hour": h,
                "playCount": map.get(&(h as i64)).copied().unwrap_or(0),
            })
        })
        .collect();
    Ok(list)
}

/// 记录收藏变更（前端 useFavorite → /api/v1/stats/favorite）
pub fn record_favorite_event(
    conn: &Connection,
    track_id: &str,
    source: &str,
    action: &str,
    track_json: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO favorite_history (track_id, source, action, at, track_json) VALUES (?1,?2,?3,?4,?5)",
        params![
            track_id,
            source,
            action,
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64,
            track_json
        ],
    )?;
    Ok(())
}

/// 服务端自治记录：与浏览器上报去重——同曲且 started_at 邻近（±5s）时
/// listened_ms 取 max 更新既有行；否则按常规规则（10 分钟窗口续写/新插入）
pub fn upsert_server_play_history(
    conn: &Connection,
    track_id: &str,
    source: &str,
    started_at: u64,
    listened_ms: u64,
    track_json: &str,
) -> Result<()> {
    // 1) 邻近去重：同曲 ±5s 内已有记录 → listened_ms 取 max
    let near: Option<(i64, i64)> = conn
        .query_row(
            "SELECT rowid, listened_ms FROM play_history WHERE track_id = ?1 \
             AND Abs(started_at - ?2) <= 5000 ORDER BY started_at DESC LIMIT 1",
            params![track_id, started_at as i64],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?;
    if let Some((rowid, prev_ms)) = near {
        if (listened_ms as i64) > prev_ms {
            conn.execute(
                "UPDATE play_history SET listened_ms = ?1 WHERE rowid = ?2",
                params![listened_ms as i64, rowid],
            )?;
        }
        return Ok(());
    }
    // 2) 常规：10 分钟窗口内同曲续写
    if let Ok(Some((rowid, prev_started, prev_listened))) = latest_play_session(conn, track_id) {
        let prev_end = prev_started.saturating_add(prev_listened);
        if started_at >= prev_started && started_at.saturating_sub(prev_end) <= 10 * 60 * 1000 {
            conn.execute(
                "UPDATE play_history SET listened_ms = listened_ms + ?1 WHERE rowid = ?2",
                params![listened_ms as i64, rowid],
            )?;
            return Ok(());
        }
    }
    // 3) 新插入 + 防膨胀裁剪
    conn.execute(
        "INSERT INTO play_history (track_id, source, started_at, listened_ms, track_json) VALUES (?1,?2,?3,?4,?5)",
        params![track_id, source, started_at as i64, listened_ms as i64, track_json],
    )?;
    conn.execute(
        "DELETE FROM play_history WHERE rowid NOT IN \
         (SELECT rowid FROM play_history ORDER BY started_at DESC LIMIT 5000)",
        [],
    )?;
    Ok(())
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
