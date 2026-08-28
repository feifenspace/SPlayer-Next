// diretta_shim.h — Diretta C Shim 公共 API
//
// 来源：HIFI_REFACTORING.md §11.1（内存所有权）/ §11.4（错误码）/ §7.7.1（148↔149 差异）
//       §11.2（线程模型，决策 D18）/ §11.3（事件回调）
//
// 本头文件定义 Diretta C Shim 层向 Rust 暴露的 C ABI。
// 实现见 diretta_shim.cpp；148/149 SDK 差异在 .cpp 内部用 #ifdef 处理，
// 对 Rust 侧完全透明（Rust 仅消费本头文件的 C 接口）。
//
// 内存所有权（§11.1）：
// - diretta_finder_t* / diretta_sync_t* 是不透明指针，Rust 侧不可解引用
// - open() 返回的指针必须用对应的 close() 释放；成对调用是调用方责任
// - C 侧用 new/delete 管理 DIRETTA::Find/Sync 对象（C++ 等价于 Box::into_raw/from_raw）
// - 跨线程释放禁止（D18）：哪个线程 open 就在哪个线程 close
//
// 错误码（§11.4）：8 个枚举值，0 表示成功，负数表示错误。
// 详细错误信息通过 thread_local 的 diretta_get_last_error() 查询。
//
// 关联任务：P3.1.2（基础 open/close/version）；P3.1.3 实现 push/控制操作 + 线程模型 + 回调。

#pragma once

#include <cstdint>
#include <stddef.h>

#include "diretta_event.h"

#ifdef __cplusplus
extern "C" {
#endif

// === 错误码（§11.4 表 7 项 + OK）===
#define DIRETTA_OK           0   // 成功
#define DIRETTA_ERR_GENERIC  (-1) // 通用错误（含 C++ 异常边界）
#define DIRETTA_ERR_NETWORK  (-2) // 网络错误（socket/路由）
#define DIRETTA_ERR_REFUSED  (-3) // 连接被拒绝
#define DIRETTA_ERR_MTU      (-4) // MTU 不匹配
#define DIRETTA_ERR_FORMAT   (-5) // 音频格式不支持
#define DIRETTA_ERR_UNDERRUN (-6) // 缓冲区下溢
#define DIRETTA_ERR_TIMEOUT  (-7) // 操作超时

// === 不透明类型 ===
// C 侧不可解引用；实际定义在 .cpp 内部。
// 用 struct tag + typedef 兼容 C99。
struct diretta_finder_opaque;
struct diretta_sync_opaque;
typedef struct diretta_finder_opaque diretta_finder_t;
typedef struct diretta_sync_opaque  diretta_sync_t;

// === Finder Setting（C 友好结构体，§7.7.1 差异已吸收）===
// 148 SDK 无 Broadcast/Multicast 字段（C Shim 在 .cpp 内 #ifdef 时忽略）；
// 149 SDK 有该字段。Rust 侧统一传入，C Shim 按版本使用。
//
// name 字段：C Shim 内部会 std::string(name) 复制，调用方在 open() 返回后
// 即可释放 name 缓冲区（§11.1 字符串所有权：调用方传入 → C Shim 复制）。
struct diretta_setting {
    std::uint64_t product_id;     // 自身 VendorID（Find::Setting::ProductID）
    std::uint16_t limit_version;  // 保护操作期间的 ID（Find::Setting::LimitVersion）
    const char*   name;           // 自身名称（Find::Setting::Name），可为 NULL
    int           nop_break;      // 绕过连接拒绝（Find::Setting::NopBreak），0/1
    int           loopback;       // 包含 loopback 接口（Find::Setting::Loopback），0/1
    std::uint64_t my_id;          // 自身 VendorID（Find::Setting::MyID）
    int           broadcast;      // 149 only：使用广播探测（默认 0）
    int           multicast;      // 149 only：使用多播探测（默认 1）
};

// === 公共 API ===

// 返回 SDK 版本号（148 或 149），由 build.rs 定义的 DIRETTA_SDK_148/149 宏决定。
std::uint16_t diretta_get_version(void);

// 返回 SDK 版本字符串（静态存储，调用方不需释放），如 "148" / "149"。
const char* diretta_get_version_string(void);

// 打开 Finder 实例。
//   setting : Finder 配置（C Shim 内部复制 name 字段）
// 返回：成功返回非 NULL 指针；失败返回 NULL，调用 diretta_get_last_error() 查错误码。
//
// 失败常见原因：
//   - DIRETTA_ERR_NETWORK : socket 创建失败
//   - DIRETTA_ERR_GENERIC : C++ 异常或 SDK 内部错误
diretta_finder_t* diretta_finder_open(const struct diretta_setting* setting);

// 关闭 Finder 实例并释放资源。NULL 安全（f == NULL 时直接返回）。
// 必须在与 open 相同的线程调用（D18）。
void diretta_finder_close(diretta_finder_t* f);

// === Finder 设备扫描 API（方案 A 新增，参考 tinyLMS-old DirettaDriver::Scan）===
//
// 设备信息结构（C 友好，所有字符串字段 NUL 终止，调用方无需释放）。
// 字符串缓冲区固定大小，超长截断（保证 NUL 终止）。
#define DIRETTA_DEVICE_NAME_MAX  128   // targetName / outputName 缓冲区大小
// IPv6 字符串最长 47 字节（"ffff:ffff:ffff:ffff:ffff:ffff:255.255.255.255"）
// + NUL = 48 字节；取 64 留余量。Diretta 设备地址本质是 IPv6（V4-mapped-V6），
// 必须用 get_V6_str/set_V6_str 往返，否则 254.x.x.x 等保留段 V4 解析会失败。
#define DIRETTA_DEVICE_IPV4_MAX  64    // 兼容 IPv6 字符串（保留旧名避免 Rust 侧大面积改名）
#define DIRETTA_DEVICE_CONFIG_MAX 256  // config URL 缓冲区大小

struct diretta_device_info {
    char          ip_str[DIRETTA_DEVICE_IPV4_MAX];   // sink IPv6 地址字符串（如 "fe80::1" 或 "::ffff:192.168.1.100"）
    std::uint16_t port;                              // sink 端口（host byte order）
    std::uint32_t ifno;                              // 接口号（Sync::open 与 setSink 需要，从 addr.get_ifno() 提取）
    std::uint16_t version;                           // 目标 SDK 版本
    std::uint64_t product_id;                        // 目标 ProductID
    char          target_name[DIRETTA_DEVICE_NAME_MAX];   // 目标名称
    char          output_name[DIRETTA_DEVICE_NAME_MAX];   // 输出端口名称
    char          config[DIRETTA_DEVICE_CONFIG_MAX];      // 配置 URL
    std::uint16_t po;                                // Output Port Number
    std::uint16_t pi;                                // Input Port Number
    int           multiport;                         // 是否多端口（0/1）
};

// 扫描 Diretta 设备（同步阻塞，参考 DirettaDriver::Scan 的 10 次重试逻辑）。
//   f           : finder 实例（不可 NULL，必须已 open）
//   out_devices : 输出缓冲区（调用方分配，可 NULL 仅查询数量）
//   max_count   : out_devices 容量（元素个数）
//   actual_count: 实际填入的设备数（指针，不可 NULL）
// 返回：DIRETTA_OK 成功；DIRETTA_ERR_NETWORK socket 错误；DIRETTA_ERR_GENERIC 实例无效或异常。
//
// 内部逻辑：
//   1. 调用 find->findOutput(resalts) 发送多播并收集 AUDIO_OUTPUT_SINK
//   2. findOutput 失败时尝试 findTarget(targetResalts) 兜底（仅枚举 target，不查询 sink）
//   3. 遍历 PortResalts（std::map<ACQUA::IPAddress, TargetConnectInfo>），
//      用 IPAddress::get_V4_str() + get_port_host() 提取 ip/port，
//      并填充 targetName/outputName/config/product_id/version/po/pi/multiport
//   4. 字符串字段在缓冲区内截断并保证 NUL 终止
//
// 注意：扫描期间会阻塞约 1-3 秒（SDK 多播 + 等待响应）。
//       调用方应在非 UI 线程执行（Rust 侧 audio::backend::diretta 已在控制线程）。
int diretta_finder_scan(diretta_finder_t* f,
                        struct diretta_device_info* out_devices,
                        int max_count,
                        int* actual_count);

// 测量到指定 sink 的真实 MTU（参考 tinyLMS-old DirettaDriver.cpp L508-527）。
//   f        : finder 实例（不可 NULL，必须已 open）
//   ip_str   : sink IPv6 地址字符串（如 "fe80::1" 或 "::ffff:192.168.1.100"）
//   port     : sink 端口（host byte order）
//   ifno     : 网络接口号
//   out_mtu  : 输出实测 MTU（指针，不可 NULL）
// 返回：DIRETTA_OK 成功；DIRETTA_ERR_GENERIC 测量失败或参数无效。
//
// 内部调用 find->measSendMTU(addr, mtu)，发送探测包计算路径 MTU。
// 失败时 out_mtu 不被修改，调用方应回退到默认 1500。
//
// 重要：connect 前必须调用此函数，设备需要先收到 MTU 探测包才会接受连接
// （参考 tcpdump 抓包结果：未调用 measSendMTU 时设备返回 error=3 拒绝连接）。
int diretta_finder_measure_mtu(diretta_finder_t* f,
                               const char* ip_str,
                               std::uint16_t port,
                               std::uint32_t ifno,
                               std::uint32_t* out_mtu);

// === Sync 生命周期 + 推流 API（P3.1.3 实现）===
//
// 线程模型（§11.2，决策 D18）：
// - diretta_sync_open / close：必须在同一线程调用（生命周期管理）
// - diretta_sync_push：单线程约束（C Shim 内部加锁保护环形缓冲区，但 Rust 侧应保证单生产者）
// - diretta_sync_set_sink / connect / disconnect / play / stop：内部 mutex 保护，可在控制线程调用
// - diretta_sync_is_online / is_playing：原子读，任意线程可调用
// - SDK getNewStream 回调（§11.3）：在 SDK 内部线程触发，使用 try_lock 避免阻塞（D18 规则 3）

// 打开 Sync 实例（推流通道）。
//   cb        : 事件回调（可 NULL，表示不接收事件）
//   user_data : 透传给回调的不透明指针（可 NULL）
// 返回：成功返回非 NULL 指针；失败返回 NULL。
//
// 注意（P3.1.3）：本函数仅构造 DirettaSyncImpl 实例，**不调用 Sync::open()**。
// Sync::open() 在 set_sink 首次调用时惰性执行（避免用户不调用 set_sink 时创建线程）。
diretta_sync_t* diretta_sync_open(diretta_event_cb cb, void* user_data);

// 关闭 Sync 实例并释放资源。NULL 安全。
// 必须在与 open 相同的线程调用（D18）。
// 内部调用 Sync::close() 后 delete 派生类实例（虚析构函数安全销毁）。
void diretta_sync_close(diretta_sync_t* s);

// 向推流缓冲区写入数据（§11.2 单线程 push）。
//   s    : sync 实例（不可 NULL）
//   data : 音频数据指针（按 negotiate_format 协商的格式解释字节）
//   size : 数据字节数
// 返回：DIRETTA_OK 成功；DIRETTA_ERR_UNDERRUN 缓冲区满；DIRETTA_ERR_GENERIC 实例无效。
//
// 内部用 mutex 保护环形缓冲区（SPSC 模式：单生产者单消费者）。
// 缓冲区满时返回 UNDERRUN 让上层重试，不阻塞调用方。
int diretta_sync_push(diretta_sync_t* s, const void* data, size_t size);

// 设置 sink 地址与缓冲参数（§11.2 控制操作，mutex 保护）。
//   s                : sync 实例
//   ip_str           : sink IPv6 地址字符串（如 "fe80::1" 或 "::ffff:192.168.1.100"），
//                      C Shim 内部 std::string 复制；用 set_V6_str 解析（支持 V4-mapped-V6）
//   port             : sink 端口（host byte order）
//   ifno             : 网络接口号（从扫描结果 addr.get_ifno() 提取，Sync::open 需要）
//   mtu              : 期望 MTU（0 表示使用默认 1500；tinyLMS-old 用 measSendMTU 实测）
//   buffer_ms        : sink 缓冲毫秒数（0 表示使用 sink 默认值；tinyLMS-old 用 30）
//   sample_rate      : PCM 采样率（Hz，如 44100 / 48000 / 96000 / 192000），
//                      用于 setSinkConfigure 与 cycle_time_us 动态计算
//   channels         : 声道数（1 = 单声道，2 = 立体声）
//   bits_per_sample  : PCM 位深（16 / 24 / 32）；32 也用于 float32 的字节数计算
// 返回：DIRETTA_OK / DIRETTA_ERR_NETWORK（地址解析失败）/ DIRETTA_ERR_GENERIC。
//
// 惰性初始化：首次调用时执行 Sync::open(ifno, ...)（创建工作线程）。
// 格式参数用于 setSinkConfigure 与 cycle_time_us 动态计算；与源 PCM 字节流
// 严格匹配，否则设备按错误速率播放（导致慢速/快速音调偏移）。
int diretta_sync_set_sink(diretta_sync_t* s,
                          const char* ip_str,
                          std::uint16_t port,
                          std::uint32_t ifno,
                          std::uint32_t mtu,
                          std::uint32_t buffer_ms,
                          std::uint32_t sample_rate,
                          std::uint32_t channels,
                          std::uint32_t bits_per_sample);

// 设置 sink 为 DSD Native 直通模式（DSD 完整形态阶段 3 新增）。
//   s                   : sync 实例
//   ip_str              : sink IPv6 地址字符串（同 set_sink）
//   port                : sink 端口
//   ifno                : 网络接口号
//   mtu                 : 期望 MTU（0 表示默认 1500）
//   buffer_ms           : sink 缓冲毫秒数（0 表示默认）
//   dsd_rate_multiplier : DSD 速率倍数（64/128/256/512）
//   dsd_byte_order      : 字节序（0 = LSB/DSF，1 = MSB/DFF）
//   channels            : 声道数（通常 2）
// 返回：DIRETTA_OK / DIRETTA_ERR_NETWORK / DIRETTA_ERR_FORMAT / DIRETTA_ERR_GENERIC。
//
// 与 set_sink 区别：
//   - 使用 DSD FormatID（FMT_DSD1 | FMT_DSD_SIZ_32 | FMT_DSD_LSB/MSB）而非 PCM
//   - cycle_time_us 基于 DSD 字节流速率（dsd_sample_rate / 8；声道数由 FMT_DSD1 内部处理）
//   - 不调用 swresample 转换，DSD 字节流直接透传到设备
int diretta_sync_set_sink_dsd(diretta_sync_t* s,
                              const char* ip_str,
                              std::uint16_t port,
                              std::uint32_t ifno,
                              std::uint32_t mtu,
                              std::uint32_t buffer_ms,
                              std::uint32_t dsd_rate_multiplier,
                              std::uint32_t dsd_byte_order,
                              std::uint32_t channels);

// 查询 DSD 协商命中的位反转 / 字节交换标志（P3）。
//   s           : sync 实例
//   bit_reverse : 输出，设备要求对源位序取反（目标 LSB != 源 LSB）
//   byte_swap   : 输出，设备要求小端传输（目标 LITTLE）
// 返回：DIRETTA_OK / DIRETTA_ERR_GENERIC。
int diretta_sync_get_dsd_transform(diretta_sync_t* s,
                                   int* bit_reverse, int* byte_swap);

// 连接到 sink（§11.2 控制操作，mutex 保护）。
//   s           : sync 实例
//   timeout_ms  : 连接超时（毫秒），0 表示 SDK 默认
// 返回：DIRETTA_OK / DIRETTA_ERR_REFUSED / DIRETTA_ERR_TIMEOUT / DIRETTA_ERR_GENERIC。
//
// 内部依次调用 connectPrepare / connect / connectWait，成功后触发 CONNECTED 事件。
int diretta_sync_connect(diretta_sync_t* s, int timeout_ms);

// 断开 sink 连接（§11.2 控制操作，mutex 保护）。
// 返回：DIRETTA_OK / DIRETTA_ERR_GENERIC。
// 成功后触发 DISCONNECTED 事件。
int diretta_sync_disconnect(diretta_sync_t* s);

// 开始播放（§11.2 控制操作，mutex 保护）。
// 返回：DIRETTA_OK / DIRETTA_ERR_GENERIC。
int diretta_sync_play(diretta_sync_t* s);

// 停止播放（暂停，连接保留）（§11.2 控制操作，mutex 保护）。
// 返回：DIRETTA_OK / DIRETTA_ERR_GENERIC。
int diretta_sync_stop(diretta_sync_t* s);

// 查询连接状态（原子读，任意线程可调用）。
// 返回：1 = 已连接（online）；0 = 未连接或实例无效。
int diretta_sync_is_online(diretta_sync_t* s);

// 查询播放状态（原子读，任意线程可调用）。
// 返回：1 = 正在播放；0 = 未播放或实例无效。
int diretta_sync_is_playing(diretta_sync_t* s);

// === Pre-mute 机制（P1 修复）===
//
// 在 stop()/reconfigure() 前调用，让 SDK 线程在接下来的 count 次 getNewStream
// 调用中输出静音帧（按当前 is_dsd_ 标志选择 0x00 / 0x69）。
// 配合 diretta_sync_wait_pre_mute_done 阻塞等待计数清零，
// 保证 SDK 已经消费完"播放中"的最后一帧后才执行 stop()/disconnect()。
//
// 调用顺序（对齐 tinyLMS-old DirettaDriver.cpp SetFormat Hard Reset L837-841）：
//   diretta_sync_trigger_pre_mute(s, 20);  // DSD
//   diretta_sync_trigger_pre_mute(s, 8);   // PCM
//   diretta_sync_wait_pre_mute_done(s, 40);
//   // 之后才 stop()/disconnect()
//
// 参数：
//   s     : sync 实例（不可 NULL）
//   count : 预静音帧数（DSD=20, PCM=8，<= 0 等于取消预静音）
// 返回：DIRETTA_OK 成功；DIRETTA_ERR_GENERIC 实例无效。
int diretta_sync_trigger_pre_mute(diretta_sync_t* s, int count);

// 阻塞等待预静音帧清零（SDK 线程消费完所有预静音帧）。
//   s           : sync 实例
//   timeout_ms  : 超时（毫秒），0 表示立即返回（仅查询）
// 返回：1 = 已清零；0 = 超时或实例无效。
int diretta_sync_wait_pre_mute_done(diretta_sync_t* s, int timeout_ms);

// === Soft Resume 辅助：清空环形缓冲区（P1 修复）===
//
// Soft Resume 路径（同类型同采样率切换位深时）调用，
// 清空 ring_buf_ 残留的旧格式数据，防止新格式解释旧字节造成爆音。
// 与 set_sink 内部的清空逻辑一致，但此处不重新 setSinkConfigure。
//
// 返回：DIRETTA_OK 成功；DIRETTA_ERR_GENERIC 实例无效。
int diretta_sync_clear_ring_buffer(diretta_sync_t* s);

// === Sink 设备能力查询（DSD 完整形态阶段 1 新增）===
//
// 参考 tinyLMS-old DirettaDriver.cpp L1251-1258 的 checkSinkSupportDSD* 实现：
//   auto info = temp_buffer->getSinkInfo();
//   device_caps_.supports_dsd_lsb = info.checkSinkSupportDSDlsb();
//   device_caps_.supports_dsd_msb = info.checkSinkSupportDSDmsb();
//
// DIRETTA::Sync::Info 字段（Sync.hpp L199-243）：
//   - supportPCM     : FormatID（!= 0 表示支持 PCM）
//   - supportDSDlsb   : FormatID（!= 0 表示支持 DSD LSB，DSF 文件格式）
//   - supportDSDmsb   : FormatID（!= 0 表示支持 DSD MSB，DFF 文件格式）
//   - latencyBuffer / latencyMax / latencyHw : 延迟信息
//   - maxSize / minMTU / reqMTU / maxMTU     : MTU 范围
//   - supportMSmode  : MS mode 支持位图
//
// 通过本函数把 Info 字段透传到 Rust 侧，由 diretta.rs::connect() 解析后
// 填充 DeviceCapabilities（替代之前 line 540-547 的硬编码占位值）。
//
// FormatID 是 enum class FormatID : std::uint64_t，C ABI 用 uint64_t 透传原始值。
// Rust 侧可通过位与运算判断具体格式位（FMT_DSD1 / FMT_DSD_LSB / FMT_DSD_MSB 等）。
struct diretta_sink_info {
    std::uint64_t support_pcm;       // FormatID raw value（PCM 能力位图）
    std::uint64_t support_dsd_lsb;    // FormatID raw value（DSD LSB 能力位图，DSF）
    std::uint64_t support_dsd_msb;    // FormatID raw value（DSD MSB 能力位图，DFF）
    std::uint16_t latency_buffer;
    std::uint16_t latency_max;
    std::uint16_t latency_hw;
    std::uint16_t max_size;
    std::uint16_t min_mtu;
    std::uint16_t req_mtu;
    std::uint32_t max_mtu;
    std::uint16_t support_ms_mode;
};

// 查询 sink 设备能力（参考 DIRETTA::Sync::getSinkInfo）。
//   s        : sync 实例（不可 NULL，必须已 connect 成功）
//   out_info : 输出 sink_info（指针，不可 NULL）
// 返回：DIRETTA_OK 成功；DIRETTA_ERR_GENERIC 实例无效或未连接。
//
// 注意：getSinkInfo() 是 const 本地操作（仅读取 SDK 内部 SinkInfo 缓存），
// 不发起网络请求，可在任意线程调用。但建议在 connect 成功后立即查询，
// 避免 SinkInfo 未被协议握手填充。
int diretta_sync_get_sink_info(diretta_sync_t* s, struct diretta_sink_info* out_info);

// 查询协商后的实际物理位深（对齐 tinyLMS-old DirettaDriver.cpp L842-849）。
//
// tinyLMS-old 在 setSinkConfigure 后读取：
//   auto actual_wid = new_sync->getSinkConfigure().getWid();
//   uint8_t actual_bit_depth = actual_wid * 8;
//   new_sync->SetPhysicalBitDepth(actual_bit_depth);
//
// PCM 格式协商总是优先选 32-bit 物理槽位（即使源是 24-bit），
// 因此实际推送字节数可能与源格式不符。
//
//   s       : sync 实例（不可 NULL，必须已 set_sink + connect 成功）
//   out_wid : 输出物理字节宽度（1=16bit, 2=24bit, 4=32bit），0 表示未连接
// 返回：DIRETTA_OK / DIRETTA_ERR_GENERIC。
uint32_t diretta_sync_get_negotiated_format(diretta_sync_t* s);

// 返回当前线程最近一次错误码（thread_local）。
// 用于 open() 返回 NULL 后查询具体原因。
// 每次成功调用 open 会重置为 DIRETTA_OK。
int diretta_get_last_error(void);

#ifdef __cplusplus
} // extern "C"
#endif
