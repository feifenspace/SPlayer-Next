# 全面检查修复计划（2026-09-08 两轮检查产出）

> 来源：《全面大检查》第一轮（API 层 / 播放链路 / 前端集成 / 文档差距）+ 第二轮（FFI 层 / 数据层 / 在线音源 / 未提交 diff / Web 外围 + 运行时冒烟测试）。
> 范围：本次检查发现的 P0/P1/P2 缺陷修复 + 既有计划（hifi-optimization-plan）中未完成项的收尾排期。
> 原则：先止血后优化；每批次独立可验证、独立可提交；前后端契约改动必须同批配套。

---

## 批次 0：前置动作（半小时，先行）

| # | 事项 | 说明 |
|---|---|---|
| 0.1 | **提交当前工作区** | 当前有 23 个修改 + 4 个新文件未提交（gapless 预载/serverQueue 等已完成的特性）。先按特性拆 commit 落盘，后续修复以独立 commit 序列推进，避免混流。`dump_direct_trace.sh` 调试脚本单独甄别是否入库 |
| 0.2 | 建立冒烟脚本 `scripts/smoke-check.sh` | 把本次手工验证固化：启动临时实例 → 封面穿越金丝雀（须 404/400）→ CORS 预检须含 PUT → `/api/ncm` 按鉴权策略断言 → SPA 深链接 Content-Type 断言 → SIGTERM 退出码。作为后续每批次的回归门 |

---

## 批次 1：P0 止血（0.5 天）

### 1.1 封面接口路径穿越（P0，一行修复）
- 位置：`native/headless-server/src/api/fs.rs` cover_get_handler（:31-36）
- 方案：id 白名单校验 `^[A-Za-z0-9._-]+$`（拒绝 `/`、`\`、`..`），不匹配直接 404。`covers/file`、`lyrics/file` 同批加同样的路径成分校验（resolve 后要求 `starts_with(base)` 双保险）。
- 验证：冒烟脚本金丝雀用例。

### 1.2 WsState 增加 current_source / current_track_id（P0）
- 位置：`native/headless-server/src/state.rs` WsState（:32-38）+ 广播点（:296/:311）
- 方案：广播结构补 `current_source`、`current_track_id`（snapshot 已有字段，序列化命名 snake_case 与前端 `currentSource` 映射已配套）。前端 `events.ts:99-101` 采纳入口无需改动。
- 验证：浏览器离场用 REST 触发接力（或 watchdog 单测）→ 观察 WS state 消息带新字段；前端手动验证切歌后标题/封面/游标推进。
- 注意：500ms 频率 × 增量字段约 100 字节，流量可忽略。

### 1.3 WS 慢消费者挂起 + FFT 订阅泄漏（P0）
- 位置：`native/headless-server/src/api/ws.rs`（:58/:69/:80 三处 send、:30-41 无心跳、:125-127 fft_release）
- 方案：① 所有 `socket.send` 包 `tokio::time::timeout(5s)`，超时 break；② 每 30s 发 `Message::Ping`，连续 2 次无 pong 判死断开；③ `fft_subscriber_count` 的 `fetch_sub` 前校验下限防负。
- 验证：单测模拟不读消息的客户端；长连接 soak 观察 fft_subscriber_count 归零。

### 1.4 CLI --host/--port 被配置文件静默覆盖（P0，冒烟实测）
- 位置：`native/headless-server/src/main.rs`（:48-83 listen_addr 解析）
- 方案：显式 CLI 传参（含 `--listen`）无条件覆盖配置文件；同时 `--host` 默认值改 `127.0.0.1`（配合批次 2 鉴权收敛）。覆盖发生时打一行 INFO 日志说明来源。
- 验证：冒烟脚本断言 `--port` 传参后实际绑定端口一致（本检查中实测传 14799 绑了 14558）。

---

## 批次 2：鉴权面一次收敛（1 天）

> 设计前提：封面/歌词经 `<img>` 标签引用，无法携带 Authorization 头。策略分两层：**短期**——非敏感资源（封面/歌词/代理）保持公开但逐个加固；**中期**——REST 全面支持 `?token=` 查询参数（WS 已支持，行为一致），供 img 场景与前端 token 通路使用。

| # | 事项 | 位置 | 方案 |
|---|---|---|---|
| 2.1 | proxy/stream、proxy/image 纳入保护或白名单化 | routes/mod.rs:279-291, online_apis.rs | 推荐纳入 token 保护段（播放直链由前端 fetch 而非 img 直引，可带头）；若需 img 直引则用 `?token=` |
| 2.2 | scan/probe 收敛 | mod.rs:266-267, library.rs:35-62 | 纳入保护段（仅 Web UI 曲库目录选择用，走 token 通道） |
| 2.3 | /api/ncm 纳入鉴权 | mod.rs:305 | nest 前套 token 中间件；**配套**：梳理前端 NCM 调用统一走带 token 通道（webPolyfill 的 apis.call 已带 Bearer），全量回归在线源功能 |
| 2.4 | CORS allow_methods 补 PUT/DELETE | routes.rs:325-329 | 一行；修复跨域队列注册（实测预检返回仅 GET,POST,OPTIONS） |
| 2.5 | token 常数时间比较 | routes.rs:349 | `subtle` 或手写常数时间比较 |
| 2.6 | axum body limit 显式放宽 | PUT /player/queue | `DefaultBodyLimit::max(8MB)`，千首级队列含 track 快照不再 413 |
| 2.7 | serve panic 传播 | main.rs:100-102 | JoinHandle 监控：serve task 结束即 log error + `std::process::exit(1)`，让 systemd 重启而非僵尸 |
| 2.8 | 前端 token 通路 | httpClient.ts:70-75 | 支持 URL `?token=` 启动注入 → setToken → REST Bearer + WS query；无 UI（运维场景足够），README 记录用法 |

验证：无 token 时全部保护端点 401；带 token 全功能回归；CORS 预检含 PUT/DELETE。

---

## 批次 3：一行级正确性（0.5 天，全部小改）

| # | 事项 | 位置 | 方案 |
|---|---|---|---|
| 3.1 | repeat "list" 解析为 Off | state.rs QueueRepeat::parse（:80-88） | `Some("list") => Self::All`（一行）。**默认循环模式队尾即停的根因**。补双向往返测试（list↔all↔off 序列化） |
| 3.2 | SPA 回退 Content-Type | static_host.rs:42-47 | 回退命中 index.html 时按 `"index.html"` 计算 MIME |
| 3.3 | 回退查找 percent-decode | static_host.rs:40-42 | 先 percent-decode 再查 EmbeddedWeb |
| 3.4 | MIME 表补全 | static_host.rs:18-33 | 补 webp/map/avif/wasm/ttf/otf/mp4/gif |
| 3.5 | routing::any → GET/HEAD | static_host.rs:86 | 非 GET/HEAD 返回 405 |
| 3.6 | CompressionLayer 排除音频 | mod.rs:302 | `CompressionLayer::new().compress_when(DefaultPredicate + NotForContentType::AUDIO)`，或对 proxy 路由单独免压缩。修复 206/Content-Range 错位 |
| 3.7 | REST/WS 状态字符串统一 | player.rs:86/:1208 | `{:?}` → 手写小写序列化（与 WS 一致），消除双协议大小写分叉 |
| 3.8 | broadcast Lagged 记日志 | ws.rs:62/:73 | `Err(Lagged(n))` 分支 warn，便于排查慢消费者 |

---

## 批次 4：引擎健壮性（2 天，P1 密集区）

### 4.1 输出层
- **ALSA on_failure 补齐**（alsa_mmap_sink.rs:214/:279/:266/:250）：open 失败、bail、prepare Err 四路统一走 `on_failure`（对齐 cpal 路径），修复"无声且无自愈"。
- **设备失效检测去字符串化**（audio_output.rs:434-435）：按错误码数值匹配 + 保留文本匹配兜底。

### 4.2 Diretta 回调安全
- **next_block None 交付静音而非 false**（direct_pcm.rs:3104-3114，direct_dsd.rs:1864-1875 同构）：仅 failed/停机返回 false；首块未就绪/欠载一律交付静音块。消除"SDK 发送线程被永久终止"的时序依赖。
- **pre_mute 固定容量化**（direct_pcm.rs:2027-2060，direct_dsd.rs:906-988）：按最大块几何一次分配、只缩不放（或双缓冲换指针延迟回收），消除控制线程 realloc 与回调裸指针并存的 UAF 窗口。
- **reset_for_transition 等待 in_flight**（direct_pcm.rs:2062-2082，direct_dsd.rs:1170）：reset 前带超时（~200ms）等待 `in_flight == NO_SLOT`；`ensure_capacity` 改只增不释放（旧缓冲延迟回收）。
- **stage prepare catch_unwind**（direct_pcm/direct_dsd Stage 分支）：panic 转 Failed 分支清理 `stage_pending`，防曲终永不触发。

### 4.3 失败路径状态机
- **load 失败重置 state**（player/transition.rs take_for_async_load:104-149 + headless player.rs:1116-1133 + bindings player.rs:806-813）：失败路径统一 `enter_paused_for_recovery` 或 stop，保证 `play()` 不再 no-op；本地源失败补发 SourceError。
- **commit_seeked 失败收尾**（transition.rs:267-298）：`?` 提前返回前 `shared.stop()`，防解码线程永久阻塞在 wait_for_space。
- **孤儿 boundary 兜底复活**（state.rs 回调）：回调只 clone 不 take，take 留给 watchdog 兜底；或兜底判据改用 now_playing 对照。修复浏览器离场 gapless 簿记丢失。
- **预载 consume_boundary 匹配才 take**（direct_preloader.rs:98-111）：mismatch 放回，不销毁有效 staged 记录。
- **watchdog 两处小项**（watchdog.rs:152-160/:188）：退避期不跳过 output_recovery 消费；autoAdvanceFailed 广播改按 episode 门控防重复。

### 4.4 NAPI 桌面路径校验
- **set_volume/seek NaN 防护**（bindings/player.rs:1135/:923）：`is_finite()` + clamp，对齐 headless 层。

### 4.5 可观测性（顺手）
- 磁盘物化 `.part` 唯一临时名 + 完成后 rename（direct.rs:284-296），消除并发写竞态。
- stream 模式网络欠载静音期打点日志（>2s 静音 warn 一次）。

验证：`cargo test -p audio-engine-core`（重点 71 项既有回归）+ 真机 T4（gapless boundary）+ 停播/切歌 50 次压力。

---

## 批次 5：数据层完整性（1.5 天）

| # | 事项 | 位置 | 方案 |
|---|---|---|---|
| 5.1 | 歌单悬挂引用 | db.rs delete_tracks_by_paths:447-460 / remove_scan_dir | 两处删除同事务清理 `playlist_tracks`；`get_all_playlists` 计数改与详情一致的口径 |
| 5.2 | remove_scan_dir 兄弟目录误删 | db.rs:330 | pattern 改 `{escaped}/%` + `{escaped}` 精确匹配两分支 |
| 5.3 | sync_sacd_tracks / get_tracks_by_artist LIKE 转义 | db.rs:767-771 / :890-896 | 补 `ESCAPE '\'`，复用 remove_scan_dir 的转义函数 |
| 5.4 | SIGTERM 优雅关闭 | main.rs:127-129 | `signal(SignalKind::terminate())` 与 ctrl_c select；关闭序：停 watchdog → 引擎 stop（Diretta teardown）→ scan_cancel 置位 → DB checkpoint（`PRAGMA optimize` + `wal_checkpoint`）→ exit(0) |
| 5.5 | Ctrl-C 不被扫描阻塞 | main.rs + library.rs | 关机路径先置 scan_cancel；`spawn_blocking` 扫描任务改 `tokio::task::spawn_blocking` + 关机时 abort（或接受等待但给日志） |
| 5.6 | 迁移 ALTER 只忽略 duplicate column | db.rs:233-236 | 检查错误消息含 "duplicate column" 才忽略，其余向上传播 |
| 5.7 | 增量扫描虚拟分轨写放大 | db.rs:340 / scanner.rs:98-114 | `get_file_records` 排除 `cue://`/SACD 虚拟轨（WHERE path NOT LIKE 'cue://%' AND NOT SACD 标记列），或 collect_removed_paths 过滤 |
| 5.8 | CUE 僵尸分轨 | db.rs:598-626/:552-558 | 父轨缺失时不再以默认参数插入；CUE 删除/解析失败时清理其虚拟行（对齐 SACD 先删后插） |
| 5.9 | play_history 加固 | db.rs:1390-1437, settings.rs | listened_ms clamp（≤ 24h）；补 track_id 索引；INSERT+裁剪包事务 |
| 5.10 | db busy_timeout | db.rs:143 | `PRAGMA busy_timeout=5000` |
| 5.11 | 扫描取消竞态 + 终态事件 | library.rs:285-295, scanner.rs:356-359 | cancel 置 scan_cancel 并等待旧任务退出（join via 通知）或扫描代际 token；取消/失败也发终态 scanProgress（补 "error" phase） |
| 5.12 | 非 UTF-8 文件名 | scanner.rs:77-81/:391 | `to_string_lossy` 时对含 U+FFFD 路径 warn 日志（完整 OsStr 路径支持列后续项，不入本批） |
| 5.13 | 网络盘卡死可恢复 | scanner.rs:367 | walkdir 放入 `spawn_isolated_blocking` + 上层超时（如 10min 无进展判失败发 error 终态）——防 is_scanning 永真 |

验证：`cargo test -p headless-server` + 手工用例：删扫描目录（含兄弟目录）→ 歌单计数一致；SIGTERM 期间正在播 → 日志出现 teardown 序列。

---

## 批次 6：前端契约对齐（1.5 天）

| # | 事项 | 位置 | 方案 |
|---|---|---|---|
| 6.1 | 错误码契约适配 | httpClient.ts:115 + utils/errors.ts:35 | httpClient 解析后端 `{code,message}` 对象为结构化错误；`isSkippableError` 按 `NotFound/BadRequest` 映射 ErrorCode 枚举。恢复 web 模式跳曲兜底与换源重试 |
| 6.2 | 刷新重建用 item.track 富快照 | core/player/index.ts:1207-1218 | `item.track` 存在时直接采用（恢复 serverId/source/CUE 分段），无 track 才走合成降级。防降级队列回推覆盖服务端富快照 |
| 6.3 | FM 模式注销服务端队列 | serverQueue.ts:88 + httpClient | 补 `deleteQueueSnapshot()`（DELETE /api/v1/player/queue）；进 FM 时调用，退 FM 重推。消除双推进竞态 |
| 6.4 | isRestoringQueue 释放兜底 | index.ts / main.ts:66-74 | playFiles/handleOrpheus 冷启动分支也走 `markServerQueueSynchronized`（finally 兜底释放） |
| 6.5 | 在线曲目 source 回推 | nextTrackPreloader.ts → serverQueue.ts | 预载解析落地 hook 触发 `pushServerQueueSnapshot`（含 300ms 防抖已有）；snapshotSourceFor 优先级补注册簿记值 |
| 6.6 | shuffle 策略（需决策） | serverQueue.ts:81 | 推荐短期方案：shuffle=on 时 `isServerQueueActive` 置 false（服务端按顺序自治或停自治，前端主导随机）——一行门控消除音画错位；服务端物化 shuffle 列入后续 |
| 6.7 | autoAdvanceFailed 防多跳 | events.ts:157-166 | 3s 内去重 + 仅当服务端确未 playing 时本地 nextTrack |
| 6.8 | 类型漂移修补 | types.ts:123-135, electronClient.ts | pushQueueSnapshot 补 track 字段；ServerQueueSnapshot 可选字段对齐后端 registered=false 响应 |
| 6.9 | 双 HttpPlayerClient 合并 | webPolyfill.ts:31 + services/client/index.ts:36 | polyfill 复用 index.ts 单例（或反之），消除双 WS/双订阅 |
| 6.10 | speed 伪成功 | httpClient.ts:961 | setSpeed 返回失败（服务端无端点），UI 不再被快照打回 1.0 的观感问题列入 WS speed 字段后续项 |

验证：vitest 补 serverQueue 同步/恢复/FM 用例；浏览器手工回归三场景（刷新恢复、FM 切换、shuffle 开关）。

---

## 批次 7：Web 兼容层（1 天，= web-compat-fix-plan 批次 1+2）

- **FIX-1** AboutSettings.vue:29 `window.electron?.process.versions ?? 服务端版本`
- **FIX-2** webPolyfill recognition 声明 `isSupported:false` + useRecognitionSession 早退 + NavSearch 识曲入口 web 隐藏
- **FIX-3** webPolyfill hotkey stubs 返回与桌面一致形态（getAll 返回当前 store 持久化值），恢复快捷键可用
- **FIX-4** stats 补 getStatsSummary 真实端点映射 + getPlayHistoryDaily 映射 `DailyPlayStats[]`（服务端如无聚合端点，先在 polyfill 聚合）
- **FIX-5** update stub 改单通道 onEvent + checkManually 复位
- **FIX-6** cache stub 补 `song:{lookup:async()=>null,...}`（修复直接 TypeError）
- **FIX-7** detectPlatform web 固定 'web' + general.ts:59 / externalLyric.ts:399 改用 platform 判断
- **library.deleteTracks 接真实服务端**（webPolyfill.ts:285），消除假成功
- **WindowControls web 守卫**（WindowControls.vue:23）
- UI-0~3 裁剪（webHidden 清单、右键菜单降级、桌面歌词入口）沿用 web-compat-fix-plan 批次 3 排期，本批先做上面 9 项止血

验证：对照 web-compat §6 手工回归清单逐项勾销。

---

## 批次 8：打包/可移植性/既有计划收尾（1 天 + 真机排期）

### 8.1 构建与 FFI（代码级）
- **build.rs 可移植性**（diretta-sys/build.rs）：SDK 路径改 `DIRETTA_SDK_DIR` env 优先 + 默认探测失败**硬报错**（C.5 可选化 feature 落地前不允许静默 stub）；`DIRETTA_ARCH` 未显式指定时警告"按构建机选择"；交叉编译分支补 CC 处理
- **scan 返回值契约**（bridge.cpp:351/:372）：`return count;`
- **QuerySync cycle==0 补 Data.P/Size**（bridge.cpp:577-589）+ Size 上限护栏
- **get_output_devices 异步化**（bindings/player.rs:1276，spawn_blocking 对齐 scan_diretta_devices）

### 8.2 既有计划遗留（对应 hifi-optimization-plan，按其优先级）
- B7.5 bridge.cpp `cycle_time_us` 死计算修复 + configTransferAuto 参数统一注释（**须真机 A/B**）
- B7.3 Target 自愈（pingTarget/Reboot 程序化）
- gapless stage 双池轮转（阶段二收尾）
- E.4 diretta-e2e-check.sh 扩展 T4-T6
- A2.1 routes.rs 拆分（修复前与批次 2/3 改动协调：**先落 A2.1 或冻结其改动窗口**——文档已警示并行硬化线冲突风险）
- 阶段三/四/五项（alsa_mmap_sink、RT 调优、DoP 打包器、rt-tuning.sh、systemd 沙箱、C.5 SDK feature 化、四项硬性测试脚本）按 hifi 计划原排期

### 8.3 真机/人工验证排期（不可代码化，集中一次进场）
1. configTransferAuto A/B + B7.5 同批验证
2. 断链回归（`ip link set down` 注入 + PRE_MUTE_WINDOW_MS/DIRECT_START_TIMEOUT 参数确认）
3. 批次 C 浏览器手工验收（自动接续/双标签/单曲循环）
4. T4 gapless boundary / T5 30min 长播 / T6 断链恢复
5. bit-perfect 回录哈希 + 48h 拷机（依赖 8.2 观测钩子）
6. 本计划批次 4 全部改动过一遍真机停播/切歌/gapless 压力

---

## 工作量与顺序总览

| 批次 | 内容 | 预估 | 依赖 |
|---|---|---|---|
| 0 | 工作区落盘 + 冒烟脚本 | 0.5h | 无 |
| 1 | P0 止血（4 项） | 0.5d | 0 |
| 2 | 鉴权面收敛 | 1d | 1（1.4 同批） |
| 3 | 一行级正确性（8 项） | 0.5d | 0 |
| 4 | 引擎健壮性 | 2d | 0（4.2 依赖真机复验） |
| 5 | 数据层完整性 | 1.5d | 0 |
| 6 | 前端契约对齐 | 1.5d | 2（6.1/6.3 依赖后端错误码与 DELETE 端点） |
| 7 | Web 兼容止血 | 1d | 6.9 可提前 |
| 8 | 打包/收尾/真机 | 1d + 排期 | 全部 |

**总计约 9.5 个工作日（代码部分），真机验证另排 1-2 天进场。**批次 1-3 结束即达到"可安全部署"底线；批次 4-5 结束达到"功能完整可信"；批次 6-7 结束 Web 端达到可用；批次 8 后进入 hifi 计划阶段三/四。

## 修复纪律

1. 每批次独立 commit（`fix(scope): 描述`），不与特性混流
2. 每批次收尾跑：`cargo test -p headless-server -p audio-engine-core`（-p 限定避开 windows-future 噪音）+ `pnpm typecheck` + `smoke-check.sh`
3. 前后端契约改动（1.2 / 2.3 / 6.1 / 6.3 / 6.8）必须同批提交、同批回归
4. 涉及 Diretta 时序的改动（4.2 全部）合并前必须真机 T4 + 停播/切歌压力，不接受纸面验证
5. P2 中未列入本计划的项（约 15 项：QQ Referer、Accept-Ranges、缓存头、settings 校验、TIDAL userId 等）登记到 code-review-fix-plan.md 遗留索引，随功能改动顺带修
