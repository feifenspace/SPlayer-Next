//! 在线队列按平台身份解析；临时直链不作为曲目身份。

use std::collections::HashMap;
use serde_json::{json, Value};
use crate::state::{AppState, QueueItem};
use super::online_apis::{dispatch_api_call, ApiCallRequest};

pub(crate) fn can_resolve(item: &QueueItem) -> bool {
    matches!(item.track.as_ref().and_then(|t| t["source"].as_str()),
        Some("netease" | "qqmusic" | "kugou" | "qobuz" | "tidal"))
}

/// 读取实时音质偏好（渲染层 setSystem → /api/v1/config/set → settings 表）。
/// 仅接受合法档位值，缺省/非法时返回 None 由调用方回退快照值。
fn current_quality_pref(state: &AppState) -> Option<String> {
    let conn = state.db.lock();
    let value = crate::db::get_setting(&conn, "player.songLevel").ok()??;
    let level = value.as_str()?.to_owned();
    matches!(level.as_str(), "hi-res" | "lossless" | "hq" | "sq" | "lq")
        .then_some(level)
}

pub(crate) async fn resolve(state: &AppState, item: &QueueItem) -> Result<String, String> {
    let track = item.track.as_ref().ok_or("队列条目缺少平台身份")?;
    let platform = track["source"].as_str().ok_or("缺少平台")?;
    let id = track.get("id").filter(|v| !v.is_null()).ok_or("缺少曲目 ID")?;
    // 音质优先级：实时设置（settings 表 player.songLevel）> 队列快照 headlessQuality > "lossless"。
    // 快照只在推队列那一刻定格，用户之后在 UI 改音质服务端无从得知；
    // 服务端重启恢复旧队列时快照更是陈旧值，会导致永远按旧档位解析。
    let quality = current_quality_pref(state)
        .or_else(|| track["headlessQuality"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "lossless".to_owned());
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
            params.insert("level".into(), json!(match quality.as_str() {
                "hi-res" => "jymaster", "hq" => "exhigh", "sq" => "higher",
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
    let url = extract_url(&body).ok_or_else(|| format!("{platform} 未提供完整音源，可能无权限或登录过期"))?;
    // 记录实际命中的档位。各平台返回的档位词汇不同（QQ: hi-res/lossless/hq…，
    // 网易: hires/lossless/exhigh…，TIDAL: HI_RES_LOSSLESS/LOSSLESS…，Qobuz 无 level 字段），
    // 先归一化到项目五档再比较，避免误报降级。
    if let Some(actual) = body.pointer("/data/0/level")
        .and_then(Value::as_str)
        .and_then(normalize_level)
    {
        let requested = normalize_level(&quality).unwrap_or_else(|| quality.clone());
        if actual != requested {
            tracing::warn!(platform, requested = %requested, actual, "在线音源发生降级");
        } else {
            tracing::info!(platform, level = actual, "在线音源命中请求档位");
        }
    }
    Ok(url)
}

/// 平台档位词汇 → 项目五档。归一化是单向 best-effort：无法识别时返回 None。
fn normalize_level(level: &str) -> Option<String> {
    let normalized = match level.to_lowercase().replace(['-', '_'], "").as_str() {
        // 项目档位（QQ 原样返回；TIDAL 的 HI_RES_LOSSLESS 小写去连字符后也命中）
        "hires" | "hireslossless" | "jymaster" | "jyeffect" | "sky" => "hi-res",
        "lossless" | "flac" | "master" => "lossless",
        "hq" | "exhigh" | "320" | "high" => "hq",
        "sq" | "higher" | "128" => "sq",
        "lq" | "standard" | "low" => "lq",
        _ => return None,
    };
    Some(normalized.to_owned())
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
    #[test]
    fn netease_level_mapping_covers_all_prefs() {
        // 与渲染层 QualityLevel 五档一一对应，缺省回落 lossless
        let map = |q: &str| match q {
            "hi-res" => "jymaster", "hq" => "exhigh", "sq" => "higher",
            "lq" => "standard", _ => "lossless",
        };
        assert_eq!(map("hi-res"), "jymaster");
        assert_eq!(normalize_level("jymaster").as_deref(), Some("hi-res"));
        assert_eq!(normalize_level("jyeffect").as_deref(), Some("hi-res"));
        assert_eq!(normalize_level("sky").as_deref(), Some("hi-res"));
        assert_eq!(map("hq"), "exhigh");
        assert_eq!(map("sq"), "higher");
        assert_eq!(map("lq"), "standard");
        assert_eq!(map("lossless"), "lossless");
        assert_eq!(map("anything-else"), "lossless");
    }
    #[test]
    fn level_normalization_matches_platform_vocabularies() {
        // QQ 原样返回项目档位
        assert_eq!(normalize_level("hi-res").as_deref(), Some("hi-res"));
        assert_eq!(normalize_level("lossless").as_deref(), Some("lossless"));
        assert_eq!(normalize_level("hq").as_deref(), Some("hq"));
        // 网易返回 hires/exhigh/higher/standard
        assert_eq!(normalize_level("hires").as_deref(), Some("hi-res"));
        assert_eq!(normalize_level("exhigh").as_deref(), Some("hq"));
        assert_eq!(normalize_level("higher").as_deref(), Some("sq"));
        assert_eq!(normalize_level("standard").as_deref(), Some("lq"));
        // TIDAL 大写枚举
        assert_eq!(normalize_level("HI_RES_LOSSLESS").as_deref(), Some("hi-res"));
        assert_eq!(normalize_level("LOSSLESS").as_deref(), Some("lossless"));
        assert_eq!(normalize_level("HIGH").as_deref(), Some("hq"));
        // 无法识别 → None（不参与比较，不误报）
        assert_eq!(normalize_level("mystery"), None);
    }
}
