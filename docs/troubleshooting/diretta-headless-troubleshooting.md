# Diretta 与 Linux Headless 核心架构与故障排查指南 (AI 协作参考)

本文档整理了 SPlayer-Next 在 Linux Headless 模式与 Diretta 网络音频推流链路中的核心架构要点、历史重大故障根因及已验证的解决方案，供后续维护人员及 AI 编程助手参考。

---

## 1. 架构总览

```
[ Web UI / 远程客户端 ]
       │ HTTP / WebSocket
       ▼
[ native/headless-server (Axum + Tokio) ]
       │ 独立 OS 线程隔离
       ▼
[ native/audio-engine-core (Player / Decoder) ]
       │ 解码 (FFmpeg / Symphonia) -> 环形缓冲区 (RingBuffer)
       ▼
[ native/audio-engine-core::diretta_output ]
       │ C FFI 绑定
       ▼
[ native/diretta-sys (C++ Shim) ]
       │ 动态链接 / 静态链接
       ▼
[ DirettaHost SDK (libDirettaHost & libACQUA) ]
       │ IPv6 Link-Local Raw/UDP Socket (eno2)
       ▼
[ Diretta Target (DAC / Bridge 硬件) ]
```

---

## 2. 关键故障定位与修复方案（重点避坑指南）

### 故障 1：Tokio 运行时嵌套 Drop 导致线程 Panic 崩溃

- **日志现象**：
  ```text
  thread 'tokio-rt-worker' panicked at tokio/src/runtime/blocking/shutdown.rs:51:21:
  Cannot drop a runtime in a context where blocking is not allowed. This happens when a runtime is dropped from within an asynchronous context.
  ```
- **根因**：
  - 音频源组件（如 `ffmpeg_audio::HttpAudioSource`）内部维护了一个独立的单线程 Tokio Runtime。
  - 在 Axum API 路由中，如果使用 `tokio::task::spawn_blocking` 执行加载或切歌，该线程属于 Tokio 的 Blocking 线程池，线程局部变量中保留了 Tokio Handle（`Handle::try_current().is_ok()`）。
  - 当旧音轨被 Drop 销毁时，内部 Runtime 在 Drop 时检测到自身处于非法的 Tokio 环境中，直接触发致命 Panic，导致 HTTP 请求失败、播放中断。
- **解决方案**：
  - 在 `native/headless-server/src/api/routes.rs` 中引入 `spawn_isolated_blocking`：
    ```rust
    pub async fn spawn_isolated_blocking<F, T>(name: &'static str, f: F) -> Result<T, String>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let res = f();
                let _ = tx.send(res);
            })
            .map_err(|e| format!("Failed to spawn OS thread {name}: {e}"))?;
        rx.await.map_err(|e| format!("OS thread {name} panicked or dropped sender: {e}"))
    }
    ```
  - 对所有涉及音频加载、定位、Diretta 扫描的耗时同步调用，全部使用 `spawn_isolated_blocking` 替代 `tokio::task::spawn_blocking`。

---

### 故障 2：Linux 实时线程权限缺失导致连接瞬间断开 (`connectWait 0`)

- **日志现象**：
  - 外部现象：日志提示 `Diretta 推流连续失败，退出推流线程 error=缓冲区下溢 consecutive=10`。
  - SDK 底层日志（启用 SysLog 后捕获）：
    ```text
    wth start CRITICAL NOSLEEP4CORE  FEEDBACK:1  Info=100
    Worker Thread Priority set Error
    connectWait notimeout
    connectWait 0
    ```
- **根因**：
  - Diretta 采用 `THRED_MODE(5)`（`CRITICAL NOSLEEP4CORE`）作为微秒级超低抖动工作线程，需要设置 Linux 实时调度策略（`SCHED_FIFO` / `SCHED_RR`）。
  - `splayer-headless.service` 默认以普通用户（非 root）运行，受 Linux 内核安全策略与 systemd 限制，没有 `CAP_SYS_NICE` 权能与 `LimitRTPRIO` 配额，内核调用 `setpriority` / `sched_setscheduler` 返回 `EPERM`，导致 Diretta 推流工作线程启动失败，`connectWait` 瞬间失败退出。
- **解决方案**：
  - 在 `/etc/systemd/system/splayer-headless.service` 与 `scripts/install-linux-headless.sh` 中配置权能与资源上限：
    ```ini
    LimitRTPRIO=infinity
    LimitMEMLOCK=infinity
    AmbientCapabilities=CAP_SYS_NICE CAP_NET_RAW CAP_NET_BIND_SERVICE
    CapabilityBoundingSet=CAP_SYS_NICE CAP_NET_RAW CAP_NET_BIND_SERVICE
    ```

---

### 故障 3：Diretta IPv6 Link-Local 网卡绑定与握手时序

- **要点**：
  1. **网卡编号（`ifno`）传递**：
     Link-Local 地址（`fe80::...`）在 Linux 下不具备全局路由，必须携带网卡索引（如 `eno2` 的 `ifno=2`）。
  2. **MTU 测量前建立路由缓存**：
     在 `measure_mtu` 调用前，必须先调用 `finder.scan(8)`，使 SDK 预热对端的目标机路由缓存，否则 `measSendMTU` 会直接报找不到网卡/主机。
  3. **C++ Shim 规范握手流程**：
     ```cpp
     // 1. 设置 Sink 并开启自动协商 (isAuto = true)
     syncbuffer_.setSink(sink_addr, buf_clock, true, mtu);
     
     // 2. 配置格式与传输参数
     syncbuffer_.setSinkConfigure(fid);
     syncbuffer_.configTransferAuto(Clock::MicroSeconds(200), ACQUA::Clock(), Clock::MicroSeconds(100000));
     syncbuffer_.setupBuffer(chunk_fs, 100, false);
     
     // 3. 准备连接并以非绑核模式启动 (-1)
     syncbuffer_.connectPrepare();
     syncbuffer_.connect(false, -1);
     syncbuffer_.connectWait();
     
     // 4. 轮询等待连接建立就绪
     for (int i = 0; i < wait_loops; ++i) {
         if (syncbuffer_.is_connect()) break;
         std::this_thread::sleep_for(std::chrono::milliseconds(50));
     }
     ```

---

### 故障 4：同步上游代码时的冲突与覆盖防护

- **原因**：
  上游仓库可能会重构 `audio-engine-core` 或依赖项，若直接覆盖拉取或未提交本地修改，会导致 Diretta 与 Headless 的定制链路被冲掉。
- **规范同步流程**：
  1. 保证本地修改已生成规范 Git Commit（如 `git commit -m "fix(diretta): ..."`）。
  2. 同步上游时使用 **`rebase`** 策略：
     ```bash
     git fetch origin dev
     git rebase origin/dev
     ```
  3. 若出现合并冲突，优先保留 `native/audio-engine-core/src/diretta_output.rs` 以及 `native/diretta-sys`、`native/headless-server` 中的 `spawn_isolated_blocking` 改造。

---

## 3. 诊断与排错命令速查

1. **查看服务实时运行日志**：
   ```bash
   journalctl -u splayer-headless.service -f
   ```
2. **正常推流日志特征**：
   ```text
   INFO audio_engine_core::diretta_output: Diretta 推流状态 tick=16600 max_amp=0.54 is_online=true playing=true
   ```
   - `is_online=true`：Target 接收端响应正常；
   - `playing=true`：播放引擎正常泵出数据；
   - `max_amp`：波形浮点幅度，正常有声时介于 `0.05 ~ 0.99` 之间。
3. **重启与更新服务**：
   ```bash
   sudo systemctl restart splayer-headless
   ```
