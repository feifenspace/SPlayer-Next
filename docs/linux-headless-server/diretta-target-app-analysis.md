# DirettaTargetApp(Target/Sink 端)代码分析 —— 开发文档

> 分析对象:`/home/songlian/DirettaTargetApp`(2026-04-16 版,readme/Makefile 基准)
> 分析日期:2026-09-06(方法:全部源码/脚本精读 + `nm -C` 库符号逆向 + 运行时字符串提取)
> 用途:为 SPlayer-Next-Headless(Host 端)提供对端(Sink)行为理解、联调参照与协议对映
> 姊妹篇:[diretta-host-sdk-148-149-150-dev-guide.md](diretta-host-sdk-148-149-150-dev-guide.md)(Host 端 SDK)

---

## 0. 结论(TL;DR)

1. **这是 Diretta 架构的 Sink(目标设备)端应用**:闭源静态库 `libDirettaApp_*.a`(约 1MB/架构,498 个导出符号)+ 极薄的启动包装代码。仓库里**唯一的实质开源实现是 DDS 内核驱动**(`DDS/diretta_direct.c`,约 450 行,Dual BSD/GPL)。
2. **开放面积极小**:`DirettaApp.hpp` 只暴露 `diretta_app_target(vid)` 与激活函数族;播放管线(ALSA 输出、格式协商、DoP/DSD Native、音量)全部在库内,由 `diretta_app_target_setting.inf` 配置驱动。
3. **许可体系内建**:未激活时库进入限制模式(字符串证据:`Play Limited Mode 6minutes` / `Limited Mode Stop 6minutes`),激活走 `diretta_app_activate` → URL/文件下载 → 公钥验证(`pub.key` PEM,编进可执行文件)。
4. **与 Host SDK 的协议对映已从符号层确认**(§4.3 表):Host 的 `Find::SinkSetting` 对应 Target 的 `Sink::ConfigGet/Set`、`Find::findStatus` 对应 `Status::*`、`Find::Transfer` 对应 `Target::SetTrancefer`、`Sync/SyncBuffer` 对应 `Sink/SinkBuffer`。
5. **DDS 驱动有一个真实缺陷**(§6.4):MAC 过滤 ioctl 用反了 `copy_to_user`(应为 `copy_from_user`),源/目的 MAC 过滤实际无法设置;文件头自述 "prototype rev 0",与 `dds.txt`"仅接收侧、原型"的定位一致。
6. **对本项目的边界**:Target 库闭源且带商业授权检查,**不可集成进 SPlayer**;价值在于——① 提供可用的对端联调靶机;② inf 参数语义(ALsaLatency/AlsaInterval/VolumeCtl/MTU)是理解 Sink 行为与 Host 侧参数选择的权威参照;③ DDS 驱动可直接编译用于 mode3 实验。

---

## 1. 项目结构与构建体系

```
DirettaTargetApp/
├── Makefile                     # 顶层:按 VID 选择 key,组装 diretta_app/ 发布目录
├── readme.txt                   # 依赖、构建命令、inf 参数文档(权威)
├── diretta_app.sh               # 运行脚本:activate → 无限循环拉起 target(3s 重启)
├── diretta_app_target_setting.inf  # 运行配置(出厂仅 [global] 空节)
├── GentooPr.key                 # VenderID="GentooPr" 的密钥文件(C++ 源码片段,见 §3.1)
├── logo*.png                    # Target 图标(Host Find::downloadIcon 拉取)
├── DirettaApp/
│   ├── DirettaApp.hpp           # 唯一公开头(见 §3)
│   └── libDirettaApp_*.a        # 闭源主库,22 个变体(见 §7)
├── diretta_app_target/          # 主程序包装(diretta_app_target.cpp,13 行)
├── diretta_app_activate/        # 激活工具(31 行)
├── diretta_app_check/           # 激活校验工具(18 行)
└── DDS/                         # DDS 内核驱动源码(唯一开源实现,见 §6)
    ├── diretta_direct.c/.h
    ├── dds.txt                  # DDS 官方说明(含 Sink::Setting MTU 字段文档)
    └── Makefile                 # 标准外部内核模块构建
```

### 1.1 构建流程(顶层 Makefile)

```sh
apt-get install make g++ libssl-dev libasound2-dev libcurl4-openssl-dev
make ARCH_NAME=x64-linux-15v2 VID=./GentooPr.key       # VID 传 key 文件路径
LOG=DISABLE make ...                                    # 可选:链接 -nolog 库并定义 SYSLOG_DISABLE
```

- `VIDN = $(notdir $(basename $(VID)))` → `GentooPr`;`ln -s ./GentooPr.key ./pub.key`,三个工具都 `#include "../pub.key"`——**key 文件即 C++ 源码片段**(定义 `DirettaVID` 与 `DirettaVIDkey`),随可执行文件编译进去。
- 子 Makefile 统一 `-DVID="\"$(VIDN)\""`;target 工具里 `diretta_app_target(VID)` 传的就是这个字符串。
- 产物目录 `diretta_app/`:target、activate 两个可执行 + `diretta_app.sh` + inf + logo*.png——**自包含发布包**;库内通过 `/proc/self/exe` 定位自身目录读取 inf/logo(字符串证据)。
- 依赖:`-lcrypto -lcurl -lasound`(-lstdc++fs);activate/check 另有 `-DWITH_GZFILEOP`(zlib 压缩 License 载荷)。
- ⚠️ readme 示例 `make ARCH_NAME=x86_64-linux VID=XXXX` **已过时**:现有库清单里没有 `libDirettaApp_x86_64-linux.a`,必须用 §7 表中的完整变体名(如 `x64-linux-15v2` 或 `x86_64-linux-gcc12`)。

### 1.2 运行模型(diretta_app.sh)

```sh
diretta_app_activate            # 检查/执行激活(未激活打印 URL 后退出)
while true; do
    diretta_app_target          # 阻塞运行 Target 主服务;退出(崩溃/被断)后 3 秒重启
    sleep 3
done
```

脚本即监督进程:target 退出码非 0 仅打印 "diretta_app_target boot up failure"。

---

## 2. 许可与激活体系

### 2.1 密钥文件格式(`*.key` = C++ 片段)

```cpp
const char DirettaVID[8+1] = "GentooPr";        // 8 字符 VenderID(Host 侧 Find::Setting::MyID 对应)
const char* DirettaVIDkey =
"-----BEGIN PUBLIC KEY-----\n" ... "-----END PUBLIC KEY-----\n";   // PEM 公钥
```

Host 发现 Target 时看到的 VenderID 即 `DirettaVID`;授权校验用配套私钥签发的许可证 + 此公钥验证(readme:"Download the license from the authentication server and perform a valid check")。

### 2.2 激活流程(diretta_app_activate.cpp,逐行语义)

```
diretta_app_activate_file()          ── 本地已有激活文件且有效?
├─ 成功 → 打印 valid/invalid,结束
└─ 失败(nofile)
   ├─ diretta_app_dl_save(activ)     ── 尝试从服务器下载并保存激活文件
   │   ├─ activ=true  → 保存成功,回到上方重校验
   │   └─ activ=false → 未找到下载文件:
   │        diretta_app_activate_url(url) ── 取激活 URL 并打印,退出
   │                                   (用户在浏览器完成注册后重新运行)
```

`diretta_app_check <hash>`:独立校验工具,`diretta_app_dl_check(done, hash)` 按哈希下载 License 并验证,打印 valid/invalid,invalid 退出码 -1。readme 注明该工具"不必包含在发布物中"。

### 2.3 未激活行为(库字符串证据)

| 字符串 | 含义 |
|---|---|
| `NO ACTIVATE` / `Diretta App activate mode` / `Activate unlock` | 激活状态切换日志 |
| `Diretta App limited mode` / `(limited)` | 进入限制模式 |
| `Play Limited Mode 6minutes` | 未激活可播放 6 分钟 |
| `Limited Mode Stop 6minutes` | 随后停止 6 分钟(循环) |

对应符号层的 `CertifiedTarget` / `CertifiedSinkAlsa`("Certified" 前缀 = 许可校验过的运行路径)。

---

## 3. 公开 API(DirettaApp.hpp,全部开放面)

```cpp
extern const char DirettaVID[8+1];         // 由 key 文件提供
extern const char* DirettaVIDkey;

// 激活函数族(§2)
extern bool diretta_app_activate_file();
extern bool diretta_app_dl_save(bool& done);
extern bool diretta_app_activate_url(std::string& str);
extern bool diretta_app_activate_valid();
extern bool diretta_app_activate_valid(std::string& hash);
extern bool diretta_app_dl_check(bool& done, const std::string& hash);

// 主入口:阻塞运行整个 Target 服务(发现应答/流接收/ALSA 输出/状态上报全在库内)
extern bool diretta_app_target(const char vid[8+1]);

// 通用工作线程包装(库内导出,包装工具未使用,可复用)
class NodeRun {
public:
    NodeRun(); ~NodeRun();
    void start(); void stop();
    bool finish(); bool is_run();
protected:
    virtual bool worker(volatile bool&)=0;   // RUN 协作标志
private:
    std::thread th; volatile bool RUN; volatile bool RET;
};
```

**13 行的 `diretta_app_target.cpp` 就是全部"应用层"**:调用一个函数,其余全在库内。inf 配置、ALSA 设备选择、格式开关都是库读文件自洽的,没有编程接口。

---

## 4. 库内部架构(nm 逆向,`libDirettaApp_x64-linux-15v3.a`,498 导出符号)

### 4.1 类清单

| 类 | 角色 | 关键方法 |
|---|---|---|
| `DIRETTA::Target` | 顶层控制平面:发现应答、远程配置、固件传输、重启 | `open(bool / int,bool / IPAddress,bool / list<IPAddress>,bool)`(按接口选择)、`getState/isPlay/getRebootCode`、`ConfigGet/ConfigSet/ConfigFinale`、`SetTrancefer/FinishTrancefer`(sic,厂商拼写)、`PlayThreadNotify/StopThreadNotify`(向上回调)、`udp_worker/udp_worker_m`(m=multiport 多流) |
| `DIRETTA::Sink` | 数据平面基类:接收 Sync 流 | `open(unsigned short&,unsigned short&)`、`YouToMy(IPAddress,Clock)`、`nop(IPAddress,unsigned short,bool)`(对映 Host setSink 的 nopBrake 应答)、`stop(unsigned short,bool)`、`setPhaseInvert(bool)`、`streamWorker/controlLoop/thread_stream_func`、`errDisconnect/preDisconnectSecond`、`convert{2,3,4,8}from{1,2,3,4}(StreamReceive&)`(声道重排)、`updateStreamRate` |
| `DIRETTA::SinkBuffer` | Sink 的缓冲变体(对映 Host SyncBuffer) | `connectStream(const std::string&)`(连接 ALSA 设备名,如 `"hw:..."`)、`pushBackStream(StreamReceive&)`、`getStream/doneStream`、`prepareFormat(FormatPreConfigure&)`(基频预备,对映 Host FormatPreConfigure)、`getStreamRate(unsigned short&,unsigned short&)`、`getLatency/setLatency(1 或 3 参)`、`transportWorker/startTransportWorker` |
| `CertifiedSinkAlsa` | ALSA 输出实现(许可路径) | `Setting`(嵌套)、`open(int)`、`setStream(StreamReceive&)`、`alsa_write/__alsa_write/_alsa_write`、`checkFormat(FormatID&)/changeFormat(FormatConfigure&)`、`startStream(Clock)`、`reopen/disconnectStream/connectStream`、`volumeCtl(short)`、`controlLoop`、`CertifiedStatus::NotifyVolume` |
| `CertifiedTarget` | 许可版 Target | `ctor(Target::Setting&, bool, 3×string, TMODE)`、`tsUpdate`、`write_worker` |
| `DIRETTA::Status` | 状态上报平面(Host `findStatus` 的数据源) | `open/close/controlLoop`、`setPlay(bool)`、`setFormat(FormatID)`、`setVolume(short)`、`NotifyValue(unsigned short,unsigned long)`、`NotifyVolume(unsigned short,short)` |
| `DIRETTA::DDS` | DDS mode3 用户态端(配 §6 驱动) | `open/close/is_open/clear`、`setPcm`、`start_receive/stop_receive`、`receiveOff(Buffer&,size_t)` |
| `FifoStreamReceive` / `StreamReceive` / `Receive`/`ReceiveBuffer`/`SendBuffer`/`Send` | 流与协议收发原语 | `push/pop/size_data`;`Send::set(MessageID,StatusID,...)` 与 Host `ConnectionBuffer` 同构 |
| `DIRETTA::Icon`(`Icon::Pixel`) | 图标(logo*.png) | 经 `Target::Setting::_Icons` 提供,Host `Find::downloadIcon` 拉取 |
| `NodeTarget`(内部) | 应用框架 | 符号显示其 worker 使用 `ACQUA::TCPV6Server`(Target 侧另有 TCPv6 服务,具体用途未文档化) |

### 4.2 ALSA 路径(字符串证据)

- 打开/配置:`snd_pcm_open`、`snd_pcm_set_params`(带 `Retry` 重试)、`snd_pcm_hw_params_any/get_buffer_size_max/min/get_channels_min/max/get_rate_min/max`、`snd_pcm_open re Err`/`alsa reopen Err`——**运行中格式切换失败会 reopen**;`connectStream/disconnectStream/reopen` 三态管理。
- `alsa format not match Err`:Host 请求格式与 DAC 能力不符时的错误路径。
- `AlsaMaxCh` 字符串存在:疑似未文档化的 inf 键(通道上限),readme 未收录,需实测。

### 4.3 与 Host SDK 的协议对映(开发联调对照表)

| Host 侧 API(148-150 SDK) | Target 库内部 | 协议观察点 |
|---|---|---|
| `Find::findOutput/findTarget` | `Target::open` 后的发现应答 | 多播/广播 MATCH_REQ |
| `Find::getOutput`(PI/PO 端口) | `Sink::open(unsigned short&,unsigned short&)` | 端口协商 |
| `Find::TargetSetting/ConfigGet/Set/Finale` | `Target::ConfigGet/ConfigSet/ConfigFinale` | 远程配置读写 |
| `Find::SinkSetting/setSinkSetting/SinkSettingFinale` | `Sink::ConfigGet/ConfigSet/ConfigFinale` | 同上,Sink 域 |
| `Find::findStatus/StatusSort` | `Status::setPlay/setFormat/setVolume/Notify*` | 状态上报(播放/格式/音量) |
| `Find::Transfer/TransferFinish`(DFirmware) | `Target::SetTrancefer/FinishTrancefer` | 固件传输 |
| `Find::Reboot` | `Target::getRebootCode` | 重启请求码 |
| `Find::downloadIcon` | `Target::Setting::_Icons`(logo*.png) | 图标下载 |
| `Sync::setSink(nopBrake)` | `Sink::nop(IPAddress,unsigned short,bool)` | 播放拒绝(NOP/brake) |
| `Sync/SyncBuffer`(pull/push) | `Sink/SinkBuffer`(streamWorker/transportWorker,pushBackStream) | 流数据面 |
| `FormatPreConfigure`(基频/latency) | `SinkBuffer::prepareFormat(FormatPreConfigure&)` | 预备格式 |
| `Sync::Info.latencyBuffer/latencyMax/latencyHw` | `SinkBuffer::getLatency/setLatency(1~3 参)` | 三个延迟字段一一对应 |
| `THRED_MODE::NORAWSOCKET` ↔ DDS mode3 | `DIRETTA::DDS` + `/dev/diretta-direct` | mode3 收发 |
| multiport(`TargetConnectInfo::multiport`) | `Target::udp_worker_m` | 多流节点 |

---

## 5. 运行时配置:`diretta_app_target_setting.inf`

INI 格式(FileInf 解析,`[global]` 节头必须存在;库经 `/proc/self/exe` 在可执行文件同目录寻找)。readme 为权威文档,以下逐键整理:

| 键 | 类型/取值 | 默认 | 说明(readme 原文语义) |
|---|---|---|---|
| `TargetName` | string | 内置默认名 | Host 端看到的目标名 |
| `DoPauto` | enable/disable | enable | 对不支持 DSD Native 的设备启用 DoP 封装 |
| `DSDNativ` | enable/disable | enable | 强制禁用 DSD Native(只走 DoP) |
| `DSD48` | enable/disable | disable | 接受 DSD 48K 系(fs×48/16) |
| `PCM32bit` / `PCM24bit` / `PCM16bit` | enable/disable | 均 enable | 各位深接受开关(能力协商=Host FormatID 位图的实际来源) |
| `AlsaLatency` | 整数 msec | 10(0=驱动默认) | ALSA 缓冲时长 |
| `AlsaInterval` | 整数 msec | 0 | **格式切换最小间隔**(距上次播放);避免 USB DAC 快速切换格式死机 |
| `VolumeCtl` | enable / mute / 数字 | disable | 启动音量:0=最大;-60 或 mute=最小;-20=-20dB 起 |
| `ExtEtherMTU` | 字节数(>1500)或 0/1/2/3 | 0(=1500) | 巨帧:典型 4088/9014/16128;或枚举 1=4088 2=9014 3=16128 |
| `EtherMTU` | 字节数 | — | 网卡实际 MTU(如 RPi5 板载网卡 3058);与 ExtEtherMTU 分开填写 |

日志相关:`LOG=DISABLE` 是**构建期**选项(inf 无法关日志);运行日志含 `AlsaLatency : / AlsaInterval :` 回显、DDS 的 `regisr dds X/unregisr dds` 等。

**与 Host 参数的联动理解**(联调要点):PCM 位深开关决定 Host `checkSinkSupport` 位图;`AlsaLatency` 大致对应 Host `Sync::Info.latencyHw/latencyBuffer` 的物理来源;`EtherMTU/ExtEtherMTU` 对应 `dds.txt` 的 `Sink::Setting::MtuSize/MtuSizeMax`(Host 日志 `DATASIZE_REQ smtu/amtu/min/req/max` 里的 min/req/max 即来自 Sink 这三个 Setting)。

---

## 6. DDS 内核驱动(`DDS/diretta_direct.c`,Dual BSD/GPL)

### 6.1 定位(dds.txt 原文要点)

- 支持 **DDS(Diretta Direct Stream)**,**不支持**音频与 Diretta 协议本体;当前**仅接收侧**(Target 用);发送侧走 RAW_SOCKET(库内实现,Host/Target 皆同)。
- 绕开 IPv6 socket 使用 MS Mode3 必须装此驱动;Host 装了也只是不生效,无副作用。
- 验证检查点:内核 `dds regist X` → Target 日志 `regisr dds X` → Host 日志 `Open Raw Socket` → 连接后双方输出 `mode3`。

### 6.2 设备与协议参数

| 项 | 值 | 出处 |
|---|---|---|
| EtherType | `0xCB4B`(`DDS_ETHERTYPE`,`dev_add_pack` 收包) | diretta_direct.c:10,379 |
| 设备节点 | `/dev/diretta-direct`(char dev,class 同名;兼容 6.4 前后 `class_create` API) | .h:12,.c:395-411 |
| 节点数 | 127(port 0 保留调试;`DDS_NODE_MAX_IOCTL` 查询) | .c:9 |
| 端口编码 | DDS 头 2 字节:`byte0&0x7F`=port,`byte1`=ctrl | .c:272-274 |
| 接收环形缓冲 | 每节点 64 深 `sk_buff*`(`DDS_RCVBF_MAX`),满则丢弃计数 | .c:8,324-335 |
| 读超时 | 默认 HZ/10=100ms;可 ioctl 设 msec | .c:163 |

### 6.3 ioctl 全集(diretta_direct.h)

| ioctl | 方向 | 语义 |
|---|---|---|
| `DDS_REGIST_IOCTL` | _IO | 注册流端口,**返回端口号**;失败 -EBUSY;fd 私有数据绑定 |
| `DDS_UNREGIST_IOCTL` | _IO | 释放端口(close 时也自动释放) |
| `DDS_CLEAR_IOCTL` | _IO | 清空接收缓冲 |
| `DDS_TIMEOUT_IOCTL` | _IOW int | read 等待超时(msec,0=默认 100ms) |
| `DDS_FILTER_SRC_IOCTL` / `DDS_FILTER_DEST_IOCTL` | _IOW mac | 源/目的 MAC 过滤(⚠️ 见 §6.4) |
| `DDS_FILTER_LEN_IOCTL` | _IOW int | 帧长过滤:≥44 时精确匹配,否则丢弃 >44 的帧(以太 60B 减头) |
| `DDS_NODE_MAX_IOCTL` | _IO | 返回 127 |
| `DDS_ETHERTYPE_IOCTL` | _IO | 返回 0xCB4B |
| `DDS_WAKEUP_IOCTL` | _IO | 中断阻塞中的 read,使其返回 0 |

`read()` 返回:2 字节 DDS 头 + 有效载荷(最多请求数);无数据时按超时等待,可被 WAKEUP 打断。

### 6.4 已知缺陷与质量注意点(阅读源码确认)

1. **MAC 过滤 ioctl 方向反了**(diretta_direct.c:122,127):

   ```c
   case DDS_FILTER_SRC_CMD://set mac address
       if(copy_to_user(node->srcmac,(void __user *)arg,6)!=0){   // 应为 copy_from_user
   ```

   `copy_to_user` 第一参是用户态目标指针,这里传了内核地址 `node->srcmac`——过滤地址设置失败(EFAULT)或未定义行为,**源/目的 MAC 过滤实际不可用**。自述 "prototype rev 0",与 dds.txt"仅接收、原型"一致;若真需要 MAC 过滤须自行修补。

2. `dds_read` 中 `consumed+=avail` 用的是本块可用量而非实际拷贝量 `cp`(c.227);在 `siz` 截断的边界上有跳读风险,正常路径二者相等。
3. 并发保护不对称:`regist/unregist` 有 spinlock,但收包路径 `dds_receive` 对 `wp/rp/regist` 的读写仅靠 `volatile`(无锁、无屏障),SMP 上属弱同步设计(原型定位,单 Target 场景可接受)。
4. 日志全部 `printk(KERN_ERR ...)`(收包 drop 每包一条,含 MAC/port/ctrl/siz/err 码 1-7),排障方便但吵;err=7(环满)已做静音处理,改为 drop 计数。
5. `read` 的 `siz<skb->len` 分支只告警不截断仍按 `siz` 拷贝(c.194-199),调用方需保证缓冲 ≥ 载荷。

### 6.5 MTU 联动(dds.txt,Target 侧权威说明)

- `Sink::Setting::MtuSizeMin`(正常 0;无法收短包时用)/ `MtuSize`(正常 1500;特殊板载网卡)/ `MtuSizeMax`(与 MtuSize 取大者为测试上限,非嵌入式仅用此值,可到 16360)/ `MaxPacketData`(正常 0xFFFF=不限;单次传输总量上限)。
- Host 侧日志 `DATASIZE_REQ smtu=XX amtu=YY min=xx req=yy max=zz`:smtu=OS 接口 MTU(测试上限),amtu=以 req/max/smtu 为上限的通信实测结果,由此决定 DirettaCycle。
- **通信包尺寸 ≠ 此 MTU 值**;Target 收、Host 发,Target 的接收可超 OS MTU 设置。
- 要吃满 MTU:启用 DDS 且传输 Profile 设 MTU MAX——Host 侧 `Sync::configTransferVarMax` 或(syncalsa 系配置)`FlexCycle=max`。
- DDS 帧开销:以太帧头 14B + DDS 头 2B = 16B;ALSA 日志 `YYYxZppc` 的 YYY 是每周期音频字节数,传输 MTU = YYY+16,Z>1 表示单周期多包。

---

## 7. 库变体与构建矩阵(`DirettaApp/`,22 个 .a)

命名:`libDirettaApp_<arch>-linux-<gcc主版本><微架构|k后缀>[-nolog].a`

| ARCH_NAME 取值 | 工具链 | 备注 |
|---|---|---|
| `x64-linux-14{,v2,v3,v4}` | GCC14 | v2/v3/v4 = x86-64-v2/v3/v4 |
| `x64-linux-15{v2,v3,v4,zen4}` | GCC15 | 同上 + zen4 专编;**有 -nolog 变体** |
| `aarch64-linux-14{,k16}` / `aarch64-linux-15{,k16}` | GCC14/15 | k16 变体;15 有 -nolog |
| `x86_64-linux-gcc12` / `aarch64-linux-gcc12` / `aarch64-16k-linux-gcc12` | GCC12(旧 ABI,老系统用) | 无 -nolog |

- `-nolog`(`LOG=DISABLE` 构建,定义 `SYSLOG_DISABLE`):完全关日志,且**仅部分架构有**(x64-15 系、aarch64-15 系)。
- 与 Host SDK 的差异:Target 库无 musl 变体、无 riscv64;多了 gcc12 老工具链变体。
- 通用服务器推荐 `x64-linux-15v3`(AVX2 基线,与 Host SDK 选型逻辑一致)。

---

## 8. 对 SPlayer-Next-Headless 的意义与边界

1. **不可集成**:Target 库闭源 + 激活限制(6 分钟播放/6 分钟停止循环)+ 需商业授权(与 memo_host.txt 的 Host SDK 许可条款同源且更严,许可校验编进二进制)。Headless 作为 Host 端,集成对象是 DirettaHostSDK(见姊妹篇),本仓库仅作对端。
2. **联调靶机**:在一台双网卡机器上跑 `diretta_app.sh`(Target)+ headless(Host)即可组成完整链路做协议/延迟/格式协商测试,无需真机 DAC;inf 的 `PCM*/DSD*/DoPauto` 开关正好用来验证 Host 的 `checkSinkSupport` 位图处理与 `inquirySupportFormat`。
3. **参数语义参照**(headless 输出参数设计的权威出处):
   - `AlsaLatency`(msec,默认 10)↔ Host `setSink` 的 sink buffer time 请求;
   - `AlsaInterval`(格式切换最小间隔)↔ headless 换采样率时的设备保护间隔策略;
   - `VolumeCtl`(0=最大/-60=最小/-20dB 起)↔ Diretta 远程音量协议(`Status::setVolume/NotifyVolume`,Host 侧 `findStatus` 可见);
   - `EtherMTU/ExtEtherMTU` ↔ `dds.txt` 的 Sink MTU 四参数 ↔ Host `mtuTest/mtuCheck/DATASIZE_REQ` 日志。
4. **DDS 实验**:若要验证 `NORAWSOCKET`/mode3 路径,可直接 `cd DDS && make KERNELDIR=/lib/modules/$(uname -r)/build && sudo insmod diretta_direct.ko`,用 §6.5 检查点核对;注意 §6.4 缺陷 1(MAC 过滤不可用)与"仅接收侧"定位。
5. **图标**:若 headless 需要显示 Target 图标,Host 侧 `Find::downloadIcon` 拉取的就是本包 `logo*.png`(ICON 枚举 8 种尺寸/单色变体)。

---

## 9. 附:文件与符号速查

- 开源源码仅:`DDS/diretta_direct.c|.h`、三个工具 main、`diretta_app.sh`、各 Makefile、`GentooPr.key`(片段示例)。
- 关键符号表:`DIRETTA::Target/Sink/SinkBuffer/Status/DDS`、`CertifiedTarget/CertifiedSinkAlsa`、`FifoStreamReceive/StreamReceive`、`diretta_app_*`、`NodeRun`;完整导出清单可用 `nm -C --defined-only DirettaApp/libDirettaApp_x64-linux-15v3.a | grep ' T '` 复现。
- 相关文档:[diretta-host-sdk-148-149-150-dev-guide.md](diretta-host-sdk-148-149-150-dev-guide.md)、[diretta-host-sdk-148-contract-analysis.md](diretta-host-sdk-148-contract-analysis.md)、[Linux Headless Hi-Fi.md](Linux%20Headless%20Hi-Fi.md)。
