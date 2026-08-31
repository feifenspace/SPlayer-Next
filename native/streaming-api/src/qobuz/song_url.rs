use std::collections::HashMap;
use serde_json::{json, Value};

use crate::error::StreamingError;
use super::credentials::{get_credentials, set_working_secret};
use super::QobuzClient;

fn format_id_for_level(level: &str) -> &'static [u32] {
    match level.to_lowercase().as_str() {
        "hi-res" | "hires" => &[27, 7, 6, 5],
        "lossless" | "flac" => &[6, 5],
        "hq" | "320" => &[5],
        "sq" | "128" | "lq" => &[5],
        _ => &[27, 7, 6, 5],
    }
}

impl QobuzClient {
    /// 获取 Qobuz 音轨播放直链 (track/getFileUrl，自动多 Secret 探测与格式降级)
    pub async fn track_get_file_url(
        &self,
        params: &HashMap<String, Value>,
    ) -> Result<Value, StreamingError> {
        let track_id = params
            .get("track_id")
            .or_else(|| params.get("id"))
            .and_then(|v| {
                v.as_str()
                    .map(ToString::to_string)
                    .or_else(|| v.as_u64().map(|n| n.to_string()))
            })
            .unwrap_or_default();

        if track_id.is_empty() {
            return Err(StreamingError::InvalidParam("Missing track_id".into()));
        }

        let level_str = params
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("hi-res");

        let candidate_formats = format_id_for_level(level_str);
        let creds = get_credentials(&self.http, false).await?;

        // 收集所有候选 secrets
        let mut try_secrets = Vec::new();
        if !creds.secret.is_empty() {
            try_secrets.push(creds.secret.clone());
        }
        for s in &creds.secrets {
            if !try_secrets.contains(s) {
                try_secrets.push(s.clone());
            }
        }

        for &format_id in candidate_formats {
            for secret in &try_secrets {
                let mut req_params = HashMap::new();
                req_params.insert("track_id".to_string(), track_id.clone());
                req_params.insert("format_id".to_string(), format_id.to_string());
                req_params.insert("intent".to_string(), "stream".to_string());

                match self
                    .request("track/getFileUrl", req_params, true, true, Some(secret.clone()))
                    .await
                {
                    Ok(body) => {
                        if let Some(url) = body.get("url").and_then(Value::as_str) {
                            if !url.is_empty() {
                                set_working_secret(secret.clone()).await;
                                let mime = body
                                    .get("mime_type")
                                    .and_then(Value::as_str)
                                    .unwrap_or("audio/flac");
                                let duration = body.get("duration").and_then(Value::as_f64);
                                let sampling_rate = body.get("sampling_rate").and_then(Value::as_f64);
                                let bit_depth = body.get("bit_depth").and_then(Value::as_i64);

                                return Ok(json!({
                                    "code": 200,
                                    "data": {
                                        "url": url,
                                        "format_id": format_id,
                                        "mime_type": mime,
                                        "duration": duration,
                                        "sampling_rate": sampling_rate,
                                        "bit_depth": bit_depth,
                                    },
                                    "raw": body
                                }));
                            }
                        }
                    }
                    Err(_) => {
                        // 继续尝试下一个 secret
                    }
                }
            }
        }

        Err(StreamingError::Api {
            status: 403,
            message: "Unable to obtain stream URL from Qobuz, subscription may be expired or track unavailable".into(),
        })
    }
}
