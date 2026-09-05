# Diretta 硬化与上游同步方案（tinyLMS-old / splayer-atom 对照）

> 生成日期：2026-09-05。基线：`dev @ dd57aba`（含批次 1-10 全部修复与 pre-mute 静音窗口，共 20 个本地提交）。
>
> 参照项目：
> - `/home/songlian/tinyLMS-old`——早期自研项目，`DIRETTA::Sync` 子类 pull 模式 + 自研 getNewStream，SDK 适配度最高的参考实现；
> - `/home/songlian/splayer-atom`——官方 SPlayer 的下游产品（Core / Control / Output 架构），包含 Diretta strict Source Direct 与 Node Core Headless Web，其 Diretta 代码为 2026-09-01 前的旧快照。
>
> 关联文档：`code-review-fix-plan.md`（§一-§十一，批次 1-10 的来源方案与实施记录）。本文档 §4.3 取代该 troubleshooting 文档故障 4 的过时同步指引。

## 一、三个代码库的定位与关系

| 代码库 | 定位 | Diretta 实现 | 与本项目的关系 |
|---|---|---|---|
| SPlayer-Next-Headless（本项目，`dev`） | 纯 Rust Axum 独立 headless 服务 + Diretta Source Direct 主线 | `bridge.cpp`：`DirectSync : DIRETTA::Sync` 子类 pull 模式，`getNewStream` 转发 host `next_block` 回调 | — |
| tinyLMS-old | 早期自研项目，SDK 适配度最高的参考实现 | `DirettaSyncImpl : DIRETTA::Sync` 子类 pull 模式，自研 `getNewStream`（**绕过了 SDK 148 顶层类的 BUG**） | 只做参照，不同步代码 |
| splayer-atom | 官方 SPlayer 的下游产品（Core / Control / Output），`master @ 383e787c` | 与本项目同一血统的**旧快照**（2026-09-01 分叉，缺批次 1-5 全部修复） | 方向一：我方修复推给 atom；方向二：甄别回移其独立修复 |

远端角色：`origin` = `SPlayer-Dev/SPlayer-Next`（**唯一正式上游**，fetch-only）；`fork` = `feifenspace/SPlayer-Next`（本方发布远端，`dev` 跟踪它，当前 ahead 20 未推送）。

## 二、来自 tinyLMS-old 的可移植项（批次 E：SDK 调参与容错）

### E1｜传输周期按 MTU 动态计算（P1，DSD 稳定性）
- tinyLMS：`DirettaDriver.cpp:711-729`——`cycle_time_us = (MTU − 48) / bytes_per_second`，clamp 100µs–10ms。注释原话："防止 DSD 高码率下由于包过大导致的 SDK 内部挂起"。
- 当前：`bridge.cpp:219` `configTransferAuto(200µs, …)`、`open(…, MilliSeconds(100), …)` 固定值。DSD256 码率下 200µs 周期 = 单包过大 + 中断频率过高。
- 改法：`open_direct_with_format` 按目标 MTU 与格式计算 cycle_time，经新 C ABI 参数下传；`configTransferAuto` 同步替换。
- 风险：中。需真机对比 DSD 锁定与长播稳定性。

### E2｜THRED_MODE 与 open 参数对齐（P1，抖动/CPU 亲和）
- tinyLMS：`THRED_MODE(289)`、CPU 参数 `-1, -1`（SDK 自选核心）、产品标识 `0x44525400`；当前：`THRED_MODE(5)`、CPU `0, 0`。
- THRED_MODE 决定 SDK 内部工作线程的调度配置——直接关系"供数抖动/杂音"类问题。
- 改法：一行参数改动；A/B 实测对比 CPU 亲和与抖动后定版。

### E3｜`connectPrepare(true)` 强制模式（P2）
- tinyLMS 硬切换（864 行）用 `connectPrepare(true)` 强制 Target 状态机彻底重置；当前 `bridge.cpp:224` 为无参调用。
- 改法：跨格式 full-reconnect 的 open 路径改为 `connectPrepare(true)`。一行。

### E4｜MTU / 设备能力按 IP 缓存（P2）
- tinyLMS：MTU 测量按 IP 缓存省去每次 50-100ms（508-527 行）；能力缓存命中实现"秒开起播"（538-555 行）；扫描发现设备名变更时自动失效缓存（427-440 行）。
- 当前：每次连接都重新 `measSendMTU`。
- 改法：bridge 层加按 IP 的 MTU/能力缓存，含失效条件。

### E5｜看门狗宽容期细化（P2，F11 增强）
tinyLMS 看门狗（1080-1168 行）有三个当前 F11 没有的容错细节：
1. `Connecting` 状态容忍（握手中不算离线）；
2. `try_lock_for(500ms)` 失败抖动消除（连续失败计数，锁被占 ≠ 链路断）；
3. 高采样率（>96k）自动放宽超时阈值。
- 改法：吸收进 `background.rs` Direct position-timer 的停滞判定与 headless 恢复看门狗。

### E6｜能力查询双轮超时 + 快速断开（P3）
- tinyLMS：探测连接双轮超时（0.5s×5 → 1.7s×20，1226-1249 行）；断开用 `disconnect_flgset/disconnect(true)` 免等 RST（1370-1374 行，避免 `disconnectWait` 阻塞 2-5s）。
- 当前：`query_target_caps` 同步 FFI 固定 2-3s。
- 改法：对齐双轮策略与快速断开。

### E7｜杂项（P3）
`FwVersion` 固件版本查询（展示）；扫描重试 10 次 + `findTarget` 广播备选（374-443 行）；`setSink` prefill 30ms + 手动 play 控制（当前 100ms + auto）。

## 三、来自 splayer-atom 的可移植项（批次 F：甄别回移）

### F1｜stage 门控放宽（2 行，建议必做）
- atom：输出选中 Diretta 时，即使全局 `preloadNextTrack` 关闭，无缝 stage 仍保持可用（`preloadNextTrack === true || selectedDevice.startsWith("diretta:")`）。
- 当前：`gapless.ts:74` 硬门控——Diretta 用户关掉全局预载会连带失去无缝切歌。
- 改法：`maybeStageDirectNext` 门控改为二者相或。

### F2｜跨格式回卷竞态（需甄别）
- atom：preloader 清理只删除"已安装 stage"的音源（`hasStagedDirectNext` 三参 `queueDirectCleanup`），避免误删正常 load 要用的 source。
- 改法：对照本项目 `nextTrackPreloader` 的清理语义甄别；语义一致则回移，不一致则记录差异。

### F3｜Web Control 运行时识别 / Web 复制兼容（小项，按需）

### 已对齐（记录，不做）
旧中文 ID3 GBK 乱码修复——本项目 `metadata/mod.rs` 已有同款 `decode_latin1_as_gbk` 实现。

### 不做回移
atom 的 Diretta/Direct 代码本体为本项目批次 1-5 之前的旧快照（缺 per-slot 静音 F1、downmix 归属表 F2、取消句柄 F5、pre-mute 窗口、`replace_drained_local` 改名、cfg 收敛等全部修复）——**方向是把我方修复推给 atom**（其 roadmap §3 已列 ffs/dev 为候选能力源），而不是从 atom 回移。

## 四、上游同步策略（重点）

### 4.1 实测现状（2026-09-05）

| 事实 | 数据 |
|---|---|
| 合并基点 | `ac0bcfa`（上次 `532ec16` merge 的基点） |
| 上游新提交 | **0**（`origin/dev` 自基点以来无新提交，当前无待合并内容） |
| 冲突面实测 | 我方触碰的 30 个文件中，9 个共同文件（events.ts / index.ts / shared player.ts / broadcast.ts / bindings player.rs 等）上游改动均为 **0 行** |
| fork 专属文件 | `native/headless-server/`、`direct_*.rs`、`diretta.rs`、`docs/linux-headless-server/` 上游不存在 → 永不冲突 |
| 发布状态 | `dev` 领先 `fork/dev` 20 个提交**未推送** |

### 4.2 同步流程（每次上游同步的 SOP）

1. **同步前**：
   - 全量验证必须全绿（`cargo check/test` ×3 crate、`pnpm typecheck`、vitest）；
   - 打备份分支：`git branch backup-before-upstream-sync-v2`；
   - 记录本方提交链起点（当前为 `bd76c9e…dd57aba` 共 20 个，全部语义独立）。
2. **合并方式决策：merge，不 rebase**。
   - 理由：① 沿用 `532ec16` 的既有先例；② 本方提交已推送 `fork/dev`，rebase 需要强推改写已发布历史；③ merge 保留双方历史，冲突一次解决。
   - 命令：`git fetch origin && git merge origin/dev`。
3. **冲突解决锚点（语义保留清单）**——冲突双方取本方侧的判定依据是以下标识符/机制必须存活：
   `fill_silence`/per-slot 静音（F1）、`downmix_extra_targets`（F2）、`trigger_pre_mute`/`pre_mute_until_ms`（pre-mute）、`open_source_with_cancel`（F5）、`handoff_drained_source`/`replace_drained_local`（§六-1 改名）、`direct_drain_handle`/`try_direct_handoff`（F7/F8）、`DirectInput::Memfd` 的 file 守卫（F4）、`LoadSuperseded`（F8）、`DIRECT_FADE_DRAIN_*`/`DIRECT_FULL_RECONNECT_STABILIZATION` 常量、`open_verified_local`/`wait_for_direct_start`（启动验证）、F11 恢复看门狗（`spawn_output_recovery_watchdog`/`pending_next`/`auto_advance_requested`）、WS `{type,data}` 信封与客户端 v1 回退。
4. **上游特别核对点**（我方改动与上游未来改动的交界面）：
   - `shared/types/player.ts`：我方 `PlayerStatus.currentSource?` 为加法字段，保留；
   - `src/core/player/events.ts` / `index.ts`：`maybeRegisterNextCandidate` / `adoptServerAdvancedTrack` / `resetServerAutoAdvance` 三处接线与 `supportsServerAutoAdvance` 分支保留；
   - `native/audio-engine/src/bindings/player.rs`：`Seeked` 臂与 `LoadSuperseded` 用法保留；
   - `electron/main/core/index.ts`：headless 移除后的直线代码不回退。
5. **同步后验证**（顺序执行）：
   `cargo check -p audio-engine-core -p audio-engine -p headless-server` → `cargo test` ×3 crate → `pnpm typecheck` → `pnpm vitest run src/services/client` → headless 冒烟（启动、播放、暂停、切歌、WS 事件、断链恢复）。
6. **推送**：`git push fork dev`（发布远端）；如上游有安全修复可额外 `git push origin dev`（需上游分支权限，通常不做）。

### 4.3 过时指引修订

`docs/troubleshooting/diretta-headless-troubleshooting.md` 故障 4 的同步指引（rebase 策略 + 保留 `diretta_output.rs`/`spawn_isolated_blocking` 清单）已过时：`diretta_output.rs` 在 Direct 架构重构后已删除，rebase 建议被本文 §4.2 的 merge 决策取代。以本文为准。

## 五、实施批次与验收

| 批次 | 内容 | 风险 | 验收 |
|---|---|---|---|
| E-1 | E1 cycle_time 动态计算 + E2 THRED_MODE/参数对齐 | 中（SDK 调参） | 真机 A/B：DSD 长播稳定、CPU 占用、抖动对比 |
| E-2 | E3-E7 容错细节（强制重置/缓存/看门狗细化/双轮超时/杂项） | 低 | cargo 三件套 + 真机回归 |
| F | F1 stage 门控放宽 + F2 甄别回移 | 低 | vitest + 真机 stage 场景 |
| S | 按 §4.2 SOP 执行一次上游同步演练 | — | 全套验证 + 冒烟 |

执行顺序建议：E-2（低风险容错）→ F（小项）→ E-1（调参，需真机）→ S（实际同步演练）。

## 六、长期架构项（不排期，记录）

- **canonical queue / Core Control Protocol**：splayer-atom 已实施完整版"队列权威上服务端"（Node Core 单一权威 + Web/Desktop 纯遥控），是本项目 §八 B 层单槽候选的架构级上位方案。触发条件：多端一致的完整队列控制成为硬需求时，按 atom `docs/api.md` 做迁移评估。
- **PlayerTransport 传输抽象**（原方案 §八 C 层）：本地 IPC 与远程 HTTP 双实现的统一接口，远期可选。
- **F11 断链阈值真机回归**：`ip link set down` 注入 + journalctl 观察，验证 `PRE_MUTE_WINDOW_MS`/`DIRECT_START_TIMEOUT`/恢复看门狗参数。
