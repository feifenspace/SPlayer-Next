#ifndef SPLAYER_DIRETTA_BRIDGE_H
#define SPLAYER_DIRETTA_BRIDGE_H

#include <stddef.h>
#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

#define SPLAYER_DIRETTA_TEXT_CAPACITY 256

typedef struct {
    char id[SPLAYER_DIRETTA_TEXT_CAPACITY];
    char name[SPLAYER_DIRETTA_TEXT_CAPACITY];
    char ipv6_addr[SPLAYER_DIRETTA_TEXT_CAPACITY];
    char full_addr[SPLAYER_DIRETTA_TEXT_CAPACITY];
    int32_t if_idx;
    char target_name[SPLAYER_DIRETTA_TEXT_CAPACITY];
    char output_name[SPLAYER_DIRETTA_TEXT_CAPACITY];
    char model_name[SPLAYER_DIRETTA_TEXT_CAPACITY];
    uint32_t mtu;
} SPlayerDirettaDevice;

typedef bool (*SPlayerDirettaNextBlock)(void* context, const uint8_t** data, size_t* len);
typedef void (*SPlayerDirettaReleaseBlock)(void* context);

const char* splayer_diretta_last_error(void);
size_t splayer_diretta_scan(SPlayerDirettaDevice* devices, size_t capacity);

void* splayer_diretta_open_direct(
    const char* target_id,
    uint32_t sample_rate,
    uint16_t channels,
    uint8_t storage_bits,
    void* source_context,
    SPlayerDirettaNextBlock next_block,
    SPlayerDirettaReleaseBlock release_block
);

void* splayer_diretta_open_dsd_direct(
    const char* target_id,
    uint32_t bit_rate,
    uint16_t channels,
    bool source_lsb_first,
    bool* wire_lsb_first,
    void* source_context,
    SPlayerDirettaNextBlock next_block,
    SPlayerDirettaReleaseBlock release_block
);

bool splayer_diretta_play(void* opaque);
bool splayer_diretta_pause(void* opaque);
void splayer_diretta_close(void* opaque);

// ============================================================================
// 目标设备能力查询（C ABI，等价于 tinyLMS QueryDeviceCapabilitiesEarly）
//
// 流程：创建临时 DIRETTA::Sync 子类 → open → setSink(false,0) → 尝试 PCM
// 48k/44.1k 32/16 配置 → configTransferAuto → connectPrepare(true) → connect
// → connectWait → 读取 Sync::Info → 提取 FormatSupport 范围 / DSD 标志 /
// MTU / MS mode → Find::FwVersion → 清理 stop/disconnect/close。
//
// 所有失败写入 last_error，C ABI 不抛异常。
// ============================================================================

// 目标能力结构（固定宽度数组，零初始值表示未知/不支持）
#define SPLAYER_DIRETTA_TEXT_MAX 128
#define SPLAYER_DIRETTA_FW_MAX    64

typedef struct {
    // 设备基本信息
    char target_name[SPLAYER_DIRETTA_TEXT_MAX];   // 目标名称
    char output_name[SPLAYER_DIRETTA_TEXT_MAX];    // 输出端口名称
    char firmware_version[SPLAYER_DIRETTA_FW_MAX]; // 固件版本字符串
    char ipv6_addr[SPLAYER_DIRETTA_TEXT_MAX];      // IPv6 地址字符串
    char full_addr[SPLAYER_DIRETTA_TEXT_MAX];     // IPv6,PORT 全地址字符串
    int32_t if_idx;                              // 接口号

    // PCM 格式支持（0 表示未知或不支持）
    uint8_t  supports_pcm;            // 非零表示支持 PCM
    uint64_t support_pcm_raw;         // FormatID raw value（位与运算判断位深/采样率）
    uint32_t pcm_min_bits;            // 最小 PCM 位深（如 16）
    uint32_t pcm_max_bits;            // 最大 PCM 位深（如 32）
    uint32_t pcm_min_sample_rate;     // 最小 PCM 采样率（Hz）
    uint32_t pcm_max_sample_rate;     // 最大 PCM 采样率（Hz）
    uint32_t pcm_min_channels;        // 最小声道数
    uint32_t pcm_max_channels;        // 最大声道数

    // DSD 格式支持
    uint8_t  supports_dsd;             // 非零表示支持 DSD
    uint8_t  supports_dsd_lsb;         // 非零表示支持 DSD LSB（DSF）
    uint8_t  supports_dsd_msb;         // 非零表示支持 DSD MSB（DFF）
    uint64_t support_dsd_lsb_raw;      // DSD LSB FormatID raw value
    uint64_t support_dsd_msb_raw;      // DSD MSB FormatID raw value
    uint32_t dsd_min_sample_rate;      // 最小 DSD 采样率（Hz）
    uint32_t dsd_max_sample_rate;      // 最大 DSD 采样率（Hz）
    uint32_t dsd_min_bits;             // 最小 DSD 位深
    uint32_t dsd_max_bits;             // 最大 DSD 位深
    uint32_t dsd_min_channels;         // 最小声道数
    uint32_t dsd_max_channels;         // 最大声道数

    // MTU 信息
    uint32_t mtu_measured;             // 实测路径 MTU
    uint16_t mtu_min;                  // 设备最小 MTU
    uint16_t mtu_req;                  // 设备请求 MTU
    uint32_t mtu_max;                  // 设备最大 MTU
    uint16_t max_size;                 // 单次传输最大数据大小（字节）

    // MS (Multi-Stream) 模式支持位图
    // bit0: MS1, bit1: MS2, bit2: MS3(DDS)
    uint16_t support_ms_mode;
} SPlayerDirettaTargetCaps;

// 查询目标设备能力（同步阻塞，约 2-3 秒）
//
// 参数：
//   target_id    : 目标 full address（IPv6,PORT 格式，等于 get_full_str()）
//   out_caps     : 输出能力结构（指针，不可 NULL）
//
// 返回值：
//   true  成功，out_caps 被填充
//   false 失败，错误信息通过 splayer_diretta_last_error() 获取
//
// 内部流程（对齐 tinyLMS DirettaDriver::QueryDeviceCapabilitiesEarly）：
//   1. 扫描发现目标，匹配 target_id（比较 get_full_str）
//   2. 创建临时 DirectSync 子类
//   3. open(ifno, ...) → setSink(false, 0)
//   4. 尝试 PCM 格式：32bit@48k, 32bit@44.1k, 16bit@48k, 16bit@44.1k
//   5. configTransferAuto → connectPrepare(true) → connect → connectWait
//   6. 读取 Sync::Info 和 FormatSupport，填充所有字段
//   7. 调用 Find::FwVersion 获取固件版本
//   8. 清理：stop → disconnect → close
bool splayer_diretta_query_target_caps(const char* target_id,
                                       SPlayerDirettaTargetCaps* out_caps);

#ifdef __cplusplus
}
#endif

#endif // SPLAYER_DIRETTA_BRIDGE_H
