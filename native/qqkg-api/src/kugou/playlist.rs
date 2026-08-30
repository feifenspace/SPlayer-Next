//! 酷狗歌单详情与歌曲列表模块（对齐桌面端 electron/main/apis/kugou/modules/playlist.ts）。

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

fn normalize_special_song(raw: &Value, fallback_cover: Option<&str>) -> Value {
    let trans = raw.get("trans_param");
    let union_cover = trans.and_then(|t| t.get("union_cover")).and_then(Value::as_str);
    let chosen_cover = union_cover.or(fallback_cover);
    let cover = fill_cover_opt(chosen_cover, 300);
    let cover_orig = fill_cover_opt(chosen_cover, 480);

    let interval = raw.get("duration").and_then(Value::as_u64).unwrap_or(0);

    let mut hashes = json!({});
    let mut qualities: Vec<&str> = Vec::new();
    let mut sizes = json!({});

    if let (Some(s), Some(h)) = (raw.get("filesize").and_then(Value::as_u64), raw.get("hash").and_then(Value::as_str)) {
        if !h.is_empty() && s > 0 {
            hashes["128k"] = json!(h);
            sizes["128k"] = json!(s);
            qualities.push("128k");
        }
    }
    if let (Some(s), Some(h)) = (raw.get("320filesize").and_then(Value::as_u64), raw.get("320hash").and_then(Value::as_str)) {
        if !h.is_empty() && s > 0 {
            hashes["320k"] = json!(h);
            sizes["320k"] = json!(s);
            qualities.push("320k");
        }
    }
    if let (Some(s), Some(h)) = (raw.get("sqfilesize").and_then(Value::as_u64), raw.get("sqhash").and_then(Value::as_str)) {
        if !h.is_empty() && s > 0 {
            hashes["flac"] = json!(h);
            sizes["flac"] = json!(s);
            qualities.push("flac");
        }
    }
    let hr_size = raw.get("hires_filesize").or_else(|| raw.get("resfilesize")).and_then(Value::as_u64);
    let hr_hash = raw.get("hires_hash").or_else(|| raw.get("reshash")).and_then(Value::as_str);
    if let (Some(s), Some(h)) = (hr_size, hr_hash) {
        if !h.is_empty() && s > 0 {
            hashes["flac24bit"] = json!(h);
            sizes["flac24bit"] = json!(s);
            qualities.push("flac24bit");
        }
    }

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

    let audio_id = raw.get("audio_id").and_then(Value::as_u64).unwrap_or(0);
    let hash = raw.get("hash").and_then(Value::as_str).unwrap_or("");

    json!({
        "id": if audio_id > 0 { audio_id.to_string() } else { hash.to_string() },
        "audioId": audio_id,
        "albumAudioId": raw.get("album_audio_id").and_then(Value::as_u64).unwrap_or(0),
        "hash": hash,
        "name": format_song_name(filename, songname),
        "artist": artist_name,
        "artists": artists,
        "album": decode_name(raw.get("album_name").and_then(Value::as_str).unwrap_or("")),
        "albumId": raw.get("album_id").and_then(|v| v.as_str().map(ToString::to_string).or_else(|| v.as_i64().map(|n| n.to_string()))).unwrap_or_default(),
        "cover": cover,
        "coverOriginal": cover_orig,
        "interval": interval,
        "duration": interval * 1000,
        "qualities": qualities,
        "hashes": hashes,
        "sizes": sizes,
        "pay": {
            "payplay": raw.get("pay_type").and_then(Value::as_i64).unwrap_or(0),
        }
    })
}

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
            .map(|s| normalize_special_song(s, img_url))
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
