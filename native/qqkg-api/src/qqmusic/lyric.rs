//! QQ音乐歌词获取与解密模块（对齐桌面端 electron/main/apis/qqmusic/modules/lyric.ts）。
//!
//! 流程：
//! 1. 请求 music.musichallSong.PlayLyricInfo.GetPlayLyricInfo (crypt: 1)
//! 2. 返回十六进制密文，使用纯 Rust TripleDES EDE + zlib 解密（crypto::qq_des::qrc_decrypt）
//! 3. 提取 QRC 逐字 / 标准 LRC / 翻译 / 罗马音

use std::collections::HashMap;
use std::io::Read;

use base64::prelude::*;
use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
use serde_json::{json, Value};

use crate::crypto::qq_des::qrc_decrypt;
use crate::error::QqkgError;
use crate::qqmusic::QqmusicClient;

const QRC_KEY: &[u8] = b"!@#)(*$%123ZXC!@!@#)(NHL";

fn b64(text: &str) -> String {
    if text.is_empty() {
        String::new()
    } else {
        BASE64_STANDARD.encode(text.as_bytes())
    }
}

fn hex_to_bytes(hex: &str) -> Option<Vec<u8>> {
    let s = hex.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        let byte = u8::from_str_radix(&s[i..i + 2], 16).ok()?;
        bytes.push(byte);
    }
    Some(bytes)
}

fn decrypt_qrc(hex: &str) -> Option<String> {
    let encrypted_bytes = hex_to_bytes(hex)?;
    let decrypted = qrc_decrypt(&encrypted_bytes, QRC_KEY);

    // 尝试 Zlib 解压
    let mut zlib_dec = ZlibDecoder::new(&decrypted[..]);
    let mut out = String::new();
    if zlib_dec.read_to_string(&mut out).is_ok() && !out.is_empty() {
        return Some(out);
    }

    // 尝试 Raw Deflate 解压
    let mut def_dec = DeflateDecoder::new(&decrypted[..]);
    let mut out2 = String::new();
    if def_dec.read_to_string(&mut out2).is_ok() && !out2.is_empty() {
        return Some(out2);
    }

    // 尝试 Gzip 解压
    let mut gz_dec = GzDecoder::new(&decrypted[..]);
    let mut out3 = String::new();
    if gz_dec.read_to_string(&mut out3).is_ok() && !out3.is_empty() {
        return Some(out3);
    }

    // 若明文本身未压缩且含歌词特征标签
    if let Ok(plain) = String::from_utf8(decrypted) {
        if plain.contains('[') || plain.contains('<') {
            return Some(plain);
        }
    }

    None
}

fn try_decrypt(hex_opt: Option<&Value>) -> Option<String> {
    let hex_str = hex_opt.and_then(Value::as_str)?.trim();
    if hex_str.is_empty() {
        return None;
    }
    decrypt_qrc(hex_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

impl QqmusicClient {
    /// 获取 QQ 音乐歌词（支持 QRC 逐字歌词、LRC、翻译与罗马音）。
    pub async fn lyric(&self, params: &HashMap<String, Value>) -> Result<Value, QqkgError> {
        let song_id = params
            .get("id")
            .or_else(|| params.get("songID"))
            .or_else(|| params.get("song_id"))
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
            })
            .unwrap_or(0);

        let song_mid = params
            .get("songmid")
            .or_else(|| params.get("mid"))
            .and_then(Value::as_str)
            .unwrap_or("");

        // 若传入的是 mid 而非数字 id，支持从字符串数字解析
        let numeric_id = if song_id > 0 {
            song_id
        } else if !song_mid.is_empty() {
            song_mid.parse::<u64>().unwrap_or(0)
        } else {
            0
        };

        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let artist = params.get("artist").and_then(Value::as_str).unwrap_or("");
        let album = params.get("album").and_then(Value::as_str).unwrap_or("");
        let duration = params.get("duration").and_then(Value::as_u64).unwrap_or(0);

        let base_param = json!({
            "albumName": b64(album),
            "crypt": 1,
            "ct": 19,
            "cv": 2111,
            "interval": duration,
            "lrc_t": 0,
            "qrc": 1,
            "qrc_t": 0,
            "roma": 1,
            "roma_t": 0,
            "singerName": b64(artist),
            "songID": numeric_id,
            "songName": b64(name),
            "trans": 1,
            "trans_t": 0,
            "type": 0,
        });

        let resp = self
            .post_fcg("music.musichallSong.PlayLyricInfo", "GetPlayLyricInfo", base_param.clone())
            .await?;

        let mut out = serde_json::Map::new();
        out.insert("code".into(), json!(200));

        let main_decrypted = try_decrypt(resp.get("lyric"));
        let qrc_t = resp.get("qrc_t").and_then(Value::as_i64).unwrap_or(0);

        if let Some(ref text) = main_decrypted {
            if qrc_t == 0 {
                out.insert("lrc".into(), json!(text));
            } else {
                out.insert("qrc".into(), json!(text));
            }
        }

        // 若只拿到 QRC，再请求一次纯 LRC
        if out.contains_key("qrc") && !out.contains_key("lrc") {
            let mut lrc_param = base_param.clone();
            if let Some(obj) = lrc_param.as_object_mut() {
                obj.insert("qrc".into(), json!(0));
                obj.insert("qrc_t".into(), json!(0));
            }
            if let Ok(lrc_resp) = self
                .post_fcg("music.musichallSong.PlayLyricInfo", "GetPlayLyricInfo", lrc_param)
                .await
            {
                if let Some(lrc_text) = try_decrypt(lrc_resp.get("lyric")) {
                    out.insert("lrc".into(), json!(lrc_text));
                }
            }
        }

        if let Some(trans) = try_decrypt(resp.get("trans")) {
            out.insert("trans".into(), json!(trans));
        }

        if let Some(roma) = try_decrypt(resp.get("roma")) {
            out.insert("roma".into(), json!(roma));
        }

        Ok(Value::Object(out))
    }
}
