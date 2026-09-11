# ALSA MMAP 直出：PCM 与原生 DSD 处理全景

> 整理自 2026-09-12 工作区代码（未提交，v10 起）。
> 核心文件：
> - `native/audio-engine-core/src/alsa_mmap_sink.rs` — MMAP 直出后端（PCM + DSD 两条写循环）
> - `native/audio-engine-core/src/audio_output.rs` — 输出后端选择 / `build_dsd_stream`
> - `native/audio-engine-core/src/direct_dsd.rs` — `DirectDsdReader`（DSF/DFF/SACD 原生 DSD 拉流）
> - `native/headless-server/src/api/player.rs` — headless 路由（`run_alsa_dsd_load` 等）

---

## 1. 入口与路由

### 1.1 设备选择协议

- 设备 ID 形如 `alsammap:hw:X,Y`（空后缀 = `default`）。`devices_handler` 把
  `alsa_mmap_sink::list_hw_devices()`（`HintIter` 枚举 `hw:` 前缀播放设备）与 cpal 枚举到的
  `alsa:hw:*` 合并进同一设备列表，条目带 `"mmap": true` 标志，UI 无需改版。
- `AudioOutput::new` 看到该前缀即走 `OutputBackend::AlsaMmap`，其余走 cpal。

### 1.2 全局超采样率守卫

`MAX_SAFE_SAMPLE_RATE = 1_000_000 Hz`（`alsa_mmap_sink.rs:55`）：

- 正常 PCM 最高 768 kHz；DSD **解码成 PCM** 最低是 DSD64 = 2.8224 MHz。
- 高于 1 MHz 的采样率请求只可能来自 DSD 解码流——在 `AudioOutput::new` 与
  `negotiate_hw_params` 两处统一拒绝。背景：2026-09-11 DSD 源经本地声卡直出时，
  超高带宽 isoc 请求拖垮 snd-usb-audio/xHCI 导致整机 hang。
- **原生 DSD 直出不受此守卫影响**：DSD 走 USB 专用 altset（DSD_U32_BE 等格式），
  ALSA "rate" 是 bitrate/divisor 的容器速率（如 DSD64 U32_BE = 88 200），且
  `run_alsa_dsd_load` 调 `AudioOutput::new` 时传 `None` 采样率，绕开 PCM 协商。

### 1.3 路由决策（`load_handler`）

```
load(source)
 ├─ alsammap 设备 + native DSD 源（.dsf/.dff/.iso，is_native_dsd_source）
 │    → run_alsa_dsd_load        # §4 原生 DSD 直出，绕过解码链
 ├─ Diretta 选择器                → run_direct_load（Diretta SDK 通道，另有文档）
 ├─ alsammap 设备 + PCM 源
 │    ├─ validate_alsammap_entry 通过 → AlsaMmapStream（§3，位纯真直出）
 │    └─ 未通过（音量≠100%/Normalization/EQ/tempo/pitch）
 │         → 降级 cpal，目标优先同卡兄弟设备 `alsa:plughw:X,Y`（仍有声音，但非位纯真）
 └─ 其他                          → cpal 常规路径
```

要点：
- 位纯真门槛（`player/mod.rs: validate_alsammap_entry`）：软件音量必须 100%、无
  ReplayGain/Normalization、无 EQ、tempo=1.0 且 pitch=0。任一不满足在**载入入口**
  自动降级，音量恢复后下一次 load 自动回到 MMAP。
- **native DSD 源豁免该门槛**（DSD 无音量/DSP 语义），不降级。
- 采样率无法精确满足时 `open_pcm` 直接报错（fail loud，绝不静默重采样）——错误以
  bad_request 返回，不会自动降级。

---

## 2. PCM 写循环公共骨架

PCM 与 DSD 两条写循环共享同一骨架，理解 PCM 版即可类比 DSD 版：

### 2.1 设备协商（`negotiate_hw_params` / `open_pcm`）

| 项 | 处理 |
|---|---|
| 打开 | `PCM::new(device, Playback, false)`，无 plug 层，hw 直开 |
| Access | `MMapInterleaved` — 应用直接写内核 DMA 缓冲映射，零拷贝 |
| 声道 | 固定 2（headless 立体声规范） |
| 格式 | 按 `S32 → S24 → S16` 优先级逐个 `test_format`，全不支持即报错 |
| 采样率 | `set_rate(rate, Nearest)` 后读回实际值**必须精确相等**，否则拒绝（防邻居速率触发隐式重采样破坏位纯真）；未指定则 rate_near 48k |
| 周期 | `period_size_near(1024)` |
| SW 参数 | `start_threshold = min(buffer_frames, period*2)`；`avail_min = period` |

协商成功在 `AlsaMmapStream::open`（探测一次后释放）与写循环线程内（正式独占）
各做一次；线程内先 `boost_current_audio_thread`（SCHED_FIFO，默认优先级 70，
可配置）+ `bind_current_thread_to_performance_cores`（性能核/隔离核绑定）。

### 2.2 主循环流程（每轮）

1. **stop 标志** → `pcm.drop()` 退出。
2. **暂停同步**：`paused` 原子标志与硬件状态不一致时——
   - 设备支持 pause（`can_pause`）：`pcm.pause()` 原位冻结时钟；
   - 不支持：暂停时 `pcm.drop()`（回到 Setup），恢复时 `prepare()`。
   - 硬件冻结期间无数据可写，睡 50ms（`WAIT_CEILING_MS`，决定 play/pause 指令
     响应延迟上界）后 continue。
3. **状态机**：
   - `XRun` → `prepare()` + 计数（欠载可恢复，不算错误）；
   - `Suspended` → `resume()`，失败退 `prepare()`；
   - `Open/Setup` → `prepare()`；
   - 其他状态 → bail（不可恢复）。
4. **诊断**：前 15s 或非 Running 状态每秒打印 state/delay/avail/写入计数。
5. **avail_update** 取得可写空间；为 0 则 `pcm.wait(50ms)`。
6. **高水位限速（v9e）**：hw 已填水位只补到 `HW_HIGH_WATERMARK_MS = 200ms`
   （env `SPLAYER_ALSAMMAP_WATERMARK_MS` 可覆盖，0 = 恢复不限速旧行为）。
   本轮可写帧数 `= min(avail, 4096, watermark_frames - filled)`：
   - 目的：消费节奏贴回真实时，解码 Shared 队列保留网络缓冲垫，position 平滑
     前进，流媒体 watchdog 不再误判"输出停滞"；预填 200ms 覆盖 RT 调度毛刺。
   - 水位已满时 **sleep `clamp(watermark/4, 2..20ms)`**，不能用 `pcm.wait`——
     它等的是 avail≥avail_min，与水位条件不等价，会立即返回造成忙转烧满一核
     （2026-09-11 v9e 实测教训）。pacing 只约束"何时写"，不碰样本路径。
7. **数据面写入（MMAP）**：见 §2.3。
8. **显式 start（v9e4）**：写成功后若 state 仍为 `Prepared` 则手动 `pcm.start()`。
   实测 commit 推进 appl_ptr 后内核并未按 start_threshold 自动启动——该路径
   曾自上线以来从未出声（"能播"只是解码侧 position 前进）。

### 2.3 PCM 数据面（f32 拉取 → 整数容器）

- 源侧沿用 `DecoderSource` 的 f32 逐样本拉取；`stopped`/`paused` 时写 f32 静音帧
  （0.0），正常帧乘 gain（`volume` 原子位型读取）。位纯真条件下 gain 恒为 1.0，
  16/24-bit 整数源经 f32 往返**无损**（除以 2^(n-1) 再乘回同系数）。
- 按协商格式分派（ALSA Format 是运行期值不能 match）：
  - `S16` → `io_i16().mmap(frames, …)`，`(v*32768).round().clamp(…)`；
  - `S24` → `io_i32_s24().mmap(…)`，24 有效位居 32 位容器**高位**（`<<8`）；
  - `S32` → `io_i32().mmap(…)`，乘 2^31。
  转换系数与解码归一化严格互逆（乘 32767 之类会引入 0.003% 失真），单测锁定。
- mmap 闭包内逐帧转换并直接写内核 DMA 映射，返回实际写入帧数。

### 2.4 错误处理与观测

- mmap 写失败且 state==XRun → prepare 重试；否则连续错误计数，
  **连续 ≥50 次**（`MAX_CONTIGUOUS_ERRORS`）→ 触发 `on_failure()`（OutputStalled
  重建链路）并退出写循环。
- `XRUN_TOTAL` 全进程原子累计（B9.6 观测钩子周期采样，拷机判据 xrun_count=0）。
- 成功写零错误计数；XRun 不计入连续错误。

---

## 3. ALSA MMAP PCM 直出（AlsaMmapStream）小结

- 控制面：`play()/pause()` 写原子标志驱动写循环；`Drop` 置 stop 并 join 线程。
- 会话归属：走常规载入链（`regular_load_worker`），每次 load/seek 按
  `AudioOutput` 配置重建流，`PlaybackStream::Alsa` 变体由 `PlaybackHandle` 持有；
  play/pause/position/音量等全部复用既有播放器状态机（与 DSD 直出的旁路
  体系不同）。
- 位置/时长/seek 由解码 Shared 队列与输出 position 常规机制提供，无特殊处理。

---

## 4. ALSA 原生 DSD 直出（AlsaDsdStream，DSD_U32_BE）

### 4.1 触发与载入（`run_alsa_dsd_load`）

条件：`alsammap:` 设备 + `is_native_dsd_source`（路径含 `.dsf`/`.dff` 或 SACD
ISO 虚拟轨 `.iso|`）。独立于 Direct 家族：无 handoff/预载/watchdog，单机直出。

载入步骤：
1. `spawn_isolated_blocking("alsa-dsd-load-worker")` 内：
   - **先在普通线程 drop `direct_initial_take`** —— HTTP 流源的 drop 链含
     ffmpeg/reqwest 的 tokio runtime，在 async 上下文 drop 会 panic → 整进程
     abort（2026-09-11 事故）；
   - ffmpeg `probe_metadata`（封面/标签，失败不阻断）；
   - `DirectDsdReader::open_local` 打开源（失败 → clear_pending_load + 500）。
2. 曲终推进：`on_eof` 闭包置位 `state.auto_advance_requested`（与 PCM Ended
   事件同源，由输出看门狗轮询消费；回调线程禁止锁 player/触发 async）。
3. `AudioOutput::new(selector, None, …)`（probe 协商，确认 alsammap 后端）→
   `build_dsd_stream(reader, on_eof)` → `PlaybackStream::AlsaDsd`。
4. `auto_play` 则立即 `stream.play()`。
5. 挂 `AlsaDsdHandle { stream, position(ms), duration, playing }` 到
   `state.alsa_dsd_stream`；启动 250ms 打点线程（Weak 引用，句柄换装 drop 后
   自动退出；暂停不累计）。now-playing 查询端把 ms 换算秒并封顶 duration。

会话管理（状态机旁路）：`play/pause/stop/now_playing` handler 检测到
`alsa_dsd_stream` 挂载时优先接管；**seek 明确拒绝**（直出流不可重定位，提示重新
加载或切歌）。stop 时 take 并 drop 句柄 → 写循环读 stop 标志退出。

### 4.2 源解析（`DirectDsdReader`）

| 容器 | 识别 | 格式解析 | 位序 |
|---|---|---|---|
| DSF | magic `DSD ` | fmt chunk：format_id 必须 0（raw DSD）、声道 ≤16、`sampling_freq`=比特率、`bits_per_sample` 1=LSB / 8=MSB、`sample_count`、`block_size`（默认 4096，必须 4 的倍数）；遍历找 `data` chunk | 由文件声明 |
| DFF | magic `FRM8` | 遍历 IFF chunk：`PROP/SND` 内 `FS  `(比特率)、`CHNL`(声道)、`CMPR` 必须 `DSD `（DST 压缩显式拒绝）、`DSD ` data chunk | MSB-first |
| SACD ISO | 虚拟路径 `…iso|…` | `SacdNativeSource`；裸 ISO 拒绝（零 LSN 虚拟轨必失败，H2.4） | MSB-first |

输出规整（两类容器殊途同归）：
- **L4R4 单元交织**：输出流为按声道 4 字节分组的交织序列（2ch 即
  `[L0..3][R0..3][L4..7][R4..7]…`）。
  - DSF 源是**逐声道 planar 块**存储：`repack_dsf_l4r4` 按 `block_size` 分块读入，
    每声道跨块取 4 字节交错输出（支持 seek 后的块内 skip）。
  - DFF 源是**逐字节交织**：`repack_dff_l4r4` 重排为 4 字节/声道分组。
- **尾部截断**：sample_count/数据长度向下取整到 32 样本（DSD_SIZ_32）整数倍
  （DSD64 下损失 ≤11µs），不再硬拒整个文件。
- `read_block(output)` 流式拉取，返回 `None` 即 EOF；输入块缓存 DSF=一个完整
  planar 块（block_size×声道）、DFF=32KB 对齐到 unit。
- `seek_seconds`：按比特位置对齐 32-bit 边界；DSF 需落在块边界+4 字节倍数偏移，
  重建块游标；DFF 按 unit（声道×4 字节）对齐；SACD 委托 source.seek_secs。

### 4.3 DSD 线格式协商（`open_dsd_pcm`）

按 **`DSD_U32_BE`(÷32) → `DSD_U16_BE`(÷16) → `DSD_U8`(÷8)** 顺序尝试（兼容不同
DAC 暴露的容器宽度：XMOS 系多为 U32_BE）：

- `alsa_rate = dsd_bit_rate / divisor`（DSD64: U32_BE→88 200，U8→352 800）；
- access 同为 `MMapInterleaved`，声道数按源（不支持即报错，提示该设备 DSD
  altset 可能仅立体声、多声道请用 Diretta）；
- rate 必须**精确命中**，否则试下一档；period_near(1024)；SW 参数与 PCM 版相同；
- 三档全部失败 → 报清晰错误：内核需设 `quirk_flags=<VID>:<PID>:8000`
  （DSD_RAW quirk）并重枚举 USB，或改用 Diretta 输出。
- `AlsaDsdStream::open` 先探测协商一次（拿到容器 rate 作 sample_rate），随即
  释放，正式独占打开在写循环线程内进行（与 PCM 版一致）。

### 4.4 DSD 写循环（`dsd_write_loop`）

骨架与 PCM 版一致（状态机 / XRun 恢复 / 水位限速 200ms / 显式 start / 连续 50
错误上报），差异在数据面：

1. **staging 缓冲**：read_block 以 256KB 块拉原始 DSD 进 staging（容量上限
   2MB ≈ DSD512 立体声 128ms），MMAP 写从 staging 头部消费，消费指针过 2MB
   定期压实。
2. **位序适配**：目标线序 **MSB-first**（kernel quirk bitrev=0 时 USB 原生 DSD
   期望）；源为 LSB-first（DSF 声明 bits_per_sample=1）则逐字节 `reverse_bits`。
   env `SPLAYER_ALSADSD_BITREV=on/off` 可强制覆盖（默认 auto 按源位序）。
   Diretta 路径线序由 SDK 协商，与本路径独立。
3. **写入**：alsa-rs 的 `io_checked` 按类型严格等值校验，DSD 三档格式无 IoFormat
   映射（io_i32/io_i16/io_u8 一律 unsupported）——走官方逃生口 **`io_bytes()`**：
   免检、字节粒度 mmap，对 U32/U16/U8 统一适用。
4. **无音量语义**（bit-perfect）：不乘 gain。
5. **暂停垫 0x69**：DSD 是 PDM，`0x00`（全 0 位流）= 连续负向满幅 DC，DAC 模拟
   端强爆音；`0x69`（01101001）01 交替密度最高 ≈ 零电荷，是专业 DSD 播放器
   标准静音值。暂停期间不读源、持续垫 0x69（保持 DMA 供流防欠载噪声，恢复
   无缝）；读取失败也垫 64KB 0x69 兜底。
6. **EOF**：read_block 返回 None 后等 staging 消费完（< 一个 frame），再等 hw
   在途数据排空（`delay ≤ 0`，上限 2s），回调 `on_eof()`（推进下一曲），
   `pcm.drop()` 退出。

---

## 5. 与其他 DSD 通道的边界

| 通道 | 载体 | 用途 |
|---|---|---|
| **ALSA MMAP 原生 DSD**（本文） | 本地 raw-DSD USB 声卡（GR40/XMOS 等，需内核 DSD_RAW quirk） | headless 本机直出 |
| Diretta native DSD | SDK 连接，`interleaved_1byte_to_block32_in_place` 重排为 InterleavedBlock32 | 网络音频 Target（主力通道） |
| DoP 打包（`dop_pack`） | Diretta PCM Direct 连接，24-bit 样本 `[marker][b0][b1]`，marker 0x05/0xFA 每 16 样本翻转，PCM 速率 = bitrate/16 | Target 固件支持 DoP 时（`dsd_transport` 配置，不静默降级） |
| DoP WAV 物化（`dop_wav`） | 整曲转 DoP WAV 进 RAM | 仅支持 PCM 的链路兜底 |
| dsd2pcm 抽取（`dsd2pcm`） | 16:1 Gaussian FIR，DSD64→88.2k / DSD128+→176.4k f32 | DSD 源走常规 PCM 解码链（系统声卡/cpal） |

---

## 6. 环境变量 / 调参一览

| 变量 | 默认 | 作用 |
|---|---|---|
| `SPLAYER_ALSAMMAP_WATERMARK_MS` | 200 | hw 预填高水位（毫秒）；0 = 恢复不限速旧行为 |
| `SPLAYER_ALSADSD_BITREV` | auto | DSD 位序强制反转 on / 不反转 off |
| RT 优先级（`configure_rt_priority`，配置注入） | 70 | SCHED_FIFO 1-99；蓝图要求 75~85 |

## 7. 关键教训索引（写循环设计依据）

1. **v9e 忙转**：水位满时不能用 `pcm.wait`（谓词不等价会立即返回），改定时 sleep。
2. **v9e4 不出声**：MMAP commit 后内核不保证按 start_threshold 自动启动，写成功
   后 state 仍 Prepared 必须显式 `pcm.start()`。
3. **2026-09-11 整机 hang**：DSD 解码 PCM 超高采样率喂本地声卡拖垮 xHCI →
   `MAX_SAFE_SAMPLE_RATE` 入口守卫；原生 DSD 走专用 altset 不受限。
4. **alsa-rs DSD 格式无 IoFormat**：`io_checked` 全拒，必须 `io_bytes()`。
5. **tokio runtime drop panic**：HTTP 源句柄必须在普通阻塞线程 drop，不能在
   async worker 上下文（否则进程 abort）。
6. **0x00 ≠ DSD 静音**：PDM 全 0 位流是负向满幅 DC，静音垫必须 0x69。
