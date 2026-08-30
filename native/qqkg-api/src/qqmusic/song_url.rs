//! QQ 音乐单曲播放链接解析模块（对齐桌面端 electron/main/apis/qqmusic/modules/song_url.ts）。
//!
//! 纯 Rust 实现，零 Node 运行时依赖：
//! - 按用户偏好音质（hi-res -> lossless -> hq -> sq -> lq）构造候选文件名序列
//! - 请求 `vkey.GetVkeyServer.CgiGetVkey` 获取 `midurlinfo` 与 `sip`
//! - 通过快速 Range: bytes=0-0 探测验证链接真实可用性并执行自动降级

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::QqkgError;
use crate::qqmusic::QqmusicClient;

struct QualityCandidate {
    prefix: &'static str,
    ext: &'static str,
    level: &'static str,
}

const CANDIDATES: [QualityCandidate; 5] = [
    QualityCandidate { prefix: "AI00", ext: ".flac", level: "hi-res" },
    QualityCandidate { prefix: "F000", ext: ".flac", level: "lossless" },
    QualityCandidate { prefix: "M800", ext: ".mp3", level: "hq" },
    QualityCandidate { prefix: "M500", ext: ".mp3", level: "sq" },
    QualityCandidate { prefix: "C400", ext: ".m4a", level: "lq" },
];

fn get_candidates(preferred: &str) -> &'static [QualityCandidate] {
    let target = preferred.to_lowercase();
    if let Some(idx) = CANDIDATES.iter().position(|c| c.level == target) {
        &CANDIDATES[idx..]
    } else {
        &CANDIDATES[..]
    }
}

const QM_API_URL: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
const QM_WEB_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

impl QqmusicClient {
    /// 极速探测音频直链在 CDN 上是否真实存在（仅读取 1 字节状态码）
    async fn probe_audio_url(&self, url: &str) -> bool {
        let req = self
            .http
            .get(url)
            .header("Range", "bytes=0-0")
            .header("Referer", "https://y.qq.com/")
            .header("User-Agent", QM_WEB_UA)
            .timeout(Duration::from_millis(1500));

        match req.send().await {
            Ok(res) => res.status().as_u16() == 200 || res.status().as_u16() == 206,
            Err(_) => false,
        }
    }

    /// 解析 QQ 音乐单曲播放直链。
    pub async fn song_url(&self, params: &HashMap<String, Value>) -> Result<Value, QqkgError> {
        let mid = params
            .get("mid")
            .or_else(|| params.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();

        if mid.is_empty() {
            return Err(QqkgError::InvalidParam("missing mid".into()));
        }

        let media_mid = params
            .get("mediaMid")
            .or_else(|| params.get("media_mid"))
            .and_then(Value::as_str)
            .unwrap_or(mid)
            .trim();

        let target_level = params
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("hq");

        let candidates = get_candidates(target_level);
        let filenames: Vec<String> = candidates
            .iter()
            .map(|c| format!("{}{}{}", c.prefix, media_mid, c.ext))
            .collect();

        let uin = self.uin();
        let playback_key = self
            .cookies
            .get("qm_keyst")
            .or_else(|| self.cookies.get("qqmusic_key"))
            .or_else(|| self.cookies.get("music_key"))
            .or_else(|| self.cookies.get("wxskey"))
            .or_else(|| self.cookies.get("pskey"))
            .map(String::as_str)
            .unwrap_or("");

        let has_auth = !playback_key.is_empty();

        // 8 位随机数字 guid
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(12345678);
        let guid = format!("{}", 10000000 + (now_nanos % 90000000));

        let uin_val: Value = if !uin.is_empty() && uin != "0" {
            uin.parse::<i64>().map(Value::from).unwrap_or(Value::from(uin.as_str()))
        } else {
            Value::from(0)
        };

        let mut comm = json!({
            "uin": uin_val,
            "format": "json",
            "ct": if has_auth { 19 } else { 24 },
            "cv": 0,
        });
        if has_auth {
            comm["authst"] = json!(playback_key);
        }

        let req_body = json!({
            "comm": comm,
            "req_0": {
                "module": "vkey.GetVkeyServer",
                "method": "CgiGetVkey",
                "param": {
                    "guid": guid,
                    "songmid": filenames.iter().map(|_| mid).collect::<Vec<_>>(),
                    "songtype": filenames.iter().map(|_| 0).collect::<Vec<_>>(),
                    "uin": if !uin.is_empty() { uin.clone() } else { "0".to_string() },
                    "loginflag": 1,
                    "platform": "20",
                    "filename": filenames,
                }
            }
        });

        let mut req_builder = self
            .http
            .post(QM_API_URL)
            .header("Content-Type", "application/json")
            .header("Referer", "https://y.qq.com/")
            .header("Origin", "https://y.qq.com")
            .header("User-Agent", QM_WEB_UA);

        let cookie_str = self.cookie_header();
        if !cookie_str.is_empty() {
            req_builder = req_builder.header("Cookie", cookie_str);
        }

        let resp = req_builder
            .json(&req_body)
            .send()
            .await
            .map_err(|e| QqkgError::Upstream(format!("QM song_url HTTP error: {e}")))?;

        let json_resp: Value = resp
            .json()
            .await
            .map_err(|e| QqkgError::BadResponse(format!("QM song_url non-JSON: {e}")))?;

        let req_0_data = json_resp
            .get("req_0")
            .and_then(|r| r.get("data"))
            .cloned()
            .unwrap_or(Value::Null);

        let infos = req_0_data
            .get("midurlinfo")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let sips: Vec<String> = req_0_data
            .get("sip")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .filter(|s| s.starts_with("https://"))
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default();

        let sip = sips
            .first()
            .cloned()
            .unwrap_or_else(|| "https://ws.stream.qqmusic.qq.com/".to_string());

        let mut available_candidates: Vec<(&QualityCandidate, String)> = Vec::new();
        for cand in candidates {
            let match_filename = format!("{}{}{}", cand.prefix, media_mid, cand.ext);
            let found_purl = infos.iter().find_map(|item| {
                let fname = item.get("filename").and_then(Value::as_str).unwrap_or("");
                let purl = item.get("purl").and_then(Value::as_str).unwrap_or("");
                if fname == match_filename && !purl.is_empty() {
                    Some(purl.to_string())
                } else {
                    None
                }
            });

            if let Some(purl) = found_purl {
                available_candidates.push((cand, format!("{sip}{purl}")));
            }
        }

        // 探测可用直链
        let mut matched: Option<(&QualityCandidate, String)> = None;
        for (cand, url) in available_candidates.iter().take(4) {
            if self.probe_audio_url(url).await {
                matched = Some((cand, url.clone()));
                break;
            }
        }

        if matched.is_none() && !available_candidates.is_empty() {
            matched = Some((available_candidates[0].0, available_candidates[0].1.clone()));
        }

        if let Some((cand, url)) = matched {
            let is_fallback = cand.level != target_level.to_lowercase();
            let format_str = cand.ext.trim_start_matches('.');

            Ok(json!({
                "code": 200,
                "data": [
                    {
                        "id": mid,
                        "url": url,
                        "level": cand.level,
                        "format": format_str,
                        "isFallback": is_fallback
                    }
                ]
            }))
        } else {
            let msg = req_0_data
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("无法获取播放链接，可能需要 VIP 或无版权");

            Ok(json!({
                "code": 403,
                "message": msg,
                "data": [
                    {
                        "id": mid,
                        "url": ""
                    }
                ]
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_ordering_and_fallback() {
        let hi_res = get_candidates("hi-res");
        assert_eq!(hi_res.len(), 5);
        assert_eq!(hi_res[0].level, "hi-res");

        let hq = get_candidates("hq");
        assert_eq!(hq.len(), 3);
        assert_eq!(hq[0].level, "hq");
        assert_eq!(hq[1].level, "sq");
        assert_eq!(hq[2].level, "lq");
    }
}
