# Diretta Host SDK 148 / 149 / 150 三版本对比开发指南

> 分析对象:`/home/songlian/DirettaHostSDK_148`、`/home/songlian/DirettaHostSDK_149`、`/home/songlian/DirettaHostSDK_150`
> 分析日期:2026-09-06(方法:三版全部源文件逐一 diff + 头文件全文精读 + Doxygen 交叉核对)
> 用途:SPlayer-Next-Headless 接入 Diretta 网络输出的选型与开发依据
> 姊妹篇:[diretta-host-sdk-148-contract-analysis.md](diretta-host-sdk-148-contract-analysis.md)(148 单版本深度契约分析,A–G 全文)

---

## 0. 结论(TL;DR)

1. **新开发一律基于 v150**。三版核心契约(先设格式后连接、pull/push 双模式、FormatID 位编码、Atomic 回调约束)完全一致;149/150 是增量演进,无架构级变化,但 150 修复了 148/149 的多处线程安全隐患并给关键 push 接口补上了返回值/文档。
2. **149 是"发现/连接层"修订版**:多流(MS)模式查询、自定义绑定端口、广播/多播发现开关、跨线程状态原子化。
3. **150 是"push 模式 + 启动流程"修订版**:`connect()` 参数首次文档化并新增 Rapid Start;`setupBuffer/setStream` 补返回值;**移除 `checkStreamStart()`**(唯一破坏性 API 变更);`notifyStreamDone` 第二参数语义改为恒 false。
4. 库文件:三版均带 GCC15 与(149 起)GCC16 工具链变体,静态链接,选型规则见 §6。
5. **注意区分本地修改与厂商原版**:148 的 `SinHost.cpp` 被本地改过(加了查找重试循环);150 目录里的 `SinHost_push/dbg/diag.cpp` 三个文件是本地调试产物,**不是厂商示例**。厂商在三版中只提供一个 `SinHost.cpp`(内容 149≡150,148 原版与之仅差一个 BOM)。

---

## 1. 三版本总览

| 项目 | 148 | 149 | 150 |
|---|---|---|---|
| `Release.hpp` ReleaseNo | 148 | 149 | 150 |
| 头文件基准时间 | 2026-01-20(Sync) | 2026-07-27(Sync) | 2026-08-27(Sync) |
| 厂商示例 | SinHost.cpp ×1 | SinHost.cpp ×1 | SinHost.cpp ×1 |
| 本地改动/产物 | SinHost.cpp 被改(重试查找+日志) | 无 | SinHost_push/dbg/diag.cpp ×3(本地调试) |
| 预编译库 | GCC15 系 | +GCC16 系、+通用无后缀 .a | 同 149 |
| Doxygen | 与头文件同步 | 已重新生成(含 is_MSmode) | 已重新生成(含 Rapid Start、ClockDiff) |
| memo_host.txt / 许可 | — | 三版逐字节相同 | — |

非 doc 文件数:148=109、149=131(含 149 遗留的构建产物 `lib/Sync.o`)、150=138。

**三版完全相同的文件**(diff 确认):`Host/Stream.hpp`、`Host/diretta_stream.h`、`Host/Receive.hpp`、`Host/Send.hpp`、`Host/SysLog.hpp`、`Host/Icon.hpp`、`Host/Diretta/*`(无扩展名 include 桥)、`SinHost/Makefile`、`memo_host.txt`,以及 ACQUA 的大部分(Array/Buffer/ThreadPriority 等)。

**有差异的文件**:

| 文件 | 148→149 | 149→150 |
|---|---|---|
| Host/Sync.hpp | DIFF | DIFF |
| Host/SyncBuffer.hpp | 同 | DIFF |
| Host/Format.hpp | DIFF | 同 |
| Host/Connection.hpp | DIFF | 同 |
| Host/Find.hpp | DIFF | 同 |
| Host/Profile.hpp | 同 | DIFF |
| Host/Release.hpp | DIFF | DIFF |
| ACQUA/Ethernet.hpp | DIFF | 同 |
| ACQUA/Socket.hpp | DIFF | 同 |
| ACQUA/UDPV6.hpp | DIFF | 同 |
| ACQUA/Clock.hpp | 同 | DIFF |

---

## 2. 148 → 149 变更明细

### 2.1 Sync.hpp:多流模式查询 + 状态原子化

**新增 `is_MSmode()`**(149 为 `Sync.hpp:175`,150 为 `Sync.hpp:176-177`,原文):

```cpp
/// @brief check connecoti mode (If `is_online` is false MSMODE_NONE is returned, so the online status cannot be verified)
MSMODE is_MSmode(){return (is_online()?(MSmodeSet==MSMODE_AUTO?MSMODE_NONE:MSMODE(MSmodeSet)):MSMODE_NONE);}
```

语义:仅在线时可查询实际生效的多流模式;`open()` 请求 `MSMODE_AUTO` 时即使已按多流运行也返回 `MSMODE_NONE`(该函数无法区分"AUTO 未协商出多流"与"离线")。MSMODE 枚举三版相同(`Sync.hpp:63-74`):`NONE=0 / MS1=1 / MS2=2 / MS3=4 / AUTO=5`,`Sync::Info.supportMSmode` 为 sink 能力位。

**线程安全修正**(148 → 149):

```cpp
volatile REQ_STATE connectState;   // 148
std::atomic<REQ_STATE> connectState;   // 149+,Sync.hpp:291(150)
MSMODE MSmodeSet;                  // 148
std::atomic<MSMODE> MSmodeSet;     // 149+,Sync.hpp:328(150)
```

对上层开发的意义:**148 中从非 SDK 线程调用 `is_connect()/is_online()/is_disconnect()` 存在数据竞争**(volatile 只保证不缓存,不保证原子读);149 起这些查询是真正线程安全的。跨线程状态轮询代码在 148 上需自行加锁,迁移到 149/150 后可去掉。

### 2.2 Format.hpp:clearExtFormat()

149 新增(150 为 `Format.hpp:186`):

```cpp
/// @brief clear  Exception format flag
void clearExtFormat();
```

`FormatConfigure` 上唯一与"异常格式"相关的公开接口,无对应 getter(SDK 未说明触发条件;从上下文看与 DDS/非标准组合格式有关)。三版其余 Format API(`FormatID` 位编码、`FormatSupport`、`FormatPreConfigure`)逐字节相同。

### 2.3 Connection.hpp:绑定端口 + 发现方式控制(**含源码级破坏性变更**)

```cpp
// 148(Host/Connection.hpp:20)
Connection(ACQUA::EthernetSocket&);
// 149/150(Host/Connection.hpp:22)—— 第二参必填,无默认值
Connection(ACQUA::EthernetSocket&,std::uint16_t);   // bindport:自定义 UDP 绑定端口
```

`sendMulti` 扩参(149/150 为 `Connection.hpp:63`,原文注释):

```cpp
/// @param  brodcast (Spoof the MAC addressmacaddress
/// @param  multicast default true
bool sendMulti(const ConnectionBuffer& msg,MessageID respMsg,
               std::map<ACQUA::IPAddress,std::unique_ptr<ConnectionBuffer> >& resalt,
               bool /*loopback*/,bool /*broadcast*/,bool /*multicast*/=true);
```

内部实现配套变化(149/150 `Connection.hpp:89-100`):`IfList` 由 protected 移至 private,新增 `std::set<std::uint32_t> IfListMask`(接口过滤缓存)、`std::uint16_t bindport`、`bool BroadcastRawSend(const IfInfo&,const ConnectionBuffer&,const IPAddress&)` 与前置声明 `struct WAUDPHeader`(原始套接字伪造 MAC 广播发送路径)。

**破坏性影响**:任何以外部 socket 构造 `Connection`(含其子类 `Sync/SyncBuffer/Find`)的调用点,148 的单参写法在 149+ 编译失败,必须补 bindport。示例不经过此路径(用默认 socket),所以 SinHost 不受影响。

### 2.4 Find.hpp:发现开关 + 绑定端口

`Find::Setting` 新增两个字段(149/150 为 `Find.hpp:38-41`,原文注释,字段顺序如下):

```cpp
/// @brief Using Multicast for Target Detection ( default disable(false (Violates the IPv6 specification
bool Broadcast;
/// @brief Using Multicast for Target Detection ( default enable(true
bool Multicast;
```

即:`Broadcast=true` 时用(伪造 MAC 的)广播做目标发现——注释明确标注"违反 IPv6 规范",默认 false;`Multicast` 是常规多播发现,默认 true。构造函数扩参(149/150 `Find.hpp:50`):

```cpp
Find(const Setting&,ACQUA::EthernetSocket&,std::uint16_t);   // 第三参 bind port
```

同时为 `Setting` 全部字段补了文档注释(148 中无注释)。其余 Find API(发现/固件/TargetSetting/SinkSetting/measSendMTU 等)三版相同。

### 2.5 ACQUA 底层:MAC 类型与日志开关

- `ACQUA/Ethernet.hpp:30-42`(149/150):新增 `enum MACtype{MAC_NOLMAL=0, MAC_MULTICAST=1, MAC_BROADCAST=2}`(注意厂商拼写 NOLMAL),`setTo(const IPAddress&,MACtype=MAC_NOLMAL)` 抽象方法扩参,新增 `virtual MACtype supportMACtype()`。`UDPV6.hpp` 的 `UDPV6EBuffer::setTo` 同步扩参。这是 2.3/2.4 广播发现的底层支撑。
- `ACQUA/Socket.hpp:26`(149/150):新增 `void setLog(bool);`——按 socket 关闭收发日志。

### 2.6 库文件

149 起新增(150 相同):

- GCC16 工具链变体:`lib{ACQUA,DirettaHost}_{aarch64,riscv64,x64}-linux-16*.a`(±`-nolog`),aarch64 另有 `-16k4` 变体;
- 通用无后缀库 `libACQUA.a` / `libDirettaHost.a`;
- 149 目录里遗留厂商构建产物 `lib/Sync.o`(150 已清理)。

同版本内 GCC15 库的大小在 148/149 间略有增长(如 `libDirettaHost_x64-linux-15v2.a` 481410 → 487228 字节),说明实现层也有改动(头文件未体现,属 `.a` 内部)。

---

## 3. 149 → 150 变更明细

### 3.1 Sync.hpp:connect() 参数首次文档化 + Rapid Start

```cpp
// 148/149:bool connect(int);            // int 参数无文档
// 150(Host/Sync.hpp:147-150),原文:
/// @brief connect to sink
/// @param CPU number occupied by the send thread(default -1 not set CPU occupied)
/// @param Rapid Start (defalt play mode)
bool connect(int=-1,bool=false);
```

- 第 1 参终于有了官方语义:**发送线程绑核 CPU 号,-1 = 不绑核**(148 分析中"未说明"项在此解决)。示例传 0。
- 第 2 参 **Rapid Start**:默认 false = 常规"play 模式"(连接完成后进入可播放就绪);true = 快速启动。SDK 未进一步解释其内部差异(是否跳过预缓冲/延迟自动调整,需实测)。
- 新增 private `void statusUpdate(REQ_STATE)`(`Sync.hpp:292`):状态机变更时的内部通知,最终会触到 protected 虚钩子 `statusUpdate()`(`Sync.hpp:259`,三版都有,可覆写)。

`SyncBuffer::connect` 同步扩参(见下)。

### 3.2 SyncBuffer.hpp:push 模式接口修订(**含唯一破坏性移除**)

150 全文签名(`SyncBuffer.hpp`,行号即 150 版):

```cpp
// :22  void → bool,新增第 5 参
/// @param Call the first callback immediately after the connection is established (need FS>=2)
/// @return setup status
bool setupBuffer(size_t FS, size_t depth, bool mute=false, FormatConfigure = FormatConfigure(), bool=false);

// :27  文档首次给出
/// @brief Get the Writable Buffer
/// @param Retrieval error (Connection exit)      ← bool& 出参:true=取缓冲失败(连接已退出)
Stream& writeStreamStart(bool&);

// :31  文档首次给出
/// @brief Adding a stream—it is added regardless of setupBuffer
void addStream(Stream&);

// :35  void → bool
/// @brief Set the stream for data transmission. push mode
/// @return false is Connection exit              ← false=连接已退出,应停止推送
bool setStream(Stream&);

// :42  与 Sync::connect 同步扩参
bool connect(bool callbackMode, int cpu=-1, bool rapidStart=false);
```

行为/语义变化:

| 项 | 148/149 | 150 | 对上层的影响 |
|---|---|---|---|
| `checkStreamStart()` | 存在(`SyncBuffer.hpp:24`) | **移除** | 引用它的代码迁移到 150 编译失败;改用 `writeStreamStart(bool&)` 的出参判断 |
| `setupBuffer` 返回值 | `void`(失败只能靠后续行为暴露) | `bool` | **必须检查返回值**,格式非法等在 setup 阶段即可发现 |
| `setStream` 返回值 | `void` | `bool`(false=连接退出) | push 循环据此优雅退出,不必再只靠 `is_connect()` 轮询 |
| `notifyStreamDone(Stream&,bool)` 第 2 参 | "@param End of playback" | **"@param allways false"** | 148/149 上若依赖第二参判断"播放到末尾",150 上该信号恒为 false,**该判断逻辑必须移除**;回收语义仍以"本次回调返回后 buf 可复用/重新提交"理解 |
| 成员 `notifyStreamFlg` | 存在 | 移除 | 私有成员,无上层影响,但说明回收通知机制内部重构过 |
| `setupBuffer` 第 5 参 | 无 | false 默认;true = "连接建立后立即调用第一次回调(要求 FS>=2)" | callback 模式下降低首包延迟用;FS 为帧数,FS=1 时不可用 |

`getLastBufferCount()/buffer_empty()/seek(int64_t)/seek_front()` 四个 push 缓冲控制接口三版都有(148 起即存在,`SyncBuffer.hpp:54-60`),单斜杠注释不进 Doxygen,属于半文档化 API。

### 3.3 Profile.hpp:configTransferSizeFix 扩参

```cpp
// 148/149(148 为 Profile.hpp:95):bool configTransferSizeFix(size_t);
// 150(Host/Profile.hpp:96),原文:
/// @brief Transmission Size Specification Mode.
/// @param Packet Data Size: Bytes
/// @param NO remainder
bool configTransferSizeFix(size_t,bool=false);
```

第 2 参 `NO remainder=true` 时要求传输块无余数(包数据尺寸必须整除周期载荷)。注意 Profile.hpp 为 **ISO-8859 编码 + CRLF**(`grep` 需 `-a`;注释里的 `×` 是 Latin-1 0xD7),打补丁时留意。

### 3.4 ACQUA/Clock.hpp:ClockDiff 工具类

150 新增(`ACQUA/Clock.hpp:126-136`),纯头文件:

```cpp
class ClockDiff:public Clock{
public:
    inline void reset(){ reset( Clock::now() ); }
    inline void reset(Clock c){ *reinterpret_cast<Clock*>(this) = c; }
    inline Clock update(){            // 返回距上次 update/reset 的间隔并重新锚定
        Clock n = Clock::now(); Clock diff = n - *this; reset( n ); return diff;
    }
};
```

适合做喂流节奏/欠载监控的周期测量,与 SDK 逻辑无耦合。

---

## 4. 核心开发契约(以 v150 行号为基准)

> 三版共通。148 的逐条深度论证(含原文引用)见姊妹篇 `diretta-host-sdk-148-contract-analysis.md`;此处按 150 行号收敛为开发规则,并标注 150 新增的能力。

### 4.1 生命周期:先设格式,后连接;无在线改格式入口

```
Find 发现目标 → measSendMTU
open(THRED_MODE, InfoCycle, ifno, name, id, cpuMain, cpuOther, rngOther, msMode)  // Sync.hpp:87
setSink(addr, bufferTime(0=默认), nopBrake, mtu)                                  // :101
setSinkConfigure(FormatConfigure)                                                 // :103
configTransferAuto / configTransferFix / configTransferVar / ...                  // :114-132
connectPrepare(true/*自动调整目标延迟*/)                                            // :146
connect(cpu=-1, rapidStart=false)                                                 // :150  ★150 起有默认参数与文档
connectWait()                                                                     // :152
play()                                                                            // :161
轮询 is_connect()/is_online();is_MSmode() 可查实际多流模式(★149+)
stop()=暂停;disconnect(wait=true)/disconnectWait()/close()
```

- 切换采样率/位深/声道/DSD 位序 = 完整 disconnect → setSink/setSinkConfigure → connect 重走;协议层格式在 `FORMAT_REQ/ANS` 握手期锁定,连接中无切换入口。
- `setSink` 第 3 参是 **nopBrake(禁用 sink 播放拒绝)**,不是 isDSD;DSD 与否由 `setSinkConfigure` 的 `FMT_DSD_*` 位决定。
- 状态机 `REQ_STATE{DISCONNECT, CONNECT_REQ, CONNECT, DISCONNECT_REQ}`:`is_connect()`=除 DISCONNECT 外全部;`is_online()`=仅 CONNECT;149 起 `connectState` 为 atomic,**跨线程查询安全**(148 上有数据竞争)。
- SyncBuffer 的收尾特有语义:`pre_disconnect(bool=false)`(停止接受写入,缓冲播完即止)→ `disconnect(true)`(声明写结束,缓冲耗尽自动断开);pull 模式直接 `disconnect()`。

### 4.2 getNewStream(pull)契约

`Sync.hpp:252-256`(150,与 148 逐字相同):

```cpp
/// @brief Callback for retrieving the send stream from the send thread (Must be processed by Atomic)
/// @param stream diretta_stream to be sent. ... (The pointer must remain valid until it is dereferenced or this function is called again.)
/// @return false indicates termination; the buffer is not processed at that time.
virtual bool getNewStream(diretta_stream&)=0;
virtual bool getNewStreamCmp();
```

- SDK 发送线程(`thread_sync_func`)调用;**必须无锁、非阻塞**("Must be processed by Atomic");返回 false = 本次终止。
- `diretta_stream = {void* P; unsigned long long Size}`(`diretta_stream.h`,三版同);SDK 只在回调内 memcpy,**不释放**;指针保活到"下一次回调或解引用完成"——惯用法是常驻成员 `Stream`,回调里 `buf = Dummy;`(仅拷 P/Size)。v148 起(三版同)无 `Period` 字段。
- `getNewStreamCmp()` 三版均无文档;SyncBuffer 覆写它配合 `notifyStreamDone(Stream&,bool)` 做回收通知。**150 起该回调第 2 参恒为 false**,不能当"播放结束"信号用。

### 4.3 Sync(pull) vs SyncBuffer(push)

唯一明文出处 `memo_host.txt`(三版同文):`Sync — buffer pull type;SyncBuffer — buffer push type`。

- **Sync**:继承并实现 `getNewStream`,数据在回调里即时生成(示例正弦波)。
- **SyncBuffer**:"Builds the send buffer for Sync"。`setupBuffer(FS帧数, depth倍深, 欠载静音, 格式, ★150:立即首回调)` 建环,两种喂法:
  - **零拷贝**:`writeStreamStart(err)` 拿可写 `Stream&` → 直接填充 →(下一轮隐式提交/或 `addStream`)——150 文档化,err 出参 true=连接退出;
  - **整块推送**:`setStream(str)` → 150 返回 bool,false=连接退出。
  - 辅助:`getLastBufferCount()/buffer_empty()/seek()/seek_front()`(半文档化)。
- `SyncBuffer::connect(bool callbackMode, int cpu=-1, bool rapidStart=false)`(150):callbackMode=true 时也走回调(用同一环),false=push。
- 块大小按 `getSinkConfigure().getFrameSize()` 帧对齐(示例唯一对齐证据);`setupBuffer` 的 FS 是**帧数不是字节**,总深 = FS×depth。

### 4.4 FormatID(三版逐字节相同,`Format.hpp:6-66`)

48-bit 位 OR 编码,兼做能力位图(`支持位图 & 请求 == 请求` 即支持):

| 位段 | 常量 |
|---|---|
| bit0–7 声道 | `CHA_1/2/4/6/8/16`,`CHA_MSK=0xFF` |
| bit8–15 PCM 编码 | `FMT_PCM_SIGNED_8/16/24/32/64`、`FMT_PCM_FLOAT_32/64`,`FMT_PCM_MSK=0xFF00` |
| bit16–19 DSD 速率 | `FMT_DSD1=0x10000`、`FMT_DSD4=0x20000`(**无 DSD2**,三版均无) |
| bit20–23 位序 | `FMT_DSD_LSB(DSF)/FMT_DSD_MSB(DFF)`、`FMT_DSD_LITTLE/BIG`,`FMT_DSD_ORDER_MSK=0xF00000` |
| bit24–31 DSD 打包 | `FMT_DSD_SIZ_32=0x2000000`(SIZ_8 被注释"not recommend"),`FMT_DSD_SIZE_MSK=0xFF000000` |
| bit32–47 采样率 | 基频 `RAT_44100=1<<33` 等(base × multiplier,`RAT_MP1..MP4096`);例:`CHA_2|FMT_PCM_SIGNED_32|RAT_44100|RAT_MP2` = PCM32/2ch/88.2kHz |
| bit55 | `DDS=0x0080000000000000ULL`(Diretta Direct 模式) |

能力查询:`getSinkInfo()` 的 `supportPCM/supportDSDlsb/supportDSDmsb`;`checkSinkSupport(FormatConfigure)`、`inquirySupportFormat(addr)`。`FormatConfigure`(`Format.hpp:124-190`)为格式构造/解析辅助(getFrameSize/get1secSize/getMuteByte/setSpeed…),150 多一个 `clearExtFormat()`。

### 4.5 周期/MTU

- `getCycleTime()/getMinCycleTime()/getCycleSize()/getCyclePackets()`(`Sync.hpp:134-141`):实际下发的传输周期与每周期数据量;`getSinkInfo().maxSize/minMTU/reqMTU/maxMTU` 为 sink 侧约束。
- `setSink` 的 MTU 参与传输块大小计算;`Find::measSendMTU(addr, out)`(两重载)实测可达 MTU;`mtuTest/mtuCheck`(`Sync.hpp:191-193`)。`THRED_MODE::NOJUMBOFRAME=8192` 可禁用巨帧。150 的 `configTransferSizeFix(size, no_remainder)` 可精确指定包数据尺寸。

### 4.6 实时/线程约束

- `THRED_MODE` 位集三版相同(`Sync.hpp:22-51`):`CRITICAL=1`(发送线程 critical 优先)、`NOSHORTSLEEP=2/NOSLEEP4CORE=4/IDLEONE=512/IDLEALL=1024/NOSLEEPFORCE=2048`(busy-loop 族)、`OCCUPIED=16`(绑核,配 open 的 cpuMain/cpuOther/rngOther 与 connect 的 cpu 参)、`FEEDBACKOFFSET=32/NOFASTFEEDBACK=256`(反馈滤波)、`LIMITRESEND=4096`、`NOJUMBOFRAME=8192`、`NOFIREWALL=16384`、`NORAWSOCKET=32768`(禁 DDS mode3)。示例惯用 `THRED_MODE(5)`=CRITICAL|NOSLEEP4CORE。
- 控制位 `playFlg/StopBuffer/StopPreFlg/StopPreDone/StopAutoDisconnect` 均 `std::atomic_bool`,`connectState/MSmodeSet` 149 起 atomic ⇒ **open/play/stop/disconnect/is_* 可安全地从任意线程调用**(148 的 connectState 例外,见 §2.1)。
- 错误模型:配置/连接类 API 全部返回 bool;不抛异常;诊断走 `ACQUA::SysLog`(Host 端口 `SyslogPortHost=19640`),`ACQUA::Socket::setLog(bool)`(★149+)可按 socket 关日志;协议应答默认 30ms 超时(`Connection.hpp:47`)。

---

## 5. 版本选择与迁移建议

**选型**:直接基于 **150**(头文件最全、文档最同步、push 接口有错误返回、状态查询无数据竞争)。149 仅在需要与其绑定生态时考虑;148 不建议新代码使用(线程安全与接口文档都差一档)。

**148 → 150 迁移清单**:

1. `checkStreamStart()` 调用点删除/改写(150 已移除),改用 `writeStreamStart(bool&)` 出参或 `setStream()` 返回值;
2. `notifyStreamDone` 覆写中依赖第 2 参"播放结束"的逻辑移除(150 恒 false);
3. `setupBuffer/setStream` 检查新返回值(可提前暴露配置错误与连接退出);
4. 若以外部 socket 构造 Connection/Find,补 bindport 实参(149 起必填);
5. 去掉为 148 `volatile connectState` 补的外部加锁(149+ 原子化);
6. 想要低首包延迟:评估 `connect(..., rapidStart=true)` 与 `setupBuffer(..., immediate_first_callback=true)`(后者要求 FS≥2),行为需实测;
7. 需要实际多流状态时用 `is_MSmode()`(注意 AUTO 模式下返回 NONE 的盲区)。

**150 仍未文档化、需实测的点**(继承自 148 分析,状态更新):`getNewStream` 允许的阻塞上限;`getNewStreamCmp` 确切调用时机;Rapid Start 的内部行为;`clearExtFormat` 的触发条件;块大小硬性对齐数值(仅知帧对齐)。`Sync::connect(int)` 参数含义、`writeStreamStart/addStream` 语义、`Find::Setting` 字段说明这三项在 149/150 已解决。

---

## 6. 预编译库选型(149/150 库清单)

命名:`lib{ACQUA,DirettaHost}_<arch>-<os>[-musl]<gcc><k|v?><microarch>[-nolog].a`

| 维度 | 取值 | 建议 |
|---|---|---|
| 架构 | x64 / aarch64 / riscv64 | 按目标机 |
| libc | glibc(无前缀)/ musl(`musl15` 等) | Alpine 用 musl 系 |
| 工具链 | `15*`(GCC15 ABI)/ `16*`(GCC16 ABI,149+) | 与编译器大版本匹配;三版都保留 15 系,混合时优先 `-15*` |
| 子变体 | `v2/v3/v4`(x86-64-v2/v3/v4)、`zen4`;aarch64 的 `k16/k4` | v3 覆盖 2013+ 的 x86-64(Haswell+,含 AVX2),通用服务器推荐 `-16v3` 或 `-15v3`;zen4 专属可选 |
| `-nolog` | 去除 SysLog 输出的裁剪版 | 生产可配合自有日志体系选用 |

链接:`-lstdc++ -lm -pthread`,示例 Makefile 用 `-flto -static`(LDFLAGS 含 `-static`,纯静态可执行)。构建:`make ARCH_NAME=x64-linux-16v3`(变量拼接库后缀)。

---

## 7. 本地修改与厂商原版的区分(重要)

| 文件 | 状态 | 证据 |
|---|---|---|
| `DirettaHostSDK_148/SinHost/SinHost.cpp` | **本地改过** | mtime 2026-08-24(同目录其他文件 2026-04-08);含 20 次重试查找循环与 "No Diretta target found!" 等日志,厂商版无 |
| `DirettaHostSDK_150/SinHost/SinHost_push.cpp` | **本地产物** | mtime 2026-08-27 23:35(厂商文件 08:36);= 厂商示例把 push 分支 `if(0)` 改 `if(1)` |
| `DirettaHostSDK_150/SinHost/SinHost_dbg.cpp` | **本地产物** | 同上;getNewStream 加了调用计数日志 |
| `DirettaHostSDK_150/SinHost/SinHost_diag.cpp` | **本地产物** | 同上;connectPrepare/connect/connectWait/play 逐阶段状态打印 + 3 秒 tick |
| `DirettaHostSDK_149/lib/Sync.o` | 厂商遗留构建产物 | — |

厂商 `SinHost.cpp` 内容:149 ≡ 150(diff 为空);与 148 原版仅差 UTF-8 BOM。做头文件级 diff 或提交补丁时,以上文件不应纳入对比基线。

---

## 8. 许可与使用限制(memo_host.txt,三版逐字节相同)

非商业用途;并入付费产品/服务/软件需商业授权(与软件自身是否收费无关,装到商业硬件/软件环境即触发);无质保、不接受提问;**仅支持 Linux**;禁止逆向工程。示例构建:`cd SinHost && make ARCH_NAME=x64-linux-15v2 && sudo ./SinHost`。

---

## 9. 关键文件索引(以 v150 为准)

| 文件 | 作用 | 版本差异 |
|---|---|---|
| `Host/Sync.hpp` | 播放引擎(pull 基类) | 149:is_MSmode+atomic;150:connect 扩参 |
| `Host/SyncBuffer.hpp` | push 缓冲 | 150:返回值/文档/checkStreamStart 移除 |
| `Host/Stream.hpp` / `diretta_stream.h` | 流对象与裸结构 | 三版同 |
| `Host/Format.hpp` | FormatID/Configure | 149:+clearExtFormat |
| `Host/Connection.hpp` | 控制连接/收发原语 | 149:bindport/sendMulti 扩参 |
| `Host/Find.hpp` | 目标发现/固件/参数设置 | 149:Broadcast/Multicast/bind port |
| `Host/Profile.hpp` | 传输 Profile/ProfileMaker | 150:configTransferSizeFix 扩参(ISO-8859+CRLF) |
| `Host/ACQUA/*` | Socket/Ethernet/Clock/线程工具 | 149:MACtype/setLog;150:ClockDiff |
| `SinHost/SinHost.cpp` | 唯一厂商示例(pull+push 双分支,push 分支 if(0) 关闭) | 149≡150 |
| `memo_host.txt` | pull/push 唯一明文出处 + 许可 | 三版同 |

相关文档:[diretta-host-sdk-148-contract-analysis.md](diretta-host-sdk-148-contract-analysis.md)(148 深度契约)、[Linux Headless Hi-Fi.md](Linux%20Headless%20Hi-Fi.md)(目标规格)。
