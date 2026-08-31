//! 酷狗专辑详情与歌曲列表模块（对齐桌面端 electron/main/apis/kugou/modules/album.ts）。

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::error::QqkgError;
use crate::kugou::KugouClient;
use crate::normalize::{decode_name, fill_cover_opt};


impl KugouClient {

    /// 获取酷狗专辑详情与歌曲列表。
    pub async fn album(&self, params: &HashMap<String, Value>) -> Result<Value, QqkgError> {
        let id = params
            .get("id")
            .or_else(|| params.get("albumid"))
            .or_else(|| params.get("album_id"))
            .and_then(|v| {
                v.as_str()
                    .map(ToString::to_string)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            })
            .unwrap_or_default();

        if id.is_empty() {
            return Err(QqkgError::InvalidParam("album id required".into()));
        }

        let info_url = format!("http://mobilecdn.kugou.com/api/v3/album/info?albumid={}&format=json", urlencoding::encode(&id));
        let song_url = format!("http://mobilecdn.kugou.com/api/v3/album/song?albumid={}&page=1&pagesize=300&format=json", urlencoding::encode(&id));

        let info_future = self.kg_request(&info_url);
        let song_future = self.kg_request(&song_url);

        let (info_res, song_res) = tokio::join!(info_future, song_future);
        let info_body = info_res?;
        let song_body = song_res?;

        let info = info_body.get("data").cloned().unwrap_or(Value::Null);
        let empty_vec = Vec::new();
        let raw_songs = song_body
            .get("data")
            .and_then(|d| d.get("info"))
            .and_then(Value::as_array)
            .unwrap_or(&empty_vec);

        let img_url = info.get("imgurl").and_then(Value::as_str);
        let cover = fill_cover_opt(img_url, 300);
        let cover_orig = fill_cover_opt(img_url, 480);

        let mut songs: Vec<Value> = Vec::new();
        for raw in raw_songs {
            let mut song_obj = crate::kugou::search::normalize_from_mobile(raw);
            if song_obj.get("cover").and_then(Value::as_str).unwrap_or("").is_empty() {
                if let Some(obj) = song_obj.as_object_mut() {
                    obj.insert("cover".into(), json!(cover));
                    obj.insert("coverOriginal".into(), json!(cover_orig));
                }
            }
            songs.push(song_obj);
        }



        Ok(json!({
            "code": 200,
            "id": info.get("albumid").and_then(|v| v.as_str().map(ToString::to_string).or_else(|| v.as_i64().map(|n| n.to_string()))).unwrap_or(id),
            "name": decode_name(info.get("albumname").and_then(Value::as_str).unwrap_or("")),
            "artist": decode_name(info.get("singername").and_then(Value::as_str).unwrap_or("")),
            "artistId": info.get("singerid").and_then(|v| v.as_str().map(ToString::to_string).or_else(|| v.as_i64().map(|n| n.to_string()))).unwrap_or_default(),
            "description": info.get("intro").and_then(Value::as_str).unwrap_or(""),
            "cover": cover,
            "coverOriginal": cover_orig,
            "publishTime": info.get("publishtime").and_then(Value::as_str).unwrap_or(""),
            "total": info.get("songcount").and_then(Value::as_u64).unwrap_or(songs.len() as u64),
            "songs": songs,
        }))
    }
}
