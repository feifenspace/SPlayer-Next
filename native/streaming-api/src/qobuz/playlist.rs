use std::collections::HashMap;
use serde_json::Value;

use crate::error::StreamingError;
use super::QobuzClient;

impl QobuzClient {
    /// 获取 Qobuz 歌单详情及歌曲
    pub async fn playlist_get(&self, params: &HashMap<String, Value>) -> Result<Value, StreamingError> {
        let playlist_id = params
            .get("playlist_id")
            .or_else(|| params.get("id"))
            .and_then(|v| {
                v.as_str()
                    .map(ToString::to_string)
                    .or_else(|| v.as_u64().map(|n| n.to_string()))
            })
            .unwrap_or_default();

        if playlist_id.is_empty() {
            return Err(StreamingError::InvalidParam("Missing playlist_id".into()));
        }

        let mut req_params = HashMap::new();
        req_params.insert("playlist_id".to_string(), playlist_id);
        req_params.insert("extra".to_string(), "tracks".to_string());

        self.request("playlist/get", req_params, false, false, None).await
    }
}
