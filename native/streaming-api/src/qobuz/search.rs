use std::collections::HashMap;
use serde_json::Value;

use crate::error::StreamingError;
use super::QobuzClient;

impl QobuzClient {
    /// 搜索 Qobuz 资源（tracks / albums / artists / playlists）
    pub async fn catalog_search(
        &self,
        params: &HashMap<String, Value>,
    ) -> Result<Value, StreamingError> {
        let query = params
            .get("query")
            .or_else(|| params.get("keyword"))
            .or_else(|| params.get("keywords"))
            .and_then(Value::as_str)
            .unwrap_or("");

        let search_type = params
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("tracks");

        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(20);

        let offset = params
            .get("offset")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(0);

        let mut req_params = HashMap::new();
        req_params.insert("query".to_string(), query.to_string());
        req_params.insert("type".to_string(), search_type.to_string());
        req_params.insert("limit".to_string(), limit.to_string());
        req_params.insert("offset".to_string(), offset.to_string());

        self.request("catalog/search", req_params, true, false, None).await
    }
}
