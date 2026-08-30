//! QQ 音乐热搜关键词模块（对齐桌面端 electron/main/apis/qqmusic/modules/hot_search.ts）。

use serde_json::{json, Value};

use crate::error::QqkgError;
use crate::qqmusic::QqmusicClient;

impl QqmusicClient {
    /// 获取 QQ 音乐热门搜索词列表。
    pub async fn hot_search(&self) -> Result<Value, QqkgError> {
        let data = self
            .post_fcg(
                "tencent_musicsoso_hotkey.HotkeyService",
                "GetHotkeyForQQMusicPC",
                json!({ "search_id": "", "uin": 0 }),
            )
            .await?;

        let empty_vec = Vec::new();
        let hot_keys = data
            .get("vec_hotkey")
            .and_then(Value::as_array)
            .unwrap_or(&empty_vec);

        let list: Vec<Value> = hot_keys
            .iter()
            .map(|item| {
                let kw = item
                    .get("query")
                    .or_else(|| item.get("title"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                json!({
                    "keyword": kw,
                    "id": item.get("id").and_then(Value::as_i64).unwrap_or(0),
                })
            })
            .collect();

        Ok(json!({
            "code": 200,
            "list": list,
        }))
    }
}
