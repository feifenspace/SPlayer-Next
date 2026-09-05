# Headless 模式与 Diretta 输出路径：多轮代码分析修复方案汇总

> 整理自多轮分析的全部结论与明细：
> ① 代码质量全面评估（注释质量 / 精简度 / 效率 / 可维护性，3 个子代理报告 + 人工复核）；
> ② 纯内存（memfd preload）+ 零拷贝（ring slot 直达 SDK 回调）链路专项分析；
> ③ Diretta 输出路径分层专项分析（§一：全链路数据流、逐层验证与分层评价）；
> ④ 播放队列控制专项与自动连播方案（§八：三形态控制归属、Q1-Q4 问题与 A/B/C 分层方案）；
> ⑤ Electron --headless 形态考古与移除记录（§九，已实施）。
>
> 生成日期：2026-09-04。分析基线：commit `6f69997`（Direct 播放中同格式 handoff）。
> 复核修订（2026-09-04 R2）：新增 F11；撤销 F7"持锁设备切换"误判；修正 F2 quad 机制描述；统一 §五批次顺序；§八事件载荷对齐实际枚举。明细见 §十。
> 实施记录（2026-09-05 R3）：§五 批次 1-10 已全部实施并独立提交（`bd76c9e`…`89c47cb`，共 11 个提交）；新增 §十一 §六-§九 收尾方案（批次 A-D）与逐项状态盘点。
> 审查范围：`native/headless-server/`（约 5.6k 行）+ `audio-engine-core` Direct/diretta 输出路径（约 8.5k 行），逐行通读 + 高危项人工复核。

## 一、Diretta 输出路径全链路

### 1.1 数据流

```text
客户端 POST /player/load (source, direct_selector="diretta:fe80::x%2")
  │
  ▼ routes.rs load_handler ── spawn_isolated_blocking("player-direct-load-worker")
  │
  ├─ 阶段1 物化: materialize_direct_input()               （缺陷 → F3/F4/F5）
  │    · preload 模式: HTTP 下载 → memfd (/proc/self/fd/N) 或磁盘缓存
  │    · stream 模式 (在线 PCM): 跳过物化，url 下沉为 HttpAudioSource 流式读
  │    · DSD 原生 (DSF/DFF/ISO): 强制 preload（chunk 定位需要 seekable）
  │
  ├─ 阶段2 handoff-first: try_direct_handoff_sequence()    （缺陷 → F7/F8）
  │    · 同格式 → 源级淡出 → 排空等待 → 块边界原子换源（不拆 Diretta 连接）
  │    · 不同格式/失败 → 回退阶段3
  │
  ├─ 阶段3 全量重连: DirectPlayback::open_local / open_reader
  │    ▼ diretta.rs DirettaDirectConnection::open_*_at
  │      · DirectPcmSource 打开 FFmpeg、同步解出首帧填 slot[0]（format 先于连接确定）
  │      · splayer_diretta_open_direct(target, rate, ch, bits, ctx, next_block, release_block)
  │
  ▼ diretta-sys bridge.cpp: connectPrepare() → connect(0) → connectWait()
  ▼ Diretta SDK 发送线程: getNewStream() → direct_pcm_next_block(ctx) → 拿裸指针
  ▼ IPv6 网络流 → Diretta Target 硬件
```

### 1.2 分层职责与逐层验证

| 层 | 实现 | 关键设计（均经逐行验证） |
|---|---|---|
| API 编排层 | routes.rs load/handoff | 三段式：锁内快照 → `spawn_isolated_blocking` 慢操作隔离 → commit；load token 竞态保护贯穿全程（结论见 §二）。缺陷在锁内慢事与双宿主重复（F7/F8） |
| 物化层 | routes.rs:543-694 | memfd 纯内存 / HttpAudioSource 流式 / DSD 强制 preload 三模式；memfd 思路成立（§二），外围缺陷见 F3-F5 |
| Direct 运行时 | direct_runtime.rs `DirectPlayback` | open / handoff / seek / play / pause / fade 全生命周期编排 + `DirectMonitor` 观测（consumed_position / transition_count / boundary_generation） |
| SDK 桥接层 | diretta.rs + diretta-sys bridge.cpp | ① **Drop 顺序正确**：先 `splayer_diretta_close` 停 SDK 发送线程，再释放 ring（diretta.rs:386-391），注释写明因果；② **DSD wire bit order 协商**：in-out 参数协商 + `set_wire_bit_order_while_paused` 适配，失败立即关连接（diretta.rs:438-447），桥接层禁止 DoP/DSD2PCM 采样域转换（有测试守护）；③ **握手期静音回送**：SDK 工作线程握手期间持续索要数据，回 false 会终止线程卡死 `connectWait`——参照 tinyLMS 回送静音块（bridge.cpp:429-439）；④ `getNewStream` 纯指针转发，零拷贝零锁（bridge.cpp:59-69），且有**源码级测试守护**（diretta.rs:598-621 断言回调体禁含 memcpy/mutex/new）；⑤ 两处 `unsafe impl Send` 均有明确注释依据 |
| 数据面 | direct_pcm.rs / direct_dsd.rs | 四态 slot 环 + 裸指针交付（状态机与内存序结论见 §二）；producer 绑性能核 + SCHED_FIFO（direct_pcm.rs:2123-2129），命令驱动（mpsc + `command_pending` 提示位 + 单一 Condvar 信号），无忙等；首帧同步解码填 slot[0]，保证 open 前格式已知（2080-2119）；repack 路径预分配（2096-2109）；回调侧 `direct_pcm_next_block` 无锁无分配（2553-2572）。**唯一内存安全破口 F1** |
| handoff 防御 | routes.rs:750-806 | 三级：① 格式预检（家族/采样率/声道，stream 模式跳过粗检交权威校验）；② 排空谓词 `drained(4)`——渐零完成 + 交付 4 块数字静音，**silence_blocks 计数顶掉设备端缓冲的旧音频**（direct_pcm.rs:1454），对网络音频协议设备端缓冲的清醒认识，单纯本地 fade 不够；③ commit 块边界校验兜底，失败回退全量重连。设计成立，实现编排缺陷见 F7/F8 |

### 1.3 分层评价

| 维度 | 评价 |
|---|---|
| 架构分层 | 清晰：编排(API) → 物化 → 运行时 → 桥接 → 数据面，职责单一 |
| 零拷贝纪律 | 优秀，且有源码级测试守护（工程上少见） |
| 实时安全 | 回调无锁无分配、producer 绑核 + RT 调度，纪律好 |
| 协议理解 | 排空计数顶设备缓冲、握手静音回送、DSD wire 协商——协议细节处理到位 |
| 内存安全 | 主链正确，1 个破口（F1） |
| 资源上界 | 物化层缺失（F3/F4），编排层锁耦合（F7） |
| 可维护性 | 双宿主重复（F8）是最大债务 |

一句话：核心设计（零拷贝 slot 环 + 源级淡出排空 + 块边界原子换源）懂行且健壮，问题集中在"边缘的工程卫生"（F1、F3-F8）与 headless 独有的可用性缺口（F11），核心数据面无需重设计。

## 二、总体结论

架构质量良好，以下核心设计经逐行验证为正确、保持不动（各层实现细节与交叉验证见 §一）：

| 环节 | 结论 |
|---|---|
| 零拷贝主链（解码 → slot → SDK 回调） | FFmpeg 直接 `read_frame` 进 slot 内 `UnsafeCell` frame，`frame.payload_ptr()` 裸指针交付，生命周期由四态状态机保护，无 UAF |
| 状态机与内存序 | `FREE→FILLING` CAS（AcqRel）、`READY` 发布（Release）、consumer 消费（Acquire）配对正确 |
| 单一 IN_FLIGHT 模型 | `release_in_flight` 释放语义正确 |
| DSD 静音 | per-slot `UnsafeCell<Box<[u8]>>` 0x69 预填（PDM 直流偏置防满幅爆音），扩容仅发生在 FILLING 独占期 |
| load token 竞态保护 | 单调 AtomicU64 + commit 等值比对 + `task_final_token` 防旧任务回退重拆，正确且 self-contained |
| 线程生命周期 | `join_aux` 有界 join、take 路径先 bump token 再移交、worker 以 token 失配快速退出，无线程泄漏，全路径最干净的部分 |
| Condvar 信号次序 | 排除了丢失唤醒 |
| `spawn_isolated_blocking` | 隔离 FFI 崩溃的设计有明确 why，不算问题 |
| direct handoff 淡出排空设计 | 源级淡出 → Condvar 事件驱动排空 → 块边界原子切换，设计成立（问题在实现编排，见 F8） |
| memfd 纯内存思路 | 字节全程不落盘，方向正确（实现缺陷见 F3、F4） |

**但有 2 个正确性 bug、2 个结构性问题和 1 个可用性缺口需要优先处理**（F1、F2、F7、F8、F11，F11 为复核轮新增）。

## 三、修复项总表

| 编号 | 问题 | 核心风险 | 溯源 |
|---|---|---|---|
| F1 | PCM 静音缓冲共享导致 UAF | 内存安全 | 评估轮 H2；专项轮 ① |
| F2 | downmix 按奇偶数映射声道 | 可听见的正确性 bug | 评估轮 H1；专项轮 ② |
| F3 | materialize 双重 GET / 双 client / 下载无上限 | 资源浪费 + 内存无界 | 专项轮 ④⑤；评估轮 M2 |
| F4 | memfd 保活依赖 `/proc/self/fd` 路径 | 隐式安全 + fd 编号复用 | 专项轮 ③ |
| F5 | handoff URL 打开无取消句柄 | supersede 后 join 最长 256s | 专项轮 ⑥ |
| F6 | DSD ReplaceLocal 与 staged 路径行为分叉 | 时间显示跳变 | 专项轮 ⑦ |
| F7 | 全局锁内慢事（600ms 排空 / 无上界 close；"持锁设备切换"经复核撤销，见 F7 表） | 卡顿全局负载 | 评估轮 H3 + M3；专项轮补充 |
| F8 | handoff 编排双调用侧重复 ~350 行且行为漂移 | 可维护性 + 真实缺陷 | 评估轮 H4 |
| F9 | 注释违规 / 超长函数 / 结构重复（S1-S4） | 可读性、可维护性 | 评估轮 P3/P4 |
| F10 | headless-server 局部效率与健壮性（M 级 + 低级明细） | 加固 | 评估轮 M1/M4-M7/M9/M10 |
| F11 | headless 输出失败/断链无服务端恢复（OutputFailed 被吞、Direct 无失活信号） | 可用性：故障后静音挂死 | 复核轮新增（对照 Electron ipc/player.ts 恢复链路） |

## 四、修复项详述

### F1｜PCM 静音改 per-slot 缓冲（消 UAF，P0）

**问题**：`native/audio-engine-core/src/direct_pcm.rs:2302-2311`，静音块存进共享的
`silence_buffer: Mutex<Option<Vec<u8>>>`，指针交付后，producer 填下一块静音时：

```rust
if buffer.len() < bytes {
    *buffer = vec![0u8; bytes];   // 旧 Vec 被 drop，而旧指针可能正被回调读取
}
```

换曲后块尺寸变化几乎必然触发 realloc → 上一块静音若仍 IN_FLIGHT，SDK 回调读到悬空指针。
触发窗口窄（换源 + 块尺寸变大 + 旧静音块在途三者同时），但实时路径不可接受。
与 DSD 侧已采用的 per-slot 设计自相矛盾。

**修复**（评估轮曾提出方案 A 按最大块预分配只 memset / 方案 B ArcSwap retired；采用 per-slot，
与 DSD `fill_slot` 语义对齐，两套代码同构）：slot 结构新增，删除共享 `silence_buffer` 字段：

```rust
/// 每 slot 独立静音缓冲：仅在 SLOT_FILLING 独占期扩容，交付后只读
silence: UnsafeCell<Box<[u8]>>,
```

producer 静音分支（处于 CAS FREE→FILLING 成功之后，独占安全）：

```rust
let silence = unsafe { &mut *slot.silence.get() };
if silence.len() < bytes {
    *silence = vec![0u8; bytes].into_boxed_slice();   // 扩容只发生在独占期
}
slot.payload_ptr.store(silence.as_mut_ptr(), Ordering::Relaxed);
```

PCM 静音即 0x00（signed 零点），预填一次后永不重写。

**验证**：静音淡出场景（关流后连续换曲触发静音块重分配）+ 既有断言。

### F2｜downmix 增益表化（消奇偶猜测，P0）

**问题**：`native/audio-engine-core/src/direct_pcm.rs:512-530`，`if ch % 2 == 1 { l } else { r }`。
FFmpeg 平面序是语义序（FL FR FC LFE BL BR SL SR），按奇偶分左右完全错：

- 4.0（quad，FL FR BL BR）：planes[2]=BL 在奇偶循环之前就命中 FC 分支
  （`!planes[2].is_null()` 判定），被当中心 −3dB 混入双耳；planes[3]=BR 奇数进 L——
  与 8 声道特化分支（`direct_pcm.rs:498-501`，bl→L、br→R）恰好相反；
- 5.1（FL FR FC LFE BL BR）：ch3=LFE 奇数被灌进左前，ch4=BL 进 R、ch5=BR 进 L，环绕颠倒；
- 7.1（FL FR FC LFE BL BR SL SR）：ch4-ch7 的 BL/BR/SL/SR 全部左右颠倒。

凡声道数无特化分支的多声道文件（4/5 声道等）都受影响。同一映射在 6 处副本重复
（i16/i32 × planar/packed：512/603/649/697/745/790 行附近）。

**修复**：调用方（持有 frame）生成归属表，传入各 downmix 函数替换 6 处副本；
先修 bug，再按 F9-S1 收敛重复。注意计数表的天生盲区：quad（FL FR BL BR）无 FC，
planes[2] 就是 BL，纯按声道数、只覆盖 idx≥3 的表修不到 idx2 的错位——快修需同时给
`channels == 4` 加 idx2 特例（planes[2] 按 BL 计 `l += 0.5`，跳过 FC 分支）：

```rust
/// 按声道数返回 idx>=3 声道的立体声归属：FFmpeg 平面序 FL FR FC LFE BL BR SL SR
/// （0 = 弃用，L/R = 归入左右，BC = 双侧）
fn downmix_channel_targets(channels: usize) -> [u8; 8] {
    match channels {
        //        idx3  idx4  idx5  idx6  idx7
        4 | 5 => [b'R', b'L', 0,    0,    0   ],  // BL→L BR→R（或 SL/SR，归属相同）
        6     => [0,    b'L', b'R', 0,    0   ],  // LFE 弃
        7     => [0,    b'B', b'L', b'R', 0   ],  // 6.1：BC 双侧 0.35，SL→L SR→R
        _     => [0,    b'L', b'R', b'L', b'R'],  // 7.1
    }
}
```

循环体查表加权（`b'B'` 时 l/r 各加 0.35，其余单侧 0.5，沿用原衰减系数）。
推荐改为按布局建表（改动量相当）：open 时用 AVFrame 自带的 `ch_layout` 语义序
（`av_channel_layout_channel` 逐声道展开）生成 0..channels 的完整归属表，一次覆盖
quad 的 idx2 错位与 6.0/6.1 等罕见布局；上面的计数表仅作 ch_layout 不可得时的回退。

**验证**：4.0 / 5.1 / 7.1 测试文件确认左右声像与 LFE 剔除。

### F3｜materialize 单次 GET + 单 client + 下载上限（P1 快赢）

**问题**：`native/headless-server/src/api/routes.rs`

- L635 先发一个 GET（body 从未消费即丢弃），memfd 分支 L675 再建一个 client、
  `download_to_memfd`（L562）重新 GET 一次 → memfd 命中时每曲目浪费一次完整请求往返；
  （复核修正：子代理"整个文件双重下载"说法过重——reqwest body 是惰性的，drop 后不会传完整文件）
- `io::copy` 无上限，目标还是 RAM → 坏 URL / 异常大响应可 OOM（磁盘回退 L688 同样无界）。

**修复**：

1. `download_to_memfd` 改签名接收已建立的 response（内部 GET 删除），首个 GET 的 response
   直接移交，header 嗅探与下载共用——双重 GET 与双 client 同时消除；Linux 分支只构建一次 client；
2. 上限常量 + `take` 截断（memfd 与磁盘回退共用）：

```rust
/// preload 物化上限：防失控响应打爆内存；覆盖 DSD128 整轨约 2.5GB/h 的量级
const DIRECT_PRELOAD_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

let mut limited = response.take(DIRECT_PRELOAD_MAX_BYTES);
std::io::copy(&mut limited, &mut file)?;
anyhow::ensure!(limited.limit() > 0, "在线音源超过 preload 大小上限");
```

### F4｜memfd 保活显式化：消灭 `/proc/self/fd` 路径（P2 结构性）

**问题**：`routes.rs:543-596`。registry 保留最近 3 个 File，路径 `/proc/self/fd/N` 绑定的是
**fd 编号**，挤出 = close 该 fd。当前恰好不可达（load 串行、materialize 后立即 open、staged
深度 1），但安全性完全依赖编排时序；更隐蔽的是 **fd 编号复用**：旧 fd close 后新 fd 恰好同号，
路径会指向另一个文件，静默解码错误内容。

**修复**（正解，让 FFmpeg 直接读 File）：`DirectPcmDecoder::open_reader(Box<dyn ReadSeek>)`
已存在，`File` 本身就是 `Read + Seek`：

```rust
enum DirectInput {
    Path(String),        // 磁盘缓存回退
    Memfd(std::fs::File) // 纯内存：File move 进 decoder，生命周期由其持有
}
```

- materialize 返回 `DirectInput`；PCM 侧 `Memfd ⇒ open_reader(Box::new(file))`；
- `/proc/self/fd`、registry、挤出策略全部删除；
- DSD 侧若无 `open_reader` 等价物，用同一个 `AvioReader` 包装补一个（组件现成）。

收益：悬空竞态与 fd 编号复用从结构上不可能，registry 及"保留 3 个"的魔法数字消失。

**验证**：preload 播放全程无磁盘读（`/proc/self/fd` 不再出现）。

### F5｜cancel handle 贯通 handoff 打开链（P1）

**问题**：`direct_pcm.rs:919` 用 `HttpAudioSource::new(source)`（无 cancel），而 stream 路径
（routes.rs:1065）带 cancel handle。supersede staged candidate 时无法掐断连接，producer join
要等它自己超时——连接超时 5s、退避最长 256s，最坏情况换源卡 4 分钟。
（同源发现：评估轮 3.2 "Drop 路径 join 可被网络打开阻塞"。）

**修复**：

- staged candidate 增加 `cancel: HttpCancelHandle`，supersede/替换时 `cancel()`；
- `DirectPcmDecoder` 增加带 handle 的打开入口（内部走 `HttpAudioSource` 的 cancel 构造，
  即 stream 路径同款），producer join 从最坏 256s 退避变成即时返回。

### F6｜DSD ReplaceLocal 对齐 staged 语义（P1 快赢）

**问题**：`native/audio-engine-core/src/direct_dsd.rs`，`replace_dsd_ring`（L919-940）无
boundary/transition_count 标记、立即覆盖 `duration_micros`；`install_staged_dsd_slot`
（L898-917）有完整 boundary 标记。同一"换源进 ring"语义两套行为，时间显示在 ReplaceLocal
切换瞬间跳变。（评估轮 S1 曾标记"是否有意待确认"，专项轮已复核定性为分叉，需修。）

**修复**：`replace_dsd_ring` 复用 `install_staged_dsd_slot` 的安装逻辑（或抽出共享 apply
函数），补齐 boundary 标记，`duration_micros` 更新时机与 staged 一致。消除双路径分叉后，
"换源进 ring"只剩一条语义。

### F7｜全局锁内慢事外化（P1）

**问题**：`state.player.lock()` 是覆盖全部播放器状态的单把 parking_lot Mutex，
以下慢操作全部发生在锁内：

| 慢事 | 位置 | 上界 |
|---|---|---|
| handoff 排空等待 `direct_wait_fade_drained`（锁 guard 存活于整个 condvar 等待期） | `routes.rs:789-793、1030-1043`；`bindings/player.rs:183-192、802-811`；stop 路径 `player/mod.rs:521-529` | 600ms（`DIRECT_FADE_DRAIN_TIMEOUT`），期间 seek/pause/status/并发 load 全部排队，HTTP 线程同被占用 |
| 持锁 drop `DirectPlayback` → SDK `splayer_diretta_close` → `disconnectWait` | `player/mod.rs:536-538`；`transition.rs:376、405`（token 失配路径）；`diretta.rs:656-661` 测试断言证实 | 对端异常时**无上界** |
| ~~持全局锁执行设备切换~~ | ~~routes.rs:2308-2336~~ | 复核撤销：`set_output_device`（player/mod.rs:209-212）仅存储设备串，微秒级、无网络握手；select 的真实缺陷是延迟生效，移入 F10-M11 |
| `volume_handler` 直接 async 持锁，与其他 handler 的 `spawn_isolated_blocking` 模式不一致 | `routes.rs:460-470` | — |
| `query_target_caps` 自述同步阻塞 2-3s | `diretta.rs:161-201` | 2-3s；scan_devices 在独立路径，是否会被锁内路径间接触发**待确认**，至少应把"调用方不得持锁"写成显式契约注释 |

**修复模式统一**（`Option::take`/clone 出句柄 → drop guard → 锁外等待/drop → token 重查一次）：

```rust
// transition.rs：direct_wait_fade_drained 拆为"取句柄"+"锁外等待"两段
let handle = {
    let state = self.state.lock();
    state.direct_playback.as_ref()
        .ok_or_else(|| anyhow!("direct playback 已被移除"))?
        .drain_signal()            // Arc<DrainSignal>
};
handle.wait_timeout(DIRECT_FADE_DRAIN_TIMEOUT);   // 锁外等待
// 等待期间只可能是 token 被取代，token 校验在等待后重查一次即可
```

drop 路径同理：先 `Option::take` 把 playback 移出状态，drop guard 后再 drop 对象。
音量 handler 如需统一可顺带隔离（现 `set_volume` 轻量，非必需）；设备切换无锁内慢事，
其缺陷在延迟生效与不可达校验（F10-M11），故障后的恢复/重建归 F11。

### F8｜handoff 编排下沉 core 单一入口（P1）

**问题**：direct 加载分支在两个调用侧重复实现，整体重复度约 75-80%
（两侧 direct 相关合计约 450-500 行中 ~350 行可由 core 单一实现替代）。逐段实测：

| 逻辑块 | routes.rs | bindings/player.rs | 重复度 |
|---|---|---|---|
| 编排函数 | `try_direct_handoff_sequence` 747-806 | `try_direct_handoff_commit` 145-199 | ~95% 逐字 |
| 原生 DSD 判定 | `is_native_dsd_source` 739-745 | 136-143 | 100% 逐字 |
| 结果枚举 | `DirectLoadOutcome` 703-711 | 125-134 | 100% 同形 |
| DRAIN 常量+注释 | 698-701 | 50-64（另 `player/mod.rs:100-105` 第三份） | 三份 |
| load 锁内 take 段 | 846-886 | 661-706 | ~95% 同构 |
| worker 编排段 | 928-1109 | 721-863 | 同构 |
| 回退重拆 | 1023-1055 | 795-824 | 同构 |
| superseded 判定 | 1007（contains） | 779（contains） | 同构，文案不同（696 vs 48） |

worker 段 4 点实质差异：① stream 模式支持仅 routes 有（929-967、1064-1085）；
② **open 后启动验证仅 bindings 有**（`wait_for_direct_start` 66-94 + `open_verified_direct_playback`
96-123，调用点 832-839；routes 1077-1085 直接返回成功）——重复已产生真实缺陷；
③ 回退稳定 sleep：routes 硬编码 800ms（1061），bindings 用常量（53-56、829-831）；
④ superseded 文案英文 vs 中文。

伴生问题：`LOAD_SUPERSEDED` 靠 `format!("{err:#}").contains(文案)` 做控制流分叉
（routes.rs:1007/1138、bindings:779），改一处措辞即静默破坏 superseded 语义。

**修复**：下沉到 `audio-engine-core::InnerPlayer` 单一入口，无工程障碍
（两侧拿到的本就是同一类型，`headless` feature 已就绪，`direct_runtime` 无条件编译）：

```rust
// audio-engine-core/src/player/transition.rs：唯一编排入口
pub struct DirectHandoffSource {
    pub path: String,
    pub track: Option<u32>,
    pub format: DirectFormat,
    pub start_pos: u64,
    // ...（同时消除 commit_direct_handoff 的 8 参数签名，transition.rs:491-533）
}

impl InnerPlayer {
    /// 保留 token → 淡出 → 锁外排空 → 块边界换源；失败回退全量重连。
    /// 调用侧只传源描述，不再各自拼装序列。
    pub fn direct_load(
        &self,
        src: DirectHandoffSource,
        stream: Option<DirectStreamParams>,   // routes 需要，bindings 传 None
    ) -> Result<u64, DirectLoadError>;        // Superseded | Timeout | ...
}
```

superseded 改结构化错误（定义在 core，随下沉一并）：

```rust
#[derive(Debug, thiserror::Error)]
#[error("load superseded")]
pub struct LoadSuperseded;

// 调用侧：
match res {
    Err(e) if e.is::<LoadSuperseded>() => DirectLoadOutcome::Superseded,
    ...
}
```

下沉顺带消除：三份 DRAIN 常量、两种 superseded 文案、魔数 vs 常量分歧、启动验证缺失、800ms 魔数。

### F9｜工程质量批次（P3/P4）

#### 9.1 注释审查明细（对照项目规范逐类）

| 类型 | 位置 | 处置 |
|---|---|---|
| 文本损坏 | `direct_pcm.rs:2126-2127`"造成推流抗跟　2"误编辑残留；DSD 版同段（direct_dsd.rs:1058-1059）语句完整 | 修复；复制式维护单向劣化的实锤 |
| 分隔线注释（明令禁止，约 17-18 处） | routes.rs 132/401/1784/1939/1997/2080/2267/2571；online_apis.rs 194/299/452/556/637/785；db.rs 891/1145/1238；direct_pcm.rs:3544 | 规范要求拆文件替代；routes.rs:941/969/1020 的"阶段 1/2/3"变体同时是 load_handler 的天然拆分边界 |
| 注释内编号列举（明令禁止） | `player/mod.rs:517-551`（stop_internal `// 0.`~`// 6.` 共 7 处）、routes.rs:2461/2480、db.rs:649/656、direct_pcm.rs 测试 `// 断言 1~4`（3673-3696） | 步骤多到需要编号就该拆函数 |
| 复述显然之事 | player/mod.rs:377、online_apis.rs:152、db.rs:1086、main.rs:54、routes.rs:2232/2240/2247（WS 循环逐行解说） | 删除或改写为真正的 why |
| 跨文件注释漂移 | `direct_dsd.rs:653-654` vs direct_pcm.rs ring 段：unsafe 安全论证 PCM 版完整、DSD 版残缺 | **必须补齐**——这是要保留的注释，单侧成立的前提被误用于另一侧会出安全问题 |
| 散文式多段 | `direct_dsd.rs:15-23`（0x69 静音 PDM 直流偏置 why，内容好） | 压缩为一段保留 |
| 重复注释 | DRAIN 常量语义注释三份（见 F8） | 下沉后只留一处 |
| 待斟酌 | headless-server 全部 60+ 函数为 `///` 散文式文档注释 | CLAUDE.md 的 JSDoc 示例面向 TS；Rust 侧全项目惯例即 `///` 中文散文，不判违规，若要统一需全项目决策 |

核查结论：`direct_runtime.rs` 的 `#[cfg]` 分支注释（66 行区域）与实际 cfg 组合行为**一致，无过时**。
评估轮曾报"routes.rs:1025 注释与 1061 sleep 矛盾"，复核不成立——该注释描述的是排空等待本身，属实。

#### 9.2 拆函数

| 函数 | 位置 | 行数 |
|---|---|---|
| `load_handler` | routes.rs:809-1265 | ~457 |
| producer 主循环 ×2 | direct_pcm.rs / direct_dsd.rs | 各 ~300 |
| `seek_handler` | routes.rs:1276-1416 | 141 |
| `fs_browse_handler` | routes.rs:2453-2569 | 117 |
| `diretta_target_info_handler` | routes.rs:2344-2445 | 102 |

#### 9.3 结构去重（S1-S4，不做预防性重构，留到下次功能性改动同一区域时顺带）

- **S1**：direct_pcm.rs ↔ direct_dsd.rs 约 700-900 行结构性复制（约占 DSD 生产代码 60-70%：
  四态状态机、producer 主循环、命令通道、C 回调、常量集合逐行同构）。复制成本已兑现：
  F2 的 bug 在 6 处副本放大、注释损坏（9.1）、F6 的行为分叉。策略：下次改动任一侧
  ring/producer 时抽 `DirectRing<P>` 泛型骨架。
- **S2**：diretta.rs 两个 connection 委托类 ~70 行逐字镜像（247-391 vs 393-507，九个方法），
  泛型 `DirettaDirectConnection<H>` 或内部函数表收敛。
- **S3**：db.rs 三段 17 列手写 upsert SQL（277-304/419-452/613-640）+ `NOT IN` 保护子查询
  散落 9 处（728/752/776/801/833/1292/1299/1306/1313）+ `get_library_stats` 4 次独立查询合一
  （1289-1325）。SQL 片段收敛为常量/CTE。
- **S4**：direct_runtime.rs 三份 cue/sacd/DSD 判定解析块（118-151/276-310/395-428，达 3+ 抽取
  门槛）——handoff 预检与实际 open 若单边修改会格式错配，抽 `resolve_direct_source` 纯函数。

#### 9.4 cfg 条件编译收敛

direct_runtime.rs:479-630 每个方法写三套 `#[cfg]` + 两处 `unreachable!()`，而 `DirectMonitor`
（28-103）已示范正确模式：Fake 作为 enum 变体，cfg 只出现在变体声明处。照此改造
`DirectTransport` 后 `unreachable!()` 全部消失（顺带统一 500/525/608/628 裸 `unreachable!()`
与 549/567 带消息的风格）。

#### 9.5 可读性细节与死代码

| 项 | 位置 | 处置 |
|---|---|---|
| `commit_direct_handoff` 8 参数（`#[allow(too_many_arguments)]`） | transition.rs:491-533 | 聚合 `DirectHandoffSource`（F8 前置） |
| `take_threads_only` 名不副实（只 bump token 不碰线程），时序敏感路径上易误用 | transition.rs:437-447 | 改名 `reserve_direct_handoff_token` |
| blanket `From<E>` 把所有错误压成 500 且丢错误链，与 routes.rs:1219 注释意图相悖 | error.rs:51-55 | anyhow 贯穿、响应边界转 `ApiError` |
| `LoadQuery.cancel_handle_id` 死字段 | routes.rs:29-34、814、1404-1406 | 删除 |
| `dsd_max_sample_rate` 键同一 `json!` 写两次（后者静默覆盖） | routes.rs:2435/2440 | 删除重复键 |
| CORS `parse().unwrap()` 一条非法配置打崩启动 | routes.rs:363 | 改 anyhow 报错 |
| 单调用点两层包装 | direct_pcm.rs:973-989 | 合并 |
| `1 % slots.len()` 无意义取模 | direct_pcm.rs:2132/2153/2164 | 删除 |
| 冗余 `use PathBuf` / 冗余 `create_dir_all` | routes.rs:474 / 604 | 删除 |
| `take_for_async_load` 与 `take_for_async_seek` ~20 行结构性重复 | transition.rs:92-135 vs 189-245 | **不动**——语义确有差异（seek 保留解码位置），不为合并而合并 |
| `handoff_local_while_paused` 命名与实际播放中调用时机不符 | direct_runtime.rs:390-452 | **待确认**后改名（见 §六） |

### F10｜headless-server 局部效率与健壮性（P2）

| # | 位置 | 问题 | 建议 |
|---|---|---|---|
| M1 | online_apis.rs:18-24 | `HTTP_CLIENT` 设 10s **total** timeout，却被流式音频代理使用（797-860）——30-60MB FLAC 慢速上游下 10s 处被硬截断 | 流代理改用只设 `connect_timeout` 的独立 client |
| M3 | routes.rs:460-470 | `volume_handler` 直接 async 持锁（原表含 diretta_select，复核撤销——见 F7 表与 M11） | 见 F7 |
| M4 | routes.rs:2129-2161、2164-2197、1536、2103 | cover/lyric 文件读取、`probe_fast` 音频探测等同步 IO 直接在 async handler 执行，阻塞 worker 线程 | 包进 `spawn_blocking` |
| M5 | db.rs:402-589 | `sync_cue_tracks`（及 sync_sacd_tracks）在**事务内**做 cue 解析、封面缩略图、音频探测（秒级），期间所有 DB 访问阻塞 | 解析/探测移到事务外，事务内只写 |
| M6 | online_apis.rs:344-415、463-548 | QQ client 每分支重复构造 **11 处**、酷狗 **9 处**，每处锁 DB + 查 cookie + clone；kugou user_detail 同一调用内 508/514 重复读 cookies | 入口构造一次传入 |
| M7 | routes.rs:2223-2265 | WS 同一位置变化走 500ms 全量快照 + broadcast 事件双通道，多客户端时全量序列化放大 CPU/带宽 | 二选一或快照只做兜底 |
| M9 | main.rs:18-21 | `listen_addr` 解析失败**静默回退 0.0.0.0:14558**——写错 `127.0.0.1` 会静默绑定全网卡；叠加在线代理路由在鉴权层之外，暴露面不受控扩大 | 解析失败报错退出 |
| M10 | db.rs:1243-1259 | `play_history` 只插不清，7×24 运行无限膨胀 | 插入时顺带按时间清理 |
| M11 | routes.rs:2308-2336 | `diretta_select_handler` 仅登记设备串（`set_output_device` 只存储，player/mod.rs:209-212），对当前曲目不生效、下次 load 才应用，也不验证目标可达性；与 Electron 的 `set_output_device → reinit_output()` 即时重建不一致 | select 后经 F11 恢复路径即时重建生效；短期至少做连接验证并在响应中明确"下一曲生效" |
| L1 | online_apis.rs:670-687 vs 700-717 | TIDAL auth_exchange / auth_token_poll cookie 持久化逻辑重复 | 抽公共段 |
| L2 | online_apis.rs:599-605、744-750 | qobuz/tidal logout 先清本地再调远端（远端失败本地已丢） | 先远端后本地 |
| L3 | routes.rs:714-737 vs 1243-1259 | `direct_load_response` 与 load_handler 非 Direct 响应 JSON 两处重复 | 收敛 |
| L4 | main.rs:56-120 vs config.rs:78-88 | 手写参数解析 60 行，config override 逻辑与 env 重复；`--host/--port` 与 listen_addr 交互隐晦（122-124） | 收敛到 config |
| L5 | db.rs:246-250 | `remove_scan_dir` LIKE 前缀未转义（`%`/`_` 可越界匹配） | 转义 |
| L6 | routes.rs:347-348 vs online_apis.rs:27-30 | NCM router 双实例，且在鉴权层之外 | 单实例并纳入鉴权 |
| L7 | db.rs:486-494 | cue 每分轨重复查询母版参数 | 按 physical_path 缓存 |
| L8 | online_apis.rs:806-810 | stream_proxy 默认 Referer 硬编码 qq | 配置化 |
| L9 | routes.rs:514/589、online_apis.rs:251 等 | 魔法数：保留 2 首/registry 3 个/10MB/128 | 具名常量 + why |
| L10 | config.rs:102-106、130 | QOBUZ_PROXY 隐式副作用；cors_origins 默认 "*" 使 localhost 条目冗余 | 显式化 |
| L11 | online_apis.rs call_* | call_netease/call_qqmusic 参数顺序不一致 | 统一 |
| L12 | 多处 | `serde_json::to_value(x).unwrap_or_default()` 吞错模式重复 | 至少记日志 |
| L13 | routes.rs:1895-1904 vs 1920-1929 | playlist add/remove track_ids 提取重复 | 2 处未到 3 处门槛，**不抽** |

安全面补充：NCM/平台代理/图片代理路由在 protected 鉴权外（L6、M9 叠加），总体暴露面需一并收敛。

### F11｜headless 输出失败与链路中断无服务端恢复（P1，复核轮新增）

**问题**：恢复链路只有 Electron 侧存在：

- 事件语义（`player/events.rs:21-23`）：`OutputStalled` 自述"需要外部重建输出"，`OutputFailed` 自述"由 JS 侧触发输出重建"；
- Electron：`ipc/player.ts:208-217` 对 outputFailed/outputStalled 均 `requestReinit` → `device.ts` 重建输出（瞬态失败延迟 300ms 重试一次，持续失败交设备事件/用户操作并置 `outputBroken` 标记），闭环完整；
- headless：`state.rs:168` 把两个事件直接吞掉（`OutputStalled | OutputFailed => {}`）；headless-server 全仓无 reinit 等价物（`reinit_output` 是 NAPI 绑定层实现，core 未提供）；`direct_runtime.rs` / `diretta.rs` 中 stall/failure/watchdog/recover 零命中；
- Direct 链路额外缺口：拉模型下 SDK 停止拉流只表现为 ring 填满、producer 阻塞、位置冻结，**没有任何失活事件**——即使接上 OutputFailed 处理，Diretta 断链（Target 断电/离网）依然无人察觉。`DirectMonitor` 已观测 `consumed_position`（direct_runtime.rs:28-103），具备检测素材。

**后果**：任意输出故障后播放器停留在 Playing（推静音或进度冻结），WS 持续广播假状态，§八 B 层自动连播永不触发；Web Control 场景浏览器关闭/后台化时无人能救，只能重启服务。

**修复**：

1. cpal 路径：state.rs 事件回调对 `OutputFailed`/`OutputStalled` 投递独立恢复任务（遵守"回调中不碰 player"的既有约束）：取 `current_source` + 最近 position → 全量 reload → seek 恢复；带有限重试（对齐 Electron 现行为：瞬态失败短延迟重试一次，持续失败进入暂停态并经 WS 上报），重试耗尽交用户操作；
2. 恢复编排与 bindings 的 `reinit_output` 同构——按 F8 同向下沉 core（`InnerPlayer` 单一入口），避免 headless 再长出第三份三段式；短期可先在 routes/state 侧拼装粗恢复兜底；
3. Direct 失活检测：`DirectMonitor` 增加 stall 判定（playing 态 `consumed_position` 冻结超阈值，或 `is_online` 持续为 false）→ 复用 `PlayerEvent::OutputStalled` 上报，进入与 1 相同的恢复路径；阈值与检测点取决于断链后 ring/producer 的最终表现（§六新增确认项，需实测）。

与 §八 A 层的关系：A 层把事件转发给客户端只是可观测性，本项才是恢复本体，两者互补。

**验证**：播放中 kill Target / 断网 / 拔出 cpal 设备，30s 内恢复播放或进入可感知暂停态；WS 快照不残留假 Playing。

## 五、统一实施路线图

| 批次 | 内容 | 性质 | 风险 |
|---|---|---|---|
| 1 | F1 + F2 | 两个真实 bug（UAF + 声像颠倒），均限于 direct_pcm.rs | 最低，收益最高 |
| 2 | F11 | 输出失败/断链服务端恢复：先粗恢复（reload + seek + 有限退避），Direct 失活检测随后 | 中，需实测断链表现（§六新增确认项） |
| 3 | F3 + F6 | 小改动快赢：省一次请求 + 上限；消 DSD 分叉 | 低 |
| 4 | F7 + F8 | 锁外化 + 编排下沉（顺带消除 DRAIN 三份、superseded 文案、800ms 魔数、启动验证缺失） | 中，需回归 handoff 全链路 |
| 5 | F5 | cancel 贯通，独立验证 supersede 行为 | 中 |
| 6 | F4 | 结构性改造（返回类型变化 + DSD 补 open_reader） | 中高 |
| 7 | F10 | headless-server 局部效率与加固，机械性、可小提交分批 | 低 |
| 8 | F9 | 注释清理 + 拆函数；S1-S4 结构去重**留到下次功能性改动同一区域时顺带** | 低 |
| 9 | 队列 A 层：WS 协议统一信封 + 事件转发补全（§八 Q2/Q3） | 纯协议层（state.rs/routes.rs + Web 客户端） | 低 |
| 10 | 队列 B 层：候选预注册 + 后端自动连播（§八 Q1/Q4） | **依赖批次 4（F7）**；Direct 无缝路径复用 staged API；前端约 30 行 | 中，需回归自动接力竞态 |

每批独立提交、独立回归；批次 1-2 完成后内存安全、正确性与可用性问题即清零。
顺序依据：先清正确性与可用性（批次 1-2），快赢随后（批次 3）；F7 与 F8 触碰同一段编排代码，合并一批回归 handoff 全链路；结构性改造（F4）待行为修复稳定后进行；加固与清理殿后。

## 六、待确认项（修前需先验证意图）

| 项 | 位置 | 说明 |
|---|---|---|
| `handoff_local_while_paused` 命名与调用时机 | direct_runtime.rs:390-452 | 名字说 while_paused，但调用链在"源级淡出→排空"之后的播放态；实现内部语义需对照 direct_pcm.rs 佐证后改名 → **已核实**：换源瞬间设备消费的是排空后的数字静音，"paused" 指源侧静默而非播放器状态（播放器保持 Playing）；改名 `handoff_drained_source` 归批次 A（§11.2） |
| `query_target_caps` 是否会被锁内路径间接触发 | diretta.rs:161-201 | 是则放大 F7；无论如何把"调用方不得持锁"写成显式契约注释 → **已核实**：全仓仅 2 个调用方（select 可达性验证 / target_info），均在 `spawn_isolated_blocking` 内，无锁内调用；契约注释归批次 A（§11.2） |
| Direct 链路失活时的最终表现 | direct_runtime.rs / bridge.cpp | SDK 停止拉流后是 ring 填满、producer 阻塞还是静默挂起？决定 F11 watchdog 的检测点（consumed_position 冻结 vs is_online）与阈值 → F11 已实现（批次 2 `ad8d6a5`，采用 consumed_position 冻结 + pending_blocks 守卫）；阈值与断链表现待真机验收 |
| ~~DSD 换源无 boundary 是否有意~~ | ~~direct_dsd.rs:919-940~~ | 已由专项轮定性为分叉，归 F6 修复 |
| ~~routes.rs:1025 注释与 1061 sleep 矛盾~~ | — | 复核不成立，撤销 |
| ~~Web Control 现有客户端是否在维护~~ | httpClient.ts / WS 协议 | 已决策：客户端"信封优先 + v1 兼容回退"，服务端 v2 单发无需双发过渡（批次 9 `bf869ce`）；旧标签页优雅降级 |
| ~~队列本体是否上后端~~ | §八 B 层 | 已决策并实施单槽候选（批次 10 `04397d7`）；完整多端队列仲裁不做 |

## 七、残余风险记录（明确不修）

| 项 | 说明 |
|---|---|
| `HttpAudioSource` RECV_TIMEOUT=10s | 网络停顿超 ring 深度（约 0.7s）会欠载，由数字静音兜底——设计内行为，可接受 |
| SACD 路径每块 Vec 分配 + 一次额外拷贝 | 非热点路径，轻微开销，不值得为此加复杂度 |
| `spawn_isolated_blocking` 每操作新建 OS 线程 | 有明确 why（FFI 崩溃隔离），设计取舍 |
| `play()` 两个分支结构重复、`pause` 与 `pause_immediately` Direct 分支逐字重复 | 2 处未到 3 处门槛；后者属"改一处忘一处即分叉"高危区，随 DirectPlayback cfg 收敛（9.4）一并处理 |
| `fs_browse_handler` has_children 每子目录 read_dir | 可接受 |

## 八、播放队列控制与自动连播方案（Web Control 场景）

### 8.1 现状结论

队列 100% 由前端控制，headless-server 是无队列的单曲播放机（哑后端，data-plane only）。三种形态的对照：

| 形态 | 队列控制者 | 结论 |
|---|---|---|
| 桌面 | renderer 全链路：queue store → candidate.ts → nextTrackPreloader → gapless.ts → events.ts | 完整闭环 |
| Electron --headless | 无（renderer 不存在） | 已移除（见 §九） |
| headless-server | 引擎事件完备但决策外置：ended/sourceError/directTrackBoundary 经 state.rs:102-171 实时转发 WS；staged 预载 API（routes.rs:1425-1498）齐备但需客户端逐曲调用 | 决策者离场即断连播（Q1） |

关键锚点：

- 前端决策链：`events.ts` 的 `ended → finishCurrentTrack`（单曲循环 seek(0)+play，否则 nextTrack）；`candidate.ts` 的 `getNextTrackCandidate`（+1 取模循环、随机模式队尾不缓存、DJ 过滤）
- **后端曲终事件是实时推送的**（state.rs:141 `{"type":"ended"}`）——曾误判"只能靠 500ms 快照轮询"，已修正；停播不是因为事件缺失，而是因为接收方（浏览器）没了
- 关浏览器瞬间播放**不会**停：后端无任何"客户端断连 → 暂停/停止"联动，前端也无 unload 停播调用（唯一 beforeunload 在 stats.ts:99，仅统计上报）

### 8.2 问题清单

> 状态（R3）：Q1 后端自动接续已实施（批次 10，**前端接线归批次 C**）；Q2 信封已实施（批次 9）；Q3 的 outputFailed/Stalled 已转发（批次 2），FftData 订阅与 Seeked 事件归**批次 B**；Q4 最小解已实施（批次 10）——详见 §11.1。

| 编号 | 问题 | 影响 |
|---|---|---|
| Q1 | **断连播**：曲终 `ended` 推给空无一人 → 无人 load 下一曲 → 播完即停。浏览器关闭、移动端后台标签被系统冻结均复现 | Web Control 核心可用性 |
| Q2 | WS 三种 schema 混杂：500ms tick 裸推 PlayerSnapshot（7 字段，routes.rs ws_run）/ 裸 WsState（4 字段无 type）/ typed 事件 / scanProgress 第四种。客户端靠"有没有 type 字段"启发式分派 | 协议脆弱，加事件即破坏客户端解析 |
| Q3 | 事件转发不全：FftData / OutputStalled / OutputFailed 被 `_ => {}` 吞掉；PlayerEvent 无 Seek 变体（seek 确认靠下一个 Position 事件兜底，最多延迟 500ms） | 可视化缺失、设备失速无感知 |
| Q4 | 多客户端无队列仲裁：任意端可同时 load、无候选所有权概念 | 多端控制踩踏 |

### 8.3 方案（A/B/C 三层，可独立实施）

#### A 层：WS 协议统一信封（解 Q2/Q3，低风险先行）

全部消息统一 `{type, data}` 结构，事件命名对齐桌面协议：

```jsonc
{ "type": "snapshot",            "data": { /* PlayerSnapshot 7 字段 */ } }
{ "type": "ended",               "data": {} }
{ "type": "sourceError",         "data": {} }                    // SourceError 现为无载荷事件；需携带 source 先扩枚举
{ "type": "directTrackBoundary", "data": { "duration": 240.0, "generation": 3 } }
{ "type": "outputFailed",        "data": {} }                    // 新增转发（现无载荷）；恢复本体在 F11
{ "type": "scanProgress",        "data": { /* 现有结构 */ } }
```

- state.rs 转发表收敛为单一 match 产信封；ws_run 的 tick/scan 同样套信封
- 兼容性：现有客户端靠裸字段解析会破坏 → WS 子协议头（`Sec-WebSocket-Protocol: splayer.v2`）协商，v1/v2 短期双发（是否需要取决于 §六确认项）
- FftData 仅在客户端显式订阅时转发（降采样），OutputStalled/OutputFailed 直接转发

#### B 层：候选预注册 + 后端自动连播（解 Q1，核心价值）

把"客户端逐曲驱动"改为"客户端注册一次，后端自动接力"——浏览器降级为遥控器，遥控器离场不影响播放：

**API**：

```text
POST   /api/v1/player/queue/next-candidate   {"source": "...", "duration_hint": 240.0}
DELETE /api/v1/player/queue/next-candidate
```

**后端状态**（AppState 新增，generation 防旧候选串代）：

```rust
pub struct PendingNext {
    pub source: String,
    pub duration_hint: Option<f64>,
    pub at_generation: u64,
}
pub pending_next: Arc<RwLock<Option<PendingNext>>>,
```

**自动接力**（在 state.rs 事件回调处；遵循现有"回调中不碰 player"的防死锁设计，只投递独立任务）：

```rust
PlayerEvent::Ended => match take_valid_pending_next() {
    Some(next) => spawn_auto_advance(next),   // load + play；失败推 sourceError
    None => push_ended_event(),               // 无候选：现行为不变
},
PlayerEvent::SourceError => {
    take_pending_next();                      // 候选失效，交还客户端决策
    push_source_error();
}
// OutputFailed/OutputStalled 不作为接力触发：恢复归 F11，恢复失败进入暂停态交客户端决策
```

**两条路径**：

1. **Direct 路径（真无缝，近零新代码）**：注册候选且当前 Direct 模式 → 复用 `direct_stage_next_handler` 的物化 + `stage_local`；`DirectTrackBoundary` 到达且候选已 stage → 自动 commit（等价于现在客户端调 `direct_commit_boundary_handler`）。前置：F7（候选注册不能卡全局锁）
2. **通用路径（亚秒级，非真无缝）**：注册时后台 `spawn_isolated_blocking` 预物化（复用 materialize 的 HTTP→RAM 能力）；ended 后 load 已物化本地文件，省网络握手，间隙从秒级压到百毫秒级。sample-accurate 无缝仍是 Direct handoff 独有

**边界语义**（与桌面对齐）：不注册候选（单曲/autoClose/随机队尾）→ 播完即停，现行为不变；随机跨尾换候选 → 客户端曲中段 POST 覆盖；多客户端仲裁（Q4）最小解 = 后写覆盖 + WS 推 `nextCandidateChanged`。

**重开浏览器接管已现成**：httpClient 3 秒自动重连 + 拉 `/api/status` 快照恢复 UI，无需新代码。

**代价**：后端 state.rs + routes.rs 约 100-150 行，无新依赖；前端 httpClient 两个方法 + 播放事件处注册，约 30 行；引擎（audio-engine-core）零改动。

#### C 层：PlayerTransport 传输抽象（远期可选）

将 core/player 的 `load/play/pause/seek/事件流` 抽为接口，本地实现（IPC → napi）与远程实现（HTTP+WS → headless-server）并存，queue/candidate/preloader/gapless 的全部队列智能在 Web 端原样复用，B 层退化为远程模式降级保底。工程量大，B 层稳定后再评估。

## 九、Electron --headless 形态：考古与移除记录（已实施）

### 9.1 考古结论

Electron --headless 与 headless-server 由**同一晚相差 16 秒的两个 commit** 同时引入（`91c7fe6` 22:52:48 新增 headless 分支 + headless-server 全套；`bbab555` 22:53:04 新增构建脚本与打包配置）——它是 audio-engine 迁移期验证"主进程 napi 引擎无显示环境可跑"的**脚手架**，从未获得完整控制面，不是产品形态。

三处残废证据：

1. **默认无控制入口**：无窗口 + `externalApi.enabled` 默认 false + `wsEnabled` 默认 false（shared/defaults/settings.ts:142-145）→ 启动后是无人能控制的进程（唯一活口是 MCP server）
2. **环境变量名 bug**：`start:headless` 设 `SP_PLAYER_HEADLESS=1`，mode.ts 读 `SPLAYER_HEADLESS` → env 从不生效，全靠 `--headless` argv；argv 缺席时会在无显示环境尝试建窗而崩溃
3. **自带控制 API 半残**：`/next`、`/prev`、`playTrack`、`addToQueue` 等走 `sendToMain` 转发给 renderer（playerControl.ts:31-39），窗口不存在时静默丢弃（broadcast.ts:24-27）——控制通道依赖被自己禁止存在的 renderer；仅 play/pause/stop/seek/volume 直达引擎有效

### 9.2 移除执行记录

| 文件 | 改动 |
|---|---|
| electron/main/core/mode.ts | 整文件删除（`isHeadless` 定义） |
| electron/main/core/index.ts | 删 import；删 Chromium `--headless`/`disable-gpu`/禁硬件加速分支；窗口创建去条件化；修正遗留注释 |
| package.json | 删 `start:headless`、`build:linux:headless` |
| electron-builder.config.ts | 删 `HEADLESS_BUILD` 变量；desktop entry 固定 orpheus 协议关联 |

净 -18 行（-25/+7）。验证：全局 grep `isHeadless|HEADLESS_BUILD|start:headless|SPLAYER_HEADLESS|core/mode` 零残留（renderer 侧本就无引用）；`pnpm typecheck:node` 通过；桌面模式行为零变化（原分支恒 false 路径改直线代码）。环境变量名不一致 bug 随模式消失。

> 注：上述改动与本文档当前均在工作区**未提交**（git status：M `electron-builder.config.ts` / `electron/main/core/index.ts` / `package.json`，D `electron/main/core/mode.ts`），实施时随对应批次一并提交。

### 9.3 遗留问题

- **sendToMain 静默丢弃在桌面模式仍存在**：renderer 崩溃或未就绪时，外部 API 的 `/next` 等队列级调用无声失败且无日志（broadcast.ts:24-27 无窗口分支）——与形态无关的桌面 bug，建议加 warn 日志，归入批次 7（F10 加固）顺带处理 → 批次 7 实际未覆盖，已改排批次 A（§11.2）

## 十、修订记录

| 日期 | 修订内容 |
|---|---|
| 2026-09-04 R2 | ① 新增 **F11**：headless 输出失败/断链无服务端恢复（Electron 有完整恢复链路，headless 吞事件且 Direct 链路无失活信号），插入批次 2；② 撤销 F7"持锁设备切换（网络握手级）"误判（`set_output_device` 仅存储设备串），select 延迟生效移入 F10-M11；③ 修正 F2 quad 机制描述（planes[2] 命中 FC 分支而非奇偶分支），快修表补 quad idx2 特例、按布局建表升级为推荐方案；④ 消除 §五脚注与批次表的顺序矛盾，重排批次并同步引用；⑤ §八 sourceError/outputFailed 载荷与实际枚举对齐；⑥ §九注明改动未提交状态 |
| 2026-09-05 R3 | 批次 1-10 全部实施完毕（`bd76c9e`…`89c47cb` 共 11 提交）；新增 **§十一** §六-§九 收尾方案（批次 A-D）；§六/§九 逐项标注核实结论与排期 |
| 2026-09-05 R4 | **批次 A-D 已全部实施**：批次 A（§九移除提交 `fa64298`、sendToMain warn 与 query_target_caps 契约注释 `3b18f7c`、`handoff_drained_source` 改名 `b608cf4`）；批次 B（FFT 订阅转发 + seeked 事件 `0e9d96d`）；批次 D（DirectTransport cfg 收敛净 −51 行 `293ca0a`）；批次 C（自动连播前端接线 `4990b9c`，浏览器手工验收待执行）。§六 待确认项全部销项 |
| 2026-09-05 R5 | 新增 **批次 E/F**（对照 tinyLMS-old 与 splayer-atom，见 `hardening-and-upstream-sync-plan.md`）：E-2 排空等待/停滞阈值按码率放宽（`5443b04`）、E-1 传输周期按 MTU 动态计算 + THRED_MODE(289)（`fcd4802`）、F1 Diretta stage 门控放宽（`de148d4`）、F2 甄别为不适用；源码守护断言同步（`5443b04`）。上游同步 SOP 已文档化（`hardening-and-upstream-sync-plan.md` §4，取代 troubleshooting 故障 4 旧指引） |

## 十一、§六-§九 收尾方案（批次 A-D）

> 前置：§五 批次 1-10 已全部实施并独立提交——`bd76c9e`(F1+F2) → `ad8d6a5`(F11) → `1a73e14`(F3+F6) → `ca1a8f9`(F7+F8) → `08eb9a8`(F4) → `2e27271`(F10 部分) → `939f191`(F9 部分) → `bf869ce`(队列 A 层) → `04397d7`(队列 B 层) → `89c47cb`(DirectInput 警告标注)。净 +1295/−573，18 文件。
> **状态（2026-09-05）：批次 A-D 已全部实施**——批次 A `fa64298`(§九移除提交) + `3b18f7c`(sendToMain warn / 契约注释) + `b608cf4`(改名)；批次 B `0e9d96d`(FFT 订阅 + seeked)；批次 D `293ca0a`(cfg 收敛，净 −51 行)；批次 C `4990b9c`(前端接线)。
> 唯一遗留：批次 C 的浏览器手工验收（关闭浏览器验证自动接续 / 双标签后写覆盖 / 单曲循环），以及 F11 断链阈值的真机回归。

### 11.1 逐项状态盘点

| 章节 | 项 | 状态 |
|---|---|---|
| §六-1 | `handoff_local_while_paused` 命名 | 待处理（语义已核实：换源瞬间设备消费排空后的数字静音，"paused" 指源侧静默而非播放器状态）→ 批次 A |
| §六-2 | `query_target_caps` 锁内触发 | 已核实无锁内调用（select 验证 / target_info 两调用方均在 `spawn_isolated_blocking` 内）→ 剩契约注释，批次 A |
| §六-3/4 | DSD boundary / 注释矛盾 | 已销（批次 3 / R2 撤销） |
| §六-5 | Web 客户端维护 → v1 双发？ | 已关闭：客户端"信封优先 + v1 回退"，服务端 v2 单发（批次 9）；旧标签页优雅降级 |
| §六-6 | 队列是否上后端 | 已决策并实施单槽候选（批次 10）；完整仲裁不做 |
| §七 | 残余风险 | 维持不修；唯一活口是 play/pause 重复随 9.4 cfg 收敛 → 批次 D |
| §八-Q1 | 断连播 | 后端已实施（批次 10 候选单槽 + 看门狗自动接续）；**前端接线未做** → 批次 C |
| §八-Q2 | WS 信封 | 已实施（批次 9，信封优先 + v1 回退） |
| §八-Q3 | 事件转发 | outputFailed/outputStalled 已转发（批次 2/9）；剩 **FftData 订阅转发**与 **Seek 确认事件** → 批次 B |
| §八-Q4 | 多端仲裁 | 已实施最小解（后写覆盖 + `nextCandidateChanged`，批次 10） |
| §九 | Electron --headless 移除 | 改动已实施但**仍未提交**；9.3 sendToMain warn 未加 → 批次 A |

### 11.2 批次 A：§六 销项 + §九 收尾（小，低风险）

1. **提交 §九 移除改动**：`electron-builder.config.ts`、`electron/main/core/index.ts`、`mode.ts` 删除、`package.json`（typecheck 已验证），独立 commit。
2. **§九-9.3**：`broadcast.ts` 的 `sendToMain` 无窗口分支加 `playerLog.warn`——外部 API `/next` 等队列级调用不再无声失败。
3. **§六-2 契约注释**：`query_target_caps` 补"调用方不得持有 player 锁——内部同步阻塞 2-3s"。
4. **§六-1 改名**（语义依据：换源瞬间设备消费排空后的数字静音，"paused" 指源侧静默）：`handoff_local_while_paused` → `handoff_drained_source`，PCM 内部 `replace_local_while_paused` → `replace_drained_local`（DSD 同步）；涉及 direct_runtime ×2、direct_pcm、direct_dsd、diretta.rs 包装器与 5 处测试调用。

### 11.3 批次 B：§八 Q3 补全（A 层-2，中）

5. **FftData 显式订阅转发**：新增 WS 客户端→服务端消息 `{type:"subscribe", data:{fft:true}}`，服务端按订阅状态转发 FFT（降采样至 ~20Hz），默认关闭——解决 state.rs 吞 FftData 的同时避免多客户端带宽放大。
6. **Seek 确认事件**：`PlayerEvent` 新增 `Seeked { position }`（commit_seeked 成功后发射），state.rs 信封转发 + 客户端处理——消除 seek 确认依赖 500ms Position 兜底的延迟。

### 11.4 批次 C：§八 Q1 前端接线（B 层收尾，中高——唯一需浏览器手工验收的批次）

7. **注册候选**：`core/player` load 成功路径调 `getNextTrackCandidate()`（candidate.ts 现成）→ `registerNextCandidate(source, duration)`；单曲循环/随机队尾/无下一曲不注册（= 播完即停，与桌面对齐）。
8. **ended 抑制 + 采纳**：`IPlayerClient` 增加 `supportsServerAutoAdvance` 能力标志（Http → true）；headless 模式下 `ended` 不再本地 `nextTrack`（避免与服务端接力双重加载竞态），改为监听 WS snapshot 的 `current_source` 变化 → 从队列按 source 定位曲目更新 media store 与歌词（复用"重开浏览器接管"逻辑）。
9. **§六-6/Q4 关闭确认**：后写覆盖 + `nextCandidateChanged` 已实现（批次 10）。

### 11.5 批次 D：§七 活口 = 9.4 cfg 收敛（中）

10. `DirectTransport` 增加 `cfg(test)` 的 `Fake` 变体（照 `DirectMonitor` 已验证的模式），direct_runtime.rs 每方法三套 `#[cfg]` 收敛为单 match、全部 `unreachable!()` 消失；顺带合并 `play`/`pause` 的 Direct 分支逐字重复（§七第 4 条随之关闭）。

### 11.6 明确不做（维持 §七 结论）

`HttpAudioSource` RECV_TIMEOUT=10s、SACD 逐块 Vec 分配、`spawn_isolated_blocking` 每操作建线程、`fs_browse` has_children 遍历。

### 11.7 验收方式

- 批次 A/D：cargo check + test + typecheck，无行为变化（批次 A-4 改名除外）。
- 批次 B：`wscat` 观察 subscribe 后 FFT 事件到达；seek 后 `seeked` 事件延迟 < 100ms。
- 批次 C：浏览器播放 → 关闭浏览器 → 曲终后 `journalctl` 确认自动接续且曲目正确；双标签后写覆盖；单曲循环播完即停。
- 风险提示：批次 C 触及前端队列/media store 状态同步，是剩余工作中唯一有 UX 回归风险的改动，单独提交并手工验收；批次 A/D 可先行。

> 后续方案（tinyLMS-old / splayer-atom 对照的 Diretta 硬化批次 E/F + 上游同步 SOP）见同目录 `hardening-and-upstream-sync-plan.md`（2026-09-05）。
