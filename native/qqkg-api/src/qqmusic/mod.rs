//! QQ 音乐客户端：统一走 u.y.qq.com/cgi-bin/musicu.fcg 的 `{comm, request}` 协议。
//!
//! 对齐桌面端 electron/main/apis/qqmusic/core/request.ts + config.ts：
//! 明文 JSON POST（无加密），靠 okhttp UA + 移动端 comm 参数伪装客户端。

mod search;
mod user_detail;

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::QqkgError;

const QM_API_URL: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
const QM_UA: &str = "okhttp/3.14.9";
const QM_REFERER: &str = "https://y.qq.com";
/// 未登录时的默认 Cookie（对齐桌面端 QM_HEADERS.Cookie）。
const QM_DEFAULT_COOKIE: &str = "tmeLoginType=-1";
/// 重试次数与退避（对齐桌面端 MAX_RETRY / RETRY_BACKOFF）。
const QM_MAX_RETRY: usize = 2;
const QM_RETRY_BACKOFF: Duration = Duration::from_millis(300);

/// 伪装 Android 客户端的 comm 字段（对齐桌面端 getCommonParams）。
fn common_params(
    uin: &str,
    session_uid: Option<&str>,
    session_sid: Option<&str>,
    session_userip: Option<&str>,
) -> Value {
    let mut comm = json!({
        "ct": 11,
        "cv": "1003006",
        "v": "1003006",
        "os_ver": "15",
        "phonetype": "24122RKC7C",
        "tmeAppID": "qqmusiclight",
        "nettype": "NETWORK_WIFI",
        "udid": "0",
        "OpenUDID": "0",
        "QIMEI36": "0",
        "uin": "0",
    });
    if !uin.is_empty() && uin != "0" {
        comm["uin"] = json!(uin);
    }
    if let Some(uid) = session_uid {
        comm["uid"] = json!(uid);
    }
    if let Some(sid) = session_sid {
        comm["sid"] = json!(sid);
    }
    if let Some(userip) = session_userip {
        comm["userip"] = json!(userip);
    }
    comm
}

pub struct QqmusicClient {
    http: reqwest::Client,
    cookies: HashMap<String, String>,
}

impl QqmusicClient {
    pub fn new(cookies: HashMap<String, String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .build()
            .unwrap_or_default();
        Self { http, cookies }
    }

    /// 提取当前登录 uin（纯数字，去 o 前缀；对齐桌面端 getQQMusicUin）。
    fn uin(&self) -> String {
        let raw = self
            .cookies
            .get("uin")
            .or_else(|| self.cookies.get("wxuin"))
            .or_else(|| self.cookies.get("p_uin"))
            .map(String::as_str)
            .unwrap_or_default();
        raw.strip_prefix('o').unwrap_or(raw).to_string()
    }

    fn cookie_header(&self) -> String {
        let entries: Vec<String> = self
            .cookies
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        if entries.is_empty() {
            QM_DEFAULT_COOKIE.to_string()
        } else {
            entries.join("; ")
        }
    }

    /// 获取客户端会话 uid / sid / userip（对齐桌面端 ensureSession）。
    async fn fetch_session(&self) -> (Option<String>, Option<String>, Option<String>) {
        let uin = self.uin();
        let body = json!({
            "comm": common_params(&uin, None, None, None),
            "request": {
                "module": "music.getSession.session",
                "method": "GetSession",
                "param": { "caller": 0, "uid": uin, "vkey": 0 }
            }
        });

        if let Ok(resp) = self.post_fcg_raw(&body).await {
            let info = resp
                .get("request")
                .and_then(|r| r.get("data"))
                .and_then(|d| d.get("session"));
            if let Some(s) = info {
                let uid = s.get("uid").and_then(Value::as_str).map(ToString::to_string);
                let sid = s.get("sid").and_then(Value::as_str).map(ToString::to_string);
                let userip = s.get("userip").and_then(Value::as_str).map(ToString::to_string);
                return (uid, sid, userip);
            }
        }
        (None, None, None)
    }

    /// 发送一次 musicu.fcg 请求，返回 `request.data` 业务数据段。
    ///
    /// 对齐桌面端 qmRequest 的重试策略：QM 后端偶发瞬时错误（如 inner=2001），
    /// 连同非零业务码一起重试，最多 2 次、300ms 退避。
    pub async fn post_fcg(
        &self,
        module: &str,
        method: &str,
        param: Value,
    ) -> Result<Value, QqkgError> {
        let (uid, sid, userip) = self.fetch_session().await;
        let body = json!({
            "comm": common_params(&self.uin(), uid.as_deref(), sid.as_deref(), userip.as_deref()),
            "request": { "module": module, "method": method, "param": param }
        });

        let mut last_err: Option<QqkgError> = None;
        for attempt in 0..=QM_MAX_RETRY {
            match self.post_fcg_once(&body).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    last_err = Some(e);
                    if attempt < QM_MAX_RETRY {
                        tokio::time::sleep(QM_RETRY_BACKOFF * (attempt as u32 + 1)).await;
                    }
                }
            }
        }
        Err(last_err.expect("retry loop runs at least once"))
    }

    async fn post_fcg_raw(&self, body: &Value) -> Result<Value, QqkgError> {
        let resp = self
            .http
            .post(QM_API_URL)
            .header("User-Agent", QM_UA)
            .header("Referer", QM_REFERER)
            .header("Cookie", self.cookie_header())
            .json(body)
            .send()
            .await
            .map_err(|e| QqkgError::Upstream(format!("QM HTTP error: {e}")))?;

        let data: Value = resp
            .json()
            .await
            .map_err(|e| QqkgError::BadResponse(format!("QM non-JSON response: {e}")))?;
        Ok(data)
    }

    async fn post_fcg_once(&self, body: &Value) -> Result<Value, QqkgError> {
        let data = self.post_fcg_raw(body).await?;
        let outer = data.get("code").and_then(Value::as_i64).unwrap_or(0);
        let inner = data
            .get("request")
            .and_then(|r| r.get("code"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if outer != 0 || inner != 0 {
            return Err(QqkgError::Upstream(format!(
                "QM API error: outer={outer} inner={inner}"
            )));
        }
        Ok(data
            .get("request")
            .and_then(|r| r.get("data"))
            .cloned()
            .unwrap_or(Value::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uin_extraction() {
        let mut cookies = HashMap::new();
        cookies.insert("uin".to_string(), "o123456".to_string());
        let c = QqmusicClient::new(cookies);
        assert_eq!(c.uin(), "123456");

        let mut cookies = HashMap::new();
        cookies.insert("wxuin".to_string(), "42".to_string());
        assert_eq!(QqmusicClient::new(cookies).uin(), "42");

        assert_eq!(QqmusicClient::new(HashMap::new()).uin(), "");
    }

    #[test]
    fn cookie_header_fallback_and_join() {
        assert_eq!(QqmusicClient::new(HashMap::new()).cookie_header(), "tmeLoginType=-1");
        let mut cookies = HashMap::new();
        cookies.insert("uin".to_string(), "1".to_string());
        cookies.insert("qm_keyst".to_string(), "abc".to_string());
        let header = QqmusicClient::new(cookies).cookie_header();
        assert!(header.contains("uin=1"));
        assert!(header.contains("qm_keyst=abc"));
        assert_eq!(header.matches(';').count(), 1);
    }

    #[test]
    fn common_params_anonymous_vs_logged_in() {
        let anon = common_params("0", None, None, None);
        assert_eq!(anon["uin"], json!("0"));
        assert_eq!(anon["ct"], json!(11));
        assert_eq!(anon["tmeAppID"], json!("qqmusiclight"));

        let logged = common_params("12345", Some("uid_1"), Some("sid_1"), None);
        assert_eq!(logged["uin"], json!("12345"));
        assert_eq!(logged["uid"], json!("uid_1"));
        assert_eq!(logged["sid"], json!("sid_1"));
    }
}
