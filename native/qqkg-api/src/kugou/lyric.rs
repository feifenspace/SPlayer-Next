//! 酷狗音乐歌词搜索、下载与 KRC 解密模块（对齐桌面端 electron/main/apis/kugou/modules/lyric.ts）。

use std::collections::HashMap;
use std::io::Read;

use base64::prelude::*;
use flate2::read::ZlibDecoder;
use regex::Regex;
use serde_json::{json, Value};

use crate::error::QqkgError;
use crate::kugou::KugouClient;
use crate::normalize::decode_name;

const KRC_KEY: [u8; 16] = [
    0x40, 0x47, 0x61, 0x77, 0x5e, 0x32, 0x74, 0x47, 0x51, 0x36, 0x31, 0x2d, 0xce, 0xd2, 0x6e, 0x69,
];

fn decrypt_krc(base64_str: &str) -> Result<String, QqkgError> {
    let raw = BASE64_STANDARD
        .decode(base64_str.trim())
        .map_err(|e| QqkgError::BadResponse(format!("KRC base64 decode error: {e}")))?;

    if raw.len() <= 4 {
        return Err(QqkgError::BadResponse("KRC payload too short".into()));
    }

    let mut payload = raw[4..].to_vec();
    for (i, byte) in payload.iter_mut().enumerate() {
        *byte ^= KRC_KEY[i % 16];
    }

    let mut decoder = ZlibDecoder::new(&payload[..]);
    let mut out = String::new();
    decoder
        .read_to_string(&mut out)
        .map_err(|e| QqkgError::BadResponse(format!("KRC zlib inflate error: {e}")))?;

    Ok(out)
}

fn ms_to_time_tag(ms: u64) -> String {
    let m = ms / 60000;
    let s = (ms % 60000) / 1000;
    let x = ms % 1000;
    format!("{:02}:{:02}.{:03}", m, s, x)
}

pub struct ParsedKrc {
    pub lrc: String,
    pub krc: String,
    pub trans: Option<String>,
    pub roma: Option<String>,
}

fn parse_krc(raw: &str) -> ParsedKrc {
    let mut text = raw.replace('\r', "");

    // 移除头部 id
    let head_id_re = Regex::new(r"(?m)^.*\[id:\$\w+\]\n?").unwrap();
    text = head_id_re.replace_all(&text, "").to_string();

    let mut trans_lines: Option<Vec<String>> = None;
    let mut roma_lines: Option<Vec<String>> = None;

    let lang_re = Regex::new(r"\[language:([\w=\\/+]+)\]").unwrap();
    if let Some(caps) = lang_re.captures(&text) {
        if let Some(m) = caps.get(1) {
            let b64_lang = m.as_str().replace('\\', "");
            if let Ok(decoded_bytes) = BASE64_STANDARD.decode(b64_lang) {
                if let Ok(json_val) = serde_json::from_slice::<Value>(&decoded_bytes) {
                    if let Some(contents) = json_val.get("content").and_then(Value::as_array) {
                        for item in contents {
                            let item_type = item.get("type").and_then(Value::as_i64).unwrap_or(-1);
                            let lines: Vec<String> = item
                                .get("lyricContent")
                                .and_then(Value::as_array)
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|sub| {
                                            sub.as_array().map(|w| {
                                                w.iter()
                                                    .filter_map(Value::as_str)
                                                    .collect::<Vec<_>>()
                                                    .join("")
                                            })
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();

                            if item_type == 0 {
                                roma_lines = Some(lines);
                            } else if item_type == 1 {
                                trans_lines = Some(lines);
                            }
                        }
                    }
                }
            }
        }
        let lang_line_re = Regex::new(r"(?m)^\[language:[\w=\\/+]+\]\n?").unwrap();
        text = lang_line_re.replace_all(&text, "").to_string();
    }

    let line_time_re = Regex::new(r"\[(\d+),(\d+)\](.*)").unwrap();
    let mut idx = 0;
    let mut krc_lines = Vec::new();

    for line in text.lines() {
        if let Some(caps) = line_time_re.captures(line) {
            let start_ms: u64 = caps.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            let time_tag = ms_to_time_tag(start_ms);
            let rest = caps.get(3).map(|m| m.as_str()).unwrap_or("");

            if let Some(ref mut r_lines) = roma_lines {
                if idx < r_lines.len() {
                    r_lines[idx] = format!("[{}]{}", time_tag, r_lines[idx]);
                }
            }
            if let Some(ref mut t_lines) = trans_lines {
                if idx < t_lines.len() {
                    t_lines[idx] = format!("[{}]{}", time_tag, t_lines[idx]);
                }
            }
            idx += 1;
            krc_lines.push(format!("[{}]{}", time_tag, rest));
        } else {
            krc_lines.push(line.to_string());
        }
    }

    let mut krc_body = krc_lines.join("\n");
    let word_time_clean_re = Regex::new(r"<(\d+,\d+),\d+>").unwrap();
    krc_body = word_time_clean_re.replace_all(&krc_body, "<$1>").to_string();

    let krc = decode_name(&krc_body);
    let lrc_word_re = Regex::new(r"<\d+,\d+>").unwrap();
    let lrc = lrc_word_re.replace_all(&krc, "").to_string();

    let trans = trans_lines
        .map(|l| decode_name(&l.join("\n")))
        .filter(|s| !s.trim().is_empty());
    let roma = roma_lines
        .map(|l| decode_name(&l.join("\n")))
        .filter(|s| !s.trim().is_empty());

    ParsedKrc {
        lrc,
        krc,
        trans,
        roma,
    }
}

impl KugouClient {
    /// 搜索并下载酷狗歌词（支持逐字 KRC、标准 LRC、翻译与罗马音）。
    pub async fn lyric(&self, params: &HashMap<String, Value>) -> Result<Value, QqkgError> {
        let hash = params
            .get("hash")
            .or_else(|| params.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();

        if hash.is_empty() {
            return Err(QqkgError::InvalidParam("KG song hash missing".into()));
        }

        let name = params
            .get("name")
            .or_else(|| params.get("keyword"))
            .and_then(Value::as_str)
            .unwrap_or("");

        let duration = params
            .get("duration")
            .or_else(|| params.get("durationMs"))
            .and_then(|v| {
                v.as_u64().map(|n| {
                    if n > 10000 {
                        n / 1000
                    } else {
                        n
                    }
                })
            })
            .unwrap_or(0);

        // 第 1 步：检索候选
        let search_url = format!(
            "http://lyrics.kugou.com/search?ver=1&man=yes&client=pc&lrctxt=1&keyword={}&hash={}&timelength={}",
            urlencoding::encode(name),
            urlencoding::encode(hash),
            duration
        );

        let search_req = self
            .http
            .get(&search_url)
            .header("KG-RC", "1")
            .header("KG-THash", "expand_search_manager.cpp:852736169:451")
            .header("User-Agent", "KuGou2012-9020-ExpandSearchManager")
            .send()
            .await
            .map_err(|e| QqkgError::Upstream(format!("Kugou lyric search error: {e}")))?;

        let search_json: Value = search_req
            .json()
            .await
            .map_err(|e| QqkgError::BadResponse(format!("Kugou lyric search json error: {e}")))?;

        let candidates = search_json
            .get("candidates")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let first = candidates.first().ok_or_else(|| {
            QqkgError::BadResponse("No lyric candidate found for Kugou song".into())
        })?;

        let id = first.get("id").and_then(Value::as_str).unwrap_or("");
        let accesskey = first.get("accesskey").and_then(Value::as_str).unwrap_or("");
        let krctype = first.get("krctype").and_then(Value::as_i64).unwrap_or(0);
        let contenttype = first.get("contenttype").and_then(Value::as_i64).unwrap_or(0);

        let fmt = if krctype == 1 && contenttype != 1 {
            "krc"
        } else {
            "lrc"
        };

        // 第 2 步：下载歌词内容
        let download_url = format!(
            "http://lyrics.kugou.com/download?ver=1&client=pc&charset=utf8&id={}&accesskey={}&fmt={}",
            urlencoding::encode(id),
            urlencoding::encode(accesskey),
            fmt
        );

        let dl_req = self
            .http
            .get(&download_url)
            .header("KG-RC", "1")
            .header("KG-THash", "expand_search_manager.cpp:852736169:451")
            .header("User-Agent", "KuGou2012-9020-ExpandSearchManager")
            .send()
            .await
            .map_err(|e| QqkgError::Upstream(format!("Kugou lyric download error: {e}")))?;

        let dl_json: Value = dl_req
            .json()
            .await
            .map_err(|e| QqkgError::BadResponse(format!("Kugou lyric download json error: {e}")))?;

        let content = dl_json
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("");

        if content.is_empty() {
            return Err(QqkgError::BadResponse("Empty Kugou lyric content".into()));
        }

        let actual_fmt = dl_json.get("fmt").and_then(Value::as_str).unwrap_or(fmt);

        let mut out = serde_json::Map::new();
        out.insert("code".into(), json!(200));

        if actual_fmt == "krc" {
            let decrypted = decrypt_krc(content)?;
            let parsed = parse_krc(&decrypted);
            out.insert("krc".into(), json!(parsed.krc));
            out.insert("lrc".into(), json!(parsed.lrc));
            if let Some(t) = parsed.trans {
                out.insert("trans".into(), json!(t));
            }
            if let Some(r) = parsed.roma {
                out.insert("roma".into(), json!(r));
            }
        } else {
            let raw_lrc = BASE64_STANDARD
                .decode(content.trim())
                .map(|b| String::from_utf8_lossy(&b).to_string())
                .unwrap_or_else(|_| content.to_string());
            out.insert("lrc".into(), json!(raw_lrc));
        }

        Ok(Value::Object(out))
    }
}
