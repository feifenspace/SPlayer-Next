//! 酷狗歌手详情与热门歌曲模块（对齐桌面端 electron/main/apis/kugou/modules/artist.ts）。

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::error::QqkgError;
use crate::kugou::KugouClient;
use crate::normalize::{decode_name, fill_cover_opt};


fn format_artist(filename: &str, singername: Option<&str>) -> String {
    if let Some(s) = singername {
        if !s.is_empty() {
            let decoded = decode_name(s);
            let parts: Vec<&str> = decoded
                .split(|c| c == '、' || c == ',' || c == ';' || c == '/')
                .map(|p| p.trim())
                .filter(|p| !p.is_empty())
                .collect();
            if !parts.is_empty() {
                return parts.join(" / ");
            }
        }
    }
    if let Some((first, _)) = filename.split_once(" - ") {
        return decode_name(first.trim());
    }
    String::new()
}

fn format_song_name(filename: &str, songname: Option<&str>) -> String {
    if let Some(s) = songname {
        if !s.is_empty() {
            return decode_name(s);
        }
    }
    if let Some((_, rest)) = filename.split_once(" - ") {
        return decode_name(rest.trim());
    }
    decode_name(filename)
}

impl KugouClient {
    /// 获取酷狗歌手详情与热门歌曲。
    pub async fn artist(&self, params: &HashMap<String, Value>) -> Result<Value, QqkgError> {
        let id = params
            .get("id")
            .or_else(|| params.get("singerid"))
            .or_else(|| params.get("singer_id"))
            .or_else(|| params.get("artistId"))
            .and_then(|v| {
                v.as_str()
                    .map(ToString::to_string)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            })
            .unwrap_or_default();

        if id.is_empty() {
            return Err(QqkgError::InvalidParam("singer id required".into()));
        }

        let page = params.get("page").and_then(Value::as_u64).unwrap_or(1);
        let limit = params.get("limit").or_else(|| params.get("pagesize")).and_then(Value::as_u64).unwrap_or(50).clamp(1, 100);

        let info_url = format!("http://mobilecdn.kugou.com/api/v3/singer/info?singerid={}&format=json", urlencoding::encode(&id));
        let song_url = format!("http://mobilecdn.kugou.com/api/v3/singer/song?singerid={}&page={}&pagesize={}&format=json", urlencoding::encode(&id), page, limit);

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
            let filename = raw.get("filename").and_then(Value::as_str).unwrap_or("");
            let singername = raw.get("singername").and_then(Value::as_str);
            let songname = raw.get("songname").and_then(Value::as_str);
            let artist_name = format_artist(filename, singername);
            let artists: Vec<Value> = if !artist_name.is_empty() {
                artist_name
                    .split(" / ")
                    .map(|name| json!({ "id": name, "name": name }))
                    .collect()
            } else {
                Vec::new()
            };

            let interval = raw.get("duration").and_then(Value::as_u64).unwrap_or(0);
            let audio_id = raw.get("audio_id").and_then(Value::as_u64).unwrap_or(0);
            let hash = raw.get("hash").and_then(Value::as_str).unwrap_or("");

            let trans = raw.get("trans_param");
            let union_cover = trans.and_then(|t| t.get("union_cover")).and_then(Value::as_str);
            let chosen_cover = union_cover.or(img_url);

            songs.push(json!({
                "id": if audio_id > 0 { audio_id.to_string() } else { hash.to_string() },
                "audioId": audio_id,
                "albumAudioId": raw.get("album_audio_id").and_then(Value::as_u64).unwrap_or(0),
                "hash": hash,
                "name": format_song_name(filename, songname),
                "artist": artist_name,
                "artists": artists,
                "album": decode_name(raw.get("album_name").and_then(Value::as_str).unwrap_or("")),
                "albumId": raw.get("album_id").and_then(|v| v.as_str().map(ToString::to_string).or_else(|| v.as_i64().map(|n| n.to_string()))).unwrap_or_default(),
                "cover": fill_cover_opt(chosen_cover, 300),
                "coverOriginal": fill_cover_opt(chosen_cover, 480),
                "interval": interval,
                "duration": interval * 1000,
            }));
        }


        Ok(json!({
            "code": 200,
            "artist": {
                "id": info.get("singerid").and_then(|v| v.as_str().map(ToString::to_string).or_else(|| v.as_i64().map(|n| n.to_string()))).unwrap_or(id),
                "name": decode_name(info.get("singername").and_then(Value::as_str).unwrap_or("")),
                "description": info.get("intro").and_then(Value::as_str).unwrap_or(""),
                "cover": cover,
                "coverOriginal": cover_orig,
                "songCount": info.get("songcount").and_then(Value::as_u64).unwrap_or(songs.len() as u64),
                "albumCount": info.get("albumcount").and_then(Value::as_u64).unwrap_or(0),
            },
            "songs": songs,
        }))
    }
}
