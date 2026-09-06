# Headless 优化实施方案——对照《Linux Headless Hi-Fi.md》蓝图

> 生成日期:2026-09-05。代码基线:**HEAD e7858db**(2026-09-05 20:56,经 §4 基线校验确认);配套文档:[headless-code-map.md](headless-code-map.md)(模块事实盘点)、[real-device-acceptance.md](real-device-acceptance.md)(真机 SOP)。
> 本文把蓝图九大要求映射到代码地图的每个模块(A1~A5、B1~B10、C、D、E、F),给出可执行方案、验收标准与优先级。

## 0. 总策略

### 0.0 项目定位声明(2026-09-05 确认)

本项目的目标是**达到商业级的质量水平**(可靠性、位纯真、稳定性、可交付性对标 §六验收矩阵),而**不是商业化**——项目秉承上游 SPlayer-Next 的开源精神持续开源。因此:
- §六测试矩阵、"商业级质量"等表述一律指**质量标准**,不构成商业发布计划;
- Diretta SDK 的非商用授权条款(见风险表)**不构成本项目的发布障碍**:开源非商用分发无需商业授权;需要做的只是**条款透明**——在 README 与发布包中声明 SDK 来源、授权边界(禁止逆向;将本软件并入付费产品/服务/用于商业硬件平台的部署场景,使用者需自行取得授权);
- 一切"交付/发布"相关表述均指**开源发布物**的质量门槛。

### 0.1 规格锚点与现状

| 蓝图要求 | 章节 | 现状 | 结论 |
|---|---|---|---|
| 纯内存播放池(mlock/64B 对齐/整轨预解压) | §3.1 | ram_buffer.rs 已实现但**零调用点**;在线源 memfd preload 已通 | 接线即可 |
| ALSA MMAP 零拷贝直出 + SCHED_FIFO 75~85 | §3.2 | 本地输出走 cpal 回调式,**完全缺失** | 最大新增点 |
| Diretta 协议栈 | §3.3 | 真实 SDK + Direct 模式,含断链恢复/MTU 自适应;SDK 能力利用度约四成(详见 §1-C+) | **已超规格不重写;增量利用 SDK 未用能力** |
| Native DSD + DoP v1.1 | §3.4 | Native DSD 完整;DoP 被契约测试刻意禁止 | 增量补齐 |
| 双池轮转无缝切歌 + 防爆音 | §3.5 | 同连接 staged 换源已实现;淡出为线性 20ms | 参数与形态微调 |
| 嵌入式 Web 控制端(rust-embed/PWA/1Hz) | §3.6 | ServeDir 磁盘托管;500ms WS+插值已达标 | rust-embed + PWA |
| Linux 系统级硬化(isolcpus/IRQ/limits) | §四 | 仅 systemd unit rtprio/memlock | 补 rt-tuning 脚本 |
| 商业级质量验收矩阵(bit-perfect/拷机/抖动/内存) | §六 | **全部无数据** | 建测试基建 |
| Cargo 分层与单向依赖屏障 | §二 | 分层成立;控制面经隔离线程持锁,RT 面无 tokio 渗透 | 维持,无需重构 |

### 0.2 优化主线(四条线)

- **L1 本地位纯真直出**:ALSA MMAP 后端 + RT 调度达标(§3.2)——当前项目只有 Diretta 是位纯真路径,本地 DAC 是"能用"级别。
- **L2 纯内存播放**:接线 ram_buffer,Direct 源支持整轨 RAM 预载(§3.1/§3.5)。
- **L3 商业级质量验收矩阵**:bit-perfect 回录、拷机、cyclictest、内存审计(§六)——没有这组数据,"商业级质量"无证据(见 §0.0 定位声明:质量标准,非商业化)。
- **L4 工程债收敛**:routes.rs 拆分、镜像代码去重、死代码处置——不直接对应蓝图,但 L1/L2 都要动 routes.rs 与 direct 路径,债先收才好动刀。

### 0.3 明确不做的事(与蓝图对照后的负决策)

1. **不重写 Diretta 栈**:§3.3 要求的能力已全部覆盖且有真机回归体系,推倒重来是纯风险。
2. **不动 Direct ring 的无锁设计**:slot CAS + 原子游标是音质正确性核心,所有重构必须保持回调路径零锁、零动态分配。
3. **不引入 spec 的 crossbeam 命令通道替换现有 Mutex 编排**:现有"隔离线程 + 短临界区 + token 抢占"已满足控制面/RT 面屏障,换通道是纯 churn。
4. **DSD 不做幅度淡出**:§3.5 的余弦淡入淡出对 1-bit 位流物理上不可行(乘法即失真),正确做法就是现行的块边界切换 + 0x69 静音垫片;余弦窗只用于 PCM。
5. **Web 能力扩展 C 档不做**(取自 web-compat §5.0,避免反复再议):任务栏歌词、OS 级全局快捷键(媒体键除外,走 MediaSession)、原生置顶歌词窗口、托盘/窗口管理、音源改写型插件(需网络中间人位置,浏览器沙箱不可给;服务端嵌 JS 引擎的工程量与插件安全面不成比例)。

---

### 0.4 工程纪律(2026-09-05 确认,适用于所有修复与新增代码)

1. **注释**:能不加就不加;必要时一行说清"为什么",禁止段落式/分隔线注释;
2. **日志**:能不打就不打;必要时一条、含关键变量、可定位即可;禁止逐步骤流水账日志;
3. **代码**:最精简实现最优行为——不做"以防万一"的防御分支,不留静默回退;
4. **错误必须 fail loud**:本次 CUE/SACD 三个 P0 的共同温床就是静默回退(cue:// 当普通路径打开、seek 失败被 `let _` 吞掉、DST 错误帧无感知)——凡失败必须报错或计数,禁止降级假装成功。


## 1. 逐模块方案

优先级:P0 = L1/L2/L3 主线阻塞项;P1 = 规格达标项;P2 = 工程债/体验。工作量:S ≤2 天 / M ≤1 周 / L >1 周。

### A1 入口与配置(main.rs / config.rs)——P1,M

**规格锚点**:§3.1(池容量)、§3.2(RT 优先级)、§四(运行模式)。
**现状**:9 个配置项,手写 CLI 解析;无任何音频后端/内存播放开关。

方案:
1. 新增配置项(全部带保守默认,不配则行为不变):
   - `playback.ram_preload: bool`(默认 false,开 = L2 纯内存播放)
   - `playback.ram_preload_max_bytes`(默认 2GiB,上限 4GiB,对齐 §3.1)
   - `audio.rt_priority`(默认 70,可调 80,§3.2 要求 75~85)
   - `audio.isolated_cores`(可选,显式指定绑核集合,覆盖自动探测)
2. CLI 解析迁移 clap 派生(顺带修 main.rs 手写循环),保持现有参数名兼容。
3. 配置文件支持部分合并(现在整文件反序列化,新增字段会破坏旧配置——L2 加配置前必须先修)。

验收:旧 config.yaml 不变可继续用;新字段注释含蓝图章节号。

### A2 路由层(routes.rs)——P0(债)/P1(功能),L

**规格锚点**:§3.6(控制面零干扰)、§二(分层屏障)。
**现状**:3,091 行 8 类职责混居;load_handler 494 行;锁风格不一致(volume/snapshot 在 async 上下文持 player 锁);双 NCM Router;鉴权面参差。

方案(分两步,先债后功能):
1. **拆分**(P0,在 L1 动刀前完成):按代码地图的内部块切为 `api/` 下子模块——`player.rs`(含 load/seek 状态机)、`direct.rs`(stage/cancel/commit + memfd 物化 853-971)、`watchdog.rs`(100-305)、`ws.rs`(2584-2698)、`library.rs`/`playlist.rs`/`settings.rs`、`diretta_api.rs`、`fs.rs`、`static_host.rs`。纯搬移不改逻辑,现有 28 个集成测试是安全网。
2. **锁风格统一**(P0):volume(714-717)、diretta_status(2726)改走 `spawn_isolated_blocking`,与 play/pause/stop 对齐;`snapshot()` 改读事件回调维护的缓存(见 A4),消除 HTTP/WS 路径上的 player 锁竞争——这是 §3.6"操作网页时音频端 CPU/供电平直"的前提。
3. **合并双 NCM Router**(P0,S):routes.rs:589 改用 online_apis.rs:37 的单例。
4. **鉴权面收敛决策**(P1):免鉴权端点清单(covers/lyrics/proxy/stream/image/probe/status)逐个标注"局域网信任"或补 token;CORS 默认 `*` 改为仅显式配置。
5. 新功能端点随 L1/L2 增量:输出后端选择(alsa-mmap 设备,见 D.3 的 `GET /api/v1/player/devices` 可先行)、RAM 预载状态查询、ENH-1 统计聚合与 ENH-2 外置歌词端点(见 D.4/D.7,注意其前置依赖)。
6. **协议升级两项(取自 atom,见 §1-G)**:①`/api/status` 扩展为携带 `protocol:{version}` 的 info 端点,客户端( httpClient.ts)启动时做版本 range 校验,不兼容抛结构化错误并停止 WS 重连(替代现在的 v1/v2 双解析长期并存);②WS 重连语义改为"重连成功先 HTTP 拉权威快照再消费事件",写操作同步返回新快照——消除事件丢失窗口,是退役 v1 兼容层的前置。

验收:拆分后 cargo test 全绿;load_handler 拆为 ≤5 个 ≤100 行的阶段函数;`curl` 压测播放中刷页面,player 锁等待时间不劣化(可用 `parking_lot` 的 contention 统计或日志采样)。

### A3 在线音源(online_apis.rs)——P2,S

**规格锚点**:§3.1(播放期存储/网络 IO 静默)。
**现状**:五平台转发式实现,结构清晰;TIDAL cookie 三段复制;流代理无总超时;**在线源在 Direct 播放中仍可能因 Range seek 触发网络 IO**。

方案:
1. TIDAL exchange/poll/refresh cookie 持久化抽公共函数(667-734)。
2. L2 落地后,`ram_preload=true` 时强制在线源走 preload 物化(routes.rs:1195 已有"stream 模式强制走 preload"的先例),播放期零网络 IO——这正是 §3.1 的要求,基础设施已在,只需补开关联动。
3. 图片代理与 API 转发属控制面,允许播放期活动,无需改。

### A4 状态与广播(state.rs)——P0,M

**规格锚点**:§3.6(1Hz 粗粒度同步 + 客户端插值;服务端负载趋零)。
**现状**:`snapshot()` 持 player 锁调 7 个 getter,每 WS 连接每 500ms 一次;事件回调已在维护 snapshot RwLock 缓存但 HTTP 路径没用它。

方案:
1. **快照缓存化**:事件回调已写 `snapshot: RwLock<Option<WsState>>`;HTTP 的 `snapshot()`(286-304)改为:优先读缓存,仅 `duration/speed/is_finished/current_source` 这几个低频字段在缓存缺失时短锁补齐。500ms×N 连接的锁竞争归零。
2. 推送间隔参数化(默认 500ms,规格 1Hz 是上限不是下限,现值已优于规格,不做"降频到 1Hz"这种倒退改动)。
3. FFT 订阅计数机制保持(已是"无消费者零开销",符合零干扰精神)。
4. **推送分级(取自 atom,见 §1-G)**:atom 把 `position/fftData` 排除出 WS 广播、Web 端改为"页面可见时 500ms 轮询 + `visibilitychange` 上报";我们可取其轻量版——后台标签页的 WS 连接暂停 position 推送(客户端 `visibilitychange` 通知或服务端按最后心跳判定),恢复可见时靠快照+插值追平,多端场景带宽与唤醒 further 下降。

验收:100 并发 WS 连接 + 持续播放,`top` 观察 CPU 空闲增量 ≤2%(对齐 plan.md 性能验收);position 推送与实际偏差 <1s(现有 T1 判据)。

### A5 持久化(db.rs)——P2,M

**规格锚点**:无直接对应(纯工程债)。
**现状**:21 列 SELECT×5 处、CUE 排除子查询×9、ON CONFLICT 列清单×3 逐字重复;全局单连接串行。

方案:
1. SQL 片段常量化(`TRACK_COLUMNS`、`NOT_CUE_CONTAINER`),`row_to_track` 配合收敛为单一映射点。
2. 评估 `Arc<Mutex<Connection>>` → 读写分离(WAL 支持多读;写仍单连接):扫描批量写是唯一长持锁点,先给扫描回调分批让锁(2057-2098 每批次间 drop),不急着引连接池。
3. `sync_cue_tracks`(218 行)拆解析/封面/落库三段。

### B1 播放器门面(player/mod.rs + events.rs)——P0,M

**规格锚点**:§二(audio-types/engine 分层的运行时体现)、§3.2(RT 纪律)。
**现状**:60+ pub fn 门面,Direct 门槛校验完备(`validate_direct_entry`:音量 100%/无归一化/EQ/原速原调)。

方案:
1. **输出后端抽象**:现在"Direct(Diretta)vs cpal"互斥逻辑散在 commit 路径(transition.rs:405-444)。为 L1 引入统一的输出选择层:设备字符串协议扩展为 `diretta:`(现有)/ `alsammap:`(新增)/ 其他(cpal 回退)。`validate_direct_entry` 的位纯真门槛语义平移到 alsammap 路径(音量≠100% 或 DSP 开启时拒绝进 MMAP 位纯真模式,自动降级 cpal)。
2. **RAM 预载模式接入**:load 协议(take→worker→commit)不变,worker 段在 `ram_preload=true` 时先物化整轨到 RamTrackBuffer(本地文件)或 memfd(在线源,已有),再以 `ReadSeek` 打开 Direct/解码——对 InnerPlayer 是源形态变化,协议零改动。
3. pub fn 面已 60+,停止膨胀:新增能力尽量走配置与设备协议,不开新门面方法。

### B2 后台线程(background.rs)——P1,M

**规格锚点**:§六.4(内存审计)、§六.2(XRUN 监控)。
**现状**:position 定时器单函数双分支(Direct/常规);停滞检测内嵌。

方案:
1. 拆 `start_position_timer` 双分支为两个函数(Direct 监控/常规轮询),共享 tick 骨架。
2. **观测钩子**(L3 依赖):position 定时器每 tick 顺带采样 RSS(`/proc/self/statm`)与 ALSA XRUN 计数(`/proc/asound/cardX/pcm0p/sub0/status` 或 `snd_pcm_info`),经低频事件(如 30s 一次)上服务端日志——48h 拷机的数据采集就靠这个,不需要外部脚本轮询。
3. 停滞阈值常量(high-rate 8/常规 6)已对齐 tinyLMS,真机 T5 验证后再动。

### B3 切换状态机(transition.rs)——P1,S

**规格锚点**:§3.5(余弦窗 10ms、静音垫片 50ms、继电器保护)。
**现状**:PCM 淡出为线性 20ms;pre-mute 80ms(对齐 tinyLMS 8 cycles);DSD 块边界硬切换;排空 4 块/600ms。

方案:
1. PCM 淡出窗形升级为**升余弦(Raised Cosine)**,窗长对齐规格 10ms(`direct_pcm.rs:1592-1604` 的 `apply_fade` 改增益曲线,一处改动);pre-mute 80ms 保留(80ms > 规格 50ms,覆盖有余,且与 tinyLMS 实测对齐——规格值是建议不是硬约束,取更严者)。
2. DSD 路径**明确不改**:0x69 垫片 + 块边界即规格 §3.5 对 DSD 的等价保护(见 0.3.4),在代码注释中标注此设计决策,防止后人误加淡出。
3. 常量双份问题(`DIRECT_FADE_DRAIN_*` 在 player/mod.rs:100-105 与 direct_runtime.rs:31-35)合并到 direct_runtime 一处。

### B4-B6 Direct 通路(direct_runtime / direct_pcm / direct_dsd)——P0(接 RAM)/P1(DoP、去重),L

**规格锚点**:§3.1(RAM 源)、§3.4(DoP)、§3.5(无缝)。
**现状**:两条路径已是位纯真直通(契约测试锁死);约 1,100 行 PCM/DSD 镜像代码;DFF DST 显式拒绝(454-457);仅本地源可 stage。

方案(按风险递增排序):
1. **RAM 源接入**(P0,M):`DirectPcmDecoder`/`DirectDsdReader` 已支持 `ReadSeek` 抽象(AvioReader/本地文件),`RamTrackBuffer::read_slice` 天然可实现 ReadSeek——L2 的接线点就在这里,不动 ring 与 producer。
2. **DoP 打包器**(P1,M):新增 `dsd/dop_pack.rs`(蓝图 §3.4 的 DopStreamPacker:marker 0x05/0xFA 每 16 帧翻转),在 **Rust 侧 producer 填槽阶段**打包为 S32_LE 帧,经现有 PCM Direct 连接发往 Target。**SDK 分析后的关键修订**(见 §1-C+):Diretta FormatID 体系(Format.hpp:7-66)**没有任何 DoP 格式位**——DoP 只是 S32_LE PCM 容器的一种位流约定,Target 是否解码 DoP 取决于其固件,**`checkSinkSupport`/caps 查询无法辨别**(PCM 支持≠DoP 支持)。因此 DoP 选择权必须交给用户:新增全局/每 Target 配置 `dsd_transport: native | dop | auto`(auto=有 Native DSD 位即 native,否则提示用户手动切换),DoP 播放失败直接报错回退,不做静默降级。触发条件不再依赖能力协商,契约测试收窄为"native DSD 段禁 DoP"(DoP 走 PCM 家族,语义互斥)。
3. **镜像去重**(P1,L,放最后):slot 状态机/ring 骨架/Monitor/StageHandle 泛型化(参数化 slot payload 类型,PCM 淡出与 DSD 位序适配作为 trait 差异点)。**前置条件**:先建共享测试套(同一组用例对 PCM/DSD 两个实例跑),锁定行为等价后再动;与 code-review-fix-plan S1-S4 合并执行。若工期紧,此项可无限期挂起——镜像代码虽丑但是稳定代码。
4. **DST 支持**(P2):两个选项——①现有 `sacd/dst_ffi.rs` + C 库(libdstdec)接入 Direct DFF 路径(454-457 处);②**atom 有纯 Rust DST 解码器**(`sacd/dst/{decoder.rs 220 行, ac.rs 75 行}`,见 §1-G),若其吞吐满足 DSD 码率可替换 C FFI,去掉一段 csrc 依赖。先确认存在 DST 编码的 DFF 实体文件需求再投入。
5. **SDK 传输调优实验项**(P2,真机 A/B,全部经配置开关暴露、默认现行为):
   - **多流模式**:open 已传 `MSMODE_AUTO`(bridge.cpp:238),caps 已导出 `support_ms_mode` 位图,但从未验证 MS2/MS3 在 DSD256+ 高码率下的带宽余量收益;
   - **THRED_MODE 矩阵**:当前 `THRED_MODE(5)`=CRITICAL|NOSLEEP4CORE(与官方 SinHost 示例一致);tinyLMS 用 289=CRITICAL|FEEDBACKOFFSET|NOFASTFEEDBACK。**注意 bridge.cpp:226 注释声称"THRED_MODE(289)+CPU -1,-1"与代码实际(5 + 0,0,0)不符**——注释是 E-2 遗留,真机 A/B 后统一;另 SDK 原生支持 `OCCUPIED(16)`+`cpuMain/cpuOther` 让发送线程自绑核,当前未启用(我们只绑了自己的 producer 线程),可作为替代/补充方案;
   - **Pacing 模式**:当前仅用 `configTransferAuto`;SDK 还有 FIX/VARIABLE/RANDOM/**TRIANGOLO** 四种 Profile 模式与 `configTransferFix/Var/Random`(Sync.hpp:114-132、Profile.hpp)——随机/三角微突发正是蓝图 §3.3"Pacing 消除 Target 电源噪声"的原生实现,高码率下值得与 AUTO 对比抖动;
   - **参数 A/B 佐证(来自 atom 对照,见 §1-G)**:`configTransferAuto` 我们用 (100ms, 0, 30µs),而 **atom 与官方 SinHost 示例一致用 (200µs, 0, 100ms)**(atom bridge.cpp:211-214)——atom README 声称在该参数下实测 PCM 768kHz/DSD512,这是参数语义复核的最强参照;`connectPrepare(true)`(强制 Target 状态机重置)是我们 E-3 的有意差异,保留。

### B7 Diretta 封装(diretta.rs)——P1,S

方案:
1. `DirettaTargetCapabilities` 增加 Native DSD wire 格式支持位查询(bridge 的 caps 流程已回传,补字段),为 B6.2 的 DoP 能力协商供数。
2. **导出 Target 延迟字段**(SDK 已有、caps 漏掉):`Sync::Info.latencyBuffer/latencyMax/latencyHw`(100µs 单位,SinkInfo 三字段,Sync.hpp:219-224)加入 `SPlayerDirettaTargetCaps`——用途:① Web 进度条按 DAC 实际出声时刻校准(position + latencyHw);② pre-mute 窗长按 latencyBuffer 自适应,替代固定 80ms。
3. **Target 自愈能力**(真机"半响应"痛点的程序化解):封装 `Find::pingTarget(IP)`(轻量存活性探测,Sync.hpp Find.hpp:235)与 `Find::Reboot(IP, true, 1)`(Find.hpp:268)——输出恢复看门狗检测到"扫描可见但永不消费"的半响应态(能力查询挂起/0 消费)时,先 `pingTarget` 确认,再经用户确认或配置开关自动 `Reboot`,替代人工"断电 30 秒"。接入点:A2 看门狗 + `/api/v1/diretta/reboot` 端点;真机 T6 验收项同步扩展。
4. **重连加速**:已知 Target 地址的重连/切换路径可用 `Find::getOutput(targetAdd, ...)`(Find.hpp:164)直接解析 sink,跳过 3 次组播发现重试(bridge.cpp:113-131),把 F11 恢复时延从秒级压到亚秒。
5. **修复 E-1 回退残留**(P0,S,真机 A/B 前置):bridge.cpp:218-220 按 MTU 动态计算 `cycle_time_us`(clamp 100µs~10ms)**但计算结果从未被使用**——`configTransferAuto`(272 行)实际传的是固定 (100ms, 0, 30µs)。历史:`fcd4802` 曾实施 E-1(动态周期 + THRED_MODE(289)),`f3b989f` 以"回退 E-1 调参待真机复测"部分回退,**回退不彻底留下死计算与 289 注释遗留**(代码实为 THRED_MODE(5),226-227 行)。修法:把 `cycle_time_us` 作为 configTransferAuto 的目标周期入参(最小/恢复周期语义对照 atom 与官方示例的 (200µs, 0, 100ms) 形态确定),或删除死计算。此修复应与 §1-B4-B6.5 的参数 A/B 同批真机验证。
6. 其余不动:Drop 顺序、契约测试、bit-order 协商均已达标。

### B8 RAM 缓冲(ram_buffer.rs)——P0,**L2 主线的落点**,M

**规格锚点**:§3.1 全部(mlock、64B 对齐、全页触碰、容量 2~4GB)。
**现状**:462 行完整实现(单写者 append/零拷贝读/mlock EPERM 降级/512MiB 上限/双池 swap),**零调用点**。

方案(接线 = 一次决策兑现规格 §3.1+§3.5 双条):
1. **64B 对齐分配**:现 `Vec<u8>` 改 `std::alloc::alloc_zeroed` + `Layout::from_size_align(cap, 64)`(对齐 §3.1 的 DMA/缓存行要求),Drop 成对 dealloc;或最低成本方案:多分配 63 字节手动对齐偏移。
2. **容量与策略**:`RAM_TRACK_MAX_BYTES` 接 config(A1 的 `ram_preload_max_bytes`);全页触碰(touch_all_pages)在 append 前不需要(alloc_zeroed 已触发缺页),mlock 保留。
3. **接线点**:`ram_preload=true` 时,headless-server 的 load worker 将整轨物化进 `RamTrackBuffer`(本地文件直接读,在线源复用 memfd 下载管道),以 ReadSeek 交付 Direct(经 B4.1)与解码路径;`RamPlayManager` 的 primary/secondary 双池承接 gapless stage——staged 曲目预载进 secondary,边界 swap,即蓝图 §3.5 的"双内存池轮转"完整形态。
4. 删除"未接线死代码"标签(代码地图附表第 1 条由此项关闭)。

验收(§3.1 量化):播放全程 `/proc/self/clear_refs` 后 Major/Minor Page Fault 增量为 0;`pmap` 中该缓冲 RSS 恒定;播放期 `iotop` 该进程磁盘读为 0。

### B9 常规输出与调度(audio_output.rs / priority.rs / source.rs / playback.rs)——P0,**L1 主线的落点**,L

**规格锚点**:§3.2 全部(MMAP、SCHED_FIFO 75~85、隔离核、RT 线程纪律)。
**现状**:cpal 回调逐样本迭代 + 逐样本音量乘法;回调线程无 RT 提升;无 hw 直开。

方案:
1. **新增 `alsa_mmap_sink.rs`**(cfg linux + headless feature):依赖 `alsa` crate(仅此新增系统依赖,alsa-lib 运行库已是 install 脚本依赖项),按蓝图 §3.2 实现并**修正其示例代码的三处缺陷**——`set_rate` 用 `ValueOr::Exact`(Nearest 会静默重采样破坏位纯真)、`mmap_begin` 的 offset 语义按 alsa-sys 实测校准、XRUN 后先 `prepare` 再重试而非直接报错。设备协议 `alsammap:hw:X,Y`(或 `alsammap:` 默认)。
2. **RT 线程纪律**(对齐 §3.2 红线):写循环预分配、无 malloc、无阻塞调用、无 Mutex——直接复用 Direct ring 的纪律风格;写循环线程 `boost_current_audio_thread` + 绑核。
3. **优先级与绑核升级**(priority.rs):`AUDIO_RT_PRIORITY` 70→80(config 可调,§3.2 要求 75~85);`detect_performance_cores` 优先读 `/sys/devices/system/cpu/isolated`(rt-tuning 脚本产出),有隔离核绑隔离核,无则现行为。cpal 回调路径与 alsa_mmap 写线程统一接入。
4. **位纯真守卫**:alsammap 路径仅接受 S16/S24/S32 原格式直写(PCM 无格式转换、无重采样、音量必须 1.0——由 validate 门槛保证,不合格自动回退 cpal 路径)。
5. cpal 路径保留为兼容回退(蓝牙/USB 声卡不带 MMAP 的场景),但逐样本迭代改块处理(`Iterator` 批量 pull)与音量=1 短路,作为独立小步提交。
6. XRUN 监控:alsa_mmap 统计 XRUN 次数并经 OutputStalled 链路上报,B2.2 的观测钩子汇总——§六.2 的 xrun_count=0 判据由此可测。

验收(§3.2/§六.3):`cyclictest -p 80 -t 1 -a <isolated> -n -i 200 -l 1000000` Max ≤15μs(rt-tuning 后);48h 拷机 xrun_count=0(§六.2);24bit/192k 回录哈希比对通过(§六.1,F1)。

### B10 解码与格式(decoder / dsd / sacd / cue / metadata)——P1/M 顺带 P2

方案:
1. DoP 打包器落位 `dsd/dop_pack.rs`(B6.2),带向量测试(DoP v1.1 标准样例帧)。
2. 解码线程已接 SCHED_FIFO;audio-dsp 嵌套线程补绑核(priority.rs 现只包解码主线程)。
3. 元数据/扫描为控制面,无规格诉求,不动。

### C FFI 层(diretta-sys)——P0(决策),S

方案:
1. **shim 层处置**(1,387 行死代码:diretta_c_shim.cpp + sync_buffer_impl.inl + diretta_c_api.h + 两个头):推荐**删除**(git 历史可找回)。SDK 分析后的复核:shim 基于的 `SyncBuffer` push 模式(SyncBuffer.hpp)自带三样差异能力——SDK 内部缓冲欠载自动静音(`setupBuffer` 第三参控制,`dmutedata` 成员)、缓冲内 `seek(int64_t)`/`seek_front()`、`notifyStreamDone` 回收回调;但其内部实现是 `std::mutex`×2 + condvar(SyncBuffer.hpp:78-81),与我们"回调路径零锁"的 Direct ring 设计相抵触,且 pre-mute/staged/位序协商等已自研得更精细。**删除结论维持**;若未来需要"极简第二后端",SDK 头文件仍在,随时可重写。
2. **可移植性**:build.rs 现状是 `DIRETTA_SDK_DIR`/`DIRETTA_SDK_ROOT` 环境变量优先、硬编码 `/home/songlian/DirettaHostSDK_{150,149,148}` 作回退——**保留 env 优先,仅改造回退段**为探测序列 `./vendor/DirettaHostSDK_* → /opt/DirettaHostSDK_* → $HOME/DirettaHostSDK_*`;headless-server/Cargo.toml 的 `ncm-api-rs = { path = "/home/songlian/ncm-api-rs" }`(Cargo.toml:39)同步处理(迁入 workspace 或发到私有 registry)。
3. 契约测试(bridge.cpp include_str! 断言)保持;DoP 落地时按 B6.2 收窄断言范围。
4. **发布构建选型**:lib/ 提供 `-nolog` 变体(运行期无 syslog 输出)与 musl 变体——生产包统一选 `-nolog`;若做 distroless/全静态镜像,`x64-linux-musl15*`/`aarch64-linux-musl15*` 可静态链接(build.rs 需补 musl target 探测)。
5. **SDK 可选化门控(取自 atom,见 §1-G)**:diretta-sys 改 optional 依赖 + `sdk` feature + `links` 键(atom `audio-engine-core/Cargo.toml:18,80`),无 SDK 环境可编译 core、跑全量 CI 单测(stub 已存在于 lib.rs:121-177);当前我们 default 恒链 SDK,无 SDK 机器连 cargo check 都过不了,阻塞 CI 化。

#### C+ Diretta Host SDK 148 能力分析(2026-09-05 实测)

SDK 结构:`Host/` 头文件约 2,000 行(Sync 328 / Find 396 / Connection 229 / Format 228 / Profile 134 / SyncBuffer 92 / Stream 102 / Send 63 / Receive 131 / Icon 74)+ `ACQUA/` 基础库(ThreadPriority/Clock/Socket/Buffer/Ethernet 等)+ `lib/` 静态库(x64 v2/v3/v4/zen4、aarch64、riscv64,各含 musl 与 `-nolog` 变体)+ `doc/`(doxygen HTML)+ `SinHost/` 官方示例(196 行,pull/push 双示例)。**仅支持 Linux**(memo_host.txt 明示)。

两种传输模型(memo_host.txt):`DIRETTA::Sync`=buffer **pull** 型(bridge 在用;pull 回调 `getNewStream` 契约:必须原子处理、返回 false 即终止、指针保活到下次调用,Sync.hpp:248-251);`DIRETTA::SyncBuffer`=buffer **push** 型(未用,见 shim 处置)。

bridge.cpp 已用能力:Find 发现 + measSendMTU;Sync open/setSink/setSinkConfigure(含 checkSinkSupport + 备选格式回退)/configTransferAuto/connectPrepare(true)/getSinkInfo(caps 提取)/getCycleTime;FwVersion。

**未用能力 → 优化机会映射**(按价值排序):

| SDK 能力 | SDK 位置 | 机会 | 去向 |
|---|---|---|---|
| `Sync::Info.latencyBuffer/latencyMax/latencyHw` | Sync.hpp:219-224 | 进度按 DAC 实际出声时刻校准;pre-mute 窗自适应 | B7.2 |
| `Find::Reboot(IP, bool, code)` | Find.hpp:268 | 半响应 Target 程序化重启,替代人工"断电 30 秒" | B7.3 |
| `Find::pingTarget(IP)` | Find.hpp:235 | 轻量存活性探测(类似 ping),替代 3s 硬超时探测 | B7.3 |
| `Find::getOutput(targetAdd, tO, …)` | Find.hpp:164 | 已知地址直接解析 sink,跳过组播发现重试,F11 恢复提速 | B7.4 |
| `Find::SinkSetting / TargetSetting(+Finale)` | Find.hpp:305-324 | Target/Sink 远程配置读写(带类型/最小值/最大值/默认值描述)→ Web"设备设置"页 | P2 新端点 |
| `Find::downloadIcon` | Find.hpp:192-195 | 设备列表展示 Target 真机图标 | P2 |
| `Info.supportPCM/DSDlsb/DSDmsb` 完整 FormatID 位掩码 | Sync.hpp:213-218 | caps 已导出 raw 值(diretta_bridge.h),UI 可用位运算精确枚举 Target 可选格式列表 | 前端消费 |
| MSMODE 多流(MS1/MS2/MS3/AUTO) | Sync.hpp:62-74 | 高码率 DSD256+ 带宽余量;`support_ms_mode` 位图已进 caps 但从未实验 | B4-B6.5 |
| THRED_MODE 精调位(OCCUPIED/LIMITRESEND/NOJUMBOFRAME/NOSLEEPFORCE…) | Sync.hpp:22-50 | 发送线程调优;OCCUPIED+cpuMain/cpuOther 原生绑核 | B4-B6.5 |
| Profile 四模式(VARIABLE/FIX/RANDOM/TRIANGOLO)+ ProfileMaker | Profile.hpp(全) | 伪随机/三角微突发 pacing——蓝图 §3.3"Pacing 消除电源噪声"的原生实现 | B4-B6.5 |
| `setHostInvertPhase` + 内建 BitSwap/ByteSwap/AntiPhase 转换 | Sync.hpp:94,259-267 | SDK 发送线程内位序/反相转换,可替代 producer 侧 repack 降 CPU | **不做**(契约:bridge 零采样域转换;producer 侧零拷贝是刻意设计) |
| 固件管理(Transfer/FwVersion/Alternate/FwURL/TransferFinish) | Find.hpp:239-262 | Target 固件更新功能 | **不做**(高风险,非播放核心) |

**SDK 层事实澄清**(方案修订依据):
- FormatID 体系**无 DoP 概念**(Format.hpp:7-66:仅 PCM Signed/Float、DSD1/DSD4、LSB/MSB 位序、LITTLE/BIG 端序、DSD_SIZ_32 容器、RAT×MP 倍频、DDS 位)→ B6.2 改为用户显式配置;
- `FormatConfigure::getMuteByte()`(Format.hpp:180)是格式对应静音字节的 SDK 权威来源——Rust 侧 0x69 硬编码正确,可经 bridge 暴露该函数供单测对齐;
- bridge.cpp:226 注释("THRED_MODE(289) / CPU -1,-1")与代码实际(THRED_MODE(5) / 0,0,0)**不符**,属 E-2 遗留,真机 A/B 时一并修正;THRED_MODE(5)=CRITICAL|NOSLEEP4CORE 与官方 SinHost 示例一致,OCCUPIED(16) 未置位时 CPU 参数不生效;
- `open()` 的 Host 名 "SPlayer-Next"(bridge.cpp:234)硬编码——Target 端显示的 Host 名可配置化;
- `open_direct` 的 `configTransferAuto(100ms, 0, 30µs)` 与官方示例 `(200µs, 0, 100ms)`、caps 查询 `(2620µs, ·, 100ms)` 三处形态各异,参数语义(最小周期/目标周期/忙时恢复周期)需对照 tinyLMS 复核后统一注释。

### D Web 前端适配层(src/)——前后端一体的功能模块矩阵(P0 修 Bug/P1 裁剪与扩展,L)

**规格锚点**:§3.6(rust-embed、PWA 离线、零干扰)。
**现状**:ServeDir 磁盘托管;无 PWA;UI 层零 headless 裁剪(仅 SideBar.vue:186 一处);7 个 Web 模式 Bug/隐患;v1 协议兼容层冗余;OutputDeviceSelector.vue 未挂载。
**本节组织**:按**前端功能模块**逐一列举(修复细节全文见 [web-compat-fix-plan.md](web-compat-fix-plan.md),已经三轮复核,此处为合并后的执行视图);每个模块给出 前端现状/需修复项/前端方案/**后端配套**——前后端成对交付,系统才达最佳状态。

#### D.0 模块总览

| # | 前端功能模块 | 需修复项 | 后端配套 | 批次 |
|---|---|---|---|---|
| 1 | 客户端抽象层 | FIX-2/3/4/5/6/7 | 协议版本协商 + 快照权威重连(A2.6) | 1 |
| 2 | 播放控制与队列 | SRV-2 接线(带 Direct 守卫)、SRV-9 | DSP 端点×5 + 409 守卫;canonical queue 演进 | 5/6 |
| 3 | 设置页 | FIX-1/3/7、UI-2、ENH-5 | `GET /api/v1/player/devices`(ENH-4) | 1/2/4 |
| 4 | 首页与统计 | FIX-4 | ENH-1 聚合端点×4(先改记录语义) | 1/3 |
| 5 | 音乐库与文件管理 | SRV-6 入口恢复、SRV-8 拖放 | 标签编辑/删除/导入端点 | 6/7 |
| 6 | 歌单与队列编辑 | —(真实 HTTP 已通) | canonical queue 演进(G.2,长期) | 随阶段四 |
| 7 | 歌词 | ENH-2、SRV-10(P3) | load_handler 探测 .lrc → external_lyrics | 3 |
| 8 | 在线音源与账号 | UI-3(登录弹窗裁剪)、FIX-6 | SRV-4 凭证服务端存储(P3 单独评审) | 2 |
| 9 | 下载与云盘 | UI-3(防误导入口) | SRV-7 下载进曲库、SRV-8 云盘上传 | 7 |
| 10 | 听歌识曲 | FIX-2、UI-3 隐藏入口 | SRV-3 服务端识别(P3) | 1/8 |
| 11 | 桌面专属形态 | UI-1 全部裁剪 | —(C 档明确不做) | 2 |
| 12 | 主题外观与频谱 | ENH-3、ENH-6 | 图片代理(已有);SRV-1 已落地待回归 | 3/4 |

#### D.1 客户端抽象层(src/services/client/,2,219 行)

**需修复**:FIX-2/3/4/5/6/7 的根因全部是 webPolyfill stub 语义与桌面 preload 契约不符——识曲 `isSupported` 返回 truthy 对象、hotkey 空配置覆盖默认键表、stats 缺方法落入 safe proxy 返回 truthy `[]`、update 缺 `onEvent` 单一事件通道、`cache.song.lookup` 未定义(未命中应为 `null`)、`detectPlatform` 按 UA 误判(服务端仅 Linux,应固定 `"linux"`)。
**前端方案**:逐项按 web-compat §2 定稿实现替换;核心纪律:**polyfill 语义逐字段对齐桌面契约**(签名/返回形状/事件通道)。
**后端配套**:`/api/status` 扩展为携带 `protocol:{version}` + 服务端 OS/能力位图的 info 端点(A2.6);WS 重连改"先拉权威快照再消费事件"——这是 FIX 类问题不再复发的结构性保障(新功能天然有版本门,polyfill 不用猜)。

#### D.2 播放控制与队列

**需修复**:SRV-2(DSP 五端点接线,替换 httpClient.ts:743-792 的"接受即成功"空实现);SRV-9 MediaSession(前端当前零使用)。
**前端方案**:SRV-2 落地后 `initPlayer` 启动同步调用即生效;SRV-9 在 `core/player` 事件层接 `navigator.mediaSession`(setActionHandler/setPositionState/metadata),**仅 Web 模式启用**(桌面由 media-ctrl 负责,避免双通道)——本机 OS 媒体面板与硬件媒体键直接控制服务端播放,是 C 档"全局快捷键"唯一可达子集,P2 性价比最高。
**后端配套**:**DSP 端点必须带 Direct 守卫**(web-compat 盲区,本方案补):Direct 激活时返回结构化 409 `direct_mode_conflicts`——引擎侧 `validate_direct_entry`/`reject_direct_sample_change`(player/mod.rs:325-334)与 Web 启动同步调用会互相打架;恢复可见的 EQ/变速设置项在 Direct 态显示"Source Direct 播放中不可用"并禁用。曲终接续/无缝边界(DirectTrackBoundary→stage→commit)已通,长期演进走 canonical queue(G.2)。

#### D.3 设置页

**需修复**:FIX-1(AboutSettings.vue:29 顶层直引 `window.electron.process.versions` 崩溃,且 `versions` 还喂 envItems 与"复制环境信息")、FIX-3(hotkey 空 binding 覆盖——修复后 inApp 快捷键 Web 完全可用,localStorage 持久化)、FIX-7、UI-2(设置项 Web 隐藏)、ENH-5(检查更新→跳转 Releases)。
**前端方案**:UI-2 走**中心化 `webHidden.ts`**("分类id[.区块id[.条目id]]"前缀匹配)+ `SettingCategory.visible` 可选字段,消费端仅 SettingsContent/useSettingModel 与 SettingsSearch 2~3 处单点过滤,上游共享分类文件零触碰;`player` 分类中被隐藏的淡入淡出/响度均衡/EQ/变速项随 SRV-2 落地逐项恢复 visible。
**后端配套**:ENH-4 的 `GET /api/v1/player/devices`(包装现成 `list_output_devices()`,audio_output.rs:160);OutputDeviceSelector.vue 移入 settings/custom,以 `visible: () => isWebMode()` 与桌面设备下拉互斥——**端点可在 B9 之前落地**,B9 后同端点追加 mmap 能力标志,UI 不改版。

#### D.4 首页与统计

**需修复**:FIX-4(本周时长 NaN;且 `getPlayHistoryDaily` 把原始记录直接喂给期望 `DailyPlayStats[]` 的消费方——"错数据"而非"空数据")。
**前端方案**:P0 补 `getStatsSummary` 全零对象(8 字段对齐 `shared/types/stats.ts:28-45`)+ `getPlayHistoryDaily` 规范返回 `[]`。
**后端配套**:ENH-1 四个聚合端点(`/api/v1/stats/summary/play`、`/stats/top?kind=…`、`/stats/hourly`、`/stats/daily`,对齐桌面 `playStats.ts` SQL 语义)。**顺序约束**:先按 atom 式统计会话(§1-G.2:与 Control 数量解耦、<5s 丢弃、同曲 reload 延续)改造 `/stats/record` 写入语义,再建聚合——否则多端/重载重复计数,聚合失真。

#### D.5 音乐库与文件管理

**需修复**:SRV-6(标签编辑/本地曲目删除,入口暂按 UI-3 隐藏,等端点恢复)、SRV-8 之拖放导入(桌面 `getPathForFile` 绝对路径在 Web 恒空)。
**前端方案**:`library.writeTags`/`deleteTracks`/`pickCoverImage` 从 safe proxy 默认失败映射到真实端点;拖放改为浏览器 `File` → multipart 上传。
**后端配套**:`POST /api/v1/library/tags`(lofty 类 crate 写 ID3/FLAC/Vorbis 标签与内嵌封面)、`POST /api/v1/library/tracks/delete`(删除后触发增量重扫)、`POST /api/v1/library/import`(落盘导入目录+触发扫描)。

#### D.6 歌单与队列编辑

**现状**:歌单 CRUD 真实 HTTP 已通,无 P0/P1 项。
**演进**(长期,随阶段四):canonical queue(§1-G.2)——Core 独占队列、generation 令牌、全量快照落库广播、stale boundary 队列权威恢复;前端从"乐观编辑"转为"命令+快照投影"。

#### D.7 歌词

**需修复**:ENH-2(`HttpPlayerClient.load` 硬编码 `externalLyrics: []`、`readLyricFile` 恒失败——本地曲目永远拿不到同名 .lrc);SRV-10 PiP 歌词窗(P3)。
**前端方案**:`httpClient.load` 映射 `payload.external_lyrics`;`readLyricFile` 改走已有免鉴权端点 `GET /api/v1/lyrics/file?path=`(routes.rs:293 现成);PiP 用 Document Picture-in-Picture(Chromium 116+,其余浏览器降级普通弹窗)。
**后端配套**:`load_handler` 探测音源同名 `<stem>.lrc/.srt`(复用扫描目录信息)返回 `external_lyrics`——在 A2.1 拆分后的新 player 模块内实施。

#### D.8 在线音源与账号

**需修复**:UI-3(LoginDialog"打开网页版登录"按钮 stub `{ok:false}`,Web 下隐藏;扫码/Cookie 登录真实可用,保留)。
**前端方案**:如上裁剪。
**后端配套**:SRV-4 流媒体凭证服务端存储(P3,架构级单独评审)——消除 IndexedDB 明文密码,`streaming-api` crate 可作服务端适配层基础。

#### D.9 下载与云盘

**需修复**:UI-3(右键"下载"开启后点击静默失败且误报"已在队列",useDownload.ts:74-78;云盘上传按钮 stub 入队即 error)。
**前端方案**:端点落地前先裁剪入口防误导;SRV-7/8 落地后恢复,侧边栏 `/download` 入口改跟随"服务端能力 + Web 模式"。
**后端配套**:SRV-7 `POST /api/v1/download/start`(复用音源解析,按文件名模板落盘**服务器曲库目录**并写标签,任务表入 SQLite,进度经 WS `download_progress` 复用 scan_progress 通道模式)——headless 语义下"给曲库补货"优于"存到访问设备";SRV-8 `POST /api/v1/cloud/upload`(multipart,带网易 cookie 转发)。

#### D.10 听歌识曲

**需修复**:FIX-2(诚实降级三件套:polyfill 显式 `isSupported:false`、`submitPcm` 返回值兜底防"卡识别中"、UI 隐藏入口)。
**前端方案**:如上;浏览器 `getUserMedia` 采集链路本身可用,保留待 SRV-3。
**后端配套**:SRV-3(P3 维持可选):匹配本质是一次网易 HTTP 调用,已核实可行;卡点仅在指纹计算移植选型(Rust 或浏览器 wasm/worker)。

#### D.11 桌面专属形态

**需修复**:UI-1(窗口控制按钮:WindowControls 根 `v-if` 追加 `&& !isWebMode()` 单点覆盖全部挂载点;桌面歌词按钮;识曲入口)。
**前端方案**:如上,配合 §0.3 第 5 条 C 档不做清单一次性定界。
**后端配套**:无。

#### D.12 主题外观与频谱

**需修复**:ENH-3(Web 直 fetch 跨域封面取色失败,主题色退化)、ENH-6(背景图 dataURL 写 localStorage 触发 5MB 配额异常);SRV-1 FFT 已落地(`0e9d96d`)待回归。
**前端方案**:ENH-3 `fetchRemoteBytes` 失败回退图片代理(`${playerClientUrl()}/api/proxy/image?url=…`);ENH-6 背景二进制迁 IndexedDB(localforage "theme"),store 只留元信息(桌面端同路径受益,单独 PR);SRV-1 收尾:验证 `fftData → playback.setFftFrame` 在 Web 端实际出波形。
**后端配套**:图片代理已有(routes.rs image_proxy_handler),无新增。

#### D.13 横切基础设施(与功能模块正交)

1. **rust-embed 内嵌**(P1,S):headless-server 加 `embedded-assets` feature,默认内嵌、`--web-root` 显式传入时优先磁盘(开发模式)——§3.6"静态资源 .rodata、首屏零磁盘 IO"。
2. **PWA**(P1,M):`vite-plugin-pwa` manifest + SW 预缓存 app shell;SW 不拦截 `/api`——§阶段5"断外网仍可离线控制"。
3. **v1 协议兼容层退役**(P2):以 A2.6 协议版本协商为前置(httpClient.ts:261-307)。
4. **webPolyfill.ts 拆分**(P2):702 行按命名空间拆 `web/` 子模块(纯搬移);与 Host Capability 枚举(atom `src/runtime.ts:5-46` 参照)合并为一份"Web 模式能力契约",替代分散的 stub 判定,并与 D.3 的 webHidden 清单互相引用。
5. **上游同步纪律**(web-compat 第三轮结论):修复收敛于 fork 独有文件(`src/services/client/`、`src/web/`、`webHidden.ts`、`native/headless-server/`),上游共享文件只允许约 10 处单点小改(各 1~10 行);唯一结构性成本是 audio-engine-core 为 fork 拆分产物,SRV-2 若遇上游 Player 方法改名,编译期即暴露,属可感知风险。
6. **实施节奏**:按 web-compat §7 八个批次执行,合计约 11~15 人日;批 1(FIX×7)与批 2(UI 裁剪)可与本文阶段一并行,批 3~8 随对应后端端点成对交付(每模块"前端+后端"一个 PR 组)。

### E 构建部署(scripts/)——P1,M

**规格锚点**:§四全部(GRUB 参数、limits.d、IRQ 亲和)、§阶段6(systemd/生产镜像)。
**现状**:install 与 package 两份 unit 安全配置分叉;无任何内核级调优;手动运行 rtprio 不足已被真机复现为问题。

方案:
1. **新增 `scripts/rt-tuning.sh`**(对齐蓝图 §四逐条):GRUB 参数(`isolcpus/nohz_full/rcu_nocbs/max_cstate/performance/audit=0`,隔离核号按机器拓扑生成并交互确认)、`/etc/security/limits.d/99-hifi-audio.conf`(rtprio 99/memlock unlimited/nice -20)、IRQ 亲和批量绑定 Core 0(保留音频设备中断白名单参数)。**风险控制**:全部改动前置 `--dry-run` 输出 diff,GRUB 改动要求显式确认 + 自动备份原文件;脚本幂等。
2. **unit 模板统一**:package 的 User=root 无沙箱模板改为与 install 同源(非 root + AmbientCapabilities + 沙箱),模板提取为单一文件两个脚本共用;data 目录 chown 逻辑 package 侧补齐。
3. **非 systemd 场景**:install 脚本检测到手动运行时打印 limits.d 提示(真机验收已记录该坑)。
4. `diretta-e2e-check.sh` 扩展 T4(Direct boundary 无 FullReconnect)/T5(30 分钟 RSS 平稳采样)/T6(断链恢复),判据照抄 real-device-acceptance.md——**这项是真机验收的自动化前提,P0**。
5. **runtime.json 发布契约(取自 atom,见 §1-G)**:打包期生成 `runtime.json`(版本/commit/工作区脏标记/SDK release/协议版本/能力位图),发布脚本解包校验、服务端经 `/api/status` 暴露——"构建期写、发布期验、运行期可查"三方一致,是开源发布物交付审计的基础设施。
6. **数据迁移 preflight(取自 atom,见 §1-G)**:db.rs 的 ALTER 迁移(db.rs:174-181)升级为版本检查——库/配置 schema 版本**新于**二进制支持版本时拒绝启动(防降级读坏);真需要迁移前先 `VACUUM INTO` 备份到 `data/backups/migration-<ts>/` + manifest;更新文档补"目录级回滚"(旧目录整体保留,新包 `cp -a` 复制旧 data,严禁旧二进制读新迁移数据)。

### F 测试资产——P0(基建)/P1(数据),M~L

**规格锚点**:§六全部(四项硬性测试)。
**现状**:71+28+28 用例功能面覆盖好;规格验收四项**零数据**;测试基建 5 处重复;Direct 无跨 crate 集成测试。

方案:
1. **`scripts/bit-perfect-verify.py`**(§六.1,P0):生成/读取基准 wav(16/44.1、24/96、24/192)→ 播放 → S/PDIF 回录 → 裁剪对齐 → PCM 块 SHA-256 比对。蓝图已有完整方法学,直接落地;脚本同时支持 ALSA loopback(无录音声卡时的 CI 降级:arecord 直连 loopback 子设备)。
2. **拷机脚本 `scripts/soak-test.sh`**(§六.2,P0):stress-ng(--cpu 4 --io 4 --vm 2 --vm-bytes 1G)+ wrk(-t4 -c100 -d60s 循环)+ B2.2 观测钩子日志采集 + xrun 断言;先跑 8h CI 变体,48h 全量人工触发。
3. **cyclictest 门禁**(§六.3):并入 rt-tuning.sh `--verify` 子命令,输出 Max Latency 与 ≤15μs 判定。
4. **内存审计**(§六.4,P1):B2.2 的 RSS 钩子 + 播放 500 曲轮换脚本(曲目清单参数化),产出 RSS 曲线与缺页计数报告;valgrind massif 按需人工。
5. **测试基建去重**:headless-server 5 个测试文件的 AppState 构造抽 `tests/common/mod.rs`;direct_pcm/direct_dsd 共享测试套(B6.3 的前置)。
6. **补 Direct 集成测试**:audio-engine-core/tests 现 grep "diretta" 为 0——至少补 DirettaConnection 打开/回调/drop 生命周期的 mock-SDK 单测(SDK stub 路径已存在,C 层可直接测)。

### G splayer-atom 对照萃取(2026-09-05,快照基线 383e787c——经 §4 校验即当前 HEAD,无前移)

#### G.0 认知修正(先校准三个前提)

1. **atom 没有 PlayerTransport**——全仓库 grep 无此名;它是我们自己 hardening 文档里的**规划概念**,不是 atom 的实现,此前文档表述有误导。
2. **atom 没有 Rust 层 preloader**——preload 逻辑在 TS 主进程(`playbackPreloader.ts`/`directGapless.ts`),原生层只有与我们就同构的 `stage_local` 预解码进 Direct ring。
3. **两边同源同构**:`DirectTransport` 枚举、`stage_local`、boundary generation 事件链、8 深 slot ring、`THRED_MODE(5)`、契约测试思路全部同款;分叉在增量功能。**方向结论不变**(hardening 批次 F):Diretta/Direct 代码本体不回移(我们更硬化:fade+drain handoff、per-slot pre-mute、断链看门狗、HTTP 源、CUE/SACD staging、能力查询、MTU 缓存、producer RT 调度均为我们独有),**只取架构机制,不取音频代码**。

#### G.1 引擎层亮点(3 项)

| atom 机制 | atom 位置 | 对我们的价值 | 去向 |
|---|---|---|---|
| SDK 可选化构建(`optional diretta-sys` + `sdk` feature + `links` 键) | audio-engine-core/Cargo.toml:18,80 | 无 SDK 机器可编译/跑 CI,解锁 CI 化 | §1-C.5 |
| `PlaybackStream`/`OutputBackend` 单变体枚举扩展点 | audio_output.rs:23-32 | 佐证 B1.1 输出后端抽象的方向:加 ALSA MMAP 后端时 player 层零改动;atom 为多后端预留的正是这个形状 | §1-B1.1 参照 |
| 纯 Rust DST 解码器 | sacd/dst/{decoder.rs 220 行, ac.rs 75 行} | 替代我们 dst_ffi + libdstdec C 依赖的候选 | §1-B4-B6.4 |

#### G.2 Core 层亮点(按价值排序,均已映射到模块方案)

| atom 机制 | atom 位置 | 一句话价值 | 去向 |
|---|---|---|---|
| 协议版本协商前置:`/api/info` 携带 `CORE_PROTOCOL_VERSION`,客户端 range 校验,不兼容抛结构化错误并停 WS 重连 | shared/constants/coreProtocol.ts:3-27、src/core/controlClient.ts:47-57、controlEventClient.ts:157-166 | 退役 v1/v2 双解析的正规路径 | §1-A2.6 |
| "HTTP 快照权威,事件仅增量"重连语义:重连先拉 `/session`+`/status` 再消费事件;写操作同步返回新快照 | controlEventClient.ts:106-119;headless/routes.ts:884-1010 | 消除断线事件丢失窗口,多端天然收敛 | §1-A2.6 |
| Canonical queue:Core 独占队列,每次变更加 `loadGeneration` 令牌 + 全量快照落库(SQLite 单行 JSON)+ 广播;曲终/Diretta boundary 由 Core 提交,stale boundary 走队列权威恢复 | playbackSession.ts:58-128,798-875 | 我们 `pending_next` 单槽是它的最小版;这是"多端仲裁"长期项的完整参照(替代 OT/版本向量,单写者串行即可) | §1-B1 长期项 |
| 纯函数 policy 层(缓存键/可跳过错误集/重试上限/preload 索引,零 I/O) | playbackSourcePolicy.ts:8-76 | 策略可单测;Rust 侧即无状态策略模块 | §1-A2 拆分时落 `policy.rs` |
| stage 串行化链:`directStageChain` promise 链保证 staging/cancel/control 全静默后才 load;preload key 含队列位置+曲目身份+policy;boundary 消费 generation+key 双校验 | playbackPreloader.ts:34,42-54,91,129-147,242-263 | 我们的 renderer 版 gapless.ts 有 generation 无串行链与 policy key——多 Control 并发 stage 时的竞态兜底 | §1-B1.2 附件 |
| native 事件唯一消费入口:所有底层事件集中一处分派给会话/统计/scrobble/重建输出 | corePlayerEvents.ts:25-92 | 我们的 state.rs 回调已接近,补齐"单消费者"纪律即可 | 维持现状 |
| Core 级统计会话:与 Control 数量解耦、<5s 丢弃、同曲 reload 延续、shutdown 结算 | headlessPlayStats.ts:14-71 | 修我们"每次 load 记一次历史"的统计语义(多端/重载不重复计数) | §1-A5 |
| 输出切换事务化:失败回滚旧设备+旧源+seek+播放态,全程 loadGeneration 保护 | playbackSession.ts:922-1015 | 我们的 diretta_select 现无回滚 | §1-A2/diretta_select 增强 |
| runtime.json 发布契约 + check-artifact 发布门禁(含 sourceDirty 诚实标记) | stage-core-system.ts:176-212、check-core-artifact.ts:80-201、coreInfo.ts | 构建期写/发布期验/运行期可查 | §1-E.5 |
| 迁移 preflight:schema 超前拒启动 + `VACUUM INTO` 备份 + manifest + 目录级回滚 | dataMigration.ts:61-140 | 开源发布物的升级/回滚安全网 | §1-E.6 |
| 频率分级推送:position/fftData 不进广播,Web 用可见性门控 500ms 轮询 | broadcast.ts:14-15、controlEventClient.ts:121-146 | 后台标签页零推送,多端带宽下降 | §1-A4.4 |
| Host Capability 显式枚举 | src/runtime.ts:5-46、src/web/api.ts:461-520 | "哪些能力永不属于服务端契约"一份清单说清 | §1-D.13.4 |

#### G.3 bridge 参数对照(新事实)

| 参数 | 我们 | atom | 官方 SinHost |
|---|---|---|---|
| `configTransferAuto` | (100ms, 0, 30µs) | **(200µs, 0, 100ms)** | (200µs, 0, 100ms) |
| `connectPrepare` | (true) 强制重置(E-3 有意) | () 无参 | () 无参 |
| Host 名 | "SPlayer-Next" | "SPlayer Atom" | "test" |

atom 与官方示例**完全一致**且声称实测 PCM 768kHz/DSD512——这是 §1-B4-B6.5 参数 A/B 的最强参照组;连同 §1-B7.5 的 `cycle_time_us` 死计算修复,三件事应同批真机验证。

#### G.4 借鉴纪律

1. 只回移**机制与契约形状**(协议/状态机/发布物),不回移音频代码本体;
2. atom 侧与我们相左的 Diretta 行为(无淡出/无 pre-mute/无在线源)是我们的优势,不跟进;
3. atom 仓库截至 2026-09-05 停留在基线 383e787c(经校验无前移);回移落地前仍应例行确认目标机制现状与其上游(SPlayer-Next)是否已收编。

---

### H CUE 与 SACD ISO 播放缺陷修复(2026-09-05 排查,P0 热修)

排查方式:两条链路(入库→虚拟路径解析→加载→seek/边界→Diretta/cpal 输出)全量走读 + 关键断点人工复核。结论:**3 个 CUE 致命缺陷 + 3 个 SACD 严重缺陷**,其中 CUE 三连在 Diretta 主路径下必现且静默。

#### H.0 共性根因(修复设计的主线)

1. **虚拟路径格式碎片化**:DB/前端用 `cue://…#track=NN`(1 基两位),引擎用管道格式 `物理路径|start|dur|track`,SACD 用 `iso|TrackNN|dur|startFrames|durFrames|startLsn|lenLsn`——三套格式、两个转换点(routes.rs:1147、gapless.ts:59),靠约定不靠类型;
2. **引擎 stage/handoff/seek 三链路不携带起始偏移**:`open_local` 正确应用 `cue_start`(direct_runtime.rs:357-401),但 `stage_local` 把 start 赋给 `_start` 丢弃(:183)、`handoff_drained_source` 丢弃 `_cue_start`(:477)、Direct 轨内 seek 不加偏移(:535-553)——open 单点正确、其余全错;
3. **probe_fast 不识别任何虚拟路径** → staged_meta 对 CUE/SACD 全失效(d4471e9 的 now-playing 转正被架空);
4. **静默回退遍地**:查库失败当普通路径打开、seek 失败 `let _` 吞掉、DST 错误帧填 0x55 无感知——按 §0.4 纪律,这批回退本身就是缺陷。

#### H.1 CUE 缺陷与修复

| # | 严重度 | 现象 | 根因(证据) | 修复 |
|---|---|---|---|---|
| H1.1 | P0 | Diretta 无缝切轨/handoff 从母版 0:00 出声(内容≈第 1 轨),必现、静默 | `stage_local` 丢弃 `_start`(:183)、`handoff_drained_source` 丢弃 `_cue_start`(:477),底层 `prepare_staged_pcm_source`/`replace_pcm_ring` 无起始参数(direct_pcm.rs:1969/2025) | `stage_local(path, start, dur)` 与 `replace_drained_local_source(path, start, cancel)` 增加 start 参数;staged/replace 打开时定位到 start(复用 open_local_at 的定位语义);DirectPlayback 持有 `start_offset` 字段(open 时写入) |
| H1.2 | P0 | CUE 轨内 seek 到 0:30 实际跳母带 0:30(串到前几轨),进度显示整体错位 | `seek_while_paused(position)` 直接绝对定位,未加 cue_start(direct_runtime.rs:535-553;正确参照 decoder.rs:131-135) | seek 前加 `start_offset`:`seek_accurate(start_offset + position)`,`seek_base = start_offset + actual`;H1.1 的字段共用 |
| H1.3 | P0 | 曲终自动接力/边界 commit 播整张母版开头 | 候选源优先 `cueAudioPath`(母版路径)而非 `cue://` 虚拟路径:serverAutoAdvance.ts:65-70、gapless.ts:215-217;f42a9fc 只删了 `?? id` 未纠正优先级 | 候选/commit 源**仅用 `track.path`**(CUE 轨即 cue://,普通轨即文件路径);删除 cueAudioPath 回退(它是展示/库字段,永远不该作加载源);后端候选校验对 `cue://` 增加查库验证,失败即拒绝该候选 |
| H1.4 | P1 | 库重扫/删轨后旧队列加载报"打开本地文件失败" | `cue://` 查库失败时原样下传,被当普通路径 File::open(routes.rs:1140-1156 无 else;direct_runtime.rs:379-381 else 分支) | fail loud:cue:// 前缀查库失败直接报错;删除"当普通路径"的 else 分支 |
| H1.5 | P1 | 无缝边界后 now-playing 停留上一曲(d4471e9 对 CUE 失效) | stage 元数据用 `probe_fast(管道路径)` 预存,probe_fast 不解析管道格式恒 None(routes.rs:1790-1808) | **删 probe 换直传**:stage_next 请求体加可选 title/artist(前端 stage 时已知),直接存 staged_meta——删代码而非加代码 |
| H1.6 | P2 | now-playing 标题/歌手是母版标签非分轨;含 PREGAP 专辑时长偏差;轨尾 FFT 串下一轨(视觉) | 元数据取自母版 tags(routes.rs:1516);parser 忽略 PREGAP/末轨 duration=None;`let _ = reader.seek` 吞错(decoder.rs:428);fft_samples 未截断(decoder.rs:553-562) | title/artist 优先取已查库的分轨记录;seek 失败改报错;fft 截断一行对齐;parser 精度挂 B10 低优先 |

#### H.2 SACD ISO 缺陷与修复

| # | 严重度 | 现象 | 根因(证据) | 修复 |
|---|---|---|---|---|
| H2.1 | P0 | multichannel 类 ISO:扫描后从库消失,或加载必报错 | probe 恒优先 TWOCH 且无回退(scarletbook.rs:763-799);Direct 硬性 `channels==2`(native_source.rs:261-266);decoder.rs:172 probe 硬编码 2ch | probe:twoch 空/损坏回退 mulch;Direct 按 ISO area 实际声道放行(FormatID CHA_6 存在,Target 不支持时报错明确);probe channels 读实际值 |
| H2.2 | P1 | 非 Diretta 输出起播秒级~分钟级延迟、长轨内存暴涨、DST 轨杂音 | cpal 路径仍走已判废弃的整轨提取(一次性读整轨入内存+写临时 DSDIFF,decoder.rs:435-441 → source.rs:508-517/845-944;native_source.rs:5-10 自述两大问题) | 快修:`read_sectors` 分块流式写临时 DSDIFF,内存 O(1)(单点改动);原生 dsd2pcm 流式解码(去 ffmpeg)挂 B10 后续 |
| H2.3 | P1 | DST ISO seek 后/坏扇区出现可闻噪声突发,无法定位 | libdstdec 错误帧 memset 0x55(全幅 Nyquist 方波,非本项目 0x69 约定),Rust 侧 `error_count` 从未自增、注释与 C 行为不符(dst_ffi.rs:214-235;dst_fram.c:428) | dst_ffi on_frame_error 置标志+计数;native_source 解码后错误帧以 0x69 覆写;error_count 接入 e7858db 观测通道(一条日志);C vendor 库不动 |
| H2.4 | P2 | 裸 .iso 路径加载必失败("没有可播放 payload"),自动连播喂入即炸 | 裸路径兜底构造零 LSN 虚拟轨(direct_dsd.rs:79-83);候选校验放行裸 ISO路径(routes.rs:1012-1017) | 裸 .iso 直接 bail("ISO 需经曲库虚拟轨播放");候选校验拒绝无 `\|` 的 .iso 结尾 source |
| H2.5 | P2 | 扫描 Done 阶段持 db 锁全量重解析所有 ISO,大库阻塞所有 API;已删 ISO 的分轨永久残留 | ISO 无 mtime 增量(scanner.rs:378-381);sync_sacd_tracks 在 db 锁内同步解析;LIKE 未转义(db.rs:705-709);孤儿轨无清理 | ISO 按 mtime 跳过(复用 file_records 机制);LIKE 转义复用 CUE 同款;补孤儿轨清理 |
| H2.6 | P3 | 末轨多读 1 扇区(lead-out 垃圾进 demuxer) | `length_lsn = end - start + 1`(scarletbook.rs:818-824)与 exclusive end 叠加 | 核对 scarletbook 语义后去掉 +1 |

#### H.3 修复顺序与预估

1. **引擎偏移链路**(H1.1+H1.2,同一次改动:一个字段+两处传参,1~1.5 天)——P0 主矛盾;
2. **前端候选+fail loud**(H1.3+H1.4,前端两处删回退+后端两处报错,0.5 天);
3. **staged_meta 直传**(H1.5,删 probe,0.5 天);
4. **SACD**(H2.1 → H2.3 → H2.2 快修 → 其余,1~2 天)。
排期:整体插入**阶段〇(热修,先行 2~4 天)**,优先级高于阶段一(用户可感知的播放正确性 > 结构性债)。


## 2. 实施阶段与依赖

```
阶段〇(CUE/SACD 播放热修,2~4 天,先行)
  H1.1+H1.2 引擎偏移链路 → H1.3+H1.4 候选与 fail loud → H1.5 staged_meta → H2.1/H2.3/H2.2
  验收:CUE 专辑 Diretta 无缝切轨出声正确、轨内 seek 正确、曲终接力正确;multichannel ISO 可入库可播;DST seek 无噪声突发
阶段一(结构性债,1~2 周)
  A2.1 拆分 routes ─┬─→ A2.2 锁统一 ──→ A4 快照缓存化
  C.1 死代码处置 ───┤
  E.4 e2e T4-T6 ────┤
  B7.3 Target 自愈(Reboot/pingTarget,半响应痛点)┤
  B7.5 cycle_time_us 死计算修复(E-1 半接线)────┘
阶段二(L2 纯内存播放,1~2 周)
  A1 配置项 → B8 ram_buffer 接线(64B 对齐/mlock/双池)→ B4.1 ReadSeek 源 → B1.2 load 协议接入
  验收:§3.1 量化判据
阶段三(L1 ALSA MMAP,2~3 周)
  B9.1 alsa_mmap_sink → B9.3 优先级/绑核 → B1.1 输出后端抽象(参照 atom OutputBackend 形状,G.1)→ D.3 设备选择 UI(含后端 devices 端点)
  验收:§六.1 回录哈希 + §六.3 cyclictest
阶段四(规格补齐与硬化,2 周)
  B6.2 DoP → B3.1 余弦淡出 → D.13.1 rust-embed + D.13.2 PWA → E.1 rt-tuning.sh
  顺带:B7.2 latency 导出 / E.2 unit 统一 / C.4 -nolog 选型 / C.5 SDK 可选化门控(G.1)
  G.2 机制回移:协议版本协商+快照权威重连(A2.6)→ 统计会话解耦(A5/E)→ runtime.json+迁移 preflight(E.5/E.6)
阶段五(商业级质量验收,1~2 周 + 48h 拷机墙钟)
  F.1 bit-perfect → F.2 拷机 → F.4 内存审计 → 真机 T1-T6 全量执行
  顺带(真机在场才做):B4-B6.5 SDK 传输调优实验(MS 模式/THRED_MODE 矩阵/TRIANGOLO pacing;含与 atom 同参 A/B,G.3)、E-2 注释修正
  发布检查:SDK 使用条款声明入 README/发布包(风险表第 1 条,开源非商用分发即满足)
  验收:§六四项全数出数据
```

并行原则:A2 拆分与 B8 无耦合可双线;B6.3 镜像去重挂起至阶段五后视情况启动;web P0×7 随时可插入。

## 3. 风险清单

| 风险 | 缓解 |
|---|---|
| **SDK 授权条款(透明化,非闸门)**:memo_host.txt 明确"非商用授权,并入付费产品/服务/软件需商业授权;不提供保修与支持;禁止逆向工程;用于商业硬件平台/商业软件的部署场景需商业授权(无论软件是否收费)" | **项目持续开源(承上游精神),非商业分发无需商业授权**——按 §0.0 定位,本项降级为"条款透明"动作:README 与发布包声明 SDK 来源与授权边界,提示商业化部署场景的使用者自行取得授权;不阻塞任何发布 |
| SDK 仅支持 Linux 且仅静态库、无源码 | 与项目 Linux headless 定位一致;SDK 行为黑盒化,一切调优以真机 A/B 为准(bridge 契约测试锁死行为边界) |
| alsa_mmap 与 cpal 双后端行为漂移 | validate 门槛统一收口在 B1.1;位纯真测试对两个后端各跑一遍 |
| DoP 触碰契约测试红线 | 断言范围收窄为"native DSD 段",DoP 走 PCM 家族,语义互斥;DoP 支持与否 SDK 无法探测,必须用户显式开启 |
| ram_preload 大文件 OOM | memfd 物化已有 2GiB 上限先例;RamTrackBuffer 同上限 + 加载失败回退流式 |
| rt-tuning 搞挂生产机 | --dry-run 默认 + GRUB 备份 + 交互确认;文档标注不可逆项 |
| 镜像去重引入音质回归 | 共享测试套先行;或永久挂起(镜像代码是稳定代码) |
| isolcpus 对 Web 服务吞吐的影响 | 隔离核数可配(默认 2 核),控制面跑剩余核,wrk 压测验证 |
| Target 程序化重启(B7.3)误伤正常设备 | 默认关闭自动重启,仅告警 + Web 手动确认按钮;Reboot 前置 pingTarget 双确认 |

## 4. 基线校验记录(2026-09-05,基线 e7858db)

对本方案全部关键代码事实做了一轮全量校验(自方案撰写基线 dd57aba 起 HEAD 前移 17 个提交,工作区另有未提交改动)。

### 4.1 结论:方案仍然成立,无需结构性修改

以下事实经逐项复核**全部维持**:

| 校验项 | 结果 |
|---|---|
| B7.5 E-1 回退残留(cycle_time_us 死计算/THRED_MODE 注释不符/固定 100ms-30µs 传参) | ✅ 成立(bridge.cpp:218-220/226-227/272-278) |
| A2 routes.rs 3,091 行、load_handler 494 行(1058-1551)、双 NCM Router、watchdog 100-305 | ✅ 成立 |
| A2 锁风格不一:volume async 直锁(715)vs play/pause/stop 隔离线程(670/690/700) | ✅ 成立 |
| A4 snapshot() 持 player 锁调 7 getter(286-304),无缓存层;WS 500ms(2601) | ✅ 成立 |
| B8 ram_buffer.rs 零调用点 | ✅ 成立 |
| C shim 三文件不参与编译;ncm-api-rs 绝对路径(Cargo.toml:39) | ✅ 成立 |
| B9 cpal 逐样本迭代+逐样本乘法(354-355)、优先级 70、回调线程无 RT/绑核 | ✅ 成立 |
| D FIX-1~7 全部未修(AboutSettings.vue:29 等)、OutputDeviceSelector 仍孤儿、v1 兼容块仍在(261-305) | ✅ 成立 |

### 4.2 修正项(已回填正文)

1. **B7.5 措辞**:补充 E-1 的"实施(fcd4802)→部分回退(f3b989f)→残留死计算"历史,定性从"半接线"改为"回退残留";
2. **C.2 措辞**:build.rs 的 env 变量优先已存在,硬编码仅为回退段——方案改为"保留 env 优先、仅改造回退段",原表述夸大了问题;
3. **G 前提**:atom 仓库实际停留在 383e787c 无前移(此前"活跃变动"判断系工作目录混淆所致),G.4 第 3 条改为例行核对建议。

### 4.3 并行进展盘点(dd57aba..e7858db,17 个提交)

均来自**另一条并行线**(《Diretta 硬化与上游同步方案》,3a185cd/971abc7),与本方案互补、无冲突、不构成重复:

- 可靠性:输出恢复重载前强制重建 Direct 连接 + 停滞重试上限 + 跳候选冷却(b9c84f6 + 工作区 120s 冷却)、无缝预载修复(319594c)、自动连播候选修复(f42a9fc)——F11 看门狗的持续演进,本方案 E.4 的 T4-T6 判据可直接复用其日志;
- 功能:now-playing 服务端权威快照 + 边界自动转正(d4471e9)、重开页面恢复(56fa461)、输出设备持久化自恢复 + 探测硬超时(807131c)——属现有架构内的功能补全,与本方案 A4 的"快照缓存化"(消除锁竞争)不重叠;
- 观测:load/stage 观测日志(e7858db)——利好 §六拷机数据采集;
- 测试/打包:T1-T3 验收脚本(0d520eb)、CAP_DAC_OVERRIDE 保留(dccf807)。

工作区未提交改动仅一处功能性变更(OUTPUT_RECOVERY_SKIP_COOLDOWN=120s,输出恢复跳候选冷却),其余为 rustfmt;与本方案条目无重叠。

### 4.4 最优性复核意见

1. **优先级排序不变**:阶段一(结构性债)→ 阶段二(L2 纯内存播放)→ 阶段三(L1 ALSA MMAP)→ 阶段四/五的顺序在校验后依然最优——并行线在现有架构内持续修补可靠性,反证了"A2 拆分 + A4 快照缓存化"的必要性(看门狗/恢复逻辑正堆积在 3,091 行的 routes.rs 里,每加一个功能都在加深债务);
2. **B7.5 提升为阶段一首批**:回退残留是死代码 + 误导注释,修复成本低且不依赖真机(删除或接线的决策可以先做,A/B 留待真机);
3. **新增一条协调约定**:并行硬化线与本方案都在改 routes.rs/watchdog 区域,开工前先落 A2.1 拆分(或至少约定改动窗口),避免两条线在同一文件上冲突。
