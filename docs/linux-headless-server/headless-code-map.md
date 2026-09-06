# Headless 模式代码地图与逐模块优化盘点

> 生成日期:2026-09-05。基线:工作区当前状态(参照 code-review-fix-plan R5 后)。
> 本文是**事实盘点**,供逐模块决策优化方向;每条事实标注 `文件:行号`。"优化切入点"为候选方向,非结论。

## 全景

```
浏览器 Web UI (src/, Vue3)
  ├─ src/services/client/          双通道抽象 (Electron IPC | HTTP+WS)
  └─ src/services/playback.ts      墙钟插值, 60FPS 渲染
        │  REST /api/v1/*  +  WS /ws (500ms snapshot + 事件)
        v
native/headless-server (Axum, ~6.2k 行)
  ├─ api/routes.rs    42 条路由 + 看门狗 + memfd 物化 + WS 会话
  ├─ api/online_apis.rs  在线音源分发/代理
  ├─ state.rs / db.rs / config.rs
        │  InnerPlayer (parking_lot Mutex) + 事件回调
        v
native/audio-engine-core (headless feature: 关 fft/midi/pipewire)
  ├─ player/            门面 + 后台线程 + 切换状态机
  ├─ direct_pcm.rs      Diretta Direct PCM 通路 (4.0k 行)
  ├─ direct_dsd.rs      Diretta Direct DSD 通路 (2.0k 行)
  ├─ direct_runtime.rs  Direct 编排层
  ├─ diretta.rs         安全封装
  └─ audio_output.rs    cpal 常规输出 (本地设备)
        │  C ABI (8 个 splayer_diretta_* 函数)
        v
native/diretta-sys  bridge.cpp → Diretta Host SDK (静态链接)
```

两条播放路径:**cpal 常规路径**(DecoderSource → cpal 流,本地设备)与 **Diretta Direct 路径**(producer 线程 → 8 深槽 ring → SDK pull 回调 → Target)。Direct 与常规互斥(`player/transition.rs:405-444`)。

---

## A. headless-server 服务层

### A1. 入口与配置 — `src/main.rs`(131 行)、`src/config.rs`(216 行)

- main.rs:CLI 11 个参数解析(手写 while 循环)、`start_server`、输出恢复看门狗启动(`main.rs:23`)、ctrl_c 保活。
- config.rs:9 个配置项(listen_addr/cors_origins/api_token/cover_cache_dir/database_path/web_root/diretta_target/proxy 等),来源三级:配置文件 → 环境变量 → CLI(`config.rs:63-106`);查找路径 7 处候选,首个存在者**整文件反序列化,不做部分合并**(`config.rs:63-69`);默认数据目录锚定 exe 所在目录,cargo target 布局回退 CWD(`config.rs:136-161`)。

优化切入点(候选):CLI 解析迁移 clap 派生;配置支持部分合并/校验报错;数据目录锚定策略显式化。

### A2. 路由层 — `src/api/routes.rs`(3,091 行,服务层最大债务点)

42 条路由(`build_router` routes.rs:368-597)。内部混居 8 类职责:

| 块 | 行号 | 约行数 |
|---|---|---|
| 输出恢复看门狗 + 自动连播 | 100-305 | 215 |
| build_router + 静态托管探测 + ncm nest | 363-597 | 235 |
| CORS + token 中间件 | 599-648 | 50 |
| 简单播放 handlers | 654-719 | 66 |
| 流缓存目录 + **memfd 物化** | 721-971 | 250 |
| **load_handler** | 1057-1551 | **494** |
| seek / now-playing | 1553-1732 | 180 |
| Direct stage/cancel/commit | 1734-1862 | 129 |
| 扫描 + library | 1864-2139 | 276 |
| 歌单 / 设置统计 / 封面歌词 | 2141-2572 | 430 |
| apis_call + WS 会话 | 2574-2698 | 125 |
| Diretta handlers + fs_browse | 2700-3066 | 367 |

超长函数:`load_handler` 494 行(`routes.rs:1058-1551`,承载 CUE 解析、锁内预检、Direct handoff/全量重连双路径、两种 commit、三种错误收尾)、`build_router` 230 行、`spawn_output_recovery_watchdog` 206 行、`seek_handler` 138 行、`library_scan_handler` 130 行。

锁模式不一致(事实):play/pause/stop/封面/歌词/FFT 走 `spawn_isolated_blocking`(独立 OS 线程,routes.rs:61-76),而 **volume(routes.rs:714-717)、snapshot(state.rs:287-303)、diretta_status(2726)直接在 async 上下文取 player 锁**。load 预检段是最长 player 锁临界区(约 40 行,`routes.rs:1098-1137`)。

其他结构事实:
- 双 NCM Router 实例并存:`online_apis.rs:37-40` 单例 + `routes.rs:589-596` nest 时另建一份。
- 鉴权面不一致:token 中间件只保护 protected 组;`/ws` 走 query 参数;covers/lyrics/proxy/stream/image/scan/probe/status 完全免鉴权(`routes.rs:528-560`);CORS 默认含 `"*"`(`config.rs:130`)。
- 看门狗为 1s 轮询原子标志设计(事件回调线程禁锁禁 async 的约束所致,`state.rs:141-149`)。
- diretta_select 可达性探测 3s 硬超时,超时线程不可取消、留在后台(`routes.rs:2770-2776`)。

优化切入点(候选):按 routes 内部块拆分模块(player/direct/library/playlist/proxy/diretta/ws/watchdog);load_handler 拆状态机;统一锁风格;合并 NCM Router;鉴权面收敛决策;code-review-fix-plan §F10 的 M/L 清单部分未实施项在此收尾。

### A3. 在线音源 — `src/api/online_apis.rs`(887 行)

- `dispatch_api_call`(157-202)按 platform 分发五平台:netease 为**进程内 axum Router oneshot 转发**(208-307,含 cookie 合并持久化与 unikey 兼容垫片);qq/kugou 薄转发 + fcg 兜底;qobuz/tidal 纯转发。
- 事实:TIDAL exchange/poll/refresh 三段 cookie 持久化近乎复制(667-734);stream 代理无总超时(28-34)。

优化切入点(候选):TIDAL cookie 逻辑抽公共函数;流代理超时策略;各平台方法映射表化(声明式)减少手写 match。

### A4. 状态与广播 — `src/state.rs`(313 行)

- AppState 聚合 config/player/db/广播频道/原子标志/快照缓存(73-97);事件回调刻意不加 player 锁,经原子标志交看门狗消费(83-86 注释)。
- `snapshot()` 持 player 锁连续调 7 个 getter,每 WS 连接每 500ms 一次(state.rs:286-304 + routes.rs:2601)。

优化切入点(候选):快照改事件维护的缓存读(回调已在写 snapshot RwLock,HTTP 侧可直接复用),消除 500ms×N 连接的锁竞争。

### A5. 持久化 — `src/db.rs`(1,466 行)

- 8 张表(tracks/scan_dirs/playlists/playlist_tracks/settings/server_state/play_history/account_sessions,WAL+NORMAL,db.rs:75-184);36 个 pub fn。
- 事实:21 列 SELECT 列表 5 处逐字重复(757-761 等);CUE 排除子查询 9 处重复;`INSERT..ON CONFLICT` 列清单 3 处重复(299/448/668);`sync_cue_tracks` 218 行(427-644);`row_to_track` 21 列手工映射(1416+);play_history 插入附带 DELETE 裁剪至 5000(1342-1346);全局单连接 `Arc<Mutex<Connection>>` 串行所有 handler(state.rs:76),扫描回调持锁批量写(2057-2098)。

优化切入点(候选):列清单/排除子查询/upsert 语句常量化;row_to_track 用 serde 或宏;评估 r2d2 或 WAL 多连接读;CUE/SACD 同步函数拆分。

---

## B. audio-engine-core 引擎层

### B1. 播放器门面 — `player/mod.rs`(1,114 行)+ `player/events.rs`

- InnerPlayer 无内部锁,靠"调用方持 Mutex<InnerPlayer>"约定;暴露 60+ pub fn(清单见盘点附录):控制/查询/load-seek async 协议(take→worker→commit,token 抢占仲裁)/Direct 专用(validate_direct_entry 门槛:音量 100%、无归一化/EQ、原速原调,`player/mod.rs:302`)。
- Direct 模式拒绝全部采样变换类接口(`reject_direct_sample_change`,325-332)。

### B2. 后台线程 — `player/background.rs`(284 行)

- position 定时器单函数双分支(Direct/常规各一套循环,91-229):200ms 推 Position、Ended/SourceError/停滞检测(常规 6 tick=1.2s,Direct 高码率 8 tick)、Direct 边界事件;fade 线程 20 步渐变(14-36);FFT 定时器 50ms(261-270)。

### B3. 切换状态机 — `player/transition.rs`(611 行)

- 三段式协议:take(锁内不 join)→ worker(join 旧线程+阻塞 IO)→ commit(token 比对)。
- Direct handoff:`try_direct_handoff`(509-560)= 格式预检(PCM 需采样率+声道一致;DSD 需声道一致)→ 源级淡出(PCM 20ms 渐零)→ **锁外**排空(4 块/600ms 兜底)→ 块边界原子换源。
- **DSD 无淡出通道**:淡出对 DSD 恒 no-op,块边界硬切换(`direct_runtime.rs:257-298`)。

### B4-B6. Direct 通路 — `direct_runtime.rs`(793)+ `direct_pcm.rs`(4,033)+ `direct_dsd.rs`(2,004)

- **最大结构性事实:PCM/DSD 两文件存在约 1,100 行同构镜像代码**——四态 slot 状态机、8 深槽 ring、单一信号通道、next_block 边界会计、80ms pre-mute 窗口、reset_for_transition、producer 主循环、seek/staged/replace、Monitor、StageHandle、C 回调,各组均双份(对照:direct_pcm.rs:1471-2774 vs direct_dsd.rs:615-1525)。差异点:PCM 有淡出态+静音块合成+HTTP/Reader 流式(AvioReader);DSD 用 0x69 静音+SetWireBitOrder+仅本地源。
- 常量重复定义:`DIRECT_FADE_DRAIN_*` 在 player/mod.rs:100-105 与 direct_runtime.rs:31-35 双份;`PRE_MUTE_WINDOW_MS`/SLOT 常量/`PRODUCER_WAIT_CEILING` 双份。
- 超长函数:`open_with_decoder` 355 行(direct_pcm.rs:2264-2618,内嵌 producer 主循环约 300 行)、`open_local_at` 237 行(direct_dsd.rs:1122-1358)、`repack_planar` 175 行、`finalize_open_inner` 111 行(含 4 段相似的逐级错误清理)。
- 设计核心(改动需谨慎):ring 无大锁,slot CAS 四态 + 原子游标,UnsafeCell 安全性由状态机保证(均有 unsafe impl Sync);SDK 持裸指针回调上下文,Arc 保活经锁外排空(direct_runtime.rs:110-120)。**这块的性能正确性建立在无锁设计上,合并 PCM/DSD 镜像时不能引入锁或动态分配到回调路径**。

优化切入点(候选):镜像机制泛型化/宏提取(slot 状态机、ring、Monitor、StageHandle 可参数化 slot 类型;下混内核与位序适配作为 trait 差异点);常量归一;producer 主循环拆命令处理函数。风险提示:此层是全项目音质正确性核心且有大量契约测试保护(diretta.rs:604-686 用 include_str! 对 bridge.cpp 做断言),重构须保持测试同步。

### B7. Diretta 封装 — `diretta.rs`(703 行)

- 职责边界清晰:FFI 调用/CString/handle 生命周期(Drop 顺序:先 splayer_diretta_close 再释放 ring,392-397、509-513)/wire bit-order 协商;解码与 ring 全在 direct_*。契约测试用 include_str! 断言 bridge.cpp(禁止 memcpy/mutex/new 进 pull 回调、Native DSD 禁 DSD2PCM/DoP)。
- `query_target_caps` 同步阻塞 2-3s,契约"调用方不得持有 player 锁"(159-206)。

### B8. RAM 缓冲 — `ram_buffer.rs`(462 行)

- **完全未接线**:`RamTrackBuffer`/`RamPlayManager`/`GAPLESS_TRIGGER_SECS=30` 在整个 native 树无任何外部调用点(仅本文件单测),lib.rs:32 仅导出。mlock(EPERM 降级)、512MiB 上限、双缓冲 swap 均已实现但闲置。

优化切入点(候选):决策——接线(作为整轨纯内存播放模式,对齐 Hi-Fi 规格)或删除;这是"规格差距评估"中 RAM Playback 缺口的现成起点。

### B9. 常规输出与调度 — `audio_output.rs`(413)+ `priority.rs`(180)+ `playback.rs`(65)+ `source.rs`

- cpal 回调逐样本 `source.next()`(Iterator 动态分发)+ 逐样本音量乘法(audio_output.rs:354-356);欠载 20ms 静音垫片(source.rs:76-81);每次 load/seek 新建流。
- SCHED_FIFO 70 + 性能核绑核(ARM big.LITTLE 按最大频率/x86 避超线程,探测失败降级不绑核);**cpal 回调线程本身未调用 boost**(仅解码线程与 Direct producer 调用)。

优化切入点(候选):回调线程 RT 升级评估;逐样本迭代改块处理(DecoderSource 已是 Iterator,可批量 pull 减少动态分发);音量=1 时跳过乘法(位纯真通路)。

### B10. 解码与格式(本次未逐行盘点,规模参考)

- decoder.rs(FFmpeg 解码→重采样→Shared 双缓冲 192/4 槽)、dsd/(DSF/DFF/DSD2PCM)、sacd/(ISO/ScarletBook/DST FFI)、cue/、dts/、hdcd/、mqa/、metadata/、scanner.rs。
- 遗留标记仅 sacd/dst_ffi.rs:449,469 测试 FIXME;核心播放文件无 TODO。

---

## C. FFI 层 — native/diretta-sys

| 文件 | 行数 | 状态 |
|---|---|---|
| build.rs | 145 | SDK 定位(DIRETTA_SDK_DIR → 硬编码 /home/songlian/DirettaHostSDK_{150,149,148})→ 缺失时 stub 模式;**仅编译 bridge.cpp**;静态链接 libDirettaHost/libACQUA;微架构解析(v2/v3/v4/zen4/auto 读 /proc/cpuinfo) |
| src/bridge.cpp | 723 | 在用:`splayer_diretta_*` 8 个 C ABI(direct 模式) |
| src/lib.rs | 177 | repr(C) 结构 + 8 个 extern 声明 + SDK 缺失 stub |
| include/diretta_bridge.h | 142 | 在用 |
| **src/diretta_c_shim.cpp** | **858** | **未编译**:不在 build.rs 编译列表,无任何构建目标引用 |
| **src/sync_buffer_impl.inl** | **391** | **未接线**:仅被 shim include |
| **include/diretta_c_api.h** | **138** | **死头文件**:无任何 include/链接 |
| include/diretta_shim.h / diretta_event.h | 389/44 | 仅为 shim 服务 |

优化切入点(候选):决策 shim 层(push 模式完整实现)是保留备用、删除、还是接线为 Direct 传输抽象的第二后端;build.rs 硬编码绝对路径 `/home/songlian/` 换成可配置探测;headless-server/Cargo.toml 中 `ncm-api-rs` 同样硬编码绝对路径 `/home/songlian/ncm-api-rs`(可移植性债务)。

---

## D. Web 前端适配层 — src/

| 文件 | 行数 | 职责 |
|---|---|---|
| services/client/index.ts | 59 | isElectron 判定 + 懒绑定单例 |
| services/client/types.ts | 106 | IPlayerClient 接口 |
| services/client/electronClient.ts | 206 | IPC 实现;桌面端 Diretta/browseFs/getNowPlaying 降级(167-205) |
| services/client/httpClient.ts | 979 | HTTP+WS 实现:token 注入、v2 信封 + v1 兼容双解析(190-307)、3s 固定重连、FFT 订阅、Diretta 三段 stage、no-op 降级清单(846-944) |
| services/client/webPolyfill.ts | 702 | window.api 全量 polyfill(20+ 命名空间),main.ts 首行导入 |
| services/playback.ts | 172 | 墙钟插值(容差 1000ms/收敛 0.2/seek 冻结) |
| core/player/events.ts | 197 | 事件分发:status store、seek 确认、自动连播分支 |
| core/player/serverAutoAdvance.ts | 143 | 浏览器降级为遥控器,曲终服务端接力 |
| core/player/gapless.ts | 230 | Direct 三段式 stage/boundary/commit |

事实:
- v1 裸格式兼容层仍在解析(httpClient.ts:261-307)——v2 信封(批次 A 已上线)稳定后可移除。
- `src/web/OutputDeviceSelector.vue`(45 行)**全仓库无 import,未挂载**。
- Web no-op 面:EQ/变速/归一化/fade/readLyricFile 等接受调用不生效(httpClient.ts:846-909),UI 层是否全部对应隐藏见 web-compat-fix-plan UI-2(未实施)。
- **web-compat-fix-plan 的 P0×7 + P1×3 修复方案"待评审",除 SRV-1(FFT 订阅,commit 0e9d96d)外均未实施**(与 memory 记录一致)。

优化切入点(候选):执行 web-compat-fix-plan P0;v1 协议兼容层退役计划;挂载或删除 OutputDeviceSelector.vue;webPolyfill 700 行按命名空间拆文件。

---

## E. 构建部署

- `package-linux-headless.sh`(649 行):产物 tar 包;**unit 模板 User=root、无 PrivateTmp/ProtectSystem**(:405-429)。
- `install-linux-headless.sh`(824 行):本机编译安装;unit 为**非 root 用户 + 完整沙箱**(640-676)。两份 unit 安全配置分叉。
- `diretta-e2e-check.sh`(99 行):T1/T2/T3 自动验收(wav 生成→load→切歌×5→候选接续)。
- `build-native.ts` **不构建 headless-server**(仅 Electron napi 模块);headless 只由 package/install 脚本 `cargo build -p headless-server`。
- workspace 成员 9 个;qqkg-api/streaming-api 在 exclude(独立版本)。

优化切入点(候选):统一两份 systemd unit 模板为单一来源;e2e-check 扩展 T4-T6(Direct boundary/长播/断链,real-device-acceptance.md 已定义判据);安装脚本补 memlock/rtprio 的非 systemd 场景提示。

---

## F. 测试资产

| 区域 | 规模 | 备注 |
|---|---|---|
| headless-server/tests | 6 文件 28 用例(1,347 行) | AppState 构造基建在 5 个文件各自重复实现 |
| audio-engine-core tests/ | 8 文件 28 用例 | **无 Diretta 相关集成测试**(grep 0 命中);Direct 逻辑靠模块内单测 + diretta.rs 契约测试 |
| 内联单测 | direct_pcm/direct_dsd/diretta.rs 内含大量 | direct_pcm.rs:2796-4033、direct_dsd.rs:1544-2004 |
| 前端 vitest | client 8 + playback 5 + fftCapture 2 + streaming 等 10 个 spec | |
| 真机 | diretta-e2e-check.sh T1-T3 自动;T4-T6 SOP 待执行 | 无 bit-perfect/拷机/抖动数据 |

优化切入点(候选):测试基建(AppState 临时库)抽公共 test-utils;direct_pcm/direct_dsd 镜像机制可用共享测试套(同一组用例跑两个实例)锁定重构等价性。

---

## 附:未接线/死代码清单(优化决策最快收益点)

1. ~~`audio-engine-core/src/ram_buffer.rs` — 零调用点~~ → **已接线**(2026-09-06 阶段二:`playback.ram_preload` 开启后 load 路径整曲物化进 mlock RAM,64B 对齐分配,Read/Seek 直通 Direct 与解码器;gapless stage 的双池轮转待接)。
2. ~~`diretta-sys/src/diretta_c_shim.cpp` + `sync_buffer_impl.inl` + 相关死头文件~~ → **已删除**(2026-09-06 阶段一 C.1,git 历史可找回)。
4. `src/web/OutputDeviceSelector.vue` — 未挂载组件。
5. `httpClient.ts` v1 裸格式兼容层 — 等 v2 稳定后可退役。
6. 双 NCM Router 实例(online_apis.rs:37-40 与 routes.rs:589-596)。
7. 硬编码绝对路径:diretta-sys/build.rs(SDK 探测)、headless-server/Cargo.toml(ncm-api-rs path)。

## 附:跨文档遗留债务索引

- code-review-fix-plan.md:批次 1-10/A-D 已实施;**S1-S4 结构去重明确"留到下次功能性改动顺带";F10 的 M/L 清单"部分"实施;批次 C 浏览器手工验收与 F11 断链阈值真机回归未做**。
- web-compat-fix-plan.md:**P0×7 未实施**(关于页崩溃/识曲卡死/快捷键失效/本周时长 NaN/更新卡死/缓存 stub/UA 平台误判),P1×3 UI 裁剪未实施,仅 SRV-1(FFT)已落地。
- hardening-and-upstream-sync-plan.md:批次 E/F 已全部实施;长期项(canonical queue/Core Control Protocol/PlayerTransport 抽象)不排期仅记录。
