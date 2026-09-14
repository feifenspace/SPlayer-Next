//! 在线队列按平台身份解析；临时直链不作为曲目身份。

use std::collections::HashMap;
use serde_json::{json, Value};
use crate::state::{AppState, QueueItem};
use super::online_apis::{dispatch_api_call, ApiCallRequest};

pub(crate) fn can_resolve(item: &QueueItem) -> bool {
    matches!(item.track.as_ref().and_then(|t| t["source"].as_str()),
        Some("netease" | "qqmusic" | "kugou" | "qobuz" | "tidal"))
}

pub(crate) async fn resolve(state: &AppState, item: &QueueItem) -> Result<String, String> {
    let track = item.track.as_ref().ok_or("队列条目缺少平台身份")?;
    let platform = track["source"].as_str().ok_or("缺少平台")?;
    let id = track.get("id").filter(|v| !v.is_null()).ok_or("缺少曲目 ID")?;
    let quality = track["headlessQuality"].as_str().unwrap_or("lossless");
    let mut params = HashMap::new();
    params.insert("level".into(), json!(quality));
    match platform {
        "qqmusic" => {
            params.insert("mid".into(), id.clone());
            if let Some(mid) = track.get("mediaId").filter(|v| !v.is_null()) {
                params.insert("mediaMid".into(), mid.clone());
            }
        }
        "kugou" => {
            params.insert("hash".into(), id.clone());
            for (key, value) in [("audioId", &track["extId"]), ("albumId", &track["album"]["id"])] {
                if !value.is_null() { params.insert(key.into(), value.clone()); }
            }
        }
        "netease" => {
            params.insert("id".into(), id.clone());
            params.insert("level".into(), json!(match quality {
                "hi-res" => "hires", "hq" => "exhigh", "sq" => "higher",
                "lq" => "standard", _ => "lossless",
            }));
        }
        "qobuz" | "tidal" => { params.insert("track_id".into(), id.clone()); }
        _ => return Err("该平台尚无服务端音源解析器".into()),
    }
    let response = tokio::time::timeout(std::time::Duration::from_secs(30),
        dispatch_api_call(ApiCallRequest { platform: platform.into(), name: "song_url".into(), params }, &state.db))
        .await.map_err(|_| "在线音源解析超时")?;
    if !response.ok { return Err(format!("{platform} 音源解析失败，请检查账号或网络")); }
    let body = response.data.or(response.body).ok_or("平台未返回音源")?;
    extract_url(&body).ok_or_else(|| format!("{platform} 未提供完整音源，可能无权限或登录过期"))
}

fn extract_url(body: &Value) -> Option<String> {
    let item = body.pointer("/data/0").or_else(|| body.get("data")).unwrap_or(body);
    if item.get("freeTrialInfo").is_some_and(|v| !v.is_null()) { return None; }
    item.get("url").and_then(Value::as_str)
        .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_trial_and_empty_responses() {
        assert_eq!(extract_url(&json!({"data":[{"url":"https://audio/song", "freeTrialInfo": {}}]})), None);
        assert_eq!(extract_url(&json!({"data":[]})), None);
        assert_eq!(extract_url(&json!({"data":{"url":"file:///etc/passwd"}})), None);
    }
    #[test]
    fn accepts_platform_response_shapes() {
        for value in [json!({"data":[{"url":"https://audio/song"}]}), json!({"data":{"url":"https://audio/song"}}), json!({"url":"https://audio/song"})] {
            assert_eq!(extract_url(&value).as_deref(), Some("https://audio/song"));
        }
    }
}
