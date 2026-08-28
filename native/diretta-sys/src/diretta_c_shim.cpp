// diretta_c_shim.cpp — Diretta Host SDK C ABI 桥接层实现
//
// 封装 DIRETTA::Find 与 DIRETTA::SyncBuffer 官方 C++ SDK 接口，
// 为 Rust 侧提供异常安全 (noexcept)、线程安全、标准 ABI 兼容的 C 函数接口。

#include "diretta_shim.h"

#include <new>
#include <string>
#include <exception>
#include <mutex>
#include <vector>
#include <cstring>
#include <atomic>
#include <algorithm>
#include <thread>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <fstream>

#define DS_DBG(fmt, ...) do { \
    std::fprintf(stderr, "[DS_DBG %s:%d] " fmt "\n", __func__, __LINE__, ##__VA_ARGS__); \
    std::fflush(stderr); \
} while (0)

#if defined(DIRETTA_SDK_148)
#  include "Find.hpp"
#  include "Release.hpp"
#  include "Sync.hpp"
#  include "SyncBuffer.hpp"
#  include "SysLog.hpp"
#  include <ACQUA/IPAddress>
#  include <ACQUA/Clock>
#  include <ACQUA/SysLog>
#elif defined(DIRETTA_SDK_149) || defined(DIRETTA_SDK_150)
#  include "Find.hpp"
#  include "Release.hpp"
#  include "Sync.hpp"
#  include "SyncBuffer.hpp"
#  include "SysLog.hpp"
#  include <ACQUA/IPAddress>
#  include <ACQUA/Clock>
#  include <ACQUA/SysLog>
#else
#  error "Neither DIRETTA_SDK_148 nor DIRETTA_SDK_149 nor DIRETTA_SDK_150 defined."
#endif

// === SysLog 初始化 ===
static std::once_flag g_syslog_init_flag;
static void ensure_syslog_initialized() {
    std::call_once(g_syslog_init_flag, []() {
        try {
            ACQUA::SysLog::initialize(ACQUA::SysLog::local0, true);
            DIRETTA::SysLogDiretta::changeLevel(ACQUA::SysLog::Info, DIRETTA::SyslogPortHost);
        } catch (...) {}
    });
}

// === thread_local 错误码 ===
static thread_local int g_last_error = DIRETTA_OK;

static inline void set_last_error(int code) {
    g_last_error = code;
}

#include "sync_buffer_impl.inl"

struct diretta_finder_opaque {
    DIRETTA::Find* find;
};

// sync 字段类型为 DirettaSyncImpl*（非 void* / Sync*），
// delete 操作通过派生类指针触发虚析构链，安全销毁，无 -Wdelete-non-virtual-dtor 警告。
struct diretta_sync_opaque {
    DirettaSyncImpl* sync;
};

// === 版本 API ===

std::uint16_t diretta_get_version(void) {
#if defined(DIRETTA_SDK_150)
    return 150;
#elif defined(DIRETTA_SDK_149)
    return 149;
#elif defined(DIRETTA_SDK_148)
    return 148;
#else
#  error "SDK version macro not defined"
#endif
}

const char* diretta_get_version_string(void) {
#if defined(DIRETTA_SDK_150)
    return "150";
#elif defined(DIRETTA_SDK_149)
    return "149";
#elif defined(DIRETTA_SDK_148)
    return "148";
#else
#  error "SDK version macro not defined"
#endif
}

// === Finder open/close ===

extern "C" diretta_finder_t* diretta_finder_open(const struct diretta_setting* setting) {
    if (setting == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return nullptr;
    }

    try {
        // 构造 DIRETTA::Find::Setting，按 148/149/150 版本填字段
        DIRETTA::Find::Setting s;

        s.ProductID    = setting->product_id;
        s.LimitVersion = setting->limit_version;
        s.Name         = (setting->name != nullptr) ? std::string(setting->name) : std::string();
        s.NopBreak     = (setting->nop_break != 0);
        s.Loopback     = (setting->loopback != 0);
        // ProductIDgroup 留空（默认空 vector），P3.1.2 不暴露此高级字段
        s.MyID         = setting->my_id;

#if defined(DIRETTA_SDK_149) || defined(DIRETTA_SDK_150)
        // 149/150 only：Broadcast / Multicast 字段（§7.7.1）
        s.Broadcast = (setting->broadcast != 0);
        s.Multicast = (setting->multicast != 0);
#elif defined(DIRETTA_SDK_148)
        // 148 SDK 无 Broadcast/Multicast 字段；setting 中的对应值被静默忽略。
        // 此处不消费 setting->broadcast / setting->multicast。
#endif

        // new Find 实例（C++ 等价 Box::into_raw）
        DIRETTA::Find* find = new DIRETTA::Find(s);

        // open socket（无参版本：所有接口）
        bool ok = find->open();
        if (!ok) {
            // open 失败：销毁实例，返回 NULL
            // 注意：close() 在 open 失败时是否安全由 SDK 保证；为防万一 try/catch 包裹
            try { find->close(); } catch (...) { /* swallow */ }
            delete find;
            set_last_error(DIRETTA_ERR_NETWORK);
            return nullptr;
        }

        // 包装到 opaque 结构
        diretta_finder_opaque* opaque = new diretta_finder_opaque;
        opaque->find = find;
        set_last_error(DIRETTA_OK);
        return reinterpret_cast<diretta_finder_t*>(opaque);
    } catch (const std::bad_alloc&) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return nullptr;
    } catch (const std::exception&) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return nullptr;
    } catch (...) {
        // 兜底：未知异常 → 通用错误
        set_last_error(DIRETTA_ERR_GENERIC);
        return nullptr;
    }
}

extern "C" void diretta_finder_close(diretta_finder_t* f) {
    if (f == nullptr) {
        // NULL 安全（§11.1）
        return;
    }

    diretta_finder_opaque* opaque = reinterpret_cast<diretta_finder_opaque*>(f);
    try {
        if (opaque->find != nullptr) {
            opaque->find->close();
            delete opaque->find;
            opaque->find = nullptr;
        }
    } catch (...) {
        // close 异常被吞掉，避免跨 C ABI 边界抛出
        // （资源泄漏风险可接受：进程退出时会回收；D18 单线程约束下不会反复触发）
    }
    delete opaque;
}

// === Finder scan（方案 A 新增）===
//
// 参考 tinyLMS-old/src/core/transport/DirettaDriver.cpp::Scan（行 374-444）：
// 10 次重试循环，每次 findOutput 失败时短暂 sleep 后重试。
// findOutput 内部已先 findTarget 再查询 sink，故无需单独兜底。
//
// 字符串字段拷贝：用 strncpy_s 风格的 safe_copy，保证 NUL 终止且截断。

namespace {
// 安全拷贝字符串到固定大小缓冲区，保证 NUL 终止且不溢出。
// src 为 std::string 时自动取 c_str() + size()。
inline void safe_copy_str(char* dst, size_t dst_size, const std::string& src) {
    if (dst_size == 0) return;
    size_t copy_len = std::min(src.size(), dst_size - 1);
    std::memcpy(dst, src.data(), copy_len);
    dst[copy_len] = '\0';
}

// 把 ACQUA::IPAddress 的 IPv6 字符串、端口、接口号填到 C 友好结构。
// 失败时 ip_str 置空字符串、port 置 0、ifno 置 0。
//
// 重要：Diretta 设备地址本质是 IPv6（存储在 ACQUA::IPAddress 的 V6 内存布局中），
// 254.x.x.x 等保留段不是合法 IPv4，set_V4_str 会失败。必须用 get_V6_str 提取，
// 对应 set_V6_str 解析（支持 V4-mapped-V6）。
inline void fill_ip_port(char* ip_dst, size_t ip_size,
                         std::uint16_t* port_out,
                         std::uint32_t* ifno_out,
                         const ACQUA::IPAddress& addr) {
    ip_dst[0] = '\0';
    *port_out = 0;
    *ifno_out = 0;
    try {
        std::string ip_str;
        addr.get_V6_str(ip_str);  // 输出 IPv6 字符串（V4-mapped-V6 形如 "::ffff:192.168.1.100"）
        safe_copy_str(ip_dst, ip_size, ip_str);
        *port_out = addr.get_port_host();
        *ifno_out = addr.get_ifno();  // 接口号（Sync::open 需要）
    } catch (...) {
        // get_V6_str 在地址异常时可能抛异常；安全降级为空字符串
        ip_dst[0] = '\0';
        *port_out = 0;
        *ifno_out = 0;
    }
}
}  // namespace

extern "C" int diretta_finder_scan(diretta_finder_t* f,
                                   struct diretta_device_info* out_devices,
                                   int max_count,
                                   int* actual_count) {
    if (actual_count == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    *actual_count = 0;

    if (f == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }

    diretta_finder_opaque* opaque = reinterpret_cast<diretta_finder_opaque*>(f);
    if (opaque->find == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }

    try {
        // 10 次重试循环（参考 DirettaDriver::Scan），每次失败 sleep 200ms
        // 总等待时间最多约 2 秒（不含 socket IO）
        constexpr int kMaxRetries = 10;
        constexpr int kRetrySleepMs = 200;

        DIRETTA::Find::PortResalts resalts;
        bool found = false;

        for (int attempt = 0; attempt < kMaxRetries; ++attempt) {
            resalts.clear();
            // findOutput 内部：先 findTarget(ts) 多播探测，再 findTarget(ts, resalts, AUDIO_OUTPUT_SINK)
            // 查询每个 target 的 sink 端口
            if (opaque->find->findOutput(resalts)) {
                found = true;
                break;
            }
            // findOutput 返回 false 表示 socket 失败（无 target 响应或网络错误）
            // 短暂 sleep 后重试（多播可能丢包）
            std::this_thread::sleep_for(std::chrono::milliseconds(kRetrySleepMs));
        }

        if (!found) {
            // 未发现任何设备（不视为错误，返回 0 个设备）
            set_last_error(DIRETTA_OK);
            return DIRETTA_OK;
        }

        // 填充 out_devices（若调用方提供了缓冲区）
        int count = 0;
        for (const auto& kv : resalts) {
            if (out_devices != nullptr && count >= max_count) {
                // 缓冲区满：剩余设备不填充（不视为错误）
                break;
            }
            const ACQUA::IPAddress& sink_addr = kv.first;
            const DIRETTA::Find::TargetConnectInfo& info = kv.second;

            if (out_devices != nullptr) {
                struct diretta_device_info& dev = out_devices[count];
                std::memset(&dev, 0, sizeof(dev));

                // IP、端口、接口号
                fill_ip_port(dev.ip_str, DIRETTA_DEVICE_IPV4_MAX, &dev.port, &dev.ifno, sink_addr);

                dev.version    = info.version;
                dev.product_id = info.productID;
                dev.po         = info.PO;
                dev.pi         = info.PI;
                dev.multiport  = info.multiport ? 1 : 0;

                safe_copy_str(dev.target_name, DIRETTA_DEVICE_NAME_MAX, info.targetName);
                safe_copy_str(dev.output_name, DIRETTA_DEVICE_NAME_MAX, info.outputName);
                safe_copy_str(dev.config,      DIRETTA_DEVICE_CONFIG_MAX, info.config);
            }
            ++count;
        }

        *actual_count = count;
        set_last_error(DIRETTA_OK);
        return DIRETTA_OK;
    } catch (const std::bad_alloc&) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    } catch (const std::exception&) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    } catch (...) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
}

// === Finder measure MTU（P3.2 Stage 7.5 新增，参考 tinyLMS-old DirettaDriver.cpp L508-527）===
//
// 设备需要先收到 MTU 探测包才会接受后续 connect 请求。
// tcpdump 抓包证实：未调用 measSendMTU 时设备返回 error=3 拒绝连接。

extern "C" int diretta_finder_measure_mtu(diretta_finder_t* f,
                                          const char* ip_str,
                                          std::uint16_t port,
                                          std::uint32_t ifno,
                                          std::uint32_t* out_mtu) {
    if (out_mtu == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    if (f == nullptr || ip_str == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }

    diretta_finder_opaque* opaque = reinterpret_cast<diretta_finder_opaque*>(f);
    if (opaque->find == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }

    try {
        // 解析 IP 地址（用 set_V6_str 支持 V4-mapped-V6）
        ACQUA::IPAddress target_addr;
        if (!target_addr.set_V6_str(std::string(ip_str))) {
            DS_DBG("measure_mtu: set_V6_str failed ip=%s", ip_str);
            set_last_error(DIRETTA_ERR_NETWORK);
            return DIRETTA_ERR_NETWORK;
        }
        target_addr.set_port_host(port);
        target_addr.set_ifno(ifno);

        // 测量 MTU
        std::uint32_t measured_mtu = 0;
        DS_DBG("calling find->measSendMTU ip=%s port=%u ifno=%u", ip_str, port, ifno);
        bool ok = opaque->find->measSendMTU(target_addr, measured_mtu);
        if (!ok || measured_mtu == 0) {
            DS_DBG("measSendMTU failed or returned 0 (ok=%d mtu=%u)",
                   ok ? 1 : 0, measured_mtu);
            set_last_error(DIRETTA_ERR_GENERIC);
            return DIRETTA_ERR_GENERIC;
        }

        DS_DBG("measSendMTU ok, measured_mtu=%u", measured_mtu);
        *out_mtu = measured_mtu;
        set_last_error(DIRETTA_OK);
        return DIRETTA_OK;
    } catch (const std::bad_alloc&) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    } catch (const std::exception&) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    } catch (...) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
}

// === Sync open/close（P3.1.3 真实实现）===

extern "C" diretta_sync_t* diretta_sync_open(diretta_event_cb cb, void* user_data) {
    // P3.1.3：构造 DirettaSyncImpl 实例（不调用 Sync::open，惰性初始化在 set_sink 中执行）
    try {
        DirettaSyncImpl* impl = new DirettaSyncImpl(cb, user_data);
        diretta_sync_opaque* opaque = new diretta_sync_opaque;
        opaque->sync = impl;
        set_last_error(DIRETTA_OK);
        return reinterpret_cast<diretta_sync_t*>(opaque);
    } catch (const std::bad_alloc&) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return nullptr;
    } catch (...) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return nullptr;
    }
}

extern "C" void diretta_sync_close(diretta_sync_t* s) {
    if (s == nullptr) {
        return;
    }
    diretta_sync_opaque* opaque = reinterpret_cast<diretta_sync_opaque*>(s);
    // 通过派生类指针 delete，触发虚析构链（~DirettaSyncImpl → ~Sync），安全销毁。
    // 析构函数内部有序关闭（stop → disconnect → close），并 catch ... 吞掉异常。
    try {
        if (opaque->sync != nullptr) {
            delete opaque->sync;
            opaque->sync = nullptr;
        }
    } catch (...) {
        // 吞异常，避免跨 C ABI 边界
    }
    delete opaque;
}

// === Sync 推流 + 控制 API（P3.1.3）===

extern "C" int diretta_sync_push(diretta_sync_t* s, const void* data, size_t size) {
    if (s == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    diretta_sync_opaque* opaque = reinterpret_cast<diretta_sync_opaque*>(s);
    if (opaque->sync == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    try {
        if (opaque->sync->push(data, size)) {
            set_last_error(DIRETTA_OK);
            return DIRETTA_OK;
        } else {
            // 缓冲区满 → UNDERRUN
            set_last_error(DIRETTA_ERR_UNDERRUN);
            return DIRETTA_ERR_UNDERRUN;
        }
    } catch (...) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
}

extern "C" int diretta_sync_set_sink(diretta_sync_t* s, const char* ip_str,
                                      std::uint16_t port, std::uint32_t ifno,
                                      std::uint32_t mtu, std::uint32_t buffer_ms,
                                      std::uint32_t sample_rate, std::uint32_t channels,
                                      std::uint32_t bits_per_sample) {
    if (s == nullptr || ip_str == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    diretta_sync_opaque* opaque = reinterpret_cast<diretta_sync_opaque*>(s);
    if (opaque->sync == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    try {
        std::string ip(ip_str);
        if (opaque->sync->set_sink(ip, port, ifno, mtu, buffer_ms,
                                    sample_rate, channels, bits_per_sample)) {
            set_last_error(DIRETTA_OK);
            return DIRETTA_OK;
        } else {
            // set_sink 失败：地址解析失败或 SDK open/setSink 失败
            set_last_error(DIRETTA_ERR_NETWORK);
            return DIRETTA_ERR_NETWORK;
        }
    } catch (...) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
}

// DSD Native 直通模式 C ABI 包装（阶段 3 新增）
extern "C" int diretta_sync_set_sink_dsd(diretta_sync_t* s, const char* ip_str,
                                          std::uint16_t port, std::uint32_t ifno,
                                          std::uint32_t mtu, std::uint32_t buffer_ms,
                                          std::uint32_t dsd_rate_multiplier,
                                          std::uint32_t dsd_byte_order,
                                          std::uint32_t channels) {
    if (s == nullptr || ip_str == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    diretta_sync_opaque* opaque = reinterpret_cast<diretta_sync_opaque*>(s);
    if (opaque->sync == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    // DSD 速率倍数有效性校验
    if (dsd_rate_multiplier != 64 && dsd_rate_multiplier != 128 &&
        dsd_rate_multiplier != 256 && dsd_rate_multiplier != 512) {
        set_last_error(DIRETTA_ERR_FORMAT);
        return DIRETTA_ERR_FORMAT;
    }
    // 字节序校验：0=LSB/DSF, 1=MSB/DFF
    if (dsd_byte_order > 1) {
        set_last_error(DIRETTA_ERR_FORMAT);
        return DIRETTA_ERR_FORMAT;
    }
    try {
        std::string ip(ip_str);
        if (opaque->sync->set_sink_dsd(ip, port, ifno, mtu, buffer_ms,
                                         dsd_rate_multiplier, dsd_byte_order,
                                         channels)) {
            set_last_error(DIRETTA_OK);
            return DIRETTA_OK;
        } else {
            // set_sink_dsd 失败：地址解析失败、SDK open 失败、checkSinkSupport 失败
            // 或 setSinkConfigure 失败（设备不支持 DSD Native）
            set_last_error(DIRETTA_ERR_FORMAT);
            return DIRETTA_ERR_FORMAT;
        }
    } catch (...) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
}

// DSD 协商变换查询（P3）：返回位反转 / 字节交换标志，供 Rust 直通前做变换。
extern "C" int diretta_sync_get_dsd_transform(diretta_sync_t* s,
                                              int* bit_reverse, int* byte_swap) {
    if (s == nullptr || bit_reverse == nullptr || byte_swap == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    diretta_sync_opaque* opaque = reinterpret_cast<diretta_sync_opaque*>(s);
    if (opaque->sync == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    try {
        bool br = false, bs = false;
        if (!opaque->sync->dsd_transform(&br, &bs)) {
            set_last_error(DIRETTA_ERR_GENERIC);
            return DIRETTA_ERR_GENERIC;
        }
        *bit_reverse = br ? 1 : 0;
        *byte_swap = bs ? 1 : 0;
        set_last_error(DIRETTA_OK);
        return DIRETTA_OK;
    } catch (...) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
}

extern "C" int diretta_sync_connect(diretta_sync_t* s, int timeout_ms) {
    if (s == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    diretta_sync_opaque* opaque = reinterpret_cast<diretta_sync_opaque*>(s);
    if (opaque->sync == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    try {
        int ret = opaque->sync->connect_sink(timeout_ms);
        set_last_error(ret);
        return ret;
    } catch (...) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
}

extern "C" int diretta_sync_disconnect(diretta_sync_t* s) {
    if (s == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    diretta_sync_opaque* opaque = reinterpret_cast<diretta_sync_opaque*>(s);
    if (opaque->sync == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    try {
        if (opaque->sync->disconnect_sink()) {
            set_last_error(DIRETTA_OK);
            return DIRETTA_OK;
        } else {
            set_last_error(DIRETTA_ERR_GENERIC);
            return DIRETTA_ERR_GENERIC;
        }
    } catch (...) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
}

extern "C" int diretta_sync_play(diretta_sync_t* s) {
    if (s == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    diretta_sync_opaque* opaque = reinterpret_cast<diretta_sync_opaque*>(s);
    if (opaque->sync == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    try {
        if (opaque->sync->play_sink()) {
            set_last_error(DIRETTA_OK);
            return DIRETTA_OK;
        } else {
            set_last_error(DIRETTA_ERR_GENERIC);
            return DIRETTA_ERR_GENERIC;
        }
    } catch (...) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
}

extern "C" int diretta_sync_stop(diretta_sync_t* s) {
    if (s == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    diretta_sync_opaque* opaque = reinterpret_cast<diretta_sync_opaque*>(s);
    if (opaque->sync == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    try {
        if (opaque->sync->stop_sink()) {
            set_last_error(DIRETTA_OK);
            return DIRETTA_OK;
        } else {
            set_last_error(DIRETTA_ERR_GENERIC);
            return DIRETTA_ERR_GENERIC;
        }
    } catch (...) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
}

extern "C" int diretta_sync_is_online(diretta_sync_t* s) {
    if (s == nullptr) {
        return 0;
    }
    diretta_sync_opaque* opaque = reinterpret_cast<diretta_sync_opaque*>(s);
    if (opaque->sync == nullptr) {
        return 0;
    }
    return opaque->sync->isOnline() ? 1 : 0;
}

extern "C" int diretta_sync_is_playing(diretta_sync_t* s) {
    if (s == nullptr) {
        return 0;
    }
    diretta_sync_opaque* opaque = reinterpret_cast<diretta_sync_opaque*>(s);
    if (opaque->sync == nullptr) {
        return 0;
    }
    return opaque->sync->isPlaying() ? 1 : 0;
}

// === Pre-mute 机制（P1 修复）===
//
// 在 stop()/reconfigure() 前调用，让 SDK 线程在接下来的 count 次 getNewStream
// 调用中输出静音帧（按当前 is_dsd_ 选择 0x00 / 0x69）。
//
// 调用顺序（对齐 tinyLMS-old DirettaDriver.cpp SetFormat Hard Reset L837-841）：
//   trigger_pre_mute(count)  // DSD=20, PCM=8
//   wait_pre_mute_done(40)  // 最多等 40ms
//   // 之后才 stop()/disconnect()
//
// 参数：
//   s     : sync 实例（不可 NULL）
//   count : 预静音帧数（DSD=20, PCM=8，<= 0 等于取消预静音）
// 返回：DIRETTA_OK 成功；DIRETTA_ERR_GENERIC 实例无效。
extern "C" int diretta_sync_trigger_pre_mute(diretta_sync_t* s, int count) {
    if (s == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    diretta_sync_opaque* opaque = reinterpret_cast<diretta_sync_opaque*>(s);
    if (opaque->sync == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    opaque->sync->trigger_pre_mute(count);
    set_last_error(DIRETTA_OK);
    return DIRETTA_OK;
}

// 阻塞等待 pre_mute_frames_ 清零（SDK 线程消费完所有预静音帧）。
//   s           : sync 实例
//   timeout_ms  : 超时（毫秒），0 表示立即返回（仅查询）
// 返回：1 = 已清零；0 = 超时或实例无效。
extern "C" int diretta_sync_wait_pre_mute_done(diretta_sync_t* s, int timeout_ms) {
    if (s == nullptr) {
        return 0;
    }
    diretta_sync_opaque* opaque = reinterpret_cast<diretta_sync_opaque*>(s);
    if (opaque->sync == nullptr) {
        return 0;
    }
    return opaque->sync->wait_pre_mute_done(timeout_ms) ? 1 : 0;
}

// === Soft Resume 辅助：清空环形缓冲区（P1 修复）===
//
// Soft Resume 路径（同类型同采样率切换位深时）调用，
// 清空 ring_buf_ 残留的旧格式数据，防止新格式解释旧字节造成爆音。
// 与 set_sink 内部的清空逻辑一致，但此处不重新 setSinkConfigure。
//
// 返回：DIRETTA_OK 成功；DIRETTA_ERR_GENERIC 实例无效。
extern "C" int diretta_sync_clear_ring_buffer(diretta_sync_t* s) {
    if (s == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    diretta_sync_opaque* opaque = reinterpret_cast<diretta_sync_opaque*>(s);
    if (opaque->sync == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    opaque->sync->clear_ring_buffer();
    set_last_error(DIRETTA_OK);
    return DIRETTA_OK;
}

// === Sink 设备能力查询（DSD 完整形态阶段 1 新增）===
//
// 参考 tinyLMS-old DirettaDriver.cpp L1251-1258：
//   auto info = temp_buffer->getSinkInfo();
//   device_caps_.supports_dsd_lsb = info.checkSinkSupportDSDlsb();
//   device_caps_.supports_dsd_msb = info.checkSinkSupportDSDmsb();
//
// getSinkInfo() 是 SDK const 方法（仅读取内部 SinkInfo 缓存，无网络请求）。
// SinkInfo 在 connect() 协议握手成功后被 SDK 内部填充；connect 前调用会返回
// 默认全零值（checkSinkSupport* 全部返回 false）。
//
// 因此建议调用顺序：set_sink → connect(OK) → get_sink_info。
//
// FormatID 是 enum class FormatID : std::uint64_t，通过 static_cast<uint64_t>
// 透传原始位图给 Rust 侧。Rust 通过位与运算判断具体格式位
// （FMT_DSD1=0x10000, FMT_DSD_LSB=0x100000, FMT_DSD_MSB=0x200000 等，
// 详见 DirettaHostSDK_149/Host/Format.hpp）。
extern "C" int diretta_sync_get_sink_info(diretta_sync_t* s, struct diretta_sink_info* out_info) {
    if (s == nullptr || out_info == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    diretta_sync_opaque* opaque = reinterpret_cast<diretta_sync_opaque*>(s);
    if (opaque->sync == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
    try {
        // getSinkInfo() 是 DIRETTA::Sync 的 const 成员函数（非静态），
        // 必须通过 DirettaSyncImpl 实例指针调用。
        // SDK 在 connect() 协议握手成功后内部填充 SinkInfo 字段。
        const DIRETTA::Sync::Info& info = opaque->sync->get_sink_info();
        // FormatID raw value（uint64_t 位图，!= 0 表示支持该类格式）
        out_info->support_pcm       = static_cast<std::uint64_t>(info.supportPCM);
        out_info->support_dsd_lsb   = static_cast<std::uint64_t>(info.supportDSDlsb);
        out_info->support_dsd_msb   = static_cast<std::uint64_t>(info.supportDSDmsb);
        // 延迟与 MTU 信息
        out_info->latency_buffer    = info.latencyBuffer;
        out_info->latency_max       = info.latencyMax;
        out_info->latency_hw        = info.latencyHw;
        out_info->max_size          = info.maxSize;
        out_info->min_mtu           = info.minMTU;
        out_info->req_mtu           = info.reqMTU;
        out_info->max_mtu           = info.maxMTU;
        out_info->support_ms_mode   = info.supportMSmode;
        set_last_error(DIRETTA_OK);
        return DIRETTA_OK;
    } catch (...) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return DIRETTA_ERR_GENERIC;
    }
}

// === 协商格式查询（对齐 tinyLMS-old DirettaDriver.cpp L842-849）===
//
// tinyLMS-old 在 setSinkConfigure 后读取实际物理字节宽度：
//   auto actual_wid = new_sync->getSinkConfigure().getWid();
//   uint8_t actual_bit_depth = actual_wid * 8;
//
// 本函数把该值暴露给 Rust 侧，供 write_samples 决定推送的字节宽度。
extern "C" std::uint32_t diretta_sync_get_negotiated_format(diretta_sync_t* s) {
    if (s == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return 0;
    }
    diretta_sync_opaque* opaque = reinterpret_cast<diretta_sync_opaque*>(s);
    if (opaque->sync == nullptr) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return 0;
    }
    try {
        const auto cfg = opaque->sync->get_sink_configure();
        const std::uint32_t wid = static_cast<std::uint32_t>(cfg.getWid());
        set_last_error(DIRETTA_OK);
        return wid;
    } catch (...) {
        set_last_error(DIRETTA_ERR_GENERIC);
        return 0;
    }
}

// === last_error 查询 ===

extern "C" int diretta_get_last_error(void) {
    return g_last_error;
}
