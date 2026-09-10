pub mod album;
pub mod artist;
pub mod auth;
pub mod config;
pub mod pkce;
pub mod playlist;
pub mod search;
pub mod song_url;
pub mod user;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde_json::Value;

use crate::error::StreamingError;
use config::{TIDAL_API_BASE, TIDAL_DEFAULT_COUNTRY, TIDAL_USER_AGENT};

pub struct TidalClient {
    http: reqwest::Client,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub user_id: Option<String>,
    pub country_code: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// 401 自动刷新产生的新 token（access, refresh）：本实例一次性缓存，
    /// 由调用方经 take_refreshed_tokens 取走并持久化到 DB——Tidal 刷新令牌
    /// 会轮转，不落盘则下次调用仍用过期 token
    fresh_tokens: Mutex<Option<(String, Option<String>)>>,
    /// 本实例是否已尝试过自动刷新：防循环（token 真失效时只刷一次）
    refresh_attempted: AtomicBool,
}

impl TidalClient {
    pub fn new(
        access_token: Option<String>,
        refresh_token: Option<String>,
        user_id: Option<String>,
        country_code: Option<String>,
        client_id: Option<String>,
        client_secret: Option<String>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self {
            http,
            access_token,
            refresh_token,
            user_id,
            country_code,
            client_id,
            client_secret,
            fresh_tokens: Mutex::new(None),
            refresh_attempted: AtomicBool::new(false),
        }
    }

    pub async fn request(
        &self,
        endpoint: &str,
        params: HashMap<String, String>,
        auth_required: bool,
    ) -> Result<Value, StreamingError> {
        if auth_required && self.access_token.is_none() {
            return Err(StreamingError::Auth("TIDAL is not logged in".into()));
        }

        let mut query = params.clone();
        let country = self
            .country_code
            .as_deref()
            .unwrap_or(TIDAL_DEFAULT_COUNTRY);
        query.entry("countryCode".to_string()).or_insert_with(|| country.to_string());

        let url = format!("{}/{}", TIDAL_API_BASE, endpoint);
        let effective_token = self.effective_token();

        let (status, body) = self.send_get(&url, &query, effective_token.as_deref()).await?;
        let sub_status = body.get("subStatus").and_then(Value::as_i64);

        // token 过期（subStatus=11003）：用存储的 refresh_token 自动续期一次
        // 并重试原请求。Tidal 刷新令牌轮转，仅尝试一次防循环烧令牌
        if status.as_u16() == 401
            && sub_status == Some(11003)
            && self.refresh_token.is_some()
            && !self.refresh_attempted.swap(true, Ordering::SeqCst)
        {
            if let Ok(tokens) = self.auth_token_refresh(&HashMap::new()).await {
                let new_access = tokens
                    .get("accessToken")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let new_refresh = tokens
                    .get("refreshToken")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                if !new_access.is_empty() {
                    *self
                        .fresh_tokens
                        .lock()
                        .unwrap_or_else(|p| p.into_inner()) = Some((new_access.clone(), new_refresh));
                    let (retry_status, retry_body) =
                        self.send_get(&url, &query, Some(&new_access)).await?;
                    if retry_status.is_success() {
                        return Ok(retry_body);
                    }
                    return Err(Self::api_error(retry_status.as_u16(), &retry_body));
                }
            }
        }

        if !status.is_success() {
            return Err(Self::api_error(status.as_u16(), &body));
        }

        Ok(body)
    }

    /// 当前生效 token：自动刷新的新 token 优先，否则原始 access_token
    fn effective_token(&self) -> Option<String> {
        let fresh = self.fresh_tokens.lock().unwrap_or_else(|p| p.into_inner());
        match fresh.as_ref() {
            Some((access, _)) => Some(access.clone()),
            None => self.access_token.clone(),
        }
    }

    async fn send_get(
        &self,
        url: &str,
        query: &HashMap<String, String>,
        token: Option<&str>,
    ) -> Result<(reqwest::StatusCode, Value), StreamingError> {
        let mut req = self
            .http
            .get(url)
            .header("User-Agent", TIDAL_USER_AGENT)
            .query(query);
        if let Some(token) = token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }
        let resp = req.send().await?;
        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| StreamingError::Parse(e.to_string()))?;
        Ok((status, body))
    }

    /// 统一错误构造：保留 Tidal 原始 userMessage + subStatus，
    /// 常见 subStatus 附带可读原因（避免上层聚合出误导性文案）
    fn api_error(status: u16, body: &Value) -> StreamingError {
        let raw = body
            .get("userMessage")
            .or_else(|| body.get("message"))
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("Unknown TIDAL API error");
        let sub_status = body.get("subStatus").and_then(Value::as_i64);
        let friendly = match sub_status {
            // 11003: token 过期（自动刷新未成功，需重新登录）
            Some(11003) => "TIDAL token 已过期且自动刷新失败，请重新登录",
            // 5003: 订阅等级不允许（订阅到期/降级 Free，所有音质取流被拒）
            Some(5003) => "TIDAL 订阅等级不允许该操作（账号无流媒体播放权限，请检查订阅状态）",
            _ => raw,
        };
        let message = match sub_status {
            Some(sub) => format!("{} [subStatus={} | {}]", friendly, sub, raw),
            None => friendly.to_string(),
        };
        StreamingError::Api { status, message }
    }

    /// 取走自动刷新产生的新 token（一次性）：调用方须持久化到 DB，
    /// 否则下次调用仍从 DB 读到过期 token
    pub fn take_refreshed_tokens(&self) -> Option<(String, Option<String>)> {
        self.fresh_tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }
}
