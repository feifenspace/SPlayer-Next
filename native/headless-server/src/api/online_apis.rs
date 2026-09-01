//! 纯 Rust 在线音乐平台 API 代理与流媒体转发模块
//!
//! 支持网易云音乐（基于本地 ncm-api-rs）、QQ音乐与酷狗音乐的原生调用，完全不依赖 Node.js。

use std::collections::HashMap;
use std::sync::LazyLock;

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use ncm_api_rs::server::build_app;
use ncm_api_rs::ApiClient;
use qqkg_api::{KugouClient, QqmusicClient, SearchParams};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tower::ServiceExt;

static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .pool_max_idle_per_host(10)
        .build()
        .unwrap_or_default()
});

/// 单例全局 ncm-api-rs Router
static NCM_ROUTER: LazyLock<axum::Router> = LazyLock::new(|| {
    let client = ApiClient::new(None);
    build_app(client)
});

/// 统一 API 调用请求体
#[derive(Debug, Deserialize)]
pub struct ApiCallRequest {
    pub platform: String,
    pub name: String,
    #[serde(default)]
    pub params: HashMap<String, Value>,
}

/// 统一 API 响应格式（对齐 Electron IPC 返回）
#[derive(Debug, Serialize)]
pub struct ApiCallResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ApiCallResponse {
    pub fn ok_body(status: u16, body: Value) -> Self {
        Self {
            ok: true,
            error: None,
            status: Some(status),
            body: Some(body),
            data: None,
        }
    }

    pub fn ok_data(data: Value) -> Self {
        Self {
            ok: true,
            error: None,
            status: Some(200),
            body: Some(data.clone()),
            data: Some(data),
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            status: Some(500),
            body: None,
            data: None,
        }
    }
}

/// 特殊不转换下划线为斜杠的网易云路由
const NCM_SPECIAL_ROUTES: &[&str] = &[
    "daily_signin",
    "fm_trash",
    "personal_fm",
    "personal_fm_mode",
];

fn ncm_method_to_route(method: &str) -> String {
    match method {
        "song_url" => "/song/url/v1".to_string(),
        "song_download_url" => "/song/download/url/v1".to_string(),
        "playmode_intelligence" => "/playmode/intelligence/list".to_string(),
        _ if NCM_SPECIAL_ROUTES.contains(&method) => format!("/{}", method),
        _ => format!("/{}", method.replace('_', "/")),
    }
}

/// 解析 Cookie 字符串为键值对 HashMap（如 "MUSIC_U=xxx; __csrf=yyy"）
pub fn parse_cookie_str(s: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for part in s.split(';') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            let k = k.trim();
            let v = v.trim();
            if !k.is_empty() {
                map.insert(k.to_string(), v.to_string());
            }
        }
    }
    map
}

/// 格式化键值对 HashMap 为标准 Cookie 请求头字符串
pub fn format_cookie_str(map: &HashMap<String, String>) -> String {
    map.iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("; ")
}

/// 解析 Set-Cookie 响应头列表，提取键值对
pub fn parse_set_cookies(headers: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for header_val in headers {
        if let Some(first) = header_val.split(';').next() {
            if let Some((k, v)) = first.trim().split_once('=') {
                let k = k.trim();
                let v = v.trim();
                if !k.is_empty() {
                    map.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
    map
}

/// 统一调用在线音源 API
pub async fn dispatch_api_call(
    req: ApiCallRequest,
    db: &parking_lot::Mutex<rusqlite::Connection>,
) -> ApiCallResponse {
    // 统一处理会话 Cookie 的存取与清除
    match req.name.as_str() {
        "set_cookie" | "setCookie" => {
            let mut cookies_map = HashMap::new();
            if let Some(c_val) = req.params.get("cookie") {
                if let Some(s) = c_val.as_str() {
                    cookies_map = parse_cookie_str(s);
                } else if let Some(map) = c_val.as_object() {
                    for (k, v) in map {
                        if let Some(s) = v.as_str() {
                            cookies_map.insert(k.clone(), s.to_string());
                        }
                    }
                }
            }
            let conn = db.lock();
            let _ = crate::db::save_account_cookies(&conn, &req.platform, &cookies_map);
            return ApiCallResponse::ok_data(json!("ok"));
        }
        "get_cookie" | "getCookie" => {
            let conn = db.lock();
            let cookies_map = crate::db::get_account_cookies(&conn, &req.platform);
            let cookie_str = format_cookie_str(&cookies_map);
            return ApiCallResponse::ok_body(200, json!({ "cookie": cookie_str }));
        }
        "clear_session" | "clearSession" => {
            let conn = db.lock();
            let _ = crate::db::clear_account_cookies(&conn, &req.platform);
            return ApiCallResponse::ok_data(json!("ok"));
        }
        _ => {}
    }

    match req.platform.as_str() {
        "netease" => call_netease(&req.name, req.params, db).await,
        "qqmusic" => call_qqmusic(db, &req.name, req.params).await,
        "kugou" => call_kugou(db, &req.name, req.params).await,
        "qobuz" => call_qobuz(db, &req.name, req.params).await,
        "tidal" => call_tidal(db, &req.name, req.params).await,
        other => ApiCallResponse::err(format!("Unsupported platform: {}", other)),
    }
}

// -------------------------------------------------------------------
// 网易云音乐 (ncm-api-rs)
// -------------------------------------------------------------------

async fn call_netease(
    name: &str,
    mut params: HashMap<String, Value>,
    db: &parking_lot::Mutex<rusqlite::Connection>,
) -> ApiCallResponse {
    let route = ncm_method_to_route(name);
    let app = NCM_ROUTER.clone();

    // 从 DB 读取已持久化的 cookies，并与 params 中透传的 cookie 合并
    let mut stored_cookies = {
        let conn = db.lock();
        crate::db::get_account_cookies(&conn, "netease")
    };
    if let Some(p_cookie) = params.get("cookie") {
        if let Some(s) = p_cookie.as_str() {
            for (k, v) in parse_cookie_str(s) {
                stored_cookies.insert(k, v);
            }
        }
    }

    let cookie_header_str = format_cookie_str(&stored_cookies);
    if !cookie_header_str.is_empty() {
        params.insert("cookie".to_string(), json!(cookie_header_str));
    }

    let json_body = serde_json::to_vec(&params).unwrap_or_default();
    let mut http_req_builder = Request::builder()
        .uri(&route)
        .method(Method::POST)
        .header(header::CONTENT_TYPE, "application/json");

    if !cookie_header_str.is_empty() {
        if let Ok(hv) = header::HeaderValue::from_str(&cookie_header_str) {
            http_req_builder = http_req_builder.header(header::COOKIE, hv);
        }
    }

    let http_req = match http_req_builder.body(Body::from(json_body)) {
        Ok(r) => r,
        Err(e) => return ApiCallResponse::err(format!("Failed to build NCM request: {}", e)),
    };

    match app.oneshot(http_req).await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let mut set_cookie_headers = Vec::new();
            for val in resp.headers().get_all(header::SET_COOKIE) {
                if let Ok(s) = val.to_str() {
                    set_cookie_headers.push(s.to_string());
                }
            }

            let bytes = match axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024).await {
                Ok(b) => b,
                Err(e) => return ApiCallResponse::err(format!("Failed to read NCM body: {}", e)),
            };
            let mut body: Value = serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| json!({ "raw": String::from_utf8_lossy(&bytes).to_string() }));

            // 登录态变更：logout 清除 session，其他响应若携带 Set-Cookie 则持久化
            if name == "logout" {
                let conn = db.lock();
                let _ = crate::db::clear_account_cookies(&conn, "netease");
            } else if !set_cookie_headers.is_empty() {
                let new_cookies = parse_set_cookies(&set_cookie_headers);
                if !new_cookies.is_empty() {
                    let mut updated = stored_cookies.clone();
                    for (k, v) in new_cookies {
                        updated.insert(k, v);
                    }
                    let conn = db.lock();
                    let _ = crate::db::save_account_cookies(&conn, "netease", &updated);
                }
            }

            // 兼容性适配：login_qr_key 返回的 unikey 包在顶层，前端期望在 data.unikey 中
            let unikey_opt = body.get("unikey").cloned();
            if let Some(unikey) = unikey_opt {
                if body.get("data").is_none() {
                    if let Some(map) = body.as_object_mut() {
                        map.insert("data".to_string(), json!({ "unikey": unikey, "code": 200 }));
                    }
                }
            }

            // 将 Set-Cookie 原始值注入 body.cookie，方便前端提取 token
            if !set_cookie_headers.is_empty() {
                if let Some(map) = body.as_object_mut() {
                    if map.get("cookie").is_none() {
                        map.insert("cookie".to_string(), json!(set_cookie_headers.join("; ")));
                    }
                }
            }

            ApiCallResponse::ok_body(status, body)
        }
        Err(e) => ApiCallResponse::err(format!("NCM router error: {}", e)),
    }
}

// -------------------------------------------------------------------
// QQ 音乐 (Pure Rust)
// -------------------------------------------------------------------

const QM_API_URL: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";

fn get_qm_common_params() -> Value {
    json!({
        "ct": 19,
        "cv": 1873,
        "v": "1003006",
        "tmeAppID": "qqmusic",
        "nettype": "NETWORK_WIFI",
        "openudid": "0000000000000000",
        "uid": "0",
        "wid": "0",
        "qimei": "000000000000000000000000000000000000",
        "qimei36": "000000000000000000000000000000000000",
    })
}

/// qqkg-api crate 调用结果转 ApiCallResponse
fn to_qqkg_resp(r: Result<Value, qqkg_api::QqkgError>) -> ApiCallResponse {
    match r {
        Ok(v) => ApiCallResponse::ok_data(v),
        Err(e) => ApiCallResponse::err(format!("QQKG API error: {e}")),
    }
}

/// 从数据库读取平台登录态 Cookie
fn load_platform_cookies(
    db: &parking_lot::Mutex<rusqlite::Connection>,
    platform: &str,
) -> HashMap<String, String> {
    let conn = db.lock();
    crate::db::get_account_cookies(&conn, platform)
}

async fn call_qqmusic(
    db: &parking_lot::Mutex<rusqlite::Connection>,
    name: &str,
    params: HashMap<String, Value>,
) -> ApiCallResponse {
    match name {
        "search" => {
            let client = QqmusicClient::new(load_platform_cookies(db, "qqmusic"));
            to_qqkg_resp(client.search(&SearchParams::from_map(&params)).await)
        }
        "user_detail" => {
            let client = QqmusicClient::new(load_platform_cookies(db, "qqmusic"));
            match client.user_detail().await {
                Ok(resp) => ApiCallResponse::ok_data(serde_json::to_value(resp).unwrap_or_default()),
                Err(e) => ApiCallResponse::err(format!("QM user_detail error: {e}")),
            }
        }
        "song_url" => {
            let client = QqmusicClient::new(load_platform_cookies(db, "qqmusic"));
            to_qqkg_resp(client.song_url(&params).await)
        }
        "album" | "album_detail" | "album_songs" => {
            let client = QqmusicClient::new(load_platform_cookies(db, "qqmusic"));
            to_qqkg_resp(client.album(&params).await)
        }
        "artist" | "artist_detail" | "artist_songs" => {
            let client = QqmusicClient::new(load_platform_cookies(db, "qqmusic"));
            to_qqkg_resp(client.artist(&params).await)
        }
        "song_list" | "playlist" => {
            let client = QqmusicClient::new(load_platform_cookies(db, "qqmusic"));
            to_qqkg_resp(client.playlist(&params).await)
        }
        "leaderboard" | "toplist" => {
            let client = QqmusicClient::new(load_platform_cookies(db, "qqmusic"));
            to_qqkg_resp(client.leaderboard(&params).await)
        }
        "hot_search" => {
            let client = QqmusicClient::new(load_platform_cookies(db, "qqmusic"));
            to_qqkg_resp(client.hot_search().await)
        }
        "login_qr_key" => {
            let qr_type = params.get("type").and_then(Value::as_str).unwrap_or("qq");
            let client = QqmusicClient::new(load_platform_cookies(db, "qqmusic"));
            match client.login_qr_key(qr_type).await {
                Ok(resp) => ApiCallResponse::ok_data(serde_json::to_value(resp).unwrap_or_default()),
                Err(e) => ApiCallResponse::err(format!("QM login_qr_key error: {e}")),
            }
        }
        "login_qr_check" => {
            let key = params.get("key").and_then(Value::as_str).unwrap_or("");
            let qr_type = params.get("type").and_then(Value::as_str).unwrap_or("qq");
            let client = QqmusicClient::new(load_platform_cookies(db, "qqmusic"));
            match client.login_qr_check(key, qr_type).await {
                Ok(check_resp) => {
                    // 当扫码成功 (status == 4) 且带有 cookies 时，将返回的凭据持久化到 account_sessions
                    if check_resp.status == 4 {
                        if let Some(new_cookies) = &check_resp.cookies {
                            let mut cookies_map = load_platform_cookies(db, "qqmusic");
                            for (k, v) in new_cookies {
                                cookies_map.insert(k.clone(), v.clone());
                            }
                            if let Some(nickname) = &check_resp.nickname {
                                cookies_map.insert("nickname".to_string(), nickname.clone());
                            }
                            if let Some(avatar) = &check_resp.avatar_url {
                                cookies_map.insert("avatar".to_string(), avatar.clone());
                            }
                            let conn = db.lock();
                            let _ = crate::db::save_account_cookies(&conn, "qqmusic", &cookies_map);
                        }
                    }
                    ApiCallResponse::ok_data(serde_json::to_value(check_resp).unwrap_or_default())
                }
                Err(e) => ApiCallResponse::err(format!("QM login_qr_check error: {e}")),
            }
        }
        "lyric" => {
            let client = QqmusicClient::new(load_platform_cookies(db, "qqmusic"));
            to_qqkg_resp(client.lyric(&params).await)
        }


        _ => {
            // 通用 fcg 请求封装
            let payload = json!({
                "comm": get_qm_common_params(),
                "request": {
                    "module": params.get("module").and_then(|v| v.as_str()).unwrap_or(""),
                    "method": params.get("method").and_then(|v| v.as_str()).unwrap_or(""),
                    "param": params.get("param").cloned().unwrap_or(Value::Object(Default::default()))
                }
            });

            match HTTP_CLIENT
                .post(QM_API_URL)
                .header(header::REFERER, "https://y.qq.com")
                .header(
                    header::USER_AGENT,
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
                )
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) => {
                    let json_val: Value = resp.json().await.unwrap_or_default();
                    ApiCallResponse::ok_data(json_val)
                }
                Err(e) => ApiCallResponse::err(format!("QM request error: {}", e)),
            }
        }
    }
}

// -------------------------------------------------------------------
// 酷狗音乐 (Pure Rust)
// -------------------------------------------------------------------

async fn call_kugou(
    db: &parking_lot::Mutex<rusqlite::Connection>,
    name: &str,
    params: HashMap<String, Value>,
) -> ApiCallResponse {
    match name {
        "search" => {
            let client = KugouClient::new(load_platform_cookies(db, "kugou"));
            to_qqkg_resp(client.search(&SearchParams::from_map(&params)).await)
        }
        "login_qr_key" => {
            let client = KugouClient::new(load_platform_cookies(db, "kugou"));
            match client.login_qr_key().await {
                Ok(resp) => ApiCallResponse::ok_data(serde_json::to_value(resp).unwrap_or_default()),
                Err(e) => ApiCallResponse::err(format!("Kugou login_qr_key error: {e}")),
            }
        }
        "login_qr_check" => {
            let key = params.get("key").and_then(Value::as_str).unwrap_or("");
            let client = KugouClient::new(load_platform_cookies(db, "kugou"));
            match client.login_qr_check(key).await {
                Ok(check_resp) => {
                    // 当扫码成功 (status == 4) 时，将返回的凭据持久化到 account_sessions
                    if check_resp.status == 4 {
                        let mut cookies_map = load_platform_cookies(db, "kugou");
                        if let Some(token) = &check_resp.token {
                            cookies_map.insert("token".to_string(), token.clone());
                        }
                        if let Some(userid) = &check_resp.userid {
                            cookies_map.insert("userid".to_string(), userid.clone());
                        }
                        if let Some(vip_token) = &check_resp.vip_token {
                            cookies_map.insert("vip_token".to_string(), vip_token.clone());
                        }
                        if let Some(vip_type) = &check_resp.vip_type {
                            cookies_map.insert("vip_type".to_string(), vip_type.clone());
                        }
                        if let Some(nickname) = &check_resp.nickname {
                            cookies_map.insert("nickname".to_string(), nickname.clone());
                        }
                        if let Some(avatar) = &check_resp.avatar_url {
                            cookies_map.insert("avatar".to_string(), avatar.clone());
                        }
                        let conn = db.lock();
                        let _ = crate::db::save_account_cookies(&conn, "kugou", &cookies_map);
                    }
                    ApiCallResponse::ok_data(serde_json::to_value(check_resp).unwrap_or_default())
                }
                Err(e) => ApiCallResponse::err(format!("Kugou login_qr_check error: {e}")),
            }
        }
        "user_detail" => {
            let client = KugouClient::new(load_platform_cookies(db, "kugou"));
            match client.user_detail().await {
                Ok(resp) => {
                    // 若已登录且有新资料，同步更新数据库缓存
                    if resp.logged_in {
                        if let Some(profile) = &resp.profile {
                            let mut cookies_map = load_platform_cookies(db, "kugou");
                            if !profile.nickname.is_empty() {
                                cookies_map.insert("nickname".to_string(), profile.nickname.clone());
                            }
                            if !profile.avatar_url.is_empty() {
                                cookies_map.insert("avatar".to_string(), profile.avatar_url.clone());
                            }
                            cookies_map.insert("vip_type".to_string(), profile.vip_level.to_string());
                            let conn = db.lock();
                            let _ = crate::db::save_account_cookies(&conn, "kugou", &cookies_map);
                        }
                    }
                    ApiCallResponse::ok_data(serde_json::to_value(resp).unwrap_or_default())
                }
                Err(e) => ApiCallResponse::err(format!("Kugou user_detail error: {e}")),
            }
        }
        "song_url" => {
            let client = KugouClient::new(load_platform_cookies(db, "kugou"));
            to_qqkg_resp(client.song_url(&params).await)
        }
        "playlist" | "song_list" => {
            let client = KugouClient::new(load_platform_cookies(db, "kugou"));
            to_qqkg_resp(client.playlist(&params).await)
        }
        "album" => {
            let client = KugouClient::new(load_platform_cookies(db, "kugou"));
            to_qqkg_resp(client.album(&params).await)
        }
        "artist" => {
            let client = KugouClient::new(load_platform_cookies(db, "kugou"));
            to_qqkg_resp(client.artist(&params).await)
        }
        "lyric" => {
            let client = KugouClient::new(load_platform_cookies(db, "kugou"));
            to_qqkg_resp(client.lyric(&params).await)
        }

        other => ApiCallResponse::err(format!("Unsupported Kugou API: {}", other)),
    }
}

// -------------------------------------------------------------------
// Qobuz (Pure Rust)
// -------------------------------------------------------------------

fn to_streaming_resp(r: Result<Value, streaming_api::StreamingError>) -> ApiCallResponse {
    match r {
        Ok(v) => ApiCallResponse::ok_data(v),
        Err(e) => ApiCallResponse::err(format!("Streaming API error: {e}")),
    }
}

async fn call_qobuz(
    db: &parking_lot::Mutex<rusqlite::Connection>,
    name: &str,
    params: HashMap<String, Value>,
) -> ApiCallResponse {
    let cookies = load_platform_cookies(db, "qobuz");
    let user_auth_token = cookies.get("user_auth_token").cloned();
    let user_id = cookies.get("user_id").cloned();
    let client = streaming_api::QobuzClient::new(user_auth_token, user_id);

    match name {
        "auth_login" | "login" => {
            match client.auth_login(&params).await {
                Ok(resp) => {
                    if let (Some(tok), Some(uid)) = (
                        resp.get("user_auth_token").and_then(Value::as_str),
                        resp.get("user_id").and_then(Value::as_str),
                    ) {
                        let mut new_cookies = HashMap::new();
                        new_cookies.insert("user_auth_token".to_string(), tok.to_string());
                        new_cookies.insert("user_id".to_string(), uid.to_string());
                        let conn = db.lock();
                        let _ = crate::db::save_account_cookies(&conn, "qobuz", &new_cookies);
                    }
                    ApiCallResponse::ok_data(resp)
                }
                Err(e) => ApiCallResponse::err(format!("Qobuz login error: {e}")),
            }
        }
        "auth_status" | "status" | "user_detail" => {
            to_streaming_resp(client.auth_status().await)
        }
        "auth_logout" | "logout" => {
            {
                let conn = db.lock();
                let _ = crate::db::clear_account_cookies(&conn, "qobuz");
            }
            to_streaming_resp(client.auth_logout().await)
        }
        "catalog_search" | "search" => {
            to_streaming_resp(client.catalog_search(&params).await)
        }
        "track_getFileUrl" | "song_url" => {
            to_streaming_resp(client.track_get_file_url(&params).await)
        }
        "album_get" | "album" => {
            to_streaming_resp(client.album_get(&params).await)
        }
        "artist_get" | "artist" => {
            to_streaming_resp(client.artist_get(&params).await)
        }
        "artist_getReleasesList" => {
            to_streaming_resp(client.artist_get_releases_list(&params).await)
        }
        "playlist_get" | "playlist" => {
            to_streaming_resp(client.playlist_get(&params).await)
        }
        "user_getFavorites" | "favorites" => {
            to_streaming_resp(client.user_get_favorites(&params).await)
        }
        "favorite_create" => {
            to_streaming_resp(client.favorite_create(&params).await)
        }
        "favorite_delete" => {
            to_streaming_resp(client.favorite_delete(&params).await)
        }
        other => ApiCallResponse::err(format!("Unsupported Qobuz API: {}", other)),
    }
}

// -------------------------------------------------------------------
// TIDAL (Pure Rust)
// -------------------------------------------------------------------

async fn call_tidal(
    db: &parking_lot::Mutex<rusqlite::Connection>,
    name: &str,
    params: HashMap<String, Value>,
) -> ApiCallResponse {
    let cookies = load_platform_cookies(db, "tidal");
    let access_token = cookies.get("access_token").cloned();
    let refresh_token = cookies.get("refresh_token").cloned();
    let user_id = cookies.get("user_id").cloned();
    let country_code = cookies.get("country_code").cloned();
    let client_id = cookies.get("client_id").cloned();
    let client_secret = cookies.get("client_secret").cloned();

    let client = streaming_api::TidalClient::new(
        access_token,
        refresh_token,
        user_id,
        country_code,
        client_id,
        client_secret,
    );

    match name {
        "auth_authorize" | "authorize" => {
            to_streaming_resp(client.auth_authorize(&params).await)
        }
        "auth_exchange" | "exchange" => {
            match client.auth_exchange(&params).await {
                Ok(resp) => {
                    let mut new_cookies = HashMap::new();
                    if let Some(tok) = resp.get("accessToken").and_then(Value::as_str) {
                        new_cookies.insert("access_token".to_string(), tok.to_string());
                    }
                    if let Some(rtok) = resp.get("refreshToken").and_then(Value::as_str) {
                        new_cookies.insert("refresh_token".to_string(), rtok.to_string());
                    }
                    if let Some(uid) = resp.get("userId").and_then(Value::as_str) {
                        new_cookies.insert("user_id".to_string(), uid.to_string());
                    }
                    if let Some(cc) = resp.get("countryCode").and_then(Value::as_str) {
                        new_cookies.insert("country_code".to_string(), cc.to_string());
                    }
                    if let Some(cid) = resp.get("clientId").and_then(Value::as_str) {
                        new_cookies.insert("client_id".to_string(), cid.to_string());
                    }
                    let conn = db.lock();
                    let _ = crate::db::save_account_cookies(&conn, "tidal", &new_cookies);
                    ApiCallResponse::ok_data(resp)
                }
                Err(e) => ApiCallResponse::err(format!("TIDAL exchange error: {e}")),
            }
        }
        "auth_device_authorization" | "device_authorization" => {
            to_streaming_resp(client.auth_device_authorization(&params).await)
        }
        "auth_token_poll" | "token_poll" => {
            match client.auth_token_poll(&params).await {
                Ok(resp) => {
                    if resp.get("status").and_then(Value::as_str) == Some("success") {
                        let mut new_cookies = HashMap::new();
                        if let Some(tok) = resp.get("accessToken").and_then(Value::as_str) {
                            new_cookies.insert("access_token".to_string(), tok.to_string());
                        }
                        if let Some(rtok) = resp.get("refreshToken").and_then(Value::as_str) {
                            new_cookies.insert("refresh_token".to_string(), rtok.to_string());
                        }
                        if let Some(uid) = resp.get("userId").and_then(Value::as_str) {
                            new_cookies.insert("user_id".to_string(), uid.to_string());
                        }
                        if let Some(cc) = resp.get("countryCode").and_then(Value::as_str) {
                            new_cookies.insert("country_code".to_string(), cc.to_string());
                        }
                        if let Some(cid) = resp.get("clientId").and_then(Value::as_str) {
                            new_cookies.insert("client_id".to_string(), cid.to_string());
                        }
                        let conn = db.lock();
                        let _ = crate::db::save_account_cookies(&conn, "tidal", &new_cookies);
                    }
                    ApiCallResponse::ok_data(resp)
                }
                Err(e) => ApiCallResponse::err(format!("TIDAL poll error: {e}")),
            }
        }
        "auth_token_refresh" | "refresh_token" => {
            match client.auth_token_refresh(&params).await {
                Ok(resp) => {
                    if let Some(tok) = resp.get("accessToken").and_then(Value::as_str) {
                        let mut updated = cookies.clone();
                        updated.insert("access_token".to_string(), tok.to_string());
                        if let Some(rtok) = resp.get("refreshToken").and_then(Value::as_str) {
                            updated.insert("refresh_token".to_string(), rtok.to_string());
                        }
                        let conn = db.lock();
                        let _ = crate::db::save_account_cookies(&conn, "tidal", &updated);
                    }
                    ApiCallResponse::ok_data(resp)
                }
                Err(e) => ApiCallResponse::err(format!("TIDAL refresh error: {e}")),
            }
        }
        "auth_status" | "status" | "user_detail" => {
            to_streaming_resp(client.auth_status().await)
        }
        "auth_logout" | "logout" => {
            {
                let conn = db.lock();
                let _ = crate::db::clear_account_cookies(&conn, "tidal");
            }
            to_streaming_resp(client.auth_logout().await)
        }
        "search" => {
            to_streaming_resp(client.search(&params).await)
        }
        "track_getStreamUrl" | "song_url" => {
            to_streaming_resp(client.track_get_stream_url(&params).await)
        }
        "album_get" | "album" => {
            to_streaming_resp(client.album_get(&params).await)
        }
        "album_getTracks" => {
            to_streaming_resp(client.album_get_tracks(&params).await)
        }
        "artist_get" | "artist" => {
            to_streaming_resp(client.artist_get(&params).await)
        }
        "artist_getAlbums" => {
            to_streaming_resp(client.artist_get_albums(&params).await)
        }
        "artist_getTopTracks" => {
            to_streaming_resp(client.artist_get_top_tracks(&params).await)
        }
        "playlist_get" | "playlist" => {
            to_streaming_resp(client.playlist_get(&params).await)
        }
        "playlist_getTracks" => {
            to_streaming_resp(client.playlist_get_tracks(&params).await)
        }
        "user_getFavorites" | "favorites" => {
            to_streaming_resp(client.user_get_favorites(&params).await)
        }
        other => ApiCallResponse::err(format!("Unsupported TIDAL API: {}", other)),
    }
}

// -------------------------------------------------------------------
// 音频串流代理转发 (Stream Proxy with Range Support)
// -------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StreamProxyQuery {
    pub url: String,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
}

/// 音频串流代理转发，处理 Range 分片并转发 Cookie / Referer
pub async fn stream_proxy_handler(
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<StreamProxyQuery>,
) -> Response {
    let mut req_builder = HTTP_CLIENT.get(&query.url);

    if let Some(range) = headers.get(header::RANGE) {
        req_builder = req_builder.header(header::RANGE, range);
    }
    if let Some(ref_str) = query.referer {
        req_builder = req_builder.header(header::REFERER, ref_str);
    } else {
        req_builder = req_builder.header(header::REFERER, "https://y.qq.com/");
    }
    if let Some(ua) = query.user_agent {
        req_builder = req_builder.header(header::USER_AGENT, ua);
    } else {
        req_builder = req_builder.header(
            header::USER_AGENT,
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        );
    }

    match req_builder.send().await {
        Ok(upstream_resp) => {
            let status = upstream_resp.status();
            let upstream_headers = upstream_resp.headers().clone();

            let mut resp_headers = HeaderMap::new();
            if let Some(ct) = upstream_headers.get(header::CONTENT_TYPE) {
                resp_headers.insert(header::CONTENT_TYPE, ct.clone());
            } else {
                resp_headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/mpeg"));
            }
            if let Some(cl) = upstream_headers.get(header::CONTENT_LENGTH) {
                resp_headers.insert(header::CONTENT_LENGTH, cl.clone());
            }
            if let Some(cr) = upstream_headers.get(header::CONTENT_RANGE) {
                resp_headers.insert(header::CONTENT_RANGE, cr.clone());
            }
            if let Some(ar) = upstream_headers.get(header::ACCEPT_RANGES) {
                resp_headers.insert(header::ACCEPT_RANGES, ar.clone());
            } else {
                resp_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            }

            resp_headers.insert(
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_static("*"),
            );

            let stream = upstream_resp.bytes_stream();
            let body = Body::from_stream(stream);

            let status_code = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK);
            (status_code, resp_headers, body).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("Stream proxy error: {}", e),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct ImageProxyQuery {
    pub url: String,
}

pub async fn image_proxy_handler(
    axum::extract::Query(query): axum::extract::Query<ImageProxyQuery>,
) -> Response {
    if query.url.is_empty() {
        return (StatusCode::BAD_REQUEST, "Missing url parameter").into_response();
    }

    let mut req_builder = HTTP_CLIENT.get(&query.url);
    req_builder = req_builder.header(
        header::USER_AGENT,
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
    );

    match req_builder.send().await {
        Ok(upstream_resp) => {
            let status = upstream_resp.status();
            let upstream_headers = upstream_resp.headers().clone();

            let mut resp_headers = HeaderMap::new();
            if let Some(ct) = upstream_headers.get(header::CONTENT_TYPE) {
                resp_headers.insert(header::CONTENT_TYPE, ct.clone());
            } else {
                resp_headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
            }
            if let Some(cl) = upstream_headers.get(header::CONTENT_LENGTH) {
                resp_headers.insert(header::CONTENT_LENGTH, cl.clone());
            }
            resp_headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=604800"),
            );
            resp_headers.insert(
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_static("*"),
            );

            let stream = upstream_resp.bytes_stream();
            let body = Body::from_stream(stream);

            let status_code = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK);
            (status_code, resp_headers, body).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("Image proxy error: {}", e),
        )
            .into_response(),
    }
}
