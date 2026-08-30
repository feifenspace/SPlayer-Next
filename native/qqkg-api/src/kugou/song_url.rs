//! 酷狗单曲播放链接解析模块（对齐桌面端 electron/main/apis/kugou/modules/song_url.ts + 智能降级与重试）。

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

fn extract_urls_from_val(val: Option<&Value>, urls: &mut Vec<String>) {
    if let Some(v) = val {
        if let Some(s) = v.as_str() {
            if !s.is_empty() {
                urls.push(s.to_string());
            }
        } else if let Some(arr) = v.as_array() {
            for item in arr {
                if let Some(s) = item.as_str() {
                    if !s.is_empty() {
                        urls.push(s.to_string());
                    }
                }
            }
        }
    }
}

impl KugouClient {
    async fn fetch_gateway_url(
        &self,
        hash: &str,
        quality: &str,
        album_id: u64,
        album_audio_id: u64,
        key: &str,
    ) -> Result<Vec<String>, QqkgError> {
        let gateway_params = vec![
            ("album_id".into(), album_id.to_string()),
            ("area_code".into(), "1".into()),
            ("hash".into(), hash.to_string()),
            ("ssa_flag".into(), "is_fromtrack".into()),
            ("version".into(), "11430".into()),
            ("page_id".into(), "151369488".into()),
            ("quality".into(), quality.to_string()),
            ("album_audio_id".into(), album_audio_id.to_string()),
            ("behavior".into(), "play".into()),
            ("pid".into(), "2".into()),
            ("cmd".into(), "26".into()),
            ("pidversion".into(), "3001".into()),
            ("IsFreePart".into(), "1".into()),
            ("ppage_id".into(), "463467626,350369493,788954147".into()),
            ("cdnBackup".into(), "1".into()),
            ("module".into(), "".into()),
            ("clientver".into(), "11430".into()),
            ("key".into(), key.to_string()),
        ];

        let resp = self
            .kg_gateway_request(
                "/v5/url",
                &gateway_params,
                &[("x-router", "trackercdn.kugou.com".into())],
            )
            .await?;

        let mut urls = Vec::new();
        extract_urls_from_val(resp.get("url"), &mut urls);
        extract_urls_from_val(resp.get("backupUrl"), &mut urls);
        extract_urls_from_val(resp.get("backup_url"), &mut urls);
        Ok(urls)
    }

    /// 解析酷狗单曲播放直链。
    pub async fn song_url(&self, params: &HashMap<String, Value>) -> Result<Value, QqkgError> {
        let hash = params
            .get("hash")
            .or_else(|| params.get("id"))
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

        let album_audio_id = params
            .get("albumAudioId")
            .or_else(|| params.get("album_audio_id"))
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
            })
            .unwrap_or(0);

        let userid = self
            .cookies
            .get("userid")
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| "0".to_string());

        let mid = device_mid().to_string();
        let key = sign_key(&hash, &mid, &userid, KG_APPID);

        let mut urls: Vec<String> = Vec::new();

        // 1. 尝试带专辑参数的主通道
        if let Ok(u) = self
            .fetch_gateway_url(&hash, quality, album_id, album_audio_id, &key)
            .await
        {
            urls.extend(u);
        }

        // 2. 若失败且传入了非 0 的 album_id/album_audio_id，尝试通用兜底（album_id=0, album_audio_id=0）
        if urls.is_empty() && (album_id > 0 || album_audio_id > 0) {
            if let Ok(u) = self
                .fetch_gateway_url(&hash, quality, 0, 0, &key)
                .await
            {
                urls.extend(u);
            }
        }

        // 3. 若质量较高且未拿到链接，尝试 128k 档位
        if urls.is_empty() && quality != "128" {
            if let Ok(u) = self.fetch_gateway_url(&hash, "128", 0, 0, &key).await {
                urls.extend(u);
            }
        }

        // 4. 若主网关仍未提取到链接，尝试 m.kugou.com 接口
        if urls.is_empty() {
            let m_url = format!("http://m.kugou.com/app/i/getSongInfo.php?cmd=playInfo&hash={hash}");
            if let Ok(m_resp) = self.kg_request(&m_url).await {
                extract_urls_from_val(m_resp.get("url"), &mut urls);
                extract_urls_from_val(m_resp.get("backup_url"), &mut urls);
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
            Ok(json!({
                "code": 500,
                "message": "未找到可用音频链接",
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
        assert_eq!(map_quality("lossless"), "flac");
        assert_eq!(map_quality("hq"), "320");
        assert_eq!(map_quality("sq"), "128");
        assert_eq!(map_quality("lq"), "128");
    }
}
