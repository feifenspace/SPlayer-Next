//! 酷狗扫码登录模块（对齐桌面端 electron/main/apis/kugou/modules/login_qr.ts）。
//!
//! 纯 Rust 实现，零 Node 运行时依赖：
//! - `login_qr_key`：请求 `login-user.kugou.com/v2/qrcode` 获取二维码 key 与网页地址
//! - `login_qr_check`：请求 `login-user.kugou.com/v2/get_userinfo_qrcode` 轮询扫码状态

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::crypto::kg_mid::device_mid;
use crate::crypto::kg_sign::signature_web_params;
use crate::error::QqkgError;
use crate::kugou::KugouClient;
use crate::normalize::secure_url;
use crate::types::{KugouQrCheckResponse, KugouQrKeyResponse, KG_CLIENTVER};

const BASE_LOGIN_URL: &str = "https://login-user.kugou.com";
const SRC_APPID: u32 = 2919;
const QR_CONTENT_PREFIX: &str = "https://h5.kugou.com/apps/loginQRCode/html/index.html?qrcode=";
const QR_TXT_TEMPLATE: &str = "https://h5.kugou.com/apps/loginQRCode/html/index.html?appid=1005&";

impl KugouClient {
    /// 发送带有 Web 签名的登录请求。
    async fn request_login(
        &self,
        path: &str,
        input_params: &[(&str, String)],
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

        let mut params: Vec<(String, String)> = vec![
            ("dfid".into(), dfid),
            ("mid".into(), mid),
            ("uuid".into(), "-".into()),
            ("clientver".into(), KG_CLIENTVER.to_string()),
            ("clienttime".into(), clienttime.to_string()),
        ];
        for (k, v) in input_params {
            params.push(((*k).into(), v.clone()));
        }

        // Web 签名：MD5(salt + 排序后的 k=v + salt)
        let signature = signature_web_params(&params);
        params.push(("signature".into(), signature));

        let query: Vec<String> = params
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect();
        let url = format!("{BASE_LOGIN_URL}{path}?{}", query.join("&"));

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| QqkgError::Upstream(format!("KG Login HTTP error: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(QqkgError::Upstream(format!("KG Login HTTP {}", status.as_u16())));
        }

        let body: Value = resp
            .json()
            .await
            .map_err(|e| QqkgError::BadResponse(format!("KG Login non-JSON response: {e}")))?;

        Ok(body)
    }

    /// 获取扫码登录二维码 Key 与内容。
    pub async fn login_qr_key(&self) -> Result<KugouQrKeyResponse, QqkgError> {
        let body = self
            .request_login(
                "/v2/qrcode",
                &[
                    ("type", "1".into()),
                    ("plat", "4".into()),
                    ("appid", "1001".into()),
                    ("qrcode_txt", QR_TXT_TEMPLATE.into()),
                    ("srcappid", SRC_APPID.to_string()),
                ],
            )
            .await?;

        let key = body
            .get("data")
            .and_then(|d| d.get("qrcode"))
            .and_then(Value::as_str)
            .ok_or_else(|| QqkgError::BadResponse("KG QR key missing in response".into()))?;

        Ok(KugouQrKeyResponse {
            code: 200,
            key: key.to_string(),
            content: format!("{QR_CONTENT_PREFIX}{key}"),
        })
    }

    /// 轮询扫码登录状态与用户信息。
    pub async fn login_qr_check(&self, key: &str) -> Result<KugouQrCheckResponse, QqkgError> {
        if key.is_empty() {
            return Err(QqkgError::InvalidParam("KG QR key is empty".into()));
        }

        let body = self
            .request_login(
                "/v2/get_userinfo_qrcode",
                &[
                    ("plat", "4".into()),
                    ("appid", "1005".into()),
                    ("srcappid", SRC_APPID.to_string()),
                    ("qrcode", key.to_string()),
                ],
            )
            .await?;

        let data = body.get("data").cloned().unwrap_or(Value::Null);
        let status = data
            .get("status")
            .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(1) as i32;

        let nickname = ["username", "nickname", "nick_name", "user_name"]
            .iter()
            .find_map(|k| data.get(*k).and_then(Value::as_str))
            .map(ToString::to_string);

        let avatar_url = [
            "userpic",
            "user_pic",
            "avatar",
            "avatar_url",
            "pic",
            "user_img",
        ]
        .iter()
        .find_map(|k| data.get(*k).and_then(Value::as_str))
        .map(|s| secure_url(s).to_string());

        let token = data.get("token").and_then(Value::as_str).map(ToString::to_string);
        let userid = data
            .get("userid")
            .and_then(|v| {
                v.as_str()
                    .map(ToString::to_string)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            });
        let vip_token = data.get("vip_token").and_then(Value::as_str).map(ToString::to_string);
        let vip_type = data
            .get("vip_type")
            .and_then(|v| {
                v.as_str()
                    .map(ToString::to_string)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            });

        Ok(KugouQrCheckResponse {
            code: 200,
            status,
            nickname,
            avatar_url,
            token,
            userid,
            vip_token,
            vip_type,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_login_qr_check_response_status4() {
        let raw = json!({
            "data": {
                "status": 4,
                "username": "测试酷狗用户",
                "userpic": "http://imge.kugou.com/avatar/100.jpg",
                "token": "kg_test_token_123",
                "userid": 12345678,
                "vip_token": "vip_abc",
                "vip_type": 1
            }
        });
        let data = &raw["data"];
        let status = data["status"].as_i64().unwrap() as i32;
        assert_eq!(status, 4);

        let nickname = data["username"].as_str().unwrap().to_string();
        let avatar = secure_url(data["userpic"].as_str().unwrap()).to_string();
        assert_eq!(nickname, "测试酷狗用户");
        assert_eq!(avatar, "https://imge.kugou.com/avatar/100.jpg");
    }

    #[test]
    fn parse_login_qr_check_response_status0_expired() {
        let raw = json!({
            "data": {
                "status": "0"
            }
        });
        let data = &raw["data"];
        let status = data
            .get("status")
            .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap();
        assert_eq!(status, 0);
    }
}
