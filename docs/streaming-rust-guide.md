# Qobuz 与 TIDAL 串流服务开发与 Rust 实现避坑指南

本文档总结了在 SPlayer Headless 原生 Rust 模块（`native/streaming-api`）以及音频引擎中接入 **Qobuz** 与 **TIDAL** 平台时踩过的核心坑点、官方协议规范与最佳实践。

---

## 目录
1. [Qobuz 核心规范与避坑要点](#1-qobuz-核心规范与避坑要点)
   - [1.1 签名机制与 Query / Header 严格隔离](#11-签名机制与-query--header-严格隔离)
   - [1.2 Bundle 诱饵 Secret 与真实密钥派生](#12-bundle-诱饵-secret-与真实密钥派生)
   - [1.3 API 参数白名单约束（如 extra 字段）](#13-api-参数白名单约束如-extra-字段)
   - [1.4 封面反代与网络连接策略](#14-封面反代与网络连接策略)
2. [TIDAL 核心规范与避坑要点](#2-tidal-核心规范与避坑要点)
   - [2.1 官方移动客户端（PKCE）与 Scope 陷阱（Error 1002）](#21-官方移动客户端pkce与-scope-陷阱error-1002)
   - [2.2 设备码授权（Device Code Flow）URL 协议补齐](#22-设备码授权device-code-flowurl-协议补齐)
   - [2.3 列表分页硬约束（Max Limit = 50 报 400 错误）](#23-列表分页硬约束max-limit--50-报-400-错误)
   - [2.4 登录态磁盘持久化与自动刷新](#24-登录态磁盘持久化与自动刷新)
3. [Rust 音频引擎与 Tokio 异步运行时架构守则](#3-rust-音频引擎与-tokio-异步运行时架构守则)
   - [3.1 禁止在 Tokio Worker 线程中直接 Drop 阻塞运行时](#31-禁止在-tokio-worker-线程中直接-drop-阻塞运行时)
   - [3.2 播放控制的全线程隔离设计](#32-播放控制的全线程隔离设计)

---

## 1. Qobuz 核心规范与避坑要点

### 1.1 签名机制与 Query / Header 严格隔离

Qobuz 对流媒体解析接口（如 `track/getFileUrl`）启用了严格的 MD5 签名校验。

#### 签名算法公式
```text
request_sig = md5( object + method + sorted_business_params + request_ts + working_secret )
```
- **示例**：`trackgetFileUrlformat_id6intentstreamtrack_id1236453341788153755abb21364...`

#### 关键约束（极易踩坑）
1. **已签名接口（Signed Endpoints）**：
   - **Query 参数**：**只能包含**参与签名的纯业务参数（如 `track_id`、`format_id`、`intent`）以及签名输出参数（`request_ts`、`request_sig`）。
   - **Header 鉴权**：`app_id` 和 `user_auth_token` **必须且只能**通过 HTTP Header 传递（`X-App-Id` 与 `X-User-Auth-Token`）。
   - **严禁**将 `app_id` 或 `user_auth_token` 放入 Query 参数中，否则 Qobuz 官方网关会判定签名不匹配并返回 400/401。
2. **普通免签接口（Unsigned Endpoints，如 `catalog/search`、`album/get`）**：
   - `app_id` 与 `user_auth_token` 可同时作为 Query 参数与 HTTP Header 发送。

---

### 1.2 Bundle 诱饵 Secret 与真实密钥派生

Qobuz 官方 Web Player 的前端 JS bundle 中包含明文的 `appSecret`，但**该字段是官方专门设置的反爬诱饵（Honeypot）**，直接使用其计算签名必定返回签名错误。

#### 正确的密钥获取机制
- 采用 **Streamrip / QobuzSpoofer** 算法，从 bundle.js 中动态提取真实的 `initialization` 密钥对（通常第二对为生效密钥）；
- 支持多 Secret 尝试机制（Fallback List），一旦某个 Secret 验证成功（返回 HTTP 200 并包含 `data.url`），则在内存中置顶缓存该 `working_secret`，避免后续重复探测。

---

### 1.3 API 参数白名单约束（如 extra 字段）

Qobuz 各接口的 `extra` 可选扩展参数具有**严格的枚举白名单**，传入未定义参数会直接报 `API error (400): Invalid argument: extra`。

| 接口 | 合法 `extra` 取值 | 注意事项 |
| :--- | :--- | :--- |
| **`album/get`** | `focus`, `focusAll`, `albumsFromSameArtist`, `track_ids` | **严禁传入 `tracks`**（该接口默认已内嵌 tracks，传 `tracks` 会触发 400 报错） |
| **`playlist/get`** | `tracks` | **必须显式传入 `extra=tracks`**，否则只返回歌单元数据，曲目列表为空 |

---

### 1.4 封面反代与网络连接策略

1. **封面图跨域与直连访问**：
   - Qobuz 封面图托管在 `static.qobuz.com`，前端浏览器在局域网跨设备访问或特定 DNS 环境下可能加载缓慢或受阻；
   - 后端需提供 `/api/proxy/image` 代理路由，由服务端拉取封面后以 `image/jpeg` 缓存转发给前端。
2. **代理连接机制**：
   - `QobuzClient::new()` 默认采用**原生直接连接**（Direct Connection），仅当显式配置 `QOBUZ_PROXY` 环境变量时才挂载代理，严禁盲目读取无效的系统环境代理导致网络中断。

---

## 2. TIDAL 核心规范与避坑要点

### 2.1 官方移动客户端（PKCE）与 Scope 陷阱（Error 1002）

使用 TIDAL 官方 Android/iOS 客户端 Client ID（`YUJf8vfXOxVvzo2W`）进行 PKCE 授权码登录，可获得最高级别 Entitlement（支持标准 LOSSLESS / Hi-Res FLAC 音频流而不被官方降级为 AAC）。

#### 生成授权 URL 的致命避坑点
```text
https://login.tidal.com/authorize?lang=en&response_type=code&client_id=YUJf8vfXOxVvzo2W&redirect_uri=https%3A%2F%2Fcom.player.tidal%2Fauth&code_challenge=...&code_challenge_method=S256&restrictSignup=true&state=...
```

- **严禁在 URL 中传入 `scope` 参数**：
  官方移动 Client ID 在 TIDAL 认证中心已预分配了完整权限。若在 URL 中附带 `scope=r_usr+w_usr+w_sub`，TIDAL 认证中心会直接弹出 **`Error 1002 (Something went wrong)`** 阻止授权；
- **必须参数**：必须携带 `lang=en`、`restrictSignup=true`、`code_challenge`、`code_challenge_method=S256`。

---

### 2.2 设备码授权（Device Code Flow）URL 协议补齐

TIDAL 的设备码授权端点（`POST /v1/oauth2/device_authorization`）返回的数据结构如下：
```json
{
  "deviceCode": "...",
  "userCode": "ABCD-1234",
  "verificationUri": "link.tidal.com",
  "verificationUriComplete": "link.tidal.com/ABCD-1234"
}
```

- **协议缺失陷阱**：返回的 `verificationUri` **不包含 `https://` 协议头**；
- **后果**：前端浏览器若直接执行 `window.open(verificationUri)`，浏览器会将其视作相对路径打开本地服务（例如 `http://192.168.31.47:14558/link.tidal.com`）导致 404；
- **Rust/前端修复准则**：后端在序列化响应或前端在打开链接前，必须统一校验并强制补齐 `https://`（`https://link.tidal.com`）。

---

### 2.3 列表分页硬约束（Max Limit = 50 报 400 错误）

TIDAL 所有分页列表接口（搜索、专辑曲目、歌手专辑、热门曲目、歌单曲目、收藏夹）具有**单页最大 50 条的硬性限制**。

```text
Streaming API error: API error (400): Too big page, max page size is [50]
```

#### 开发准则
- 无论前端还是后端，在构造 `req_params` 时必须对 `limit` 做强制截断：
  ```rust
  let limit = params
      .get("limit")
      .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
      .unwrap_or(50)
      .min(50); // 必须 clamp 到 50，防止触发 400
  ```
- 若需要获取超过 50 条数据，必须通过分页迭代器（Paginate Loop）以 `offset += 50` 循环获取并合并结果。

---

### 2.4 登录态磁盘持久化与自动刷新

- TIDAL Access Token 通常具有 24 小时~7 天有效期，Refresh Token 长期有效；
- 后端在初始化时需检测 `expires_at`。当距过期时间小于安全裕量（如 5 分钟）时，通过 `POST /v1/oauth2/token`（`grant_type=refresh_token`）自动无感刷新并写回持久化存储。

---

## 3. Rust 音频引擎与 Tokio 异步运行时架构守则

### 3.1 禁止在 Tokio Worker 线程中直接 Drop 阻塞运行时

在 Rust 异步 Web 框架（如 Axum / Tokio）中管理音频流管道时，若网络音源解码器内部包含了阻塞式网络客户端（Blocking reqwest / I/O Client）：

#### 崩溃现象
```text
thread 'tokio-rt-worker' panicked at tokio/runtime/blocking/shutdown.rs:
Cannot drop a runtime in a context where blocking is not allowed. This happens when a runtime is dropped from within an asynchronous context.
```

#### 崩溃根因
- 当切歌、停止（`player.stop()`）、暂停或新流替代旧流时，旧的解码线程句柄（`JoinHandle<DecoderData>`）及其内部持有的网络流实例在 Tokio 异步协程上下文中被就地析构；
- Tokio 严禁在异步线程中执行可能阻塞的析构操作，直接引发整个 Tokio 线程池 Panic，导致所有音频推流连接崩溃中断。

---

### 3.2 播放控制的全线程隔离设计

所有涉及音频流初始化、状态转换与流析构的操作，必须通过独立 OS 线程隔离执行：

```rust
/// 播放/停止/切歌等控制接口必须使用专用阻塞工作线程
async fn stop_handler(State(state): State<AppState>) -> Json<PlayerResponse> {
    let _ = spawn_isolated_blocking("player-stop-worker", move || {
        let mut player = state.player.lock();
        player.stop(); // 内部的旧流 join 与 drop 将在专用 OS 线程中安全释放
    })
    .await;
    Json(PlayerResponse::ok(json!({ "status": "stopped" })))
}
```

---

## 4. 总结速查表

| 功能点 | Qobuz | TIDAL |
| :--- | :--- | :--- |
| **认证方式** | `user_id` + `user_auth_token` | PKCE 授权码 / Device Code 设备码 |
| **流直链签名** | 严格 MD5 签名（参数隔离，鉴权走 Header） | Bearer Token 鉴权 |
| **单页最大 Limit** | 500 | **严格限制 50**（超限必报 400） |
| **专辑曲目接口** | `album/get` 内置（`extra=focus`） | `album/get` + `album/getTracks` |
| **歌单曲目接口** | `playlist/get`（需传 `extra=tracks`） | `playlist/get` + `playlists/{id}/tracks` |
| **推流稳定性** | 原生 FLAC HTTP 解码，隔离 Drop | 原生 FLAC / DASH 分段转封装，隔离 Drop |
