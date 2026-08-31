//! 酷狗歌单详情与歌曲列表模块（对齐桌面端 electron/main/apis/kugou/modules/playlist.ts）。

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::error::QqkgError;
use crate::kugou::KugouClient;
use crate::normalize::{decode_name, fill_cover_opt};

impl KugouClient {

    /// 获取酷狗歌单详情与歌曲列表。
    pub async fn playlist(&self, params: &HashMap<String, Value>) -> Result<Value, QqkgError> {
        let id = params
            .get("id")
            .or_else(|| params.get("specialid"))
            .or_else(|| params.get("special_id"))
            .and_then(|v| {
                v.as_str()
                    .map(ToString::to_string)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            })
            .unwrap_or_default();

        if id.is_empty() {
            return Err(QqkgError::InvalidParam("id required".into()));
        }

        let info_url = format!("http://mobilecdn.kugou.com/api/v3/special/info?specialid={}&format=json", urlencoding::encode(&id));
        let song_url = format!("http://mobilecdn.kugou.com/api/v3/special/song?specialid={}&page=1&pagesize=300&format=json", urlencoding::encode(&id));

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


        let songs: Vec<Value> = raw_songs
            .iter()
            .map(|raw| {
                let mut song_obj = crate::kugou::search::normalize_from_mobile(raw);
                if song_obj.get("cover").and_then(Value::as_str).unwrap_or("").is_empty() {
                    if let Some(obj) = song_obj.as_object_mut() {
                        obj.insert("cover".into(), json!(cover));
                        obj.insert("coverOriginal".into(), json!(cover_orig));
                    }
                }
                song_obj
            })
            .collect();


        Ok(json!({
            "code": 200,
            "id": info.get("specialid").and_then(|v| v.as_str().map(ToString::to_string).or_else(|| v.as_i64().map(|n| n.to_string()))).unwrap_or(id),
            "name": decode_name(info.get("specialname").and_then(Value::as_str).unwrap_or("")),
            "description": info.get("intro").and_then(Value::as_str).unwrap_or(""),
            "creator": decode_name(info.get("nickname").and_then(Value::as_str).unwrap_or("")),
            "cover": cover,
            "coverOriginal": cover_orig,
            "playCount": info.get("playcount").and_then(Value::as_u64).unwrap_or(0),
            "total": info.get("songcount").and_then(Value::as_u64).unwrap_or(songs.len() as u64),
            "songs": songs,
        }))
    }
}
