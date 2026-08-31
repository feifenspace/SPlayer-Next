use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::error::StreamingError;
use super::config::{
    TIDAL_ANDROID_REDIRECT_URI, TIDAL_AUTH_BASE, TIDAL_DEFAULT_CLIENT_ID,
    TIDAL_DEFAULT_CLIENT_SECRET, TIDAL_LOGIN_BASE, TIDAL_OFFICIAL_CLIENT_ID, TIDAL_SCOPE,
    TIDAL_USER_AGENT,
};
use super::pkce::{generate_code_challenge, generate_code_verifier, generate_state};
use super::TidalClient;

#[derive(Debug, Clone)]
struct PendingPkce {
    code_verifier: String,
    client_id: String,
    redirect_uri: String,
    created_at: u64,
}

static PENDING_PKCE: RwLock<Option<HashMap<String, PendingPkce>>> = RwLock::const_new(None);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl TidalClient {
    /// 第一步（官方移动端 PKCE 流程）：生成高音质授权 URL
    pub async fn auth_authorize(
        &self,
        params: &HashMap<String, Value>,
    ) -> Result<Value, StreamingError> {
        let client_id = params
            .get("clientId")
            .or_else(|| params.get("client_id"))
            .and_then(Value::as_str)
            .unwrap_or(TIDAL_OFFICIAL_CLIENT_ID)
            .to_string();

        let redirect_uri = params
            .get("redirectUri")
            .or_else(|| params.get("redirect_uri"))
            .and_then(Value::as_str)
            .unwrap_or(TIDAL_ANDROID_REDIRECT_URI)
            .to_string();

        let verifier = generate_code_verifier();
        let challenge = generate_code_challenge(&verifier);
        let state = generate_state();

        let mut lock = PENDING_PKCE.write().await;
        let map = lock.get_or_insert_with(HashMap::new);
        // 清理超过 15 分钟的过期 pending
        map.retain(|_, v| now_secs().saturating_sub(v.created_at) < 900);
        let pending = PendingPkce {
            code_verifier: verifier.clone(),
            client_id: client_id.clone(),
            redirect_uri: redirect_uri.clone(),
            created_at: now_secs(),
        };
        map.insert(state.clone(), pending.clone());
        map.insert("__latest".to_string(), pending);

        let query = vec![
            ("lang", "en"),
            ("response_type", "code"),
            ("client_id", client_id.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("restrictSignup", "true"),
            ("state", state.as_str()),
        ];
        let qs = query
            .into_iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        let auth_url = format!("{}/authorize?{}", TIDAL_LOGIN_BASE, qs);

        Ok(json!({
            "status": "success",
            "url": auth_url,
            "state": state,
            "redirectUri": redirect_uri,
        }))
    }

    /// 第二步（官方移动端 PKCE 流程）：用回贴的 code 换取 Access Token 与 Refresh Token
    pub async fn auth_exchange(
        &self,
        params: &HashMap<String, Value>,
    ) -> Result<Value, StreamingError> {
        let code = params
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| StreamingError::InvalidParam("Missing code parameter".into()))?;

        let state = params.get("state").and_then(Value::as_str).unwrap_or("");

        let pending = {
            let mut lock = PENDING_PKCE.write().await;
            if let Some(map) = lock.as_mut() {
                if !state.is_empty() {
                    map.remove(state)
                } else if !map.is_empty() {
                    let first_key = map.keys().next().cloned().unwrap();
                    map.remove(&first_key)
                } else {
                    None
                }
            } else {
                None
            }
        };

        let verifier = pending
            .as_ref()
            .map(|p| p.code_verifier.clone())
            .unwrap_or_else(generate_code_verifier);

        let client_id = pending
            .as_ref()
            .map(|p| p.client_id.as_str())
            .unwrap_or(TIDAL_OFFICIAL_CLIENT_ID);

        let redirect_uri = pending
            .as_ref()
            .map(|p| p.redirect_uri.as_str())
            .unwrap_or(TIDAL_ANDROID_REDIRECT_URI);

        let form_params = [
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("code_verifier", verifier.as_str()),
            ("redirect_uri", redirect_uri),
        ];

        let token_url = format!("{}/token", TIDAL_AUTH_BASE);
        let resp = self
            .http
            .post(&token_url)
            .header("User-Agent", TIDAL_USER_AGENT)
            .form(&form_params)
            .send()
            .await?;

        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| StreamingError::Parse(e.to_string()))?;

        if !status.is_success() {
            let msg = body
                .get("error_description")
                .or_else(|| body.get("error"))
                .or_else(|| body.get("user_message"))
                .and_then(Value::as_str)
                .unwrap_or("TIDAL token exchange failed");
            return Err(StreamingError::Api {
                status: status.as_u16(),
                message: msg.to_string(),
            });
        }

        let access_token = body.get("access_token").and_then(Value::as_str).unwrap_or("");
        let refresh_token = body.get("refresh_token").and_then(Value::as_str).unwrap_or("");
        let expires_in = body.get("expires_in").and_then(Value::as_u64).unwrap_or(86400);

        let user = body.get("user");
        let user_id = user.and_then(|u| u.get("userId")).map(|v| v.to_string()).unwrap_or_default();
        let username = user.and_then(|u| u.get("username").or_else(|| u.get("email"))).and_then(Value::as_str).unwrap_or("TIDAL User");
        let country_code = user.and_then(|u| u.get("countryCode")).and_then(Value::as_str).unwrap_or("US");

        Ok(json!({
            "status": "success",
            "accessToken": access_token,
            "refreshToken": refresh_token,
            "expiresIn": expires_in,
            "expiresAt": now_secs() + expires_in,
            "userId": user_id,
            "username": username,
            "countryCode": country_code,
            "clientId": client_id,
            "raw": body
        }))
    }

    /// 设备码授权第一步：获取 device_code 与 user_code
    pub async fn auth_device_authorization(
        &self,
        params: &HashMap<String, Value>,
    ) -> Result<Value, StreamingError> {
        let client_id = params
            .get("clientId")
            .or_else(|| params.get("client_id"))
            .and_then(Value::as_str)
            .unwrap_or(TIDAL_DEFAULT_CLIENT_ID);

        let url = format!("{}/device_authorization", TIDAL_AUTH_BASE);
        let form = [("client_id", client_id), ("scope", TIDAL_SCOPE)];

        let resp = self
            .http
            .post(&url)
            .header("User-Agent", TIDAL_USER_AGENT)
            .form(&form)
            .send()
            .await?;

        let status = resp.status();
        let mut body: Value = resp.json().await.map_err(|e| StreamingError::Parse(e.to_string()))?;

        if !status.is_success() {
            let msg = body.get("error_description").and_then(Value::as_str).unwrap_or("Failed to request device authorization");
            return Err(StreamingError::Api {
                status: status.as_u16(),
                message: msg.to_string(),
            });
        }

        if let Some(obj) = body.as_object_mut() {
            if let Some(uri) = obj.get("verificationUri").and_then(Value::as_str) {
                if !uri.starts_with("http://") && !uri.starts_with("https://") {
                    obj.insert("verificationUri".to_string(), json!(format!("https://{uri}")));
                }
            }
            if let Some(uri) = obj.get("verificationUriComplete").and_then(Value::as_str) {
                if !uri.starts_with("http://") && !uri.starts_with("https://") {
                    obj.insert("verificationUriComplete".to_string(), json!(format!("https://{uri}")));
                }
            }
        }

        Ok(body)
    }

    /// 设备码授权第二步：轮询 Token 端点
    pub async fn auth_token_poll(
        &self,
        params: &HashMap<String, Value>,
    ) -> Result<Value, StreamingError> {
        let device_code = params
            .get("deviceCode")
            .or_else(|| params.get("device_code"))
            .and_then(Value::as_str)
            .ok_or_else(|| StreamingError::InvalidParam("Missing deviceCode".into()))?;

        let client_id = params
            .get("clientId")
            .or_else(|| params.get("client_id"))
            .and_then(Value::as_str)
            .unwrap_or(TIDAL_DEFAULT_CLIENT_ID);

        let client_secret = params
            .get("clientSecret")
            .or_else(|| params.get("client_secret"))
            .and_then(Value::as_str)
            .unwrap_or(TIDAL_DEFAULT_CLIENT_SECRET);

        let url = format!("{}/token", TIDAL_AUTH_BASE);
        let form = [
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("scope", TIDAL_SCOPE),
        ];

        let resp = self
            .http
            .post(&url)
            .header("User-Agent", TIDAL_USER_AGENT)
            .form(&form)
            .send()
            .await?;

        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| StreamingError::Parse(e.to_string()))?;

        if status.as_u16() == 400 {
            if let Some(err) = body.get("error").and_then(Value::as_str) {
                if err == "authorization_pending" {
                    return Ok(json!({ "status": "pending" }));
                } else if err == "expired_token" {
                    return Ok(json!({ "status": "expired" }));
                } else if err == "access_denied" {
                    return Ok(json!({ "status": "denied" }));
                }
            }
        }

        if !status.is_success() {
            let msg = body.get("error_description").and_then(Value::as_str).unwrap_or("Device token poll failed");
            return Err(StreamingError::Api { status: status.as_u16(), message: msg.to_string() });
        }

        let access_token = body.get("access_token").and_then(Value::as_str).unwrap_or("");
        let refresh_token = body.get("refresh_token").and_then(Value::as_str).unwrap_or("");
        let expires_in = body.get("expires_in").and_then(Value::as_u64).unwrap_or(86400);

        let user = body.get("user");
        let user_id = user.and_then(|u| u.get("userId")).map(|v| v.to_string()).unwrap_or_default();
        let username = user.and_then(|u| u.get("username").or_else(|| u.get("email"))).and_then(Value::as_str).unwrap_or("TIDAL User");
        let country_code = user.and_then(|u| u.get("countryCode")).and_then(Value::as_str).unwrap_or("US");

        Ok(json!({
            "status": "success",
            "accessToken": access_token,
            "refreshToken": refresh_token,
            "expiresIn": expires_in,
            "expiresAt": now_secs() + expires_in,
            "userId": user_id,
            "username": username,
            "countryCode": country_code,
            "clientId": client_id,
            "raw": body
        }))
    }

    /// 刷新 Access Token
    pub async fn auth_token_refresh(
        &self,
        params: &HashMap<String, Value>,
    ) -> Result<Value, StreamingError> {
        let refresh_token = params
            .get("refreshToken")
            .or_else(|| params.get("refresh_token"))
            .and_then(Value::as_str)
            .or_else(|| self.refresh_token.as_deref())
            .ok_or_else(|| StreamingError::Auth("Missing refreshToken".into()))?;

        let client_id = params
            .get("clientId")
            .or_else(|| params.get("client_id"))
            .and_then(Value::as_str)
            .or_else(|| self.client_id.as_deref())
            .unwrap_or(TIDAL_OFFICIAL_CLIENT_ID);

        let client_secret = params
            .get("clientSecret")
            .or_else(|| params.get("client_secret"))
            .and_then(Value::as_str)
            .or_else(|| self.client_secret.as_deref())
            .unwrap_or("");

        let mut form_params = vec![
            ("client_id", client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", TIDAL_SCOPE),
        ];
        if !client_secret.is_empty() {
            form_params.push(("client_secret", client_secret));
        }

        let url = format!("{}/token", TIDAL_AUTH_BASE);
        let resp = self
            .http
            .post(&url)
            .header("User-Agent", TIDAL_USER_AGENT)
            .form(&form_params)
            .send()
            .await?;

        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| StreamingError::Parse(e.to_string()))?;

        if !status.is_success() {
            let msg = body.get("error_description").and_then(Value::as_str).unwrap_or("Token refresh failed");
            return Err(StreamingError::Api { status: status.as_u16(), message: msg.to_string() });
        }

        let new_access = body.get("access_token").and_then(Value::as_str).unwrap_or("");
        let new_refresh = body.get("refresh_token").and_then(Value::as_str).unwrap_or(refresh_token);
        let expires_in = body.get("expires_in").and_then(Value::as_u64).unwrap_or(86400);

        Ok(json!({
            "status": "success",
            "accessToken": new_access,
            "refreshToken": new_refresh,
            "expiresIn": expires_in,
            "expiresAt": now_secs() + expires_in,
        }))
    }

    pub async fn auth_status(&self) -> Result<Value, StreamingError> {
        if let Some(token) = &self.access_token {
            if !token.is_empty() {
                return Ok(json!({
                    "loggedIn": true,
                    "userId": self.user_id,
                    "countryCode": self.country_code,
                    "clientId": self.client_id,
                }));
            }
        }
        Ok(json!({ "loggedIn": false }))
    }

    pub async fn auth_logout(&self) -> Result<Value, StreamingError> {
        Ok(json!({ "status": "success", "loggedIn": false }))
    }
}
