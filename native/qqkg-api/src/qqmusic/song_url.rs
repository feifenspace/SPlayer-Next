//! QQ 音乐播放链接获取模块：基于纯 Rust 实现，对齐 MemoryPlay 实战验证的 CgiGetVkey 降级流与最新 zzc 签名算法。

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};

use crate::error::QqkgError;
use crate::qqmusic::QqmusicClient;

pub const QM_WEB_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// QQ zzc 请求签名常数（与 MemoryPlay 保持 100% 一致）
const ZZC_PART1: [usize; 8] = [23, 14, 6, 36, 16, 40, 7, 19];
const ZZC_PART2: [usize; 8] = [16, 1, 32, 12, 19, 27, 8, 5];
const ZZC_SCRAMBLE: [u8; 20] = [
    89, 39, 179, 150, 218, 82, 58, 252, 177, 52, 186, 123, 120, 64, 242, 133, 143, 161, 121, 179,
];

/// 计算 QQ 音乐安全请求 sign 参数（SHA-1 + Scramble + Base64 算法）
pub fn zzc_sign(text: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(text.as_bytes());
    let sha1_bytes = hasher.finalize();
    let sha1_hex = format!("{:X}", sha1_bytes);
    let hex_chars: Vec<char> = sha1_hex.chars().collect();

    let part1: String = ZZC_PART1
        .iter()
        .filter_map(|&idx| hex_chars.get(idx))
        .collect();

    let part2: String = ZZC_PART2
        .iter()
        .filter_map(|&idx| hex_chars.get(idx))
        .collect();

    let mut part3 = Vec::with_capacity(20);
    for (i, &scramble_val) in ZZC_SCRAMBLE.iter().enumerate() {
        if i < sha1_bytes.len() {
            part3.push(scramble_val ^ sha1_bytes[i]);
        }
    }

    let b64 = base64::engine::general_purpose::STANDARD
        .encode(&part3)
        .replace('/', "")
        .replace('+', "")
        .replace('=', "");

    format!("zzc{}{}{}", part1, b64, part2).to_lowercase()
}

/// 兼容别名
pub fn qq_sign(text: &str) -> String {
    zzc_sign(text)
}

/// 单函数快捷获取歌曲播放链接
pub async fn get_song_url(
    cookies: &HashMap<String, String>,
    songmid: &str,
) -> Result<String, QqkgError> {
    let client = QqmusicClient::new(cookies.clone());
    let mut params = HashMap::new();
    params.insert("mid".to_string(), json!(songmid));
    let res = client.song_url(&params).await?;
    let url = res
        .get("data")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|item| item.get("url"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if url.is_empty() {
        return Err(QqkgError::BadResponse("无法获取播放直链，可能需要绿钻 VIP 或无版权".to_string()));
    }
    Ok(url.to_string())
}

impl QqmusicClient {
    /// 解析 QQ 音乐单曲播放直链（完全对齐 MemoryPlay 逐级降档与 CgiGetVkey 规范）
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

        let target_level = params
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("hq");

        let uin = self.uin();
        let try_types: &[i32] = match target_level.to_lowercase().as_str() {
            "hi-res" | "lossless" => &[2, 1, 0],
            "hq" => &[1, 0],
            _ => &[0],
        };

        let mut available_url: Option<(String, &'static str, &'static str)> = None;

        for &st in try_types {
            let req_body = json!({
                "req_0": {
                    "module": "vkey.GetVkeyServer",
                    "method": "CgiGetVkey",
                    "param": {
                        "guid": "834721266",
                        "songmid": [mid],
                        "songtype": [st],
                        "uin": if !uin.is_empty() { uin.clone() } else { "0".to_string() },
                        "loginflag": 1,
                        "platform": "20"
                    }
                },
                "comm": {
                    "uin": if !uin.is_empty() && uin != "0" {
                        uin.parse::<i64>().map(Value::from).unwrap_or(Value::from(uin.as_str()))
                    } else {
                        Value::from(0)
                    },
                    "format": "json",
                    "ct": 24,
                    "cv": 0
                }
            });

            let data_str = serde_json::to_string(&req_body)
                .map_err(|e| QqkgError::BadResponse(format!("序列化请求参数失败: {e}")))?;
            let sign = zzc_sign(&data_str);
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis().to_string())
                .unwrap_or_else(|_| "0".to_string());

            let signed_url = format!("https://u.y.qq.com/cgi-bin/musicu.fcg?_={timestamp}&sign={sign}");

            let resp = self
                .http
                .post(&signed_url)
                .header("Content-Type", "application/json")
                .header("Referer", "https://y.qq.com/")
                .header("Origin", "https://y.qq.com")
                .header("User-Agent", QM_WEB_UA)
                .header("Cookie", self.cookie_header())
                .body(data_str)
                .send()
                .await;

            if let Ok(r) = resp {
                if let Ok(json_resp) = r.json::<Value>().await {
                    let req_0_data = json_resp
                        .get("req_0")
                        .or_else(|| json_resp.get("data").and_then(|d| d.get("req_0")))
                        .and_then(|r| r.get("data"))
                        .cloned()
                        .unwrap_or(Value::Null);

                    let info = req_0_data
                        .get("midurlinfo")
                        .and_then(Value::as_array)
                        .and_then(|arr| arr.first());

                    let purl = info
                        .and_then(|i| i.get("purl"))
                        .and_then(Value::as_str)
                        .unwrap_or("");

                    if !purl.is_empty() {
                        let sip = req_0_data
                            .get("sip")
                            .and_then(Value::as_array)
                            .and_then(|arr| {
                                arr.iter().find_map(|s| s.as_str().filter(|u| u.starts_with("http://") || u.starts_with("https://")))
                            })
                            .unwrap_or("http://aqqmusic.tc.qq.com/");

                        let full_url = if purl.starts_with("http://") || purl.starts_with("https://") {
                            purl.to_string()
                        } else {
                            format!("{sip}{purl}")
                        };

                        let (level_name, ext_name) = match st {
                            2 => ("lossless", "flac"),
                            1 => ("hq", "mp3"),
                            _ => ("sq", "mp3"),
                        };

                        available_url = Some((full_url, level_name, ext_name));
                        break;
                    }
                }
            }
        }

        if let Some((url, level, format_str)) = available_url {
            let is_fallback = level != target_level.to_lowercase();
            Ok(json!({
                "code": 200,
                "data": [
                    {
                        "id": mid,
                        "url": url,
                        "level": level,
                        "format": format_str,
                        "isFallback": is_fallback
                    }
                ]
            }))
        } else {
            Ok(json!({
                "code": 403,
                "message": "无法获取播放链接，该曲目可能需要 QQ 音乐绿钻 VIP 或无版权",
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
