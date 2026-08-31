use std::collections::HashMap;
use base64::Engine;
use serde_json::{json, Value};

use crate::error::StreamingError;
use super::TidalClient;

fn tidal_quality_for_level(level: &str) -> &'static [&'static str] {
    match level.to_lowercase().as_str() {
        "hi-res" | "hires" => &["HI_RES_LOSSLESS", "LOSSLESS", "HIGH", "LOW"],
        "lossless" | "flac" => &["LOSSLESS", "HIGH", "LOW"],
        "hq" | "320" => &["HIGH", "LOW"],
        "sq" | "128" | "lq" => &["LOW"],
        _ => &["HI_RES_LOSSLESS", "LOSSLESS", "HIGH", "LOW"],
    }
}

impl TidalClient {
    /// 获取 TIDAL 播放直链（解析 BTS / DASH 得到无损 FLAC / Hi-Res 音源）
    pub async fn track_get_stream_url(
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

        let qualities = tidal_quality_for_level(level_str);

        for &quality in qualities {
            let mut req_params = HashMap::new();
            req_params.insert("audioquality".to_string(), quality.to_string());
            req_params.insert("playbackmode".to_string(), "STREAM".to_string());
            req_params.insert("assetpresentation".to_string(), "FULL".to_string());

            let endpoint = format!("tracks/{}/playbackinfopostpaywall", track_id);
            match self.request(&endpoint, req_params, true).await {
                Ok(body) => {
                    let mime_type = body
                        .get("manifestMimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let manifest_b64 = body.get("manifest").and_then(Value::as_str).unwrap_or("");

                    if mime_type == "application/vnd.tidal.bts" && !manifest_b64.is_empty() {
                        if let Ok(decoded) =
                            base64::engine::general_purpose::STANDARD.decode(manifest_b64)
                        {
                            if let Ok(manifest_json) = serde_json::from_slice::<Value>(&decoded) {
                                if let Some(urls) = manifest_json.get("urls").and_then(Value::as_array) {
                                    if let Some(first_url) = urls.first().and_then(Value::as_str) {
                                        let codecs = manifest_json
                                            .get("codecs")
                                            .and_then(Value::as_str)
                                            .unwrap_or("flac");
                                        let bit_depth = body.get("bitDepth").and_then(Value::as_i64);
                                        let sample_rate = body.get("sampleRate").and_then(Value::as_f64);
                                        let sound_quality = body
                                            .get("audioQuality")
                                            .and_then(Value::as_str)
                                            .unwrap_or(quality);

                                        return Ok(json!({
                                            "code": 200,
                                            "data": {
                                                "url": first_url,
                                                "audioQuality": sound_quality,
                                                "codec": codecs,
                                                "bitDepth": bit_depth,
                                                "sampleRate": sample_rate,
                                            },
                                            "raw": body
                                        }));
                                    }
                                }
                            }
                        }
                    } else if let Some(direct_url) = body.get("url").and_then(Value::as_str) {
                        if !direct_url.is_empty() {
                            return Ok(json!({
                                "code": 200,
                                "data": {
                                    "url": direct_url,
                                    "audioQuality": quality,
                                },
                                "raw": body
                            }));
                        }
                    }
                }
                Err(_) => {
                    // 降级尝试下一个音质
                }
            }
        }

        Err(StreamingError::Api {
            status: 403,
            message: "Unable to obtain stream URL from TIDAL, track may be unavailable or subscription expired".into(),
        })
    }
}
