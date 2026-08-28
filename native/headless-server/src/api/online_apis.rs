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
            body: None,
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

/// 统一调用在线音源 API
pub async fn dispatch_api_call(req: ApiCallRequest) -> ApiCallResponse {
    match req.platform.as_str() {
        "netease" => call_netease(&req.name, req.params).await,
        "qqmusic" => call_qqmusic(&req.name, req.params).await,
        "kugou" => call_kugou(&req.name, req.params).await,
        other => ApiCallResponse::err(format!("Unsupported platform: {}", other)),
    }
}

// -------------------------------------------------------------------
// 网易云音乐 (ncm-api-rs)
// -------------------------------------------------------------------

async fn call_netease(name: &str, params: HashMap<String, Value>) -> ApiCallResponse {
    let route = ncm_method_to_route(name);
    let app = NCM_ROUTER.clone();

    let json_body = serde_json::to_vec(&params).unwrap_or_default();
    let http_req = match Request::builder()
        .uri(&route)
        .method(Method::POST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json_body))
    {
        Ok(r) => r,
        Err(e) => return ApiCallResponse::err(format!("Failed to build NCM request: {}", e)),
    };

    match app.oneshot(http_req).await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let mut cookies = Vec::new();
            for val in resp.headers().get_all(header::SET_COOKIE) {
                if let Ok(s) = val.to_str() {
                    cookies.push(s.to_string());
                }
            }

            let bytes = match axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024).await {
                Ok(b) => b,
                Err(e) => return ApiCallResponse::err(format!("Failed to read NCM body: {}", e)),
            };
            let mut body: Value = serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| json!({ "raw": String::from_utf8_lossy(&bytes).to_string() }));

            // 兼容性适配：若返回中包含 unikey 但缺少 data.unikey，则注入 data 包装
            let unikey_opt = body.get("unikey").cloned();
            if let Some(unikey) = unikey_opt {
                if body.get("data").is_none() {
                    if let Some(map) = body.as_object_mut() {
                        map.insert("data".to_string(), json!({ "unikey": unikey, "code": 200 }));
                    }
                }
            }

            // 如果有 set-cookie，注入到 body.cookie 方便前端读取
            if !cookies.is_empty() {
                if let Some(map) = body.as_object_mut() {
                    if map.get("cookie").is_none() {
                        map.insert("cookie".to_string(), json!(cookies.join("; ")));
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

async fn call_qqmusic(name: &str, params: HashMap<String, Value>) -> ApiCallResponse {
    match name {
        "search" => {
            let keyword = params
                .get("keyword")
                .or_else(|| params.get("keywords"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let page = params.get("page").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
            let page_size = params
                .get("pageSize")
                .or_else(|| params.get("limit"))
                .and_then(|v| v.as_u64())
                .unwrap_or(20) as u32;

            let payload = json!({
                "comm": get_qm_common_params(),
                "request": {
                    "module": "music.search.SearchCgiService",
                    "method": "DoSearchForQQMusicDesktop",
                    "param": {
                        "query": keyword,
                        "page_num": page,
                        "num_per_page": page_size,
                        "search_type": 0
                    }
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
                    let data = json_val
                        .get("request")
                        .and_then(|r| r.get("data"))
                        .cloned()
                        .unwrap_or(json_val);
                    ApiCallResponse::ok_data(data)
                }
                Err(e) => ApiCallResponse::err(format!("QM search error: {}", e)),
            }
        }
        "lyric" => {
            let song_mid = params
                .get("songmid")
                .or_else(|| params.get("mid"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let url = format!(
                "https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg?songmid={}&format=json&nobase64=1",
                urlencoding::encode(song_mid)
            );

            match HTTP_CLIENT
                .get(&url)
                .header(header::REFERER, "https://y.qq.com")
                .header(
                    header::USER_AGENT,
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
                )
                .send()
                .await
            {
                Ok(resp) => {
                    let text = resp.text().await.unwrap_or_default();
                    let cleaned = text
                        .trim_start_matches("MusicJsonCallback(")
                        .trim_end_matches(')');
                    let json_val: Value =
                        serde_json::from_str(cleaned).unwrap_or_else(|_| json!({ "lyric": text }));
                    ApiCallResponse::ok_data(json_val)
                }
                Err(e) => ApiCallResponse::err(format!("QM lyric error: {}", e)),
            }
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

async fn call_kugou(name: &str, params: HashMap<String, Value>) -> ApiCallResponse {
    match name {
        "search" => {
            let keyword = params
                .get("keyword")
                .or_else(|| params.get("keywords"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let page = params.get("page").and_then(|v| v.as_u64()).unwrap_or(1);
            let page_size = params
                .get("pageSize")
                .or_else(|| params.get("limit"))
                .and_then(|v| v.as_u64())
                .unwrap_or(20);

            let url = format!(
                "http://mobilecdn.kugou.com/api/v3/search/song?format=json&keyword={}&page={}&pagesize={}&showtype=1",
                urlencoding::encode(keyword),
                page,
                page_size
            );

            match HTTP_CLIENT
                .get(&url)
                .header(
                    header::USER_AGENT,
                    "Android712-AndroidPhone-8983-18-0-NetMusic-wifi",
                )
                .send()
                .await
            {
                Ok(resp) => {
                    let json_val: Value = resp.json().await.unwrap_or_default();
                    ApiCallResponse::ok_data(json_val)
                }
                Err(e) => ApiCallResponse::err(format!("Kugou search error: {}", e)),
            }
        }
        "lyric" => {
            let hash = params.get("hash").and_then(|v| v.as_str()).unwrap_or("");
            let keyword = params.get("keyword").and_then(|v| v.as_str()).unwrap_or("");
            let duration = params.get("duration").and_then(|v| v.as_u64()).unwrap_or(0);

            let url = format!(
                "http://lyrics.kugou.com/search?ver=1&man=yes&client=pc&keyword={}&duration={}&hash={}",
                urlencoding::encode(keyword),
                duration,
                hash
            );

            match HTTP_CLIENT
                .get(&url)
                .header(header::USER_AGENT, "KuGou2012-8000-Official-SearchLyric")
                .send()
                .await
            {
                Ok(resp) => {
                    let json_val: Value = resp.json().await.unwrap_or_default();
                    ApiCallResponse::ok_data(json_val)
                }
                Err(e) => ApiCallResponse::err(format!("Kugou lyric search error: {}", e)),
            }
        }
        other => ApiCallResponse::err(format!("Unsupported Kugou API: {}", other)),
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
