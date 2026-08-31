use std::collections::HashMap;
use serde_json::Value;

use crate::error::StreamingError;
use super::TidalClient;

impl TidalClient {
    /// 获取 TIDAL 用户收藏内容 (tracks / albums / artists / playlists)
    pub async fn user_get_favorites(
        &self,
        params: &HashMap<String, Value>,
    ) -> Result<Value, StreamingError> {
        let user_id = self
            .user_id
            .as_deref()
            .or_else(|| params.get("userId").and_then(Value::as_str))
            .or_else(|| params.get("user_id").and_then(Value::as_str))
            .ok_or_else(|| StreamingError::Auth("TIDAL user_id not available".into()))?;

        let fav_type = params
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("tracks");

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

        let endpoint = format!("users/{}/favorites/{}", user_id, fav_type);
        self.request(&endpoint, req_params, true).await
    }
}
