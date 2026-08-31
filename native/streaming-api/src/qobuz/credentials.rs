use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::error::StreamingError;
use super::config::{
    CREDS_CACHE_TTL_SECS, CREDS_DISK_TTL_SECS, QOBUZ_FIREFOX_UA, QOBUZ_WEB_BASE,
};

/// 已验证的生产环境凭证对（2026-08 实测通过）
pub const PROD_APP_ID: &str = "798273057";
pub const PROD_INIT: &str = "abb21364945c0583309667d13ca3d93a";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QobuzCredentials {
    pub app_id: String,
    pub secret: String,
    pub secrets: Vec<String>,
    pub saved_at: u64,
}

static WORKING_SECRET: RwLock<Option<String>> = RwLock::const_new(None);
static MEM_CREDS: RwLock<Option<QobuzCredentials>> = RwLock::const_new(None);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn disk_cache_path() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home).join(".config/splayer-headless/qobuz_creds.json");
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        return p;
    }
    PathBuf::from("/tmp/splayer_qobuz_creds.json")
}

fn load_from_disk() -> Option<QobuzCredentials> {
    let p = disk_cache_path();
    if !p.exists() {
        return None;
    }
    let content = fs::read_to_string(p).ok()?;
    let creds: QobuzCredentials = serde_json::from_str(&content).ok()?;
    if creds.app_id.is_empty() {
        return None;
    }
    if now_secs().saturating_sub(creds.saved_at) >= CREDS_DISK_TTL_SECS {
        return None;
    }
    Some(creds)
}

fn save_to_disk(creds: &QobuzCredentials) {
    let p = disk_cache_path();
    if let Ok(json) = serde_json::to_string(creds) {
        let _ = fs::write(p, json);
    }
}

fn load_from_qobuz_dl() -> Option<QobuzCredentials> {
    let home = std::env::var("HOME").ok()?;
    let p = PathBuf::from(home).join(".config/qobuz-dl/config.ini");
    if !p.exists() {
        return None;
    }
    let content = fs::read_to_string(p).ok()?;
    let app_id_re = Regex::new(r"(?m)^\s*app_id\s*=\s*(\d+)\s*$").ok()?;
    let secrets_re = Regex::new(r"(?m)^\s*secrets\s*=\s*(\S+)\s*$").ok()?;

    let app_id = app_id_re.captures(&content)?.get(1)?.as_str().to_string();
    let secrets_str = secrets_re.captures(&content)?.get(1)?.as_str();
    let secrets: Vec<String> = secrets_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if secrets.is_empty() {
        return None;
    }
    let secret = secrets[0].clone();
    Some(QobuzCredentials {
        app_id,
        secret,
        secrets,
        saved_at: now_secs(),
    })
}

async fn fetch_dynamic_credentials(client: &reqwest::Client) -> Result<QobuzCredentials, StreamingError> {
    let login_url = format!("{}/login", QOBUZ_WEB_BASE);
    let html = client
        .get(&login_url)
        .header("User-Agent", QOBUZ_FIREFOX_UA)
        .send()
        .await?
        .text()
        .await?;

    let bundle_re = Regex::new(r#"src="(/resources/[^"]*bundle[^"]*\.js)""#)
        .map_err(|e| StreamingError::Parse(e.to_string()))?;
    let bundle_path = bundle_re
        .captures(&html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .ok_or_else(|| StreamingError::Parse("bundle.js not found in Qobuz login page".into()))?;

    let bundle_url = format!("{}{}", QOBUZ_WEB_BASE, bundle_path);
    let js = client
        .get(&bundle_url)
        .header("User-Agent", QOBUZ_FIREFOX_UA)
        .send()
        .await?
        .text()
        .await?;

    // app_id：收集所有 appId:"(\d{9})" 匹配，优先生产值 PROD_APP_ID (798273057)
    let app_id_re = Regex::new(r#"appId:"(\d{9})""#).map_err(|e| StreamingError::Parse(e.to_string()))?;
    let matched_app_ids: Vec<String> = app_id_re
        .captures_iter(&js)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect();

    let app_id = if matched_app_ids.contains(&PROD_APP_ID.to_string()) {
        PROD_APP_ID.to_string()
    } else {
        matched_app_ids
            .first()
            .cloned()
            .unwrap_or_else(|| PROD_APP_ID.to_string())
    };

    // 提取 seed/timezone
    let seed_re = Regex::new(r#"[a-z]\.initialSeed\("([\w=]+)",window\.utimezone\.([a-z]+)\)"#)
        .map_err(|e| StreamingError::Parse(e.to_string()))?;
    let mut seeds: Vec<(String, String)> = Vec::new();
    for cap in seed_re.captures_iter(&js) {
        if let (Some(seed), Some(tz)) = (cap.get(1), cap.get(2)) {
            let tz_s = tz.as_str().to_string();
            let seed_s = seed.as_str().to_string();
            if !seeds.iter().any(|(t, _)| t == &tz_s) {
                seeds.push((tz_s, seed_s));
            }
        }
    }

    let mut tz_caps = Vec::new();
    for (tz, _) in &seeds {
        if let Some(first) = tz.chars().next() {
            let cap = format!("{}{}", first.to_uppercase(), &tz[1..]);
            tz_caps.push(cap);
        }
    }

    let mut combined: HashMap<String, String> = HashMap::new();
    if !tz_caps.is_empty() {
        let info_pattern = format!(r#"name:"\w+/({})",info:"([\w=]+)",extras:"([\w=]+)""#, tz_caps.join("|"));
        if let Ok(info_re) = Regex::new(&info_pattern) {
            for cap in info_re.captures_iter(&js) {
                if let (Some(tz_c), Some(info), Some(extras)) = (cap.get(1), cap.get(2), cap.get(3)) {
                    let tz_lower = tz_c.as_str().to_lowercase();
                    if let Some((_, seed_val)) = seeds.iter().find(|(t, _)| t == &tz_lower) {
                        if !combined.contains_key(&tz_lower) {
                            combined.insert(tz_lower, format!("{}{}{}", seed_val, info.as_str(), extras.as_str()));
                        }
                    }
                }
            }
        }
    }

    let mut secrets: Vec<String> = Vec::new();
    for (tz, _) in &seeds {
        if let Some(comb) = combined.get(tz) {
            if comb.len() > 44 {
                let slice = &comb[0..comb.len() - 44];
                if let Ok(decoded_bytes) = base64::engine::general_purpose::STANDARD.decode(slice) {
                    if let Ok(sec_str) = String::from_utf8(decoded_bytes) {
                        if !sec_str.is_empty() && !secrets.contains(&sec_str) {
                            secrets.push(sec_str);
                        }
                    }
                }
            }
        }
    }

    // 生产 init 优先置顶
    if let Some(pos) = secrets.iter().position(|s| s == PROD_INIT) {
        let prod = secrets.remove(pos);
        secrets.insert(0, prod);
    } else {
        secrets.insert(0, PROD_INIT.to_string());
    }

    let secret = secrets[0].clone();
    Ok(QobuzCredentials {
        app_id,
        secret,
        secrets,
        saved_at: now_secs(),
    })
}

pub async fn get_credentials(client: &reqwest::Client, force_refresh: bool) -> Result<QobuzCredentials, StreamingError> {
    if force_refresh {
        let mut w = MEM_CREDS.write().await;
        *w = None;
        let mut ws = WORKING_SECRET.write().await;
        *ws = None;
        let p = disk_cache_path();
        let _ = fs::remove_file(p);
    }

    // 1. 环境变量
    if let (Ok(app_id), Ok(secret)) = (std::env::var("QOBUZ_APP_ID"), std::env::var("QOBUZ_APP_SECRET")) {
        if !app_id.is_empty() && !secret.is_empty() {
            return Ok(QobuzCredentials {
                app_id,
                secret: secret.clone(),
                secrets: vec![secret],
                saved_at: now_secs(),
            });
        }
    }

    // 内存缓存
    {
        let r = MEM_CREDS.read().await;
        if let Some(c) = r.as_ref() {
            if !c.app_id.is_empty() && now_secs().saturating_sub(c.saved_at) < CREDS_CACHE_TTL_SECS {
                return Ok(c.clone());
            }
        }
    }

    // 2. qobuz-dl 配置
    if let Some(c) = load_from_qobuz_dl() {
        let mut w = MEM_CREDS.write().await;
        *w = Some(c.clone());
        save_to_disk(&c);
        return Ok(c);
    }

    // 3. 磁盘缓存
    if let Some(c) = load_from_disk() {
        let mut w = MEM_CREDS.write().await;
        *w = Some(c.clone());
        return Ok(c);
    }

    // 4. 动态抓取
    match fetch_dynamic_credentials(client).await {
        Ok(c) => {
            {
                let mut w = MEM_CREDS.write().await;
                *w = Some(c.clone());
            }
            save_to_disk(&c);
            Ok(c)
        }
        Err(_) => {
            // 5. 静态内置保底凭证（实测验证生产值）
            let fallback = QobuzCredentials {
                app_id: PROD_APP_ID.to_string(),
                secret: PROD_INIT.to_string(),
                secrets: vec![PROD_INIT.to_string()],
                saved_at: now_secs(),
            };
            let mut w = MEM_CREDS.write().await;
            *w = Some(fallback.clone());
            Ok(fallback)
        }
    }
}

pub async fn get_working_secret() -> Option<String> {
    let r = WORKING_SECRET.read().await;
    r.clone()
}

pub async fn set_working_secret(sec: String) {
    let mut w = WORKING_SECRET.write().await;
    *w = Some(sec);
}
