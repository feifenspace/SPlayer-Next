use std::collections::HashMap;
use serde_json::Value;

use crate::error::StreamingError;
use super::TidalClient;

impl TidalClient {
    /// 获取 TIDAL 歌单详情
    pub async fn playlist_get(
        &self,
        params: &HashMap<String, Value>,
    ) -> Result<Value, StreamingError> {
        let playlist_id = params
            .get("playlist_id")
            .or_else(|| params.get("id"))
            .or_else(|| params.get("uuid"))
            .and_then(Value::as_str)
            .unwrap_or_default();

        if playlist_id.is_empty() {
            return Err(StreamingError::InvalidParam("Missing playlist_id".into()));
        }

        let endpoint = format!("playlists/{}", playlist_id);
        self.request(&endpoint, HashMap::new(), true).await
    }

    /// 获取 TIDAL 歌单中的歌曲列表
    pub async fn playlist_get_tracks(
        &self,
        params: &HashMap<String, Value>,
    ) -> Result<Value, StreamingError> {
        let playlist_id = params
            .get("playlist_id")
            .or_else(|| params.get("id"))
            .or_else(|| params.get("uuid"))
            .and_then(Value::as_str)
            .unwrap_or_default();

        if playlist_id.is_empty() {
            return Err(StreamingError::InvalidParam("Missing playlist_id".into()));
        }

        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(50)
            .min(50);

        let offset = params
            .get("offset")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(0);

        let mut req_params = HashMap::new();
        req_params.insert("limit".to_string(), limit.to_string());
        req_params.insert("offset".to_string(), offset.to_string());

        let endpoint = format!("playlists/{}/tracks", playlist_id);
        self.request(&endpoint, req_params, true).await
    }
}
