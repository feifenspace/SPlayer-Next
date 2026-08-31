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
        let mut req = self
            .http
            .get(&url)
            .header("User-Agent", TIDAL_USER_AGENT)
            .query(&query);

        if let Some(token) = &self.access_token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }

        let resp = req.send().await?;
        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| StreamingError::Parse(e.to_string()))?;

        if !status.is_success() {
            let msg = body
                .get("userMessage")
                .or_else(|| body.get("message"))
                .or_else(|| body.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("Unknown TIDAL API error");
            return Err(StreamingError::Api {
                status: status.as_u16(),
                message: msg.to_string(),
            });
        }

        Ok(body)
    }
}
