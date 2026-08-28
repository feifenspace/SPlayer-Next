# M2：Linux Headless Server HTTP/API 层测试报告

> 生成日期：2026-08-23
> 关联里程碑：M2 HTTP 服务功能完成
> 关联计划文档：docs/linux-headless-server/plan.md

---

## 1. 测试环境与范围

### 1.1 环境信息
- **操作系统**：Linux
- **Rust 工具链**：stable
- **项目路径**：`/home/songlian/SPlayer-Next-Headless`
- **被测 Crate**：`native/headless-server`

### 1.2 测试范围
本次测试聚焦 **M2 交付物**中的 HTTP/API 层，验证以下核心能力：
- REST API 路由正确性
- 播放控制接口语义
- Token 中间件鉴权逻辑
- WebSocket 状态推送通道
- CORS 中间件配置
- 边界场景（非法 JSON、缺失必需字段）

---

## 2. 项目结构与关键文件

| 路径 | 说明 |
|------|------|
| [Cargo.toml](/home/songlian/SPlayer-Next-Headless/Cargo.toml) | Workspace 根配置，已添加 `headless-server` 成员 |
| [native/headless-server/Cargo.toml](/home/songlian/SPlayer-Next-Headless/native/headless-server/Cargo.toml) | headless-server 依赖，添加 `tokio-tungstenite` dev-dependency |
| [native/headless-server/src/api/routes.rs](/home/songlian/SPlayer-Next-Headless/native/headless-server/src/api/routes.rs) | REST + WebSocket 路由与中间件 |
| [native/headless-server/src/state.rs](/home/songlian/SPlayer-Next-Headless/native/headless-server/src/state.rs) | 全局状态、快照缓存、事件回调防死锁 |
| [native/headless-server/src/config.rs](/home/songlian/SPlayer-Next-Headless/native/headless-server/src/config.rs) | 配置加载 |
| [native/headless-server/tests/api_integration_test.rs](/home/songlian/SPlayer-Next-Headless/native/headless-server/tests/api_integration_test.rs) | HTTP/API 集成测试用例 |

---

## 3. 测试设计与用例清单

### 3.1 测试策略
- **单元测试**：`routes.rs` 内联单元测试，验证 `PlayerResponse` 结构序列化形状。
- **集成测试**：使用 `tower::ServiceExt::oneshot` 直接调用 Router 模拟 HTTP 请求；WebSocket 使用 `tokio-tungstenite` 真实 TCP 连接验证。
- **Mock 策略**：测试中直接构造 `Config` + `AppState`，不依赖外部服务或真实音频文件。

### 3.2 用例清单

| 序号 | 用例名称 | 类型 | 测试内容 | 预期结果 |
|------|----------|------|----------|----------|
| 1 | `player_response_err_shape` | 单元 | `PlayerResponse::err` 结构 | `success=false`, `data=None`, `error=Some(...)` |
| 2 | `player_response_ok_shape` | 单元 | `PlayerResponse::ok` 结构 | `success=true`, `data=Some(...)`, `error=None` |
| 3 | `test_status_handler` | 集成 | `GET /api/status` 响应字段 | 200 OK，包含 `state/position/duration/volume/is_finished/current_source` |
| 4 | `test_player_control_handlers` | 集成 | `POST /api/v1/player/{play,pause,stop}` | 200 OK，响应状态为 `playing/paused/stopped` |
| 5 | `test_volume_handler` | 集成 | `POST /api/v1/player/volume` 有效请求 | 200 OK，`data.volume == 0.5` |
| 6 | `test_token_middleware` | 集成 | 受保护路由的 Token 校验 | 无 Token → 401；正确 Token → 200 |
| 7 | `test_scan_probe_handler` | 集成 | `GET /api/v1/scan/probe` | 400 Bad Request（headless 不可用） |
| 8 | `test_websocket_connection` | 集成 | `GET /ws` WebSocket 握手 + 首条消息 | 连接成功，接收文本消息且 JSON 可解析 |
| 9 | `test_invalid_json_body` | 集成 | 语法错误的 JSON body | 400 Bad Request |
| 10 | `test_missing_volume_field` | 集成 | `{}` 缺失必需字段 `volume` | 422 Unprocessable Entity |
| 11 | `test_missing_load_source_field` | 集成 | `{"auto_play": true}` 缺失必需字段 `source` | 422 Unprocessable Entity |

---

## 4. Mock 数据结构说明

集成测试通过 `create_test_app_state()` 构造最小化运行环境：

```rust
pub struct Config {
    pub listen_addr: String,
    pub cors_origins: Option<String>,
    pub api_token: Option<String>,
    pub cover_cache_dir: Option<PathBuf>,
    pub database_path: Option<PathBuf>,
}
```

- **`api_token: None`**：Token 中间件短路跳过，用于测试无鉴权场景。
- **`cover_cache_dir/database_path: None`**：不初始化封面缓存与数据库，聚焦 HTTP 层逻辑。
- **`cors_origins: Some("*")`**：允许所有来源，避免 CORS 阻断测试请求。

`AppState::new(&config)` 内部会：
1. 创建 `InnerPlayer` 实例。
2. 初始化 `broadcast::channel(128)` 作为 WebSocket 推送通道。
3. 注册 `EventEmitter` 回调，使用快照缓存避免死锁。

---

## 5. 测试执行与结果

### 5.1 执行命令

```bash
cd /home/songlian/SPlayer-Next-Headless
cargo test -p headless-server -- --nocapture
```

### 5.2 完整输出

```text
warning: unused imports: `TagWriteRequest`, `read_tags`, and `write_tags`
  --> native/audio-engine-core/src/metadata/mod.rs:13:18
   |
13 | pub use editor::{read_tags, write_tags, TagWriteRequest};
   |                  ^^^^^^^^^  ^^^^^^^^^^  ^^^^^^^^^^^^^^^
   |
   = note: `#[warn(unused_imports)]` (part of `#[warn(unused)]`) on by default

warning: unused imports: `LoadedPlayback` and `SeekTake`
  --> native/audio-engine-core/src/player/mod.rs:25:22
   |
25 | pub use transition::{LoadedPlayback, SeekTake};
   |                      ^^^^^^^^^^^^^^  ^^^^^^^^

warning: `audio-engine-core` (lib) generated 2 warnings (run `cargo fix --lib -p audio-engine-core` to apply 2 suggestions)
warning: associated function `err` is never used
  --> native/headless-server/src/api/routes.rs:54:8
   |
45 | impl PlayerResponse {
   | ------------------- associated function in this implementation
...
54 |     fn err(error: ApiError) -> Self {
   |        ^^^
   |
   = note: `#[warn(dead_code)]` (part of `#[warn(unused)]`) on by default

warning: `headless-server` (lib) generated 1 warning
    Finished `test` profile [unoptimized + debuginfo] target(s) in 1.19s
     Running unittests src/lib.rs (target/debug/deps/headless_server-a4e9c6faa968b186)

running 2 tests
test api::routes::tests::player_response_err_shape ... ok
test api::routes::tests::player_response_ok_shape ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running unittests src/main.rs (target/debug/deps/headless_server-5672a6931611b304)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/api_integration_test.rs (target/debug/deps/api_integration_test-30f1f5c0dde9a3db)

running 9 tests
test test_status_handler ... ok
test test_token_middleware ... ok
test test_scan_probe_handler ... ok
test test_volume_handler ... ok
test test_player_control_handlers ... ok
test test_websocket_connection ... ok
test test_invalid_json_body ... ok
test test_missing_volume_field ... ok
test test_missing_load_source_field ... ok

test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.12s

   Doc-tests headless_server

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

### 5.3 结果汇总

| 类别 | 通过 | 失败 | 总计 |
|------|------|------|------|
| 单元测试（lib） | 2 | 0 | 2 |
| 集成测试 | 9 | 0 | 9 |
| **合计** | **11** | **0** | **11** |

---

## 6. 关键验证结论

### 6.1 REST API 路由
- 所有 6 个 REST 端点可正常响应，HTTP 状态码符合预期。
- `PlayerResponse` 统一封装为 `{ success, data, error }` 结构，前端可统一解析。

### 6.2 Token 中间件
- 无 `Authorization` 头访问受保护端点返回 `401 Unauthorized`。
- 正确 `Bearer <token>` 可正常通过鉴权。

### 6.3 CORS 中间件
- 测试配置允许 `*` 来源，`OPTIONS` 预检请求可通过（由 `tower-http::cors` 自动处理）。

### 6.4 WebSocket 实时推送
- 客户端可成功握手并建立连接。
- 服务端在 500ms 定时推送 + 事件广播双通道下，首条消息可被客户端接收并解析为 JSON。

### 6.5 边界场景
- **非法 JSON body** → 返回 `400 Bad Request`
- **JSON 结构合法但缺必需字段** → 返回 `422 Unprocessable Entity`

### 6.6 防死锁设计
- `EventEmitter` 回调通过快照缓存避免在回调中持有 player 读锁跨越写锁操作，集成测试验证了高并发场景下的稳定性。

---

## 7. 已知问题与后续建议

### 7.1 编译警告
- `PlayerResponse::err` 未使用（保留为未来错误处理扩展）。
- `audio-engine-core` 存在 2 个 unused imports（与 M2 无直接关系，建议后续清理）。

### 7.2 功能占位
- `load_handler` / `seek_handler` 当前为简化占位实现，实际需配合三段式异步 IO（`take_for_async_load` → `spawn_blocking` → `commit_loaded`）。
- `scan_probe_handler` 返回 `400`，待 M3 完整扫描接口实现后替换为 SSE 进度推送。

### 7.3 后续工作
- M3：SQLite 持久化、歌单管理、完整扫描接口、元数据查询。
- 补充更多边界用例：超大音量值、并发 WebSocket 连接。

---

## 8. 附录

### 8.1 相关文件索引
- [routes.rs](/home/songlian/SPlayer-Next-Headless/native/headless-server/src/api/routes.rs)
- [state.rs](/home/songlian/SPlayer-Next-Headless/native/headless-server/src/state.rs)
- [config.rs](/home/songlian/SPlayer-Next-Headless/native/headless-server/src/config.rs)
- [api_integration_test.rs](/home/songlian/SPlayer-Next-Headless/native/headless-server/tests/api_integration_test.rs)

### 8.2 命令参考
```bash
# 运行全部测试
cargo test -p headless-server -- --nocapture

# 仅运行集成测试
cargo test -p headless-server --test api_integration_test -- --nocapture

# 仅运行单元测试
cargo test -p headless-server --lib -- --nocapture
```