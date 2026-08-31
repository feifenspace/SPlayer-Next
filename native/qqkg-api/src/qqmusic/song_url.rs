//! QQ 音乐播放链接获取模块：支持 musics.fcg (含 sign 算法签名) 与 musicu.fcg 双通道。
//!
//! 纯 Rust 实现，零 Node 运行时依赖：
//! - 包含 QQ 音乐 Web 端动态 `sign` 签名算法 (`qq_sign`)
//! - 支持 `get_song_url` 快捷单曲直链解析
//! - 支持多音质候选序列探测（hi-res -> lossless -> hq -> sq -> lq）与 Range 探针自动降级

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{json, Value};

use crate::error::QqkgError;
use crate::qqmusic::QqmusicClient;

/// musics.fcg 接口 URL（带 sign 签名）
pub const MUSICS_FCG_URL: &str = "https://u6.y.qq.com/cgi-bin/musics.fcg";
/// musicu.fcg 接口 URL
pub const MUSICU_FCG_URL: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
/// 请求编码格式
pub const ENCODING: &str = "ag-1";
/// 时间戳参数名
pub const TIMESTAMP_PARAM: &str = "_";
/// sign 参数名
pub const SIGN_PARAM: &str = "sign";

pub const QM_WEB_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// 固定映射数组（基于 QQ 音乐 Web 端 getSecuritySign 抓包分析）
const HEAD_MAP: [usize; 16] = [8, 17, 18, 19, 4, 9, 2, 20, 14, 15, 5, 16, 6, 10, 3, 7];
const TAIL_MAP: [usize; 16] = [12, 1, 2, 15, 11, 7, 0, 13, 4, 10, 14, 8, 6, 5, 9, 3];
const MIDDLE_MAP: [u8; 16] = [
    0x21, 0x31, 0x19, 0x06, 0x25, 0x18, 0x12, 0x38,
    0x36, 0x2D, 0x0E, 0x0F, 0x03, 0x23, 0x2B, 0x26,
];

/// 计算 QQ 音乐安全请求 sign 参数。
///
/// 算法步骤：
/// 1. 计算 data_str 的 128 位 MD5 摘要与 32 位小写 Hex 字符串
/// 2. 取 MD5 原始 16 字节与 fixed middleMap 进行逐字节异或 (XOR)
/// 3. 标准 Base64 编码并剔除 `+`、`/`、`=` 字符
/// 4. 根据 head_map / tail_map 索引提取 hex_md5 字符
/// 5. 拼接 `"zzc" + head + s + tail` 并转小写
pub fn qq_sign(data_str: &str) -> String {
    // 1. 计算 128-bit MD5 摘要
    let digest = md5::compute(data_str.as_bytes());
    let hex_md5 = format!("{:x}", digest);
    let hex_chars: Vec<char> = hex_md5.chars().collect();

    // 2. 原始 16 字节与 middle_map 异或
    let raw_bytes: [u8; 16] = digest.0;
    let middle: Vec<u8> = raw_bytes
        .iter()
        .zip(MIDDLE_MAP.iter())
        .map(|(x, y)| x ^ y)
        .collect();

    // 3. Base64 编码并清洗特殊字符
    let s = base64::engine::general_purpose::STANDARD
        .encode(&middle)
        .replace('+', "")
        .replace('/', "")
        .replace('=', "");

    // 4. 按映射表提取 head 与 tail（带越界保护）
    let head: String = HEAD_MAP
        .iter()
        .filter_map(|&idx| hex_chars.get(idx))
        .collect();

    let tail: String = TAIL_MAP
        .iter()
        .filter_map(|&idx| hex_chars.get(idx))
        .collect();

    format!("zzc{}{}{}", head, s, tail).to_lowercase()
}

/// 构造 musics.fcg 请求的完整请求体
pub fn construct_comm(uin: &str, media_mid: &str) -> Value {
    json!({
        "req_0": {
            "module": "vkey.GetVkeyServer",
            "method": "CgiGetVkey",
            "param": {
                "guid": "5316616415",
                "songmid": [media_mid],
                "songtype": [0],
                "uin": uin,
                "loginflag": 1,
                "platform": "20",
                "filename": [format!("M800{}.mp3", media_mid)],
            }
        },
        "comm": {
            "uin": uin,
            "format": "json",
            "ct": 24,
            "cv": 4747474
        }
    })
}

/// 从 cookies 中提取 uin（纯数字，去 o 前缀）
pub fn extract_uin(cookies: &HashMap<String, String>) -> String {
    cookies
        .get("uin")
        .or_else(|| cookies.get("wxuin"))
        .or_else(|| cookies.get("p_uin"))
        .map(String::as_str)
        .unwrap_or("0")
        .strip_prefix('o')
        .unwrap_or("0")
        .to_string()
}

/// 构建 Cookie 标头
pub fn build_cookie_header(cookies: &HashMap<String, String>) -> String {
    let entries: Vec<String> = cookies
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{}={}", k, v))
        .collect();
    if entries.is_empty() {
        "tmeLoginType=-1".to_string()
    } else {
        entries.join("; ")
    }
}

/// 单函数快捷获取歌曲播放链接（使用 musics.fcg 带 sign 签名）
pub async fn get_song_url(
    cookies: &HashMap<String, String>,
    media_mid: &str,
) -> Result<String, QqkgError> {
    let uin = extract_uin(cookies);
    let comm = construct_comm(&uin, media_mid);
    let data_str = serde_json::to_string(&comm)
        .map_err(|e| QqkgError::BadResponse(format!("序列化请求参数失败: {e}")))?;

    let sign = qq_sign(&data_str);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_string());

    let url = format!(
        "{}?{}={}&encoding={}&sign={}",
        MUSICS_FCG_URL, TIMESTAMP_PARAM, timestamp, ENCODING, sign
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap_or_default();

    let resp = client
        .post(&url)
        .header("User-Agent", QM_WEB_UA)
        .header("Referer", "https://y.qq.com")
        .header("Cookie", build_cookie_header(cookies))
        .header("Content-Type", "application/json")
        .body(data_str)
        .send()
        .await
        .map_err(|e| QqkgError::Upstream(format!("QQ音乐请求失败: {e}")))?;

    let data: Value = resp
        .json()
        .await
        .map_err(|e| QqkgError::BadResponse(format!("QQ音乐响应解析失败: {e}")))?;

    if data.get("code").and_then(Value::as_i64).unwrap_or(0) != 0 {
        let msg = data
            .get("message")
            .or_else(|| data.get("msg"))
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        return Err(QqkgError::Upstream(format!("QQ音乐API错误: {msg}")));
    }

    let purl = data
        .get("req_0")
        .or_else(|| data.get("data").and_then(|d| d.get("req_0")))
        .and_then(|r| r.get("data"))
        .and_then(|d| d.get("midurlinfo"))
        .and_then(|m| m.get(0))
        .and_then(|m| m.get("purl"))
        .and_then(Value::as_str)
        .unwrap_or("");

    if purl.is_empty() {
        return Err(QqkgError::BadResponse("无法获取 purl 直链".to_string()));
    }

    let sip = data
        .get("req_0")
        .or_else(|| data.get("data").and_then(|d| d.get("req_0")))
        .and_then(|r| r.get("data"))
        .and_then(|d| d.get("sip"))
        .and_then(Value::as_array)
        .and_then(|arr| arr.iter().find_map(|s| s.as_str().filter(|u| u.starts_with("https://"))))
        .unwrap_or("https://ws.stream.qqmusic.qq.com/");

    Ok(format!("{sip}{purl}"))
}

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

    /// 解析 QQ 音乐单曲播放直链（支持完整多音质回退探测与签名请求）
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

        let now_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
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
            "cv": 4747474,
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

        let data_str = serde_json::to_string(&req_body)
            .map_err(|e| QqkgError::BadResponse(format!("序列化请求参数失败: {e}")))?;
        let sign = qq_sign(&data_str);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis().to_string())
            .unwrap_or_else(|_| "0".to_string());

        let signed_url = format!(
            "{}?{}={}&encoding={}&sign={}",
            MUSICS_FCG_URL, TIMESTAMP_PARAM, timestamp, ENCODING, sign
        );

        let mut req_builder = self
            .http
            .post(&signed_url)
            .header("Content-Type", "application/json")
            .header("Referer", "https://y.qq.com/")
            .header("Origin", "https://y.qq.com")
            .header("User-Agent", QM_WEB_UA);

        let cookie_str = self.cookie_header();
        if !cookie_str.is_empty() {
            req_builder = req_builder.header("Cookie", cookie_str);
        }

        let resp = req_builder
            .body(data_str.clone())
            .send()
            .await;

        // 如果带 sign 的 musics.fcg 请求失败，降级尝试传统 musicu.fcg 请求
        let json_resp: Value = match resp {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or(Value::Null),
            _ => {
                let fallback_resp = self
                    .http
                    .post(MUSICU_FCG_URL)
                    .header("Content-Type", "application/json")
                    .header("Referer", "https://y.qq.com/")
                    .header("Origin", "https://y.qq.com")
                    .header("User-Agent", QM_WEB_UA)
                    .header("Cookie", self.cookie_header())
                    .body(data_str)
                    .send()
                    .await
                    .map_err(|e| QqkgError::Upstream(format!("QM song_url HTTP error: {e}")))?;
                fallback_resp
                    .json()
                    .await
                    .map_err(|e| QqkgError::BadResponse(format!("QM song_url non-JSON: {e}")))?
            }
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
    fn test_qq_sign() {
        let data_str = "module=vkey&method=GetVkeyServer&param={\"data\":{\"req_0\":{\"module\":\"vkey\",\"method\":\"GetVkeyServer\",\"param\":{\"guid\":\"5316616415\",\"songmid\":[\"003Isrv23YcRUM\"],\"songtype\":[0],\"uin\":\"50233185\",\"loginflag\":1,\"format\":\"json\"},\"comm\":{\"uin\":\"50233185\",\"format\":\"json\",\"ct\":24,\"cv\":4747474}},\"uin\":\"50233185\",\"format\":\"json\",\"ct\":24,\"cv\":4747474,\"platform\":\"yqq.json\",\"needNewCode\":1,\"data\":{\"req_0\":{\"module\":\"vkey\",\"method\":\"GetVkeyServer\",\"param\":{\"guid\":\"5316616415\",\"songmid\":[\"003Isrv23YcRUM\"],\"songtype\":[0],\"uin\":\"50233185\",\"loginflag\":1,\"format\":\"json\"},\"comm\":{\"uin\":\"50233185\",\"format\":\"json\",\"ct\":24,\"cv\":4747474}}}}";
        let sign = qq_sign(data_str);
        assert!(sign.starts_with("zzc"));
        assert!(sign.len() > 20);
    }

    #[test]
    fn test_extract_uin() {
        let mut cookies = HashMap::new();
        cookies.insert("uin".to_string(), "o123456".to_string());
        assert_eq!(extract_uin(&cookies), "123456");

        let mut cookies = HashMap::new();
        cookies.insert("wxuin".to_string(), "42".to_string());
        assert_eq!(extract_uin(&cookies), "42");

        assert_eq!(extract_uin(&HashMap::new()), "0");
    }

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
