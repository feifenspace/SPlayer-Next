//! QQ 音乐扫码登录模块（对齐桌面端 electron/main/apis/qqmusic/modules/login_qr.ts）。
//!
//! 纯 Rust 实现，支持 QQ 扫码与微信扫码两种原生协议：
//! - `login_qr_key`：获取二维码 Key 与图片内容（data:image/...;base64）
//! - `login_qr_check`：轮询扫码状态并完成授权登录，返回平台凭据供落库

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use regex::Regex;
use serde_json::{json, Value};

use crate::error::QqkgError;
use crate::qqmusic::QqmusicClient;
use crate::types::{QmQrCheckResponse, QmQrKeyResponse};

const WEB_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// QQ 系哈希算法（用于 ptqrtoken 及 g_tk 计算）
pub fn hash33(s: &str, seed: i64) -> i64 {
    let mut h = seed;
    for b in s.bytes() {
        h = (((h << 5) + h + b as i64) & 0xffffffff) as i64;
    }
    h & 2147483647
}

fn extract_cookies(resp: &reqwest::Response, existing: &mut HashMap<String, String>) {
    for val in resp.headers().get_all(reqwest::header::SET_COOKIE) {
        if let Ok(s) = val.to_str() {
            let first = s.split(';').next().unwrap_or("");
            if let Some((k, v)) = first.split_once('=') {
                let k = k.trim();
                let v = v.trim();
                if !k.is_empty() && !v.is_empty() {
                    existing.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
}

fn stringify_cookies(cookies: &HashMap<String, String>) -> String {
    cookies
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

impl QqmusicClient {
    /// 获取二维码 Key 及图片内容
    /// @param qr_type 扫码类型，"qq" 或 "wx"，默认 "qq"
    pub async fn login_qr_key(&self, qr_type: &str) -> Result<QmQrKeyResponse, QqkgError> {
        let is_wx = qr_type.eq_ignore_ascii_case("wx");

        if is_wx {
            let params = [
                ("appid", "wx48db31d50e334801"),
                ("redirect_uri", "https://y.qq.com/portal/wx_redirect.html?login_type=2&surl=https://y.qq.com/"),
                ("response_type", "code"),
                ("scope", "snsapi_login"),
                ("state", "STATE"),
                ("href", "https://y.qq.com/mediastyle/music_v17/src/css/popup_wechat.css#wechat_redirect"),
            ];
            let query: Vec<String> = params
                .iter()
                .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
                .collect();
            let connect_url = format!("https://open.weixin.qq.com/connect/qrconnect?{}", query.join("&"));

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|e| QqkgError::Upstream(format!("Reqwest client error: {e}")))?;

            let res = client
                .get(&connect_url)
                .header(reqwest::header::USER_AGENT, WEB_UA)
                .send()
                .await
                .map_err(|e| QqkgError::Upstream(format!("获取微信登录页面失败: {e}")))?;

            let html = res
                .text()
                .await
                .map_err(|e| QqkgError::Upstream(format!("读取微信登录页面失败: {e}")))?;

            let re = Regex::new(r#"uuid=([^"&]+)"#).map_err(|e| QqkgError::BadResponse(e.to_string()))?;
            let uuid = re
                .captures(&html)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str())
                .ok_or_else(|| QqkgError::BadResponse("获取微信登录二维码 uuid 失败".to_string()))?;

            let qr_res = client
                .get(format!("https://open.weixin.qq.com/connect/qrcode/{uuid}"))
                .header(reqwest::header::REFERER, "https://open.weixin.qq.com/connect/qrconnect")
                .header(reqwest::header::USER_AGENT, WEB_UA)
                .send()
                .await
                .map_err(|e| QqkgError::Upstream(format!("获取微信登录二维码图片失败: {e}")))?;

            let bytes = qr_res
                .bytes()
                .await
                .map_err(|e| QqkgError::Upstream(format!("读取微信登录二维码图片失败: {e}")))?;

            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

            Ok(QmQrKeyResponse {
                code: 200,
                key: uuid.to_string(),
                content: format!("data:image/jpeg;base64,{b64}"),
                qr_type: "wx".to_string(),
            })
        } else {
            // 默认 QQ 扫码
            let rand_val: f64 = {
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(123456);
                (nanos as f64) / 1_000_000_000.0
            };
            let url = format!(
                "https://ssl.ptlogin2.qq.com/ptqrshow?appid=716027609&e=2&l=M&s=3&d=72&v=4&t={rand_val}&daid=383&pt_3rd_aid=100497308"
            );

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|e| QqkgError::Upstream(format!("Reqwest client error: {e}")))?;

            let res = client
                .get(&url)
                .header(reqwest::header::REFERER, "https://xui.ptlogin2.qq.com/")
                .header(reqwest::header::USER_AGENT, WEB_UA)
                .send()
                .await
                .map_err(|e| QqkgError::Upstream(format!("获取 QQ 登录二维码失败: {e}")))?;

            let mut cookies = HashMap::new();
            extract_cookies(&res, &mut cookies);
            let qrsig = cookies
                .get("qrsig")
                .cloned()
                .ok_or_else(|| QqkgError::BadResponse("未能获取到 QQ 登录 qrsig".to_string()))?;

            let bytes = res
                .bytes()
                .await
                .map_err(|e| QqkgError::Upstream(format!("读取 QQ 登录二维码图片失败: {e}")))?;

            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

            Ok(QmQrKeyResponse {
                code: 200,
                key: qrsig,
                content: format!("data:image/png;base64,{b64}"),
                qr_type: "qq".to_string(),
            })
        }
    }

    /// 轮询二维码扫描状态
    /// @param key 二维码 Key（QQ 为 qrsig，微信为 uuid）
    /// @param qr_type 扫码类型，"qq" 或 "wx"
    pub async fn login_qr_check(
        &self,
        key: &str,
        qr_type: &str,
    ) -> Result<QmQrCheckResponse, QqkgError> {
        let is_wx = qr_type.eq_ignore_ascii_case("wx");

        if is_wx {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default();
            let url = format!("https://lp.open.weixin.qq.com/connect/l/qrconnect?uuid={key}&_={now}");

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(35))
                .build()
                .map_err(|e| QqkgError::Upstream(format!("Reqwest client error: {e}")))?;

            let res = client
                .get(&url)
                .header(reqwest::header::REFERER, "https://open.weixin.qq.com/")
                .header(reqwest::header::USER_AGENT, WEB_UA)
                .send()
                .await
                .map_err(|e| QqkgError::Upstream(format!("轮询微信扫码状态失败: {e}")))?;

            let text = res.text().await.unwrap_or_default();
            let re = Regex::new(r"window\.wx_errcode=(\d+);window\.wx_code='([^']*)'").unwrap();
            let caps = re.captures(&text);

            let Some(caps) = caps else {
                return Ok(QmQrCheckResponse {
                    code: 200,
                    status: 1, // 等待扫码
                    nickname: None,
                    avatar_url: None,
                    cookies: None,
                });
            };

            let errcode: i32 = caps.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            let wx_code = caps.get(2).map(|m| m.as_str()).unwrap_or("");

            if errcode == 404 {
                return Ok(QmQrCheckResponse {
                    code: 200,
                    status: 2, // 已扫码待确认
                    nickname: None,
                    avatar_url: None,
                    cookies: None,
                });
            }
            if errcode == 402 || errcode == 403 {
                return Ok(QmQrCheckResponse {
                    code: 200,
                    status: 0, // 已过期或已取消
                    nickname: None,
                    avatar_url: None,
                    cookies: None,
                });
            }

            if errcode == 405 && !wx_code.is_empty() {
                // 微信扫码成功，调用 music.login.LoginServer.Login 换取凭据
                let fcg_body = json!({
                    "comm": { "tmeLoginType": 1 },
                    "req_0": {
                        "module": "music.login.LoginServer",
                        "method": "Login",
                        "param": { "code": wx_code, "strAppid": "wx48db31d50e334801" }
                    }
                });

                let client_http = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .unwrap_or_default();

                let fcg_res = client_http
                    .post(crate::qqmusic::QM_API_URL)
                    .header(reqwest::header::USER_AGENT, WEB_UA)
                    .header(reqwest::header::REFERER, "https://y.qq.com")
                    .json(&fcg_body)
                    .send()
                    .await
                    .map_err(|e| QqkgError::Upstream(format!("QM Login HTTP error: {e}")))?;

                let fcg_val: Value = fcg_res
                    .json()
                    .await
                    .map_err(|e| QqkgError::BadResponse(format!("QM Login JSON error: {e}")))?;

                let login_data = fcg_val
                    .get("req_0")
                    .and_then(|r| r.get("data"))
                    .cloned()
                    .unwrap_or_default();

                let musicid = login_data
                    .get("musicid")
                    .and_then(|v| v.as_i64().map(|n| n.to_string()).or_else(|| v.as_str().map(ToString::to_string)))
                    .or_else(|| login_data.get("str_musicid").and_then(Value::as_str).map(ToString::to_string))
                    .unwrap_or_default();
                let uin_str = musicid.strip_prefix('o').unwrap_or(&musicid).to_string();

                let musickey = login_data
                    .get("musickey")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();

                let mut saved_cookies = HashMap::new();
                saved_cookies.insert("uin".to_string(), uin_str.clone());
                saved_cookies.insert("wxuin".to_string(), uin_str.clone());
                saved_cookies.insert("qm_keyst".to_string(), musickey.clone());
                saved_cookies.insert("qqmusic_key".to_string(), musickey);
                saved_cookies.insert("tmeLoginType".to_string(), "1".to_string());

                if let Some(euin) = login_data.get("encryptUin").and_then(Value::as_str) {
                    saved_cookies.insert("euin".to_string(), euin.to_string());
                }
                if let Some(openid) = login_data.get("openid").and_then(Value::as_str) {
                    saved_cookies.insert("wxopenid".to_string(), openid.to_string());
                }
                if let Some(unionid) = login_data.get("unionid").and_then(Value::as_str) {
                    saved_cookies.insert("psrf_qqunionid".to_string(), unionid.to_string());
                }
                if let Some(rt) = login_data.get("refresh_token").and_then(Value::as_str) {
                    saved_cookies.insert("wxrefresh_token".to_string(), rt.to_string());
                }
                if let Some(at) = login_data.get("access_token").and_then(Value::as_str) {
                    saved_cookies.insert("psrf_qqaccess_token".to_string(), at.to_string());
                }
                if let Some(rk) = login_data.get("refresh_key").and_then(Value::as_str) {
                    saved_cookies.insert("qm_refresh_key".to_string(), rk.to_string());
                }
                if let Some(exp) = login_data.get("expired_at").and_then(Value::as_i64) {
                    saved_cookies.insert("psrf_access_token_expiresAt".to_string(), exp.to_string());
                }

                let last_4 = if uin_str.len() >= 4 {
                    &uin_str[uin_str.len() - 4..]
                } else {
                    &uin_str
                };

                return Ok(QmQrCheckResponse {
                    code: 200,
                    status: 4,
                    nickname: Some(format!("微信用户_{last_4}")),
                    avatar_url: None,
                    cookies: Some(saved_cookies),
                });
            }

            Ok(QmQrCheckResponse {
                code: 200,
                status: 1,
                nickname: None,
                avatar_url: None,
                cookies: None,
            })
        } else {
            // QQ 扫码
            let ptqrtoken = hash33(key, 0);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default();
            let query = format!(
                "u1=https%3A%2F%2Fgraph.qq.com%2Foauth2.0%2Flogin_jump&ptqrtoken={ptqrtoken}&ptredirect=0&h=1&t=1&g=1&from_ui=1&ptlang=2052&action=0-0-{now}&js_ver=20102616&js_type=1&pt_uistyle=40&aid=716027609&daid=383&pt_3rd_aid=100497308&has_onekey=1"
            );
            let check_url = format!("https://ssl.ptlogin2.qq.com/ptqrlogin?{query}");

            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|e| QqkgError::Upstream(format!("Reqwest client error: {e}")))?;

            let res = client
                .get(&check_url)
                .header(reqwest::header::REFERER, "https://xui.ptlogin2.qq.com/")
                .header(reqwest::header::COOKIE, format!("qrsig={key};"))
                .header(reqwest::header::USER_AGENT, WEB_UA)
                .send()
                .await
                .map_err(|e| QqkgError::Upstream(format!("轮询 QQ 扫码状态失败: {e}")))?;

            let mut initial_cookies = HashMap::new();
            initial_cookies.insert("qrsig".to_string(), key.to_string());
            extract_cookies(&res, &mut initial_cookies);

            let text = res.text().await.unwrap_or_default();
            let re_cb = Regex::new(r"ptuiCB\((.*?)\)").unwrap();
            let Some(caps) = re_cb.captures(&text) else {
                return Ok(QmQrCheckResponse {
                    code: 200,
                    status: 1,
                    nickname: None,
                    avatar_url: None,
                    cookies: None,
                });
            };

            let args_raw = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let re_arg = Regex::new(r"'((?:\\.|[^'])*)'").unwrap();
            let args: Vec<String> = re_arg
                .captures_iter(args_raw)
                .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
                .collect();

            let status_code = args.get(0).map(String::as_str).unwrap_or("");
            let nickname = args.get(5).cloned().filter(|s| !s.is_empty());

            if status_code == "65" {
                return Ok(QmQrCheckResponse {
                    code: 200,
                    status: 0, // 已失效
                    nickname: None,
                    avatar_url: None,
                    cookies: None,
                });
            }
            if status_code == "67" {
                return Ok(QmQrCheckResponse {
                    code: 200,
                    status: 2, // 正在验证中
                    nickname,
                    avatar_url: None,
                    cookies: None,
                });
            }

            if status_code == "0" {
                let jump_url = args.get(2).map(String::as_str).unwrap_or("");
                if !jump_url.starts_with("http") {
                    return Err(QqkgError::BadResponse(format!("无效的跳转链接: {jump_url}")));
                }

                // 请求 check_sig 校验跳转并建立会话 Cookie
                let check_sig_res = client
                    .get(jump_url)
                    .header(reqwest::header::REFERER, "https://xui.ptlogin2.qq.com/")
                    .header(reqwest::header::COOKIE, stringify_cookies(&initial_cookies))
                    .header(reqwest::header::USER_AGENT, WEB_UA)
                    .send()
                    .await
                    .map_err(|e| QqkgError::Upstream(format!("check_sig 请求失败: {e}")))?;

                let mut session_cookies = initial_cookies.clone();
                extract_cookies(&check_sig_res, &mut session_cookies);

                let p_skey = session_cookies
                    .get("p_skey")
                    .or_else(|| session_cookies.get("p_sKey"))
                    .or_else(|| session_cookies.get("skey"))
                    .or_else(|| session_cookies.get("pskey"))
                    .cloned()
                    .ok_or_else(|| QqkgError::BadResponse("获取 p_skey 失败".to_string()))?;

                let g_tk = hash33(&p_skey, 5381);
                let auth_time = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or_default();
                let rand_ui = format!(
                    "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                    (auth_time & 0xffffffff) as u32,
                    ((auth_time >> 32) & 0xffff) as u16,
                    0x4000 | (((auth_time >> 48) & 0x0fff) as u16),
                    0x8000 | (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos() & 0x3fff) as u16,
                    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() & 0xffffffffffff
                );

                let auth_body = [
                    ("response_type", "code".into()),
                    ("client_id", "100497308".into()),
                    ("redirect_uri", "https://y.qq.com/portal/wx_redirect.html?login_type=1&surl=https://y.qq.com/".into()),
                    ("scope", "get_user_info,get_app_friends".into()),
                    ("state", "state".into()),
                    ("switch", "".into()),
                    ("from_ptlogin", "1".into()),
                    ("src", "1".into()),
                    ("update_auth", "1".into()),
                    ("openapi", "1010_1030".into()),
                    ("g_tk", g_tk.to_string()),
                    ("auth_time", auth_time.to_string()),
                    ("ui", rand_ui),
                ];

                let form_str = auth_body
                    .iter()
                    .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
                    .collect::<Vec<_>>()
                    .join("&");

                let auth_res = client
                    .post("https://graph.qq.com/oauth2.0/authorize")
                    .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(reqwest::header::REFERER, "https://xui.ptlogin2.qq.com/")
                    .header(reqwest::header::COOKIE, stringify_cookies(&session_cookies))
                    .header(reqwest::header::USER_AGENT, WEB_UA)
                    .body(form_str)
                    .send()
                    .await
                    .map_err(|e| QqkgError::Upstream(format!("authorize 请求失败: {e}")))?;

                let location = auth_res
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default();

                let re_code = Regex::new(r"(?:\?|&)code=([^&]+)").unwrap();
                let code = re_code
                    .captures(location)
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str())
                    .ok_or_else(|| QqkgError::BadResponse(format!("获取 QQ 授权 code 失败, location={location}")))?;

                // QQLogin 换取音乐凭据
                let fcg_body = json!({
                    "comm": { "tmeLoginType": 2 },
                    "req_0": {
                        "module": "QQConnectLogin.LoginServer",
                        "method": "QQLogin",
                        "param": { "code": code }
                    }
                });

                let client_http = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .unwrap_or_default();

                let fcg_res = client_http
                    .post(crate::qqmusic::QM_API_URL)
                    .header(reqwest::header::USER_AGENT, WEB_UA)
                    .header(reqwest::header::REFERER, "https://y.qq.com")
                    .json(&fcg_body)
                    .send()
                    .await
                    .map_err(|e| QqkgError::Upstream(format!("QM QQLogin HTTP error: {e}")))?;

                let fcg_val: Value = fcg_res
                    .json()
                    .await
                    .map_err(|e| QqkgError::BadResponse(format!("QM QQLogin JSON error: {e}")))?;

                let login_data = fcg_val
                    .get("req_0")
                    .and_then(|r| r.get("data"))
                    .cloned()
                    .unwrap_or_default();

                let re_uin = Regex::new(r"(?:\?|&)uin=([^&]+)").unwrap();
                let jump_uin = re_uin
                    .captures(jump_url)
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();

                let musicid = login_data
                    .get("musicid")
                    .and_then(|v| v.as_i64().map(|n| n.to_string()).or_else(|| v.as_str().map(ToString::to_string)))
                    .or_else(|| login_data.get("str_musicid").and_then(Value::as_str).map(ToString::to_string))
                    .unwrap_or(jump_uin);
                let uin_str = musicid.strip_prefix('o').unwrap_or(&musicid).to_string();

                let musickey = login_data
                    .get("musickey")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();

                let mut saved_cookies = HashMap::new();
                saved_cookies.insert("uin".to_string(), uin_str.clone());
                saved_cookies.insert("qm_keyst".to_string(), musickey.clone());
                saved_cookies.insert("qqmusic_key".to_string(), musickey);
                saved_cookies.insert("tmeLoginType".to_string(), "2".to_string());

                if let Some(euin) = login_data.get("encryptUin").and_then(Value::as_str) {
                    saved_cookies.insert("euin".to_string(), euin.to_string());
                }
                if let Some(openid) = login_data.get("openid").and_then(Value::as_str) {
                    saved_cookies.insert("psrf_qqopenid".to_string(), openid.to_string());
                }
                if let Some(unionid) = login_data.get("unionid").and_then(Value::as_str) {
                    saved_cookies.insert("psrf_qqunionid".to_string(), unionid.to_string());
                }
                if let Some(rt) = login_data.get("refresh_token").and_then(Value::as_str) {
                    saved_cookies.insert("psrf_qqrefresh_token".to_string(), rt.to_string());
                }
                if let Some(at) = login_data.get("access_token").and_then(Value::as_str) {
                    saved_cookies.insert("psrf_qqaccess_token".to_string(), at.to_string());
                }
                if let Some(rk) = login_data.get("refresh_key").and_then(Value::as_str) {
                    saved_cookies.insert("qm_refresh_key".to_string(), rk.to_string());
                }
                if let Some(exp) = login_data.get("expired_at").and_then(Value::as_i64) {
                    saved_cookies.insert("psrf_access_token_expiresAt".to_string(), exp.to_string());
                }

                let last_4 = if uin_str.len() >= 4 {
                    &uin_str[uin_str.len() - 4..]
                } else {
                    &uin_str
                };

                let disp_nickname = nickname.unwrap_or_else(|| format!("QQ用户_{last_4}"));
                let avatar = format!("https://q.qlogo.cn/headimg_dl?dst_uin={uin_str}&spec=100");

                return Ok(QmQrCheckResponse {
                    code: 200,
                    status: 4,
                    nickname: Some(disp_nickname),
                    avatar_url: Some(avatar),
                    cookies: Some(saved_cookies),
                });
            }

            Ok(QmQrCheckResponse {
                code: 200,
                status: 1,
                nickname: None,
                avatar_url: None,
                cookies: None,
            })
        }
    }
}
