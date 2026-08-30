//! 酷狗用户详情模块（对齐桌面端 electron/main/apis/kugou/modules/user_detail.ts）。
//!
//! 纯 Rust 实现，零 Node 运行时依赖：
//! - 校验本地会话中的 `token` 和 `userid`
//! - 通过 RSA no-padding 算法加密 `p` 鉴权参数
//! - 请求网关 `/v3/get_my_info`（`x-router: usercenter.kugou.com`），获取并规范化用户信息

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::crypto::kg_rsa::rsa_encrypt_kugou;
use crate::error::QqkgError;
use crate::kugou::KugouClient;
use crate::normalize::secure_url;
use crate::types::{PlatformProfile, UserDetailResponse};

impl KugouClient {
    /// 获取酷狗用户登录状态及个人资料。
    pub async fn user_detail(&self) -> Result<UserDetailResponse, QqkgError> {
        let token = match self.cookies.get("token").filter(|s| !s.is_empty()) {
            Some(t) => t,
            None => return Ok(UserDetailResponse::not_logged_in(Some("未登录酷狗账号"))),
        };

        let userid_str = match self.cookies.get("userid").filter(|s| !s.is_empty()) {
            Some(u) => u,
            None => return Ok(UserDetailResponse::not_logged_in(Some("未登录酷狗账号"))),
        };

        let clienttime = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();

        // 构造明文并经 RSA no-padding 加密后转大写
        let plaintext = json!({
            "token": token,
            "clienttime": clienttime
        })
        .to_string();
        let p = rsa_encrypt_kugou(&plaintext).to_uppercase();

        let userid_val: Value = userid_str
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::from(userid_str.as_str()));

        let post_body = json!({
            "visit_time": clienttime,
            "usertype": 1,
            "p": p,
            "userid": userid_val
        });

        let body_str = post_body.to_string();

        let resp_body = match self
            .kg_gateway_post(
                "/v3/get_my_info",
                &[("plat".into(), "1".into())],
                &body_str,
                &[("x-router", "usercenter.kugou.com".into())],
            )
            .await
        {
            Ok(b) => b,
            Err(e) => {
                // 网关调用失败时如果 token 失效或网络错误
                return Err(e);
            }
        };

        let data = resp_body.get("data").cloned().unwrap_or(Value::Null);

        let stored_nickname = self.cookies.get("nickname").cloned();
        let stored_avatar = self.cookies.get("avatar").cloned();

        let nickname = ["nickname", "username", "user_name"]
            .iter()
            .find_map(|k| data.get(*k).and_then(Value::as_str))
            .map(ToString::to_string)
            .or(stored_nickname)
            .unwrap_or_else(|| format!("KG {userid_str}"));

        let avatar_url = ["pic", "userpic", "user_pic", "avatar"]
            .iter()
            .find_map(|k| data.get(*k).and_then(Value::as_str))
            .map(|s| secure_url(s).to_string())
            .or(stored_avatar)
            .unwrap_or_default();

        let vip_type = data
            .get("vip_type")
            .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .or_else(|| {
                self.cookies
                    .get("vip_type")
                    .and_then(|s| s.parse::<i64>().ok())
            })
            .unwrap_or(0);

        let has_vip_token = self
            .cookies
            .get("vip_token")
            .map(|s| !s.is_empty())
            .unwrap_or(false);

        let is_vip = vip_type > 0 || has_vip_token;

        let profile = PlatformProfile {
            user_id: userid_str.clone(),
            nickname,
            avatar_url,
            is_vip,
            vip_level: vip_type,
        };

        Ok(UserDetailResponse::logged_in(profile))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn user_detail_not_logged_in_when_no_token() {
        let client = KugouClient::new(HashMap::new());
        let res = client.user_detail().await.unwrap();
        assert_eq!(res.logged_in, false);
        assert!(res.profile.is_none());
    }
}
