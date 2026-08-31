//! QQ 音乐排行榜模块（对齐桌面端 electron/main/apis/qqmusic/modules/leaderboard.ts）。

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::error::QqkgError;
use crate::qqmusic::QqmusicClient;



impl QqmusicClient {
    /// 获取排行榜歌曲列表及期数信息。
    pub async fn leaderboard(&self, params: &HashMap<String, Value>) -> Result<Value, QqkgError> {
        let topid = params
            .get("topid")
            .or_else(|| params.get("id"))
            .and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
            })
            .unwrap_or(4); // 默认飙升榜

        let period = params
            .get("period")
            .and_then(Value::as_str)
            .unwrap_or("");

        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(50);

        let offset = params
            .get("offset")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(0);

        let data = self
            .post_fcg(
                "musicToplist.ToplistInfoServer",
                "GetDetail",
                json!({
                    "topId": topid,
                    "topid": topid,
                    "num": limit,
                    "offset": offset,
                    "period": period,
                }),
            )
            .await?;

        // 兼容 songInfoList / songlist / song_list 字段
        let empty_vec = Vec::new();
        let raw_song_list = data
            .get("songInfoList")
            .or_else(|| data.get("songlist"))
            .or_else(|| data.get("songList"))
            .or_else(|| data.get("data").and_then(|d| d.get("songInfoList")))
            .and_then(Value::as_array)
            .unwrap_or(&empty_vec);


        let mut songs: Vec<Value> = Vec::new();
        for item in raw_song_list {
            if let Some(song) = item.get("songInfo") {
                songs.push(crate::qqmusic::search::normalize_song(song));
            }
        }


        Ok(json!({
            "code": 200,
            "title": data.get("title").and_then(Value::as_str).unwrap_or(""),
            "subTitle": data.get("titleDetail").and_then(Value::as_str).unwrap_or(""),
            "updateTime": data.get("updateTime").and_then(Value::as_str).unwrap_or(""),
            "cover": data.get("headPicUrl").and_then(Value::as_str).unwrap_or(""),
            "songs": songs,
        }))
    }
}
