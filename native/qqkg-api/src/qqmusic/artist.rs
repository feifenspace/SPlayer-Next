//! QQ 音乐歌手详情、热门歌曲与专辑列表模块（对齐桌面端 electron/main/apis/qqmusic/modules/artist.ts）。

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::error::QqkgError;
use crate::normalize::join_singers;
use crate::qqmusic::QqmusicClient;


impl QqmusicClient {
    /// 获取歌手详情、热门歌曲及专辑列表。
    pub async fn artist(&self, params: &HashMap<String, Value>) -> Result<Value, QqkgError> {
        let mid = params
            .get("mid")
            .or_else(|| params.get("id"))
            .or_else(|| params.get("singerMid"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();

        if mid.is_empty() {
            return Err(QqkgError::InvalidParam("mid required".into()));
        }

        let offset = params
            .get("offset")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(0);
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(50)
            .clamp(1, 100);

        let include_albums = params
            .get("includeAlbums")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        let songs_future = self.post_fcg(
            "musichall.song_list_server",
            "GetSingerSongList",
            json!({ "singerMid": mid, "order": 1, "begin": offset, "num": limit }),
        );

        let albums_future = async {
            if include_albums {
                self.post_fcg(
                    "music.web_singer_info_svr",
                    "get_singer_album",
                    json!({ "singermid": mid, "order": "time", "begin": 0, "num": 200, "exstatus": 1 }),
                )
                .await
            } else {
                Ok(json!({ "list": [], "total": 0 }))
            }
        };

        let (songs_res, albums_res) = tokio::join!(songs_future, albums_future);
        let songs_data = songs_res?;
        let albums_data = albums_res.unwrap_or(json!({ "list": [], "total": 0 }));

        let empty_vec = Vec::new();
        let raw_song_list = songs_data
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
            let artist_name = singers.map(|s| join_singers(s)).unwrap_or_default();

            let album_name = song
                .get("album")
                .and_then(|a| a.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let album_mid = song
                .get("album")
                .and_then(|a| a.get("mid"))
                .and_then(Value::as_str)
                .unwrap_or("");

            let interval = song.get("interval").and_then(Value::as_u64).unwrap_or(0);
            let file = song.get("file");

            let cover = format!("https://y.gtimg.cn/music/photo_new/T002R300x300M000{album_mid}.jpg");
            let cover_orig = format!("https://y.gtimg.cn/music/photo_new/T002R800x800M000{album_mid}.jpg");

            songs.push(json!({
                "id": song.get("id").and_then(|v| v.as_i64()).map(|n| n.to_string()).unwrap_or_default(),
                "mid": song_mid,
                "name": song.get("title").or_else(|| song.get("name")).and_then(Value::as_str).unwrap_or(""),
                "artist": artist_name,
                "artists": singers.cloned().unwrap_or_default(),
                "album": album_name,
                "albumMid": album_mid,
                "cover": cover,
                "coverOriginal": cover_orig,
                "duration": interval * 1000,
                "mediaMid": file.and_then(|f| f.get("media_mid")).and_then(Value::as_str).unwrap_or(""),
            }));
        }

        let raw_albums = albums_data
            .get("list")
            .and_then(Value::as_array)
            .unwrap_or(&empty_vec);

        let mut albums: Vec<Value> = Vec::new();
        let mut first_artist_name = String::new();
        for item in raw_albums {
            let album_mid_str = item.get("album_mid").and_then(Value::as_str).unwrap_or("");
            let album_name_str = item.get("album_name").and_then(Value::as_str).unwrap_or("");
            let singer_name_str = item.get("singer_name").and_then(Value::as_str).unwrap_or("");
            if first_artist_name.is_empty() && !singer_name_str.is_empty() {
                first_artist_name = singer_name_str.to_string();
            }

            albums.push(json!({
                "id": album_mid_str,
                "name": album_name_str,
                "artist": singer_name_str,
                "trackCount": item.get("latest_song").and_then(|ls| ls.get("song_count")).and_then(Value::as_u64).unwrap_or(0),
                "publishTime": item.get("pub_time").and_then(Value::as_str).unwrap_or(""),
                "cover": format!("https://y.gtimg.cn/music/photo_new/T002R300x300M000{album_mid_str}.jpg"),
            }));
        }

        let singer_name = if !first_artist_name.is_empty() {
            first_artist_name
        } else if let Some(first_song) = songs.first() {
            first_song.get("artist").and_then(Value::as_str).unwrap_or("").to_string()
        } else {
            String::new()
        };

        Ok(json!({
            "code": 200,
            "artist": {
                "mid": songs_data.get("singerMid").and_then(Value::as_str).unwrap_or(mid),
                "name": singer_name,
                "songCount": songs_data.get("totalNum").and_then(Value::as_u64).unwrap_or(songs.len() as u64),
                "albumCount": albums_data.get("total").and_then(Value::as_u64).unwrap_or(albums.len() as u64),
            },
            "songs": songs,
            "albums": albums,
        }))
    }
}
