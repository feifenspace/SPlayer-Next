use std::collections::HashMap;
use serde_json::Value;

use crate::error::StreamingError;
use super::QobuzClient;

impl QobuzClient {
    /// 获取 Qobuz 专辑详情与曲目
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

        let mut req_params = HashMap::new();
        req_params.insert("album_id".to_string(), album_id);

        let extra = params
            .get("extra")
            .and_then(Value::as_str)
            .unwrap_or("focus");
        req_params.insert("extra".to_string(), extra.to_string());

        self.request("album/get", req_params, false, false, None).await
    }
}
