//! QQ 音乐歌单详情与歌曲列表模块（对齐桌面端 electron/main/apis/qqmusic/modules/song_list.ts）。

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::error::QqkgError;
use crate::normalize::join_singers;
use crate::qqmusic::QqmusicClient;


const SONGLIST_URL: &str =
    "https://c.y.qq.com/qzone/fcg-bin/fcg_ucc_getcdinfo_byids_cp.fcg?type=1&json=1&utf8=1&onlysonglist=0&platform=yqq&needNewCode=0";

impl QqmusicClient {
    /// 获取歌单详情及歌曲列表。
    pub async fn playlist(&self, params: &HashMap<String, Value>) -> Result<Value, QqkgError> {
        let id = params
            .get("id")
            .or_else(|| params.get("disstid"))
            .or_else(|| params.get("dissid"))
            .and_then(|v| {
                v.as_str()
                    .map(ToString::to_string)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            })
            .unwrap_or_default();

        if id.is_empty() {
            return Err(QqkgError::InvalidParam("id required".into()));
        }

        let url = format!("{SONGLIST_URL}&disstid={}", urlencoding::encode(&id));

        let mut req_builder = self
            .http
            .get(&url)
            .header("Referer", "https://y.qq.com/")
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
            );

        let cookie_str = self.cookie_header();
        if !cookie_str.is_empty() {
            req_builder = req_builder.header("Cookie", cookie_str);
        }

        let resp = req_builder
            .send()
            .await
            .map_err(|e| QqkgError::Upstream(format!("QM playlist HTTP error: {e}")))?;

        let text = resp
            .text()
            .await
            .map_err(|e| QqkgError::BadResponse(format!("QM playlist read text error: {e}")))?;

        let cleaned = text
            .trim()
            .trim_start_matches("jsonCallback(")
            .trim_end_matches(')')
            .trim_end_matches(';');

        let data: Value = serde_json::from_str(cleaned)
            .map_err(|e| QqkgError::BadResponse(format!("QM playlist non-JSON: {e}")))?;

        let cd = match data.get("cdlist").and_then(Value::as_array).and_then(|a| a.first()) {
            Some(c) => c,
            None => return Ok(json!({ "code": 404, "message": "歌单不存在" })),
        };

        let empty_vec = Vec::new();
        let raw_songs = cd.get("songlist").and_then(Value::as_array).unwrap_or(&empty_vec);

        let mut songs: Vec<Value> = Vec::new();
        for item in raw_songs {
            let mid = match item.get("songmid").and_then(Value::as_str) {
                Some(m) if !m.is_empty() => m,
                _ => continue,
            };

            let singers = item.get("singer").and_then(Value::as_array);
            let artist_name = singers.map(|s| join_singers(s)).unwrap_or_default();

            let album_name = item.get("albumname").and_then(Value::as_str).unwrap_or("");
            let album_mid = item.get("albummid").and_then(Value::as_str).unwrap_or("");
            let interval = item.get("interval").and_then(Value::as_u64).unwrap_or(0);

            let cover = format!("https://y.gtimg.cn/music/photo_new/T002R300x300M000{album_mid}.jpg");
            let cover_orig = format!("https://y.gtimg.cn/music/photo_new/T002R800x800M000{album_mid}.jpg");

            songs.push(json!({
                "id": item.get("songid").and_then(|v| v.as_i64()).map(|n| n.to_string()).unwrap_or_default(),
                "mid": mid,
                "name": item.get("songname").and_then(Value::as_str).unwrap_or(""),
                "artist": artist_name,
                "artists": singers.cloned().unwrap_or_default(),
                "album": album_name,
                "albumMid": album_mid,
                "cover": cover,
                "coverOriginal": cover_orig,
                "duration": interval * 1000,
                "mediaMid": item.get("strMediaMid").and_then(Value::as_str).unwrap_or(""),
                "size128": item.get("size128").and_then(Value::as_u64).unwrap_or(0),
                "size320": item.get("size320").and_then(Value::as_u64).unwrap_or(0),
                "sizeApe": item.get("sizeape").and_then(Value::as_u64).unwrap_or(0),
                "sizeFlac": item.get("sizeflac").and_then(Value::as_u64).unwrap_or(0),
                "sizeOgg": item.get("sizeogg").and_then(Value::as_u64).unwrap_or(0),
            }));
        }

        Ok(json!({
            "code": 200,
            "id": cd.get("disstid").and_then(|v| v.as_str().map(ToString::to_string).or_else(|| v.as_i64().map(|n| n.to_string()))).unwrap_or(id),
            "name": cd.get("dissname").and_then(Value::as_str).unwrap_or(""),
            "description": cd.get("desc").and_then(Value::as_str).unwrap_or(""),
            "creator": cd.get("nickname").and_then(Value::as_str).unwrap_or(""),
            "cover": cd.get("logo").and_then(Value::as_str).unwrap_or(""),
            "playCount": cd.get("visitnum").and_then(Value::as_u64).unwrap_or(0),
            "total": cd.get("songnum").and_then(Value::as_u64).unwrap_or(songs.len() as u64),
            "songs": songs,
        }))
    }
}
