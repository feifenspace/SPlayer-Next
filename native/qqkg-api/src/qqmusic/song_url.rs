//! QQ 音乐播放链接获取模块：基于纯 Rust 实现，支持 Hi-Res 母带 (AI00/RS01)、SQ 无损 (F000)、HQ (M800) 及免费档逐级降级与最新 zzc 签名算法。

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

struct QualityCandidate {
    prefix: &'static str,
    ext: &'static str,
    level: &'static str,
}

fn get_candidates(level: &str) -> Vec<QualityCandidate> {
    match level.to_lowercase().as_str() {
        "hi-res" => vec![
            QualityCandidate { prefix: "AI00", ext: ".flac", level: "hi-res" },
            QualityCandidate { prefix: "RS01", ext: ".flac", level: "hi-res" },
            QualityCandidate { prefix: "Q000", ext: ".flac", level: "hi-res" },
            QualityCandidate { prefix: "FD00", ext: ".flac", level: "lossless" },
            QualityCandidate { prefix: "F000", ext: ".flac", level: "lossless" },
            QualityCandidate { prefix: "M800", ext: ".mp3", level: "hq" },
            QualityCandidate { prefix: "M500", ext: ".mp3", level: "sq" },
            QualityCandidate { prefix: "C400", ext: ".m4a", level: "lq" },
        ],
        "lossless" => vec![
            QualityCandidate { prefix: "FD00", ext: ".flac", level: "lossless" },
            QualityCandidate { prefix: "F000", ext: ".flac", level: "lossless" },
            QualityCandidate { prefix: "M800", ext: ".mp3", level: "hq" },
            QualityCandidate { prefix: "M500", ext: ".mp3", level: "sq" },
            QualityCandidate { prefix: "C400", ext: ".m4a", level: "lq" },
        ],
        "hq" => vec![
            QualityCandidate { prefix: "M800", ext: ".mp3", level: "hq" },
            QualityCandidate { prefix: "M500", ext: ".mp3", level: "sq" },
            QualityCandidate { prefix: "C400", ext: ".m4a", level: "lq" },
        ],
        "sq" => vec![
            QualityCandidate { prefix: "M500", ext: ".mp3", level: "sq" },
            QualityCandidate { prefix: "C400", ext: ".m4a", level: "lq" },
        ],
        _ => vec![
            QualityCandidate { prefix: "C400", ext: ".m4a", level: "lq" },
            QualityCandidate { prefix: "M500", ext: ".mp3", level: "sq" },
        ],
    }
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
    /// 查询歌曲详情以获取真实的 file.media_mid (strMediaMid)
    async fn resolve_real_media_mid(&self, songmid: &str) -> Option<String> {
        let comm = json!({
            "comm": { "uin": 0, "format": "json", "ct": 24, "cv": 4747474 },
            "req_0": {
                "module": "music.pf_song_detail_svr",
                "method": "get_song_detail_yqq",
                "param": { "song_mid": songmid }
            }
        });
        let resp = self
            .http
            .post("https://u.y.qq.com/cgi-bin/musicu.fcg")
            .header("Content-Type", "application/json")
            .header("User-Agent", QM_WEB_UA)
            .header("Referer", "https://y.qq.com/")
            .header("Cookie", self.cookie_header())
            .json(&comm)
            .send()
            .await
            .ok()?;
        let json_val: Value = resp.json().await.ok()?;
        let media_mid = json_val
            .get("req_0")
            .and_then(|r| r.get("data"))
            .and_then(|d| d.get("track_info"))
            .and_then(|t| t.get("file"))
            .and_then(|f| f.get("media_mid"))
            .and_then(Value::as_str)?
            .trim();
        if !media_mid.is_empty() {
            Some(media_mid.to_string())
        } else {
            None
        }
    }

    /// 解析 QQ 音乐单曲播放直链（支持 Hi-Res、SQ 无损、HQ 及免费档全级别探测与 VIP 鉴权）
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

        let mut media_mid = params
            .get("mediaMid")
            .or_else(|| params.get("media_mid"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();

        if media_mid.is_empty() {
            if let Some(real) = self.resolve_real_media_mid(mid).await {
                media_mid = real;
            } else {
                media_mid = mid.to_string();
            }
        }

        let target_level = params
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("hi-res");

        let candidates = get_candidates(target_level);
        let mut filenames: Vec<String> = candidates
            .iter()
            .map(|c| format!("{}{}{}", c.prefix, media_mid, c.ext))
            .collect();

        if media_mid != mid {
            for c in &candidates {
                let alt_fn = format!("{}{}{}", c.prefix, mid, c.ext);
                if !filenames.contains(&alt_fn) {
                    filenames.push(alt_fn);
                }
            }
        }

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

        let uin_val: Value = if !uin.is_empty() && uin != "0" {
            uin.parse::<i64>().map(Value::from).unwrap_or(Value::from(uin.as_str()))
        } else {
            Value::from(0)
        };

        let mut comm = json!({
            "uin": uin_val,
            "format": "json",
            "ct": 24,
            "cv": 4747474,
        });
        if !playback_key.is_empty() {
            comm["authst"] = json!(playback_key);
        }

        let req_body = json!({
            "comm": comm,
            "req_0": {
                "module": "vkey.GetVkeyServer",
                "method": "CgiGetVkey",
                "param": {
                    "guid": "834721266",
                    "songmid": filenames.iter().map(|_| mid).collect::<Vec<_>>(),
                    "songtype": filenames.iter().map(|_| 0).collect::<Vec<_>>(),
                    "uin": if !uin.is_empty() { uin.clone() } else { "0".to_string() },
                    "loginflag": 1,
                    "platform": "20",
                    "filename": filenames,
                }
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

        let json_resp: Value = match resp {
            Ok(r) => r.json().await.unwrap_or(Value::Null),
            Err(e) => return Err(QqkgError::Upstream(format!("QM song_url HTTP error: {e}"))),
        };

        let req_0_data = json_resp
            .get("req_0")
            .or_else(|| json_resp.get("data").and_then(|d| d.get("req_0")))
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
                    .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default();

        let sip = sips
            .first()
            .cloned()
            .unwrap_or_else(|| "http://aqqmusic.tc.qq.com/".to_string());

        let mut matched: Option<(&QualityCandidate, String)> = None;

        for cand in &candidates {
            let primary_fn = format!("{}{}{}", cand.prefix, media_mid, cand.ext);
            let alt_fn = format!("{}{}{}", cand.prefix, mid, cand.ext);

            let found_purl = infos.iter().find_map(|item| {
                let fname = item.get("filename").and_then(Value::as_str).unwrap_or("");
                let purl = item.get("purl").and_then(Value::as_str).unwrap_or("");
                if (fname == primary_fn || fname == alt_fn) && !purl.is_empty() {
                    Some(purl.to_string())
                } else {
                    None
                }
            });

            if let Some(purl) = found_purl {
                let full_url = if purl.starts_with("http://") || purl.starts_with("https://") {
                    purl
                } else {
                    format!("{sip}{purl}")
                };

                // 轻量级 HEAD 校验：腾讯部分母带(RS01)会下发虚拟 vkey 但 CDN 实际 404，通过 HEAD 验证确保 100% 可播
                let is_available = match self.http.head(&full_url).send().await {
                    Ok(resp) => resp.status().is_success() || resp.status().as_u16() == 206,
                    Err(_) => false,
                };

                if is_available {
                    matched = Some((cand, full_url));
                    break;
                }
            }
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
            Ok(json!({
                "code": 403,
                "message": "无法获取播放链接，该曲目可能需要 QQ 音乐绿钻 VIP/SVIP 或无版权",
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
