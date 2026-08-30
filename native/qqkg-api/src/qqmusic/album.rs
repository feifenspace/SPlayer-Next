//! QQ 音乐专辑歌曲列表模块（对齐桌面端 electron/main/apis/qqmusic/modules/album.ts）。

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::error::QqkgError;
use crate::normalize::join_singers;
use crate::qqmusic::QqmusicClient;


impl QqmusicClient {
    /// 获取专辑歌曲列表及元数据。
    pub async fn album(&self, params: &HashMap<String, Value>) -> Result<Value, QqkgError> {
        let mid = params
            .get("mid")
            .or_else(|| params.get("id"))
            .or_else(|| params.get("albumMid"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();

        if mid.is_empty() {
            return Err(QqkgError::InvalidParam("mid required".into()));
        }

        let data = self
            .post_fcg(
                "music.musichallAlbum.AlbumSongList",
                "GetAlbumSongList",
                json!({ "albumMid": mid, "albumID": 0, "begin": 0, "num": 999, "order": 2 }),
            )
            .await?;

        let empty_vec = Vec::new();
        let raw_song_list = data
            .get("songList")
            .and_then(Value::as_array)
            .unwrap_or(&empty_vec);

        let mut songs: Vec<Value> = Vec::new();
        for entry in raw_song_list {
            let song = match entry.get("songInfo") {
                Some(s) => s,
                None => continue,
            };
            let song_mid = match song.get("mid").and_then(Value::as_str) {
                Some(m) if !m.is_empty() => m,
                _ => continue,
            };

            let singers = song.get("singer").and_then(Value::as_array);
            let artist = singers.map(|s| join_singers(s)).unwrap_or_default();

            let album_name = song
                .get("album")
                .and_then(|a| a.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let album_mid = song
                .get("album")
                .and_then(|a| a.get("mid"))
                .and_then(Value::as_str)
                .unwrap_or(mid);

            let interval = song.get("interval").and_then(Value::as_u64).unwrap_or(0);
            let file = song.get("file");

            let cover = format!("https://y.gtimg.cn/music/photo_new/T002R300x300M000{album_mid}.jpg");
            let cover_orig = format!("https://y.gtimg.cn/music/photo_new/T002R800x800M000{album_mid}.jpg");

            let size_new = file.and_then(|f| f.get("size_new")).and_then(Value::as_array);
            let size_hires = size_new
                .and_then(|a| a.first())
                .and_then(Value::as_u64)
                .unwrap_or(0);

            songs.push(json!({
                "id": song.get("id").and_then(|v| v.as_i64()).map(|n| n.to_string()).unwrap_or_default(),
                "mid": song_mid,
                "name": song.get("title").or_else(|| song.get("name")).and_then(Value::as_str).unwrap_or(""),
                "artist": artist,
                "artists": singers.cloned().unwrap_or_default(),
                "album": album_name,
                "albumMid": album_mid,
                "cover": cover,
                "coverOriginal": cover_orig,
                "duration": interval * 1000,
                "mediaMid": file.and_then(|f| f.get("media_mid")).and_then(Value::as_str).unwrap_or(""),
                "pay": {
                    "payalbum": song.get("pay").and_then(|p| p.get("pay_month")).map(|v| v == 0).unwrap_or(false) as u8,
                    "payplay": song.get("pay").and_then(|p| p.get("pay_play")).and_then(Value::as_i64).unwrap_or(0),
                },
                "size128": file.and_then(|f| f.get("size_128mp3")).and_then(Value::as_u64).unwrap_or(0),
                "size320": file.and_then(|f| f.get("size_320mp3")).and_then(Value::as_u64).unwrap_or(0),
                "sizeApe": file.and_then(|f| f.get("size_ape")).and_then(Value::as_u64).unwrap_or(0),
                "sizeFlac": file.and_then(|f| f.get("size_flac")).and_then(Value::as_u64).unwrap_or(0),
                "sizeOgg": file.and_then(|f| f.get("size_192ogg")).and_then(Value::as_u64).unwrap_or(0),
                "sizeHiRes": size_hires,
                "hiResSampleRate": file.and_then(|f| f.get("hires_sample")).and_then(Value::as_u64).unwrap_or(0),
                "hiResBitDepth": file.and_then(|f| f.get("hires_bitdepth")).and_then(Value::as_u64).unwrap_or(0),
            }));
        }

        let total = data.get("totalNum").and_then(Value::as_u64).unwrap_or(songs.len() as u64);

        Ok(json!({
            "code": 200,
            "mid": mid,
            "total": total,
            "songs": songs,
        }))
    }
}
