//! QQ 音乐用户基础资料模块（对齐桌面端 electron/main/apis/qqmusic/modules/user_detail.ts）。
//!
//! 纯 Rust 实现，零 Node 运行时依赖：
//! - 校验 Cookie 中的 uin 与 key（qm_keyst / qqmusic_key / pskey / p_skey / skey）
//! - 请求 `c.y.qq.com/rsc/fcgi-bin/fcg_get_profile_homepage.fcg` 获取官方用户资料
//! - 官方接口异常时，使用 `QQ用户_{uin后4位}` 与 `headimg_dl` 头像进行兜底

use serde_json::Value;

use crate::error::QqkgError;
use crate::normalize::secure_url;
use crate::qqmusic::QqmusicClient;
use crate::types::{PlatformProfile, UserDetailResponse};

const QM_WEB_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

impl QqmusicClient {
    /// 检查是否存在有效的登录凭据
    pub fn has_login_credentials(&self) -> bool {
        let uin = self.uin();
        if uin.is_empty() || uin == "0" {
            return false;
        }
        let has_key = [
            "qm_keyst",
            "qqmusic_key",
            "pskey",
            "p_skey",
            "skey",
            "p_uin",
        ]
        .iter()
        .any(|k| {
            self.cookies
                .get(*k)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
        });
        has_key
    }

    /// 获取 QQ 音乐用户详情。
    pub async fn user_detail(&self) -> Result<UserDetailResponse, QqkgError> {
        let uin = self.uin();
        if !self.has_login_credentials() {
            return Ok(UserDetailResponse {
                code: 301,
                logged_in: false,
                message: Some("未登录 QM 账号".to_string()),
                profile: None,
            });
        }

        let suffix = if uin.len() >= 4 {
            &uin[uin.len() - 4..]
        } else {
            &uin
        };
        let fallback_nickname = format!("QQ用户_{suffix}");
        let default_avatar = format!("https://q.qlogo.cn/headimg_dl?dst_uin={uin}&spec=100");

        let url = format!(
            "https://c.y.qq.com/rsc/fcgi-bin/fcg_get_profile_homepage.fcg?cid=205360838&userid={}&reqfrom=1&format=json&inCharset=utf8&outCharset=utf-8&platform=yqq.json&needNewCode=0",
            urlencoding::encode(&uin)
        );

        let cookie_str = self.cookie_header();

        let resp_result = self
            .http
            .get(&url)
            .header("Referer", "https://y.qq.com/")
            .header("Origin", "https://y.qq.com")
            .header("User-Agent", QM_WEB_UA)
            .header("Cookie", cookie_str)
            .send()
            .await;

        let profile = match resp_result {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(json_body) = resp.json::<Value>().await {
                    let code = json_body.get("code").and_then(Value::as_i64).unwrap_or(-1);
                    let creator = json_body.get("data").and_then(|d| d.get("creator"));

                    if code == 0 && creator.is_some() {
                        let c = creator.unwrap();
                        let nickname = c
                            .get("nick")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(ToString::to_string)
                            .unwrap_or(fallback_nickname);

                        let avatar_url = c
                            .get("headpic")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(|s| secure_url(s).to_string())
                            .unwrap_or(default_avatar);

                        let is_vip = c.get("is_vip").and_then(Value::as_i64).unwrap_or(0) > 0
                            || c.get("is_super_vip").and_then(Value::as_i64).unwrap_or(0) > 0
                            || c.get("vip").and_then(Value::as_i64).unwrap_or(0) > 0;

                        let vip_level = c
                            .get("vip_level")
                            .and_then(Value::as_i64)
                            .unwrap_or(0);

                        PlatformProfile {
                            user_id: uin,
                            nickname,
                            avatar_url,
                            is_vip,
                            vip_level,
                        }
                    } else {
                        // 官方接口返回非 0，兜底返回
                        PlatformProfile {
                            user_id: uin,
                            nickname: fallback_nickname,
                            avatar_url: default_avatar,
                            is_vip: false,
                            vip_level: 0,
                        }
                    }
                } else {
                    PlatformProfile {
                        user_id: uin,
                        nickname: fallback_nickname,
                        avatar_url: default_avatar,
                        is_vip: false,
                        vip_level: 0,
                    }
                }
            }
            _ => {
                // 网络或 HTTP 异常，兜底基础 UIN 资料
                PlatformProfile {
                    user_id: uin,
                    nickname: fallback_nickname,
                    avatar_url: default_avatar,
                    is_vip: false,
                    vip_level: 0,
                }
            }
        };

        Ok(UserDetailResponse::logged_in(profile))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn user_detail_not_logged_in_without_cookies() {
        let client = QqmusicClient::new(HashMap::new());
        let res = client.user_detail().await.unwrap();
        assert_eq!(res.logged_in, false);
        assert_eq!(res.code, 301);
    }

    #[test]
    fn has_login_credentials_check() {
        let mut cookies = HashMap::new();
        let client = QqmusicClient::new(cookies.clone());
        assert!(!client.has_login_credentials());

        cookies.insert("uin".to_string(), "10001".to_string());
        let client = QqmusicClient::new(cookies.clone());
        assert!(!client.has_login_credentials()); // 没有 key

        cookies.insert("qm_keyst".to_string(), "token123".to_string());
        let client = QqmusicClient::new(cookies.clone());
        assert!(client.has_login_credentials());
    }
}
