//! 酷狗单曲播放链接解析模块（对齐桌面端 electron/main/apis/kugou/modules/song_url.ts）。
//!
//! 纯 Rust 实现，零 Node 运行时依赖：
//! - 基于 `sign_key(hash, mid, userid, appid)` 计算签名
//! - 请求网关 `/v5/url`（`x-router: trackercdn.kugou.com`）
//! - 解析 `url` 与 `backup_url`，返回规范化播放链接

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::crypto::kg_mid::device_mid;
use crate::crypto::kg_sign::sign_key;
use crate::error::QqkgError;
use crate::kugou::KugouClient;
use crate::types::KG_APPID;

fn map_quality(level: &str) -> &'static str {
    match level.to_lowercase().as_str() {
        "hi-res" => "high",
        "lossless" => "flac",
        "hq" => "320",
        "sq" => "128",
        "lq" => "128",
        _ => "320",
    }
}

impl KugouClient {
    /// 解析酷狗单曲播放直链。
    pub async fn song_url(&self, params: &HashMap<String, Value>) -> Result<Value, QqkgError> {
        let hash = params
            .get("hash")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_lowercase();

        if hash.is_empty() {
            return Err(QqkgError::InvalidParam("KG song hash missing".into()));
        }

        let level_str = params
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("hq");
        let quality = map_quality(level_str);

        let album_id = params
            .get("albumId")
            .or_else(|| params.get("album_id"))
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
            })
            .unwrap_or(0);

        let audio_id = params
            .get("audioId")
            .or_else(|| params.get("audio_id"))
            .or_else(|| params.get("album_audio_id"))
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
            })
            .unwrap_or(0);

        let is_free_part = params
            .get("freePart")
            .and_then(Value::as_bool)
            .map(|b| if b { "1" } else { "0" })
            .unwrap_or("0");

        let userid = self
            .cookies
            .get("userid")
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| "0".to_string());

        let mid = device_mid().to_string();
        let key = sign_key(&hash, &mid, &userid, KG_APPID);

        let gateway_params = vec![
            ("album_id".into(), album_id.to_string()),
            ("area_code".into(), "1".into()),
            ("hash".into(), hash.clone()),
            ("ssa_flag".into(), "is_fromtrack".into()),
            ("version".into(), "11430".into()),
            ("page_id".into(), "151369488".into()),
            ("quality".into(), quality.to_string()),
            ("album_audio_id".into(), audio_id.to_string()),
            ("behavior".into(), "play".into()),
            ("pid".into(), "2".into()),
            ("cmd".into(), "26".into()),
            ("pidversion".into(), "3001".into()),
            ("IsFreePart".into(), is_free_part.into()),
            ("ppage_id".into(), "463467626,350369493,788954147".into()),
            ("cdnBackup".into(), "1".into()),
            ("module".into(), "".into()),
            ("clientver".into(), "11430".into()),
            ("key".into(), key),
        ];

        let resp = self
            .kg_gateway_request(
                "/v5/url",
                &gateway_params,
                &[("x-router", "trackercdn.kugou.com".into())],
            )
            .await?;

        let mut urls: Vec<String> = Vec::new();

        if let Some(u) = resp.get("url") {
            if let Some(s) = u.as_str() {
                if !s.is_empty() {
                    urls.push(s.to_string());
                }
            } else if let Some(arr) = u.as_array() {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        if !s.is_empty() {
                            urls.push(s.to_string());
                        }
                    }
                }
            }
        }

        if let Some(bu) = resp.get("backup_url") {
            if let Some(s) = bu.as_str() {
                if !s.is_empty() {
                    urls.push(s.to_string());
                }
            } else if let Some(arr) = bu.as_array() {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        if !s.is_empty() {
                            urls.push(s.to_string());
                        }
                    }
                }
            }
        }

        if !urls.is_empty() {
            Ok(json!({
                "code": 200,
                "data": {
                    "url": urls[0]
                }
            }))
        } else {
            let errcode = resp.get("errcode").and_then(Value::as_i64).unwrap_or(500);
            let msg = resp
                .get("error")
                .or_else(|| resp.get("msg"))
                .and_then(Value::as_str)
                .unwrap_or("未找到可用音频链接");
            Ok(json!({
                "code": errcode,
                "message": msg,
                "data": null
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_mapping_variants() {
        assert_eq!(map_quality("hi-res"), "high");
        assert_eq!(map_quality("HI-RES"), "high");
        assert_eq!(map_quality("lossless"), "flac");
        assert_eq!(map_quality("hq"), "320");
        assert_eq!(map_quality("sq"), "128");
        assert_eq!(map_quality("lq"), "128");
        assert_eq!(map_quality("unknown"), "320");
    }
}
