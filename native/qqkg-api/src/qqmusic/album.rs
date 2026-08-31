//! QQ 音乐专辑歌曲列表模块（对齐桌面端 electron/main/apis/qqmusic/modules/album.ts）。

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::error::QqkgError;
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
            if let Some(song) = entry.get("songInfo") {
                songs.push(crate::qqmusic::search::normalize_song(song));
            }
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
