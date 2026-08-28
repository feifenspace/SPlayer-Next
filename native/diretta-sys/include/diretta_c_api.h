#ifndef DIRETTA_C_API_H
#define DIRETTA_C_API_H

#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

// -------------------------------------------------------------------------
// 数据结构定义
// -------------------------------------------------------------------------

typedef struct {
    char ipv6_addr[64];
    char full_addr[80];
    int if_idx;
    char target_name[64];
    char output_name[64];
    char model_name[64];
    char firmware_version[32];
    uint32_t mtu;
} diretta_target_info_t;

typedef struct {
    bool supports_pcm;
    bool supports_dsd;
    bool supports_dsd_lsb;
    bool supports_dsd_msb;
    uint32_t pcm_min_sample_rate;
    uint32_t pcm_max_sample_rate;
    uint8_t pcm_min_bits;
    uint8_t pcm_max_bits;
    uint16_t pcm_min_channels;
    uint16_t pcm_max_channels;
    uint32_t dsd_min_sample_rate;
    uint32_t dsd_max_sample_rate;
    uint8_t dsd_min_bits;
    uint8_t dsd_max_bits;
    uint16_t dsd_min_channels;
    uint16_t dsd_max_channels;
    uint32_t mtu_min;
    uint32_t mtu_max;
    uint32_t mtu_req;
    uint32_t max_size;
    uint32_t support_ms_mode;
} diretta_sink_caps_t;

typedef void* diretta_find_handle_t;
typedef void* diretta_sync_handle_t;

/// 回调函数：当 Diretta 底层实时发送线程索要音频帧时触发
/// user_data: 注册时传入的上下文指针
/// out_ptr: 传出参数，指向提供的数据缓冲指针（若为 NULL 则由底层填充静音）
/// out_bytes: 传出参数，实际提供的数据字节数
/// cycle_size: 本次微周期请求的字节大小
/// is_dsd: 当前格式是否为 Native DSD
typedef bool (*diretta_pull_samples_fn)(
    void* user_data,
    const uint8_t** out_ptr,
    size_t* out_bytes,
    size_t cycle_size,
    bool is_dsd
);

// -------------------------------------------------------------------------
// Target 发现与能力探测 API (Find)
// -------------------------------------------------------------------------

diretta_find_handle_t diretta_find_create(void);
void diretta_find_destroy(diretta_find_handle_t handle);
bool diretta_find_open(diretta_find_handle_t handle);
int diretta_find_scan(diretta_find_handle_t handle, diretta_target_info_t* out_targets, int max_targets, int retry_count);
uint32_t diretta_find_measure_mtu(diretta_find_handle_t handle, const char* ipv6_addr, int if_idx);

// -------------------------------------------------------------------------
// Sync 传输流控制 API (Sync)
// -------------------------------------------------------------------------

diretta_sync_handle_t diretta_sync_create(diretta_pull_samples_fn callback, void* user_data);
void diretta_sync_destroy(diretta_sync_handle_t handle);

bool diretta_sync_open(
    diretta_sync_handle_t handle,
    int if_idx,
    uint32_t cycle_time_us,
    int thread_mode,
    int ms_mode
);

bool diretta_sync_set_sink(
    diretta_sync_handle_t handle,
    const char* ipv6_addr,
    int if_idx,
    uint32_t mtu,
    uint32_t prefill_ms
);

bool diretta_sync_check_support(
    diretta_sync_handle_t handle,
    uint32_t sample_rate,
    uint16_t channels,
    uint8_t bit_depth,
    bool is_dsd,
    bool is_dsd_lsb
);

bool diretta_sync_configure_and_connect(
    diretta_sync_handle_t handle,
    uint32_t sample_rate,
    uint16_t channels,
    uint8_t bit_depth,
    bool is_dsd,
    bool is_dsd_lsb,
    uint32_t cycle_time_us
);

bool diretta_sync_get_sink_caps(diretta_sync_handle_t handle, diretta_sink_caps_t* out_caps);

bool diretta_sync_play(diretta_sync_handle_t handle);
bool diretta_sync_stop(diretta_sync_handle_t handle);

void diretta_sync_trigger_pre_mute(diretta_sync_handle_t handle, int frame_count);
void diretta_sync_wait_pre_mute_done(diretta_sync_handle_t handle, int timeout_ms);

bool diretta_sync_disconnect(diretta_sync_handle_t handle);
void diretta_sync_close(diretta_sync_handle_t handle);

bool diretta_sync_is_connected(diretta_sync_handle_t handle);
bool diretta_sync_is_online(diretta_sync_handle_t handle);

#ifdef __cplusplus
}
#endif

#endif // DIRETTA_C_API_H
