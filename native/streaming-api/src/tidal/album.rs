use std::collections::HashMap;
use serde_json::Value;

use crate::error::StreamingError;
use super::TidalClient;

impl TidalClient {
    /// 获取 TIDAL 专辑详情
    pub async fn album_get(&self, params: &HashMap<String, Value>) -> Result<Value, StreamingError> {
        let album_id = params
            .get("album_id")
            .or_else(|| params.get("id"))
            .and_then(|v| {
                v.as_str()
                    .map(ToString::to_string)
                    .or_else(|| v.as_u64().map(|n| n.to_string()))
            })
            .unwrap_or_default();

        if album_id.is_empty() {
            return Err(StreamingError::InvalidParam("Missing album_id".into()));
        }

        let endpoint = format!("albums/{}", album_id);
        self.request(&endpoint, HashMap::new(), true).await
    }

    /// 获取 TIDAL 专辑内的歌曲列表
    pub async fn album_get_tracks(
        &self,
        params: &HashMap<String, Value>,
    ) -> Result<Value, StreamingError> {
        let album_id = params
            .get("album_id")
            .or_else(|| params.get("id"))
            .and_then(|v| {
                v.as_str()
                    .map(ToString::to_string)
                    .or_else(|| v.as_u64().map(|n| n.to_string()))
            })
            .unwrap_or_default();

        if album_id.is_empty() {
            return Err(StreamingError::InvalidParam("Missing album_id".into()));
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

        let endpoint = format!("albums/{}/tracks", album_id);
        self.request(&endpoint, req_params, true).await
    }
}
