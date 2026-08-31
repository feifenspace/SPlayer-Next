pub mod album;
pub mod artist;
pub mod auth;
pub mod config;
pub mod credentials;
pub mod playlist;
pub mod search;
pub mod song_url;
pub mod user;

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::error::StreamingError;
use config::{QOBUZ_API_BASE, QOBUZ_FIREFOX_UA};
use credentials::{get_credentials, get_working_secret};

pub struct QobuzClient {
    http: reqwest::Client,
    pub user_auth_token: Option<String>,
    pub user_id: Option<String>,
}

impl QobuzClient {
    pub fn new(user_auth_token: Option<String>, user_id: Option<String>) -> Self {
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent(QOBUZ_FIREFOX_UA);

        // 仅当显式设置 QOBUZ_PROXY 环境变量时才启用代理；默认直连
        if let Ok(proxy_url) = std::env::var("QOBUZ_PROXY") {
            if !proxy_url.is_empty() {
                if let Ok(p) = reqwest::Proxy::all(&proxy_url) {
                    builder = builder.proxy(p);
                }
            }
        }
        if let Ok(ip) = std::env::var("QOBUZ_RESOLVE_IP") {
            if let Ok(addr) = format!("{}:443", ip.trim()).parse() {
                builder = builder.resolve("www.qobuz.com", addr);
            }
        }

        let http = builder.build().unwrap_or_default();
        Self { http, user_auth_token, user_id }
    }

    /// 生成 request_sig（公式不变，变的是喂进来的参数）。
    ///
    /// 实测验证（8.2.0-b034 bundle，HTTP 200）：
    ///   md5( object + method + 按key排序的(key+value) + 时间戳 + initialization值 )
    /// 例：md5("track" + "getFileUrl" + "format_id5intentstreamtrack_id108819174"
    ///         + "1788105139" + "abb21364945c0583309667d13ca3d93a")
    ///
    /// 关键约束：
    ///   param_str 只含业务参数 —— app_id/user_auth_token 走 HTTP 头，
    ///   request_ts/request_sig 是签名产物，都不能参与拼接。
    ///   secret 形参传 initialization 值（不是 bundle 里的 appSecret，那是反调试诱饵）。
    pub fn build_signature(
        method: &str,
        action: &str,
        params: &HashMap<String, String>,
        ts: u64,
        secret: &str,
    ) -> String {
        let mut keys: Vec<&String> = params.keys().collect();
        keys.sort();
        let mut param_str = String::new();
        for k in keys {
            if let Some(v) = params.get(k) {
                param_str.push_str(k);
                param_str.push_str(v);
            }
        }
        let raw = format!("{method}{action}{param_str}{ts}{secret}");
        format!("{:x}", md5::compute(raw.as_bytes()))
    }

    pub async fn request(
        &self,
        endpoint: &str,
        params: HashMap<String, String>,
        signed: bool,
        auth_required: bool,
        custom_secret: Option<String>,
    ) -> Result<Value, StreamingError> {
        let creds = get_credentials(&self.http, false).await?;

        if auth_required && self.user_auth_token.is_none() {
            return Err(StreamingError::Auth("Qobuz is not logged in".into()));
        }

        // 修复①：query 只放业务参数，app_id/token 一律走 HTTP 头。
        // 旧代码先 insert 这两个再签名 → 混入 param_str → 400 Invalid Request Signature。
        let mut query = params.clone();

        if signed {
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let secret_str = match custom_secret {
                Some(s) => s,
                None => match get_working_secret().await {
                    Some(ws) => ws,
                    None => creds.secret.clone(),
                },
            };

            let parts: Vec<&str> = endpoint.split('/').collect();
            let method = parts.first().copied().unwrap_or("");
            let action = parts.get(1).copied().unwrap_or("");

            let sig = Self::build_signature(method, action, &query, ts, &secret_str);
            query.insert("request_ts".to_string(), ts.to_string());
            query.insert("request_sig".to_string(), sig);
        } else {
            // 未签名请求在 query 中补充 app_id 与 token（保证开放端点正常访问）
            query.insert("app_id".to_string(), creds.app_id.clone());
            if let Some(token) = &self.user_auth_token {
                query.insert("user_auth_token".to_string(), token.clone());
            }
        }

        let url = format!("{}/{}", QOBUZ_API_BASE, endpoint);

        // 网页播放器同款请求头。app_id 必须与 token 所属应用一致（798273057）。
        let mut req = self
            .http
            .get(&url)
            .header("X-App-Id", &creds.app_id)
            .header("User-Agent", QOBUZ_FIREFOX_UA);
        if let Some(token) = &self.user_auth_token {
            req = req.header("X-User-Auth-Token", token);
        }
        let resp = req.query(&query).send().await?;

        let status = resp.status();
        let body: Value = resp.json().await
            .map_err(|e| StreamingError::Parse(e.to_string()))?;

        if !status.is_success() {
            let msg = body.get("message").and_then(Value::as_str)
                .unwrap_or("Unknown Qobuz API error");
            return Err(StreamingError::Api {
                status: status.as_u16(),
                message: msg.to_string(),
            });
        }

        Ok(body)
    }
}
