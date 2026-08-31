use std::collections::HashMap;
use serde_json::Value;

use crate::error::StreamingError;
use super::TidalClient;

impl TidalClient {
    /// 获取 TIDAL 歌手详情
    pub async fn artist_get(&self, params: &HashMap<String, Value>) -> Result<Value, StreamingError> {
        let artist_id = params
            .get("artist_id")
            .or_else(|| params.get("id"))
            .and_then(|v| {
                v.as_str()
                    .map(ToString::to_string)
                    .or_else(|| v.as_u64().map(|n| n.to_string()))
            })
            .unwrap_or_default();

        if artist_id.is_empty() {
            return Err(StreamingError::InvalidParam("Missing artist_id".into()));
        }

        let endpoint = format!("artists/{}", artist_id);
        self.request(&endpoint, HashMap::new(), true).await
    }

    /// 获取 TIDAL 歌手专辑列表
    pub async fn artist_get_albums(
        &self,
        params: &HashMap<String, Value>,
    ) -> Result<Value, StreamingError> {
        let artist_id = params
            .get("artist_id")
            .or_else(|| params.get("id"))
            .and_then(|v| {
                v.as_str()
                    .map(ToString::to_string)
                    .or_else(|| v.as_u64().map(|n| n.to_string()))
            })
            .unwrap_or_default();

        if artist_id.is_empty() {
            return Err(StreamingError::InvalidParam("Missing artist_id".into()));
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

        let endpoint = format!("artists/{}/albums", artist_id);
        self.request(&endpoint, req_params, true).await
    }

    /// 获取 TIDAL 歌手热门曲目
    pub async fn artist_get_top_tracks(
        &self,
        params: &HashMap<String, Value>,
    ) -> Result<Value, StreamingError> {
        let artist_id = params
            .get("artist_id")
            .or_else(|| params.get("id"))
            .and_then(|v| {
                v.as_str()
                    .map(ToString::to_string)
                    .or_else(|| v.as_u64().map(|n| n.to_string()))
            })
            .unwrap_or_default();

        if artist_id.is_empty() {
            return Err(StreamingError::InvalidParam("Missing artist_id".into()));
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

        let endpoint = format!("artists/{}/toptracks", artist_id);
        self.request(&endpoint, req_params, true).await
    }
}
