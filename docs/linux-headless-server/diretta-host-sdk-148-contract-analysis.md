# Diretta Host SDK 148 开发契约分析报告

> 分析对象:`/home/songlian/DirettaHostSDK_148`(头文件 + Doxygen HTML + 示例)
> 分析日期:2026-09-06
> 用途:为 SPlayer-Next-Headless 接入 Diretta 网络输出(alsa-target 模式的 SDK 替代路径)提供 API 契约依据
>
> 结论标注约定:"SDK 未明确说明"表示在 SDK 148 的头文件与 Doxygen 中均无文档、仅有间接证据,集成时需按保守假设处理。

---

## A. 音频格式切换的要求 / setSink 参数含义

**结论:SDK 的契约是"先设格式、后连接"。格式(setSinkConfigure)和传输参数(setSink/configTransfer*)都必须在 connectPrepare/connect 之前设置;SDK 没有提供任何"在连接状态下在线切换采样率/位深/声道/DSD 速率位序"的 API 或文档。切换格式的事实做法是走完整的 disconnect → setSink/setSinkConfigure → connectPrepare → connect → connectWait 流程(SDK 未用文字明确写出"必须重连",但接口设计中不存在在线切换入口)。**

证据:

- `/home/songlian/DirettaHostSDK_148/Host/Sync.hpp:101-103`

  ```cpp
  bool setSink(const ACQUA::IPAddress&,ACQUA::Clock,bool,std::uint32_t);  // setup sink connection
  bool setSinkConfigure(FormatConfigure);                                 // set sink playback format
  ```

  二者声明在 `connectPrepare()/connect()/connectWait()`(146-150 行)之前,示例中(`/home/songlian/DirettaHostSDK_148/SinHost/SinHost.cpp:174-187`)调用顺序为 `open → setSink → setSinkConfigure → configTransferAuto → connectPrepare → connect → connectWait → play`。

- 格式协商在协议层发生在连接阶段:`/home/songlian/DirettaHostSDK_148/doc/namespaceDIRETTA.html` 的 `MessageID` 枚举含 `FORMAT_REQ/FORMAT_ANS、CONNECTPRE_REQ/CONNECTPRE_ANS、CONNECT_REQ/CONNECT_ANS、DISCONNECT_REQ/DISCONNECT_ANS`——即格式是连接握手的一部分,不是流中可变参数。

- `setSink` 的第 3 个参数**不是 isDSD**。Doxygen 原文(`doc/classDIRETTA_1_1Sync.html` setSink 节,对应 `Sync.hpp:96-101`):

  > Parameters: **sink** address / **Sink** buffer time (if zeoro use default sink buffer time) / **Disable** Sink's playback rejection (NOP) / **Activate** MTU Use this value to calculate the transmission size.

  即参数依次为:sink 地址、sink 缓冲时间(0 = 用默认值)、"禁用 sink 的播放拒绝(NOP/brake)"、MTU(用于计算传输块大小)。示例注释(`SinHost.cpp:123`):`[target sink addres] [target buffer request] [nop brake] [host interface mtu]`。

- **是否是 DSD 由 setSinkConfigure 的 FormatID 决定**(`FormatID::FMT_DSD_*` 位),配套判断 API:`Sync.hpp:177-180` `checkSinkSupport(FormatConfigure)`、`getSinkConfigure()`;`Sync.hpp:182` `inquirySupportFormat(const ACQUA::IPAddress&)` "Retrieve the supported formats for Sink"。

- `changeWorkMode(THRED_MODE)`(`Sync.hpp:88-89`)只能在线改线程模式,无任何格式相关的 "change" API。

---

## B. Stream / getNewStream 的契约(pull 回调)

**回调签名与线程约束**(`/home/songlian/DirettaHostSDK_148/Host/Sync.hpp:248-252`,原文):

```cpp
/// @brief Callback for retrieving the send stream from the send thread (Must be processed by Atomic)
/// @param stream diretta_stream to be sent. Base class for Stream(diretta_stream = Stream copy to the base struct Stream::diretta_stream).
///        (The pointer must remain valid until it is dereferenced or this function is called again.)
/// @return false indicates termination; the buffer is not processed at that time.
virtual bool getNewStream(diretta_stream&)=0;
virtual bool getNewStreamCmp();
```

- **回调线程**:由 SDK 内部发送线程调用(成员 `std::thread thread_sync_node; void thread_sync_func();`,`Sync.hpp:276-277`),不是主线程。
- **原子性要求**:"(Must be processed by Atomic)",示例中再次强调 `SinHost.cpp:23` `//It must be processed atomic`——实现必须无锁、无阻塞等待。
- **内存所有权**:`diretta_stream` 只是 `{void* P; unsigned long long Size;}`(`/home/songlian/DirettaHostSDK_148/Host/diretta_stream.h:5-17`)。SDK 只在回调期间读取该指针(memcpy 到发送缓冲,`Sync.hpp:259-272` 的 `memcpySync*` 函数族),**不释放 buffer**;生命周期契约是"指针在下一次调用 getNewStream 之前必须保持有效"。示例把 `Stream Dummy` 作为类成员常驻(`SinHost.cpp:27,54,57`:`buf = Dummy;`——只拷贝 P/Size 到基类结构)。释放由 `Stream`/`ACQUA::Buffer` 析构负责(`Stream.hpp:99-102` 私有成员 `ACQUA::Buffer Data`)。
- **返回 false 的含义**:"false indicates termination; the buffer is not processed at that time." 即终止发送,本次 buffer 不被处理。
- **阻塞要求**:SDK 未明确说明"禁止阻塞",但结合 "Must be processed by Atomic" + 在发送线程调用,隐含要求非阻塞。
- `getNewStreamCmp()` 无任何文档注释(`Sync.hpp:252` 仅声明);`SyncBuffer` 覆写了它(`SyncBuffer.hpp:63`),并配合 `notifyStreamDone(Stream&, bool)`——"Callback for retrieving the playback buffer. @param buf Stream buffer. @param End of playback"(`SyncBuffer.hpp:64-67`),可推断 Cmp 是"上一个 stream 已被消费完"的回收通知,但**SDK 未明确说明**其确切语义。
- `Stream` 结构契约:`Data.P`/`Size` 继承自 `_diretta_stream`;`get()/get_8/16/32/64()` 返回各宽度指针(`Stream.hpp:69-88`);`resize_noremap` "Change the length without altering the actual memory size. if the size is zero, free the memory."(`Stream.hpp:33-46`);拷贝构造/赋值被 delete,仅可移动(`Stream.hpp:91-92`)。
- `SyncBuffer.hpp:23-25` 的 `writeStreamStart(bool&)`、`checkStreamStart()`、`addStream(Stream&)` **无文档注释**,SDK 未明确说明其用法。

---

## C. Sync(pull)vs SyncBuffer(push)

**结论(原文直接说明)**,`/home/songlian/DirettaHostSDK_148/memo_host.txt`:

> DIRETTA:Sync — buffer pulll type
> DIRETTA:SyncBuffer — buffer push type

- **Sync(pull)**:"Sync class Processing stream transmission"(`Sync.hpp:18`);`getNewStream` 为纯虚函数(`Sync.hpp:251`),发送线程每次要发数据时**拉取**——由 app 在回调中即时生成/填充数据。适合低延迟、数据可实时合成的场景(如 SinHost.cpp 的 TestSync 正弦波)。
- **SyncBuffer(push)**:"Builds the send buffer for Sync"(`SyncBuffer.hpp:9`),继承自 Sync 并覆写 `getNewStream`;app 先 `setupBuffer(FS, depth, ...)` 建环形缓冲,再用 `setStream(Stream&)` "Set the stream for data transmission. push mode"(`SyncBuffer.hpp:27-28`)推入。SDK 在数据耗尽时可自动填 mute(第 3 参 "Do not generate mute when depleted",`SyncBuffer.hpp:18`;示例 `SinHost.cpp:131-132` 注释 `[buffersize fs] [stac size] [buffer underrun is mute]`,传 false = 欠载出静音)。适合文件播放/解码线程与网络线程解耦的场景。
- **connect 的分叉点**:`SyncBuffer::connect(bool, int)`(`SyncBuffer.hpp:30-34`):"@param **true is callbacl mode. false is push mode.** @param CPU number of the main thread (when occupied)"——同一缓冲类可用回调模式或 push 模式启动。
- 结束方式不同:SyncBuffer 用 `pre_disconnect()` + `disconnect(true)`(见 D);Sync 直接 `disconnect()`。

---

## D. connect 流程与 disconnect/stop

**标准顺序(示例 `SinHost.cpp:174-191` pull 模式;119-169 push 模式)**:

```
open(mode, InfoCycle, ifno, name, id, cpuMain, cpuOther, rngOther, msMode)   // Sync.hpp:87
setSink(addr, bufferTime, nopBrake, mtu)                                     // Sync.hpp:101
setSinkConfigure(FormatID...)                                                // Sync.hpp:103
configTransferAuto / configTransferFix / configTransferVar ...               // Sync.hpp:114-132
connectPrepare(bool=true)   // "prepare to connect / Automatically adjust Trget delay"  Sync.hpp:144-146
connect(int)                // "connect to sink"                               Sync.hpp:147-148
connectWait()               // "wait for connection completion"                Sync.hpp:149-150
play()                      // "start playback"                                Sync.hpp:158-159
... 播放中用 is_connect()/is_online() 轮询 ...
stop() / disconnect(...) / disconnectWait() / close()
```

- `connectPrepare(true)`:默认参数含义 "Automatically adjust Trget delay"。
- `Sync::connect(int)` 的 int 参数在文档中**未命名、未说明**(doc 页参数为空);`SyncBuffer::connect(bool,int)` 的 int 明确为 "CPU number of the main thread (when occupied)"。示例传 0。
- **状态机**(`Sync.hpp:166-171`):`is_connect()` = `CONNECT_REQ||CONNECT||DISCONNECT_REQ`;`is_online()` = 仅 `CONNECT`;`is_disconnect()` = `DISCONNECT_REQ||DISCONNECT`。内部枚举 `REQ_STATE{DISCONNECT, CONNECT_REQ, CONNECT, DISCONNECT_REQ}`(280-286 行)。
- **disconnect(bool wait=true)**:`Sync.hpp:153-155` "@param wait wait for disconnection completion"。`true`(默认)= 阻塞直到断开完成;`false` = 只发起断开、立即返回,之后可调用 `disconnectWait()`(157 行)等待。另有 `disconnect_flgset()`(151-152 行)"Set the flag to start cutting"。
- **SyncBuffer 的特殊语义**(`SyncBuffer.hpp:41-47`):
  - `pre_disconnect(bool=false)` "Pre-disconnect the sink"(停止接受写入,示例 `SinHost.cpp:160` 在推完最后一块后调用 `pre_disconnect(true)`);
  - `disconnect(bool=true)` "Disconnect from the sink. **Declare the end of buffer writing.** @param **true: Automatically terminates when the buffer is exhausted**"——true 表示缓冲播完自动断开,false 立即断。
- **stop()**:"stop playback(pause)"(`Sync.hpp:160-161`),是**暂停**而非断开;`isPlay()` 反映 `playFlg`(`std::atomic_bool`,291 行,可跨线程调用)。SDK 未明确说明 stop 与 disconnect 的强制先后关系;示例 pull 模式从未调用 stop(直接结束进程),push 模式用 `pre_disconnect(true)` 代替 stop。

---

## E. FormatID 编码规则(`/home/songlian/DirettaHostSDK_148/Host/Format.hpp:6-66`)

类注释原文(第 6 行):

> Format ID definition. Diretta represents playback formats in 64-bit. **Composed entirely of bit ORs**, it enables determination of which formats the sink supports. When performing a bitwise AND operation, the result will be the same value.

即 FormatID 同时用作"播放格式"和"能力位图":`支持位图 & 请求 == 请求` 则支持。**限 48 bit**(第 7 行注释 `//limit 48bit`;另有第 64 行 `DDS=0x0080000000000000ULL` 位于 bit55,注释 //48bit):

| 位段 | 常量(行号) | 含义 |
|---|---|---|
| bit0–7 声道 | `CHA_1=1<<0, CHA_2=1<<1, CHA_4=1<<2, CHA_6=1<<3, CHA_8=1<<4, CHA_16=1<<5, CHA_MSK=0xFF`(9-15 行) | 声道数 |
| bit8–15 PCM 位深 | `FMT_PCM_SIGNED_8/16/24/32/64 = 0x100/0x200/0x400/0x800/0x1000`,`FMT_PCM_FLOAT_32=0x2000, FMT_PCM_FLOAT_64=0x4000, FMT_PCM_MSK=0xFF00`(19-26 行) | 位深/编码,二者互斥按 OR 之一 |
| bit16–19 DSD 速率 | `FMT_DSD1=0x10000, FMT_DSD4=0x20000, FMT_DSD_BIT_MSK=0xF0000`(30-32 行) | DSD 倍速:**v148 只有 DSD1(1bit 档)与 DSD4;不存在 FMT_DSD2**(全 SDK grep 无 DSD2) |
| bit20–23 位序 | `FMT_DSD_LSB=0x100000 //DSF, FMT_DSD_MSB=0x200000 //DFF, FMT_DSD_LITTLE=0x400000, FMT_DSD_BIG=0x800000, FMT_DSD_ORDER_MSK=0xF00000`(33-37 行) | LSB/MSB first(对应 DSF/DFF 文件格式)+ 字节序 |
| bit24–31 DSD 打包宽度 | `//FMT_DSD_SIZ_8=0x1000000 //not recommend`(被注释掉),`FMT_DSD_SIZ_32=0x2000000 //LSB 1 2 3 4 (LE)  MSB 4 3 2 1(LE)`, `FMT_DSD_SIZE_MSK=0xFF000000`(38-40 行) | 每字打包位数 |
| bit32–47 采样率 | `RAT_8000=1<<32, RAT_44100=1<<33, RAT_48000=1<<34`(基频,`RAT_BASE_MSK=0x7_00000000`);`RAT_MP1=1<<35 … RAT_MP4096=1<<47`(倍频,`RAT_MP_MSK=0xFFF8_00000000`);`RAT_MSK=0xFFFF_00000000`(44-62 行) | **采样率 = base × multiplier**。示例 `SinHost.cpp:126`:`CHA_2|FMT_PCM_SIGNED_32|RAT_44100|RAT_MP2` 注释 "PCM32bit 2ch 88.2KHz" |
| bit55 | `DDS=0x0080000000000000ULL`(64 行) | Diretta Direct 模式(`FormatConfigure::isDDS()/setDDS()`,Format.hpp:182-184) |

- `Sync::Info` 能力查询(`Sync.hpp:199-243`):`supportPCM / supportDSDlsb / supportDSDmsb`(三个 FormatID 位图,非 0 即支持:`checkSinkSupportPCM()/checkSinkSupportDSD()/checkSinkSupportDSDlsb()/checkSinkSupportDSDmsb()`)。
- `FormatSupport`(Format.hpp:83-122)提供从能力位图取 min/max 的 API:`getChMin/Max、havePCM、haveDSD、getBitsMin/Max、getSpeedBaseMin/Max、getSpeedMultMin/Max、getSpeedMin/Max、getNoneBaseMin、getFrameMax`。
- `FormatConfigure`(Format.hpp:124-188)是"生成播放 FormatID 的辅助类":`setSpeed(uint64_t)/setChannel/setFormat、getBits/getChannel/getFrameSize/get1secSize/getMuteByte/isDSD/isDSDmsb/...`。
- 另有独立的 `FormatPreID{FMT_PCM=0x100, FMT_FLOAT=0x200, FMT_DSD1=0x400, FMT_DSD4=0x800, FMT_MSK=0xFF00}` + `FormatPreConfigure`(Format.hpp:190-226),用途 "Pre-format configuration (**For setting the base frequency**)",带 `setLatency/getLatency`。

---

## F. setStream / Period / 块大小 / MTU

- **Period 字段在本版(148)中不存在**:`diretta_stream` 仅有 `Data.P` 与 `Size`(`diretta_stream.h:5-17`;`doc/struct__diretta__stream.html` 同样只列出这两个成员)。若在旧版 SDK 资料中看到 `Period`,v148 已移除。每次传输的数据量改由 `getCycleSize()/getCyclePackets()/Info.maxSize` 表达。
- **push 块大小要求**:`setStream(Stream&)` 本身无大小文档;示例 `SinHost.cpp:129-131,151` 表明块大小应为**帧对齐**:

  ```cpp
  const int alliment = syncbuffer.getSinkConfigure().getFrameSize();   // = wid*channels
  const int fs1sec  = syncbuffer.getSinkConfigure().get1secSize()/alliment;
  str.resize(fs1sec*alliment);  // 1 秒数据,按帧对齐
  syncbuffer.setStream(str);
  ```

- **setupBuffer**(`SyncBuffer.hpp:15-20`):"@param **FS frame count not byte size** / @param **depth (total size = FS*depth)** / @param Do not generate mute when depleted / @param format Configure format"。
- **每周期传输量**(`Sync.hpp:134-141`):`getCycleTime()` "Generated transmission interval"、`getMinCycleTime()`、`getCycleSize()` "**Stream data size per transmission**"、`getCyclePackets()` "Stream packet count per transmission"。
- **MTU 关系**:`setSink` 第 4 参 "Activate MTU Use this value to calculate the transmission size"(`Sync.hpp:100`);`Sync.hpp:186-189` `mtuTest()/mtuCheck()`;`Sync::Info.maxSize`("sink maximum data size per transmission (bytes)")、`minMTU/reqMTU/maxMTU`(`Sync.hpp:225-232`);线程模式位 `NOJUMBOFRAME=8192` "Do not use jumbo frame(MTU)"(`Sync.hpp:45-46`)。ProfileMaker 侧对应 `configTransferSizeFix(size_t)` "Transmission Size Specification Mode. PacketData Size: Bytes" 与 `configTransferSizeMax()`(doc/classDIRETTA_1_1ProfileMaker.html;`Profile.hpp:93-97`)。
- 对齐的硬性要求(如必须为 4/8 字节倍数)**SDK 未明确说明**;唯一可引用的对齐证据是示例的 `getFrameSize()` 对齐。

---

## G. 实时/线程/错误处理约束

- **getNewStream 原子性**(最重要的 RT 约束):见 B,`Sync.hpp:248` "(Must be processed by Atomic)" + `SinHost.cpp:23`。
- **线程模式 THRED_MODE**(`Sync.hpp:21-51`,bit-OR 组合):`CRITICAL=1`(发送线程提为 critical 优先级)、`NOSHORTSLEEP=2`/`NOSLEEP4CORE=4`/`IDLEONE=512`/`IDLEALL=1024`/`NOSLEEPFORCE=2048`(busy loop 策略)、`OCCUPIED=16`(绑核,配合 open 的 cpuMain/cpuOther/rngOther)、`FEEDBACKOFFSET=32`(反馈改滑动平均)、`NOFASTFEEDBACK=256`、`LIMITRESEND=4096`、`NOJUMBOFRAME=8192`、`NOFIREWALL=16384`、`NORAWSOCKET=32768`("Do not use raw socket(no use DDS mode3)")。示例用 `THRED_MODE(5)` = CRITICAL|NOSLEEP4CORE(`SinHost.cpp:122,174`)。
- **线程模型**:Sync 内部一线程 `thread_sync_node`(`Sync.hpp:276-277`);SyncBuffer 另有 `thread_buffer_node` + `mtxRead/mtxWrite/cvRead/cvWrite` 双锁双条件变量(`SyncBuffer.hpp:76-81`);控制位 `playFlg/StopBuffer/StopPreFlg/StopPreDone/notifyStreamFlg/StopAutoDisconnect` 均为 `std::atomic_bool`(`Sync.hpp:291`、`SyncBuffer.hpp:84-88`),`connectState` 为 `volatile`(`Sync.hpp:287`)——即 play/stop/disconnect 可从其他线程发起。可覆写钩子:`startSyncWorker()/statusUpdate()`(`Sync.hpp:254-255`)、`startBufferWorker()/notifyStreamDone()`(`SyncBuffer.hpp:66-69`)、`syncWorker()`(`Sync.hpp:195-196` "sync worker process")。
- **CPU 亲和/优先级工具**:`ACQUA::ThreadPriority`(`Host/ACQUA/ThreadPriority.hpp`)提供 `Priority::CRITICAL`、线程优先级修改、CPU 核数查询、亲和性设置。
- **错误处理契约**:`open/setSink/setSinkConfigure/configTransferFix|Var|Random/connectPrepare/connect/connectWait/mtuCheck/checkSinkSupport/inquirySupportFormat/inquiryParameter` 全部返回 bool(`Sync.hpp:87-189`);连接状态用 `is_connect/is_online/is_disconnect/is_active` 轮询;协议层 `Connection::sendAck(...)` 默认 30ms 应答超时(`Connection.hpp:46`);`ConnectionBuffer::empty()` 为 "NAC etc no ACK"(`Connection.hpp:150-151`)。SDK 不抛异常,错误通过返回值 + `ACQUA::SysLog`(`SysLog.hpp`,Host 端口 `SyslogPortHost=19640`)表达。
- **许可/平台约束**(`memo_host.txt`):非商业用途;商用需授权;无任何质保、不接受提问;仅支持 Linux;禁止逆向工程。`Host/Release.hpp`:`ReleaseNo = 148`。
- **SDK 未明确说明的部分**(集成时需保守处理或实测验证):
  - `getNewStream` 是否允许短暂阻塞;
  - `getNewStreamCmp` 的确切调用时机;
  - `writeStreamStart/checkStreamStart/addStream` 的用法;
  - `Sync::connect(int)` 参数含义;
  - 块大小/内存对齐的硬性数值要求;
  - 在线切换格式是否被协议容忍。

---

## 关键文件清单

- 头文件:`/home/songlian/DirettaHostSDK_148/Host/Sync.hpp`、`Stream.hpp`、`SyncBuffer.hpp`、`Format.hpp`、`Connection.hpp`、`Profile.hpp`、`diretta_stream.h`、`Receive.hpp`、`Send.hpp`、`Find.hpp`
- 示例:`/home/songlian/DirettaHostSDK_148/SinHost/SinHost.cpp`
- 文档:`/home/songlian/DirettaHostSDK_148/doc/classDIRETTA_1_1Sync.html`、`classDIRETTA_1_1SyncBuffer.html`、`classDIRETTA_1_1Stream.html`、`structDIRETTA_1_1Sync_1_1Info.html`、`struct__diretta__stream.html`、`classDIRETTA_1_1FormatSupport.html`、`classDIRETTA_1_1FormatConfigure.html`、`classDIRETTA_1_1ProfileMaker.html`、`namespaceDIRETTA.html`
- 说明:`/home/songlian/DirettaHostSDK_148/memo_host.txt`(Sync=pull / SyncBuffer=push 的唯一明文出处)
