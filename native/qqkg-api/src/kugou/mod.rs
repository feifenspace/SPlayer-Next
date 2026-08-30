//! 酷狗客户端：mobilecdn 公开接口 + 网关鉴权请求。
//!
//! 对齐桌面端 electron/main/apis/kugou/core/request.ts：
//! - `kg_request`：基础 GET（mobilecdn / songsearch / lyrics 等公开接口）
//! - `kg_gateway_request`：Android 签名 + 设备标识注入（complexsearch 等网关路由）

mod login_qr;
mod search;
mod song_url;
mod user_detail;


use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::crypto::kg_mid::device_mid;
use crate::crypto::kg_sign::signature_android_params;
use crate::error::QqkgError;
use crate::types::KG_APPID;
use crate::types::KG_CLIENTVER;


const KG_GATEWAY_URL: &str = "https://gateway.kugou.com";
const KG_GATEWAY_UA: &str = "Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi";
const KG_REQ_TIMEOUT: Duration = Duration::from_secs(8);

/// 清除 KG 响应的注释包裹（对齐桌面端 cleanKgResponse）。
pub fn clean_kg_response(text: &str) -> String {
    text.trim()
        .trim_start_matches("<!--KG_TAG_RES_START-->")
        .trim_end_matches("<!--KG_TAG_RES_END-->")
        .to_string()
}

/// KG 响应错误码校验（error_code / errcode / err_code 任一非 0 且非 200 则失败）。
fn check_kg_code(body: &Value) -> Result<(), QqkgError> {
    let code = ["error_code", "errcode", "err_code"]
        .iter()
        .find_map(|k| body.get(*k).and_then(Value::as_i64))
        .unwrap_or(0);
    if code != 0 && code != 200 {
        let msg = body
            .get("msg")
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(QqkgError::Upstream(format!(
            "KG API error_code={code}, msg={msg}"
        )));
    }
    Ok(())
}

fn parse_kg_body(text: &str) -> Result<Value, QqkgError> {
    let cleaned = clean_kg_response(text);
    let body: Value = serde_json::from_str(&cleaned)
        .map_err(|e| QqkgError::BadResponse(format!("KG non-JSON response: {e}")))?;
    check_kg_code(&body)?;
    Ok(body)
}

pub struct KugouClient {
    http: reqwest::Client,
    cookies: HashMap<String, String>,
}

impl KugouClient {
    pub fn new(cookies: HashMap<String, String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(KG_REQ_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self { http, cookies }
    }

    /// 基础 GET 请求（对齐桌面端 kgRequest：清理响应 + 错误码校验）。
    pub async fn kg_request(&self, url: &str) -> Result<Value, QqkgError> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| QqkgError::Upstream(format!("KG HTTP error: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(QqkgError::Upstream(format!("KG HTTP {}", status.as_u16())));
        }
        let text = resp
            .text()
            .await
            .map_err(|e| QqkgError::BadResponse(format!("KG read body error: {e}")))?;
        parse_kg_body(&text)
    }

    /// 带签名的网关 GET 请求（对齐桌面端 kgGatewayRequest）。
    pub async fn kg_gateway_request(
        &self,
        path: &str,
        params: &[(String, String)],
        extra_headers: &[(&str, String)],
    ) -> Result<Value, QqkgError> {
        self.kg_gateway_execute(reqwest::Method::GET, path, params, "", extra_headers)
            .await
    }

    /// 带签名的网关 POST 请求（对齐桌面端 kgGatewayRequest method=POST）。
    pub async fn kg_gateway_post(
        &self,
        path: &str,
        params: &[(String, String)],
        body: &str,
        extra_headers: &[(&str, String)],
    ) -> Result<Value, QqkgError> {
        self.kg_gateway_execute(reqwest::Method::POST, path, params, body, extra_headers)
            .await
    }

    async fn kg_gateway_execute(
        &self,
        method: reqwest::Method,
        path: &str,
        params: &[(String, String)],
        body: &str,
        extra_headers: &[(&str, String)],
    ) -> Result<Value, QqkgError> {
        let clienttime = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let mid = device_mid().to_string();
        let dfid = self
            .cookies
            .get("dfid")
            .filter(|v| !v.is_empty())
            .cloned()
            .unwrap_or_else(|| "-".to_string());

        let mut merged: Vec<(String, String)> = vec![
            ("dfid".into(), dfid.clone()),
            ("mid".into(), mid.clone()),
            ("uuid".into(), "-".into()),
            ("appid".into(), KG_APPID.to_string()),
            ("clientver".into(), KG_CLIENTVER.to_string()),
            ("clienttime".into(), clienttime.to_string()),
        ];
        if let Some(token) = self.cookies.get("token").filter(|v| !v.is_empty()) {
            merged.push(("token".into(), token.clone()));
        }
        if let Some(userid) = self.cookies.get("userid").filter(|v| !v.is_empty()) {
            merged.push(("userid".into(), userid.clone()));
        }
        merged.extend(params.iter().cloned());

        // 签名按参数名排序后拼接，带 body（signature_android_params 内部处理顺序）
        let signature = signature_android_params(&merged, body);
        merged.push(("signature".into(), signature));

        let query: Vec<String> = merged
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect();
        let url = format!("{KG_GATEWAY_URL}{path}?{}", query.join("&"));

        let mut req = self
            .http
            .request(method.clone(), &url)
            .header("User-Agent", KG_GATEWAY_UA)
            .header("dfid", &dfid)
            .header("clienttime", clienttime.to_string())
            .header("mid", &mid)
            .header("kg-rc", "1")
            .header("kg-thash", "5d816a0")
            .header("kg-rec", "1")
            .header("kg-rf", "B9EDA08A64250DEFFBCADDEE00F8F25F");

        if method == reqwest::Method::POST {
            req = req
                .header("Content-Type", "application/json")
                .body(body.to_string());
        }

        for (k, v) in extra_headers {
            req = req.header(*k, v);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| QqkgError::Upstream(format!("KG Gateway HTTP error: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(QqkgError::Upstream(format!("KG Gateway HTTP {}", status.as_u16())));
        }
        let text = resp
            .text()
            .await
            .map_err(|e| QqkgError::BadResponse(format!("KG read body error: {e}")))?;
        parse_kg_body(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clean_kg_response_strips_wrappers() {
        assert_eq!(
            clean_kg_response("<!--KG_TAG_RES_START-->{\"a\":1}<!--KG_TAG_RES_END-->"),
            "{\"a\":1}"
        );
        assert_eq!(clean_kg_response("  {\"a\":1}  "), "{\"a\":1}");
    }

    #[test]
    fn check_kg_code_semantics() {
        assert!(check_kg_code(&json!({ "error_code": 0 })).is_ok());
        assert!(check_kg_code(&json!({ "errcode": 200 })).is_ok());
        assert!(check_kg_code(&json!({ "data": {} })).is_ok()); // 缺码视为 0
        assert!(check_kg_code(&json!({ "error_code": 301, "msg": "x" })).is_err());
    }

    #[test]
    fn parse_kg_body_full_chain() {
        let body = parse_kg_body("<!--KG_TAG_RES_START-->{\"error_code\":0,\"data\":{}}<!--KG_TAG_RES_END-->");
        assert!(body.is_ok());
        assert!(parse_kg_body("not json").is_err());
    }
}
