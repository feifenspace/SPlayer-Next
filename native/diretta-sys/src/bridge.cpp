#include <Diretta/Find>
#include <Diretta/Format>
#include <Diretta/Sync>
#include <ACQUA/Clock>

#include "diretta_bridge.h"

#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

namespace {

constexpr std::size_t kTextCapacity = 256;
thread_local std::string g_last_error;

using SPlayerDirettaNextBlock = bool (*)(void*, const std::uint8_t**, std::size_t*);
using SPlayerDirettaReleaseBlock = void (*)(void*);

void set_error(std::string message) {
  g_last_error = std::move(message);
}

// v7 诊断开关：读取环境变量并夹取合法范围；未设置时返回默认值
int env_int(const char* name, int default_value, int lo, int hi) {
  const char* raw = std::getenv(name);
  if (raw == nullptr || raw[0] == '\0') {
    return default_value;
  }
  const int value = std::atoi(raw);
  if (value < lo || value > hi) {
    return default_value;
  }
  return value;
}

void clear_error() {
  g_last_error.clear();
}

template <std::size_t N>
void copy_text(char (&dst)[N], const std::string& value) {
  static_assert(N > 0);
  const std::size_t count = std::min(value.size(), N - 1);
  std::memcpy(dst, value.data(), count);
  dst[count] = '\0';
}

class DirectSync final : public DIRETTA::Sync {
 public:
  DirectSync(
    void* source_context,
    SPlayerDirettaNextBlock next_block,
    SPlayerDirettaReleaseBlock release_block)
      : source_context_(source_context),
        next_block_(next_block),
        release_block_(release_block) {}

  // v11：post-play pre-roll 静音（SPLAYER_DIRECT_PREROLL_MS，默认 0）。
  // play() 启动后前 N ms 强制交付静音块，给 DAC PLL 锁相/输出级稳定留出
  // 物理时间，之后 SDK 拉到的第一个块才是真实音频（配合上层淡入）
  void set_preroll(std::uint32_t ms, double bytes_per_second) {
    if (ms == 0 || bytes_per_second <= 0.0) return;
    const long long bytes = static_cast<long long>(
      (static_cast<double>(ms) / 1000.0) * bytes_per_second);
    if (bytes <= 0) return;
    // 复审加固：在控制线程预reserve静音缓冲——上限 = 最大信息包周期
    //（compute_cycle_time_us 钳制 10ms）的字节数 + 冗余。回调内的
    // assign(cycle) 在容量充足时仅填充不复分配，RT 线程零堆分配
    preroll_silence_.reserve(
      static_cast<std::size_t>(bytes_per_second * 0.011) + 64);
    preroll_remaining_bytes_.store(bytes, std::memory_order_release);
  }

  // 复审加固：S16 wire 回退时在控制线程预留降位缓冲（入参为输入 i32 块
  // 字节上限，输出 i16 减半），保证回调内 assign 不触发堆分配
  void reserve_wire_convert(std::size_t input_bytes) {
    wire_convert_buf_.reserve(input_bytes / 2 + 64);
  }

  void prepareFallbackSilence() {
    std::uint8_t mute = 0x00;
    try { mute = getSinkConfigure().getMuteByte(); } catch (...) {}
    fallback_silence_.assign(65535, mute);
  }

  void releaseSourceBlock() {
    if (release_block_ != nullptr && source_context_ != nullptr) {
      release_block_(source_context_);
    }
  }

 protected:
  bool getNewStream(diretta_stream& stream) override {
    // 暂停时持续交付静音且不请求真实源块，播放位置不会前进。
    if (pause_silence_active_.load(std::memory_order_acquire) &&
        !pause_silence_.empty()) {
      stream.Data.P = pause_silence_.data();
      stream.Size = pause_silence_.size();
      pause_silence_callbacks_.fetch_add(1, std::memory_order_acq_rel);
      return true;
    }
    // pre-roll 窗口：仅在 play() 生效后（isPlay）消耗，握手期拉取不触发。
    // 静音块大小取 SDK 当前周期尺寸；不消费真实音源数据
    if (preroll_remaining_bytes_.load(std::memory_order_acquire) > 0 && isPlay()) {
      const std::size_t cycle = getCycleSize();
      if (cycle > 0) {
        if (preroll_silence_.size() != cycle) {
          std::uint8_t mute = 0x00;
          try {
            mute = getSinkConfigure().getMuteByte();
          } catch (...) {
          }
          preroll_silence_.assign(cycle, mute);
        }
        stream.Data.P = preroll_silence_.data();
        stream.Size = cycle;
        const std::int64_t remaining =
          preroll_remaining_bytes_.fetch_sub(static_cast<std::int64_t>(cycle),
                                             std::memory_order_acq_rel) -
          static_cast<std::int64_t>(cycle);
        if (remaining <= 0) {
          std::fprintf(stderr, "[diretta-v11] preroll silence done, real audio starts\n");
          std::fflush(stderr);
        }
        return true;
      }
    }
    const std::uint8_t* data = nullptr;
    std::size_t size = 0;
    if (next_block_ == nullptr || source_context_ == nullptr ||
        !next_block_(source_context_, &data, &size) || data == nullptr || size == 0) {
      // SDK148 treats false as sender termination.  A source gap is silence, never EOF.
      const auto cycle = getCycleSize();
      if (cycle == 0 || fallback_silence_.size() < cycle) return false;
      stream.Data.P = fallback_silence_.data();
      stream.Size = cycle;
      return true;
    }
    // v11-1: Rust 源 payload 恒为 i32 容器；wire 协商为 SIGNED_16 时在桥内
    // 降位（>>16 取高 16 位，bit-exact 对应容器升位的逆变换）。
    // 缓冲按需增长（首块分配一次后复用；assign 语义同 preroll 静音垫）
    if (wire_storage_bits == 16) {
      const std::size_t sample_count = size / 4;
      const std::size_t needed = sample_count * 2;
      if (wire_convert_buf_.size() < needed) {
        wire_convert_buf_.assign(needed, 0);
      }
      const auto* src = reinterpret_cast<const std::int32_t*>(data);
      auto* dst = reinterpret_cast<std::int16_t*>(wire_convert_buf_.data());
      for (std::size_t i = 0; i < sample_count; ++i) {
        dst[i] = static_cast<std::int16_t>(src[i] >> 16);
      }
      stream.Data.P = wire_convert_buf_.data();
      stream.Size = needed;
      return true;
    }
    stream.Data.P = const_cast<std::uint8_t*>(data);
    stream.Size = size;
    return true;
  }

 private:
  void* source_context_;
  SPlayerDirettaNextBlock next_block_;
  SPlayerDirettaReleaseBlock release_block_;
  std::atomic<std::int64_t> preroll_remaining_bytes_{0};
  std::vector<std::uint8_t> preroll_silence_;
  std::vector<std::uint8_t> fallback_silence_;
  // 暂停期间由 SDK 回调复用的静音块；控制线程准备，实时回调只读。
  std::atomic_bool pause_silence_active_{false};
  std::atomic_uint32_t pause_silence_callbacks_{0};
  std::vector<std::uint8_t> pause_silence_;

 public:
  // v11-1: 实际协商的 wire 存储位深（默认 32）。S32 被 Target 拒绝而回退
  // S16 wire 时由上层置 16，getNewStream 据此在桥内把 i32 容器降位；
  // 热重配也以此为准（S16 wire 连接不得被重配回 S32）
  std::uint8_t wire_storage_bits = 32;

  // 暂停时保持 Sync 时钟和网络传输运行，只交付数字静音。
  // 控制线程在置位前准备好缓冲，SDK 回调线程只读取该缓冲。
  bool setPauseSilence(bool paused) {
    const bool was_paused = pause_silence_active_.load(std::memory_order_acquire);
    if (paused) {
      // 若已经在静音态，回调线程可能正读取 pause_silence_；只重置计数，
      // 绝不能重新分配该缓冲，否则会破坏 SDK 对回调指针生命周期的要求。
      if (was_paused) {
        pause_silence_callbacks_.store(0, std::memory_order_release);
        return true;
      }
      const std::size_t cycle = getCycleSize();
      if (cycle == 0) return false;
      std::uint8_t mute = 0x00;
      try {
        mute = getSinkConfigure().getMuteByte();
      } catch (...) {
      }
      pause_silence_.assign(cycle, mute);
      pause_silence_callbacks_.store(0, std::memory_order_release);
      pause_silence_active_.store(true, std::memory_order_release);
    } else {
      pause_silence_active_.store(false, std::memory_order_release);
    }
    return was_paused;
  }

  // 关闭连接前，维持当前 Diretta 会话并让 SDK 实际拉取一段静音。不能只靠
  // Rust 侧淡出：一旦 close() 立即 stop，最后一个静音块未必已经到达 Target。
  // 此处与 DirettaRendererUPnP 的 stopPlayback(false) 对齐；它不改变曲目
  // 数据或 DAC 格式，仅在旧格式会话中发送数字静音。
  void flush_silence_before_stop() noexcept {
    if (!is_connect() || !isPlay()) return;
    try {
      setPauseSilence(true);
      constexpr std::uint32_t kMinCallbacks = 20;
      constexpr auto kMinDuration = std::chrono::milliseconds(120);
      constexpr auto kTimeout = std::chrono::milliseconds(220);
      const auto started = std::chrono::steady_clock::now();
      const auto deadline = started + kTimeout;
      while ((pause_silence_callbacks_.load(std::memory_order_acquire) < kMinCallbacks ||
              std::chrono::steady_clock::now() - started < kMinDuration) &&
             std::chrono::steady_clock::now() < deadline) {
        std::this_thread::sleep_for(std::chrono::milliseconds(1));
      }
      std::fprintf(stderr, "[diretta-v12] shutdown silence callbacks=%u\n",
                   pause_silence_callbacks_.load(std::memory_order_acquire));
      std::fflush(stderr);
    } catch (...) {
      // teardown must continue even if an SDK query fails.
    }
  }

 private:
  std::vector<std::uint8_t> wire_convert_buf_;
};

struct DirettaConnection {
  std::unique_ptr<DIRETTA::Find> find;
  std::unique_ptr<DirectSync> sync;
  DIRETTA::FormatConfigure format;
  // setSink 时记录的请求缓冲时长（µs）：Sink 自报值不可用时兜底
  std::uint64_t requested_sink_buffer_us = 0;
  // v11：open 时实测的 MTU，热重配重算传输周期用
  std::uint32_t mtu = 1500;

  ~DirettaConnection() {
    shutdown();
  }

  void shutdown() noexcept {
    if (!sync) return;
    try {
      if (sync->is_connect()) {
        sync->stop();
        sync->releaseSourceBlock();
        sync->disconnect_flgset();
        sync->disconnect(false);
        sync->disconnectWait();
      } else {
        sync->releaseSourceBlock();
      }
      sync->close();
    } catch (...) {
      // SDK 清理失败不能穿过 C ABI 或析构边界
    }
  }
};

bool discover(DIRETTA::Find& find, DIRETTA::Find::PortResalts& results) {
  if (!find.open()) {
    set_error("failed to open Diretta discovery socket");
    return false;
  }
  // 【对齐 tinyLMS】发现重试 + 广播备选：Target 冷启动时公告可能滞后
  for (int retry = 0; retry < 3; ++retry) {
    if (retry > 0) {
      std::this_thread::sleep_for(std::chrono::milliseconds(300));
    }
    if (find.findOutput(results) && !results.empty()) {
      return true;
    }
    // 备选：标准广播发现（findTarget），转换后供按地址匹配
    DIRETTA::Find::TargetResalts targets;
    if (find.findTarget(targets) && !targets.empty()) {
      for (const auto& [addr, info] : targets) {
        results[addr] = DIRETTA::Find::TargetConnectInfo();
      }
      if (!results.empty()) {
        return true;
      }
    }
  }
  set_error("Diretta target discovery failed");
  return false;
}

// 【对齐 tinyLMS】MTU 测量结果按 IP 缓存：省去局域网重复测量的 50-100ms。
// 默认值（1500）不缓存——下次连接会重试真实测量
std::mutex g_mtu_cache_mutex;
std::map<std::string, std::uint32_t> g_mtu_cache;

// 跨格式切歌必须重建 Sync 会话，但 Target 的 IPv6/端口和网卡索引在同一进程内
// 稳定。缓存已成功发现过的端点，避免每次 Find::findOutput 固定等待约 272ms。
// 缓存只用于建新会话的寻址，不复用旧连接、不跳过 setSink/格式协商/connectWait；
// Target 变更后新的 target_id 不命中，仍会走完整发现。
struct CachedTargetEndpoint {
  ACQUA::IPAddress address;
  std::uint32_t mtu = 1500;
};
std::mutex g_target_endpoint_cache_mutex;
std::map<std::string, CachedTargetEndpoint> g_target_endpoint_cache;

bool cached_target_for(const std::string& target_id, CachedTargetEndpoint& out) {
  std::lock_guard<std::mutex> lock(g_target_endpoint_cache_mutex);
  const auto it = g_target_endpoint_cache.find(target_id);
  if (it == g_target_endpoint_cache.end()) return false;
  out = it->second;
  return true;
}

void cache_target_endpoint(const std::string& target_id,
                           const ACQUA::IPAddress& address,
                           std::uint32_t mtu) {
  std::lock_guard<std::mutex> lock(g_target_endpoint_cache_mutex);
  g_target_endpoint_cache[target_id] = CachedTargetEndpoint{address, mtu};
}

std::uint32_t measured_mtu_for(const ACQUA::IPAddress& target, DIRETTA::Find& find) {
  {
    std::lock_guard<std::mutex> lock(g_mtu_cache_mutex);
    auto it = g_mtu_cache.find(target.get_full_str());
    if (it != g_mtu_cache.end()) {
      return it->second;
    }
  }
  std::uint32_t mtu = 0;
  if (!find.measSendMTU(target, mtu) || mtu == 0) {
    mtu = 1500;
    return mtu;
  }
  {
    std::lock_guard<std::mutex> lock(g_mtu_cache_mutex);
    g_mtu_cache[target.get_full_str()] = mtu;
  }
  return mtu;
}

DIRETTA::FormatID pcm_format(std::uint8_t storage_bits) {
  switch (storage_bits) {
    case 16:
      return DIRETTA::FormatID::FMT_PCM_SIGNED_16;
    case 32:
      return DIRETTA::FormatID::FMT_PCM_SIGNED_32;
    default:
      return DIRETTA::FormatID::NONE;
  }
}

// 【对齐 tinyLMS】按 MTU 与格式字节率动态计算传输周期（Information Packet
// Cycle）：cycle = (MTU - IPv6/UDP 头 48B) / 字节率，钳制 100µs~10ms。
// 【真机依据】切歌静默期"哒"声定位实验（杂音落在新会话建立段）：官方样例的
// 固定 100ms 信息包周期 + configTransferAuto(200µs,0,100ms) 与 tinyLMS 的
// 格式相关周期（DSD64≈2ms / PCM44.1≈8ms，configTransferAuto(cycle,0,30ms)）
// 相比，TargetApp 会按协商周期重配置 DAC 输出时钟——极端周期组合是哒声
// 嫌疑；tinyLMS 同类 target 真机验证该组参数无杂音
static std::uint32_t compute_cycle_time_us(std::uint32_t mtu, double bytes_per_second) {
  const std::uint32_t udp_overhead = 48; // IPv6(40) + UDP(8)
  const std::uint32_t efficient = (mtu > udp_overhead) ? (mtu - udp_overhead) : 1452;
  if (bytes_per_second <= 0.0) {
    return 4000; // 字节率未知时兜底 4ms
  }
  double cycle = (static_cast<double>(efficient) / bytes_per_second) * 1000000.0;
  if (cycle < 100.0) {
    cycle = 100.0;
  }
  if (cycle > 10000.0) {
    cycle = 10000.0;
  }
  return static_cast<std::uint32_t>(cycle);
}

void* open_direct_with_format(
  const char* target_id,
  std::uint32_t sample_rate,
  std::uint16_t channels,
  const DIRETTA::FormatID* candidate_formats,
  std::size_t candidate_count,
  void* source_context,
  SPlayerDirettaNextBlock next_block,
  SPlayerDirettaReleaseBlock release_block,
  const char* format_name,
  std::size_t* used_candidate_index,
  double bytes_per_second) {
  try {
    const auto open_started = std::chrono::steady_clock::now();
    auto connection = std::make_unique<DirettaConnection>();

    DIRETTA::Find::Setting setting;
    setting.Loopback = false;
    setting.ProductID = 0;
    connection->find = std::make_unique<DIRETTA::Find>(setting);

    ACQUA::IPAddress target;
    std::uint32_t mtu = 1500;
    CachedTargetEndpoint cached;
    const bool target_cache_hit = cached_target_for(target_id, cached);
    auto discovery_done = open_started;
    auto mtu_done = open_started;
    if (target_cache_hit) {
      target = cached.address;
      mtu = cached.mtu;
    } else {
      DIRETTA::Find::PortResalts results;
      if (!discover(*connection->find, results) || results.empty()) {
        if (g_last_error.empty()) set_error("no Diretta targets found");
        return nullptr;
      }
      discovery_done = std::chrono::steady_clock::now();
      for (const auto& [address, _info] : results) {
        if (address.get_full_str() == target_id) {
          target = address;
          break;
        }
      }
      if (target.is_empty()) {
        set_error("requested Diretta target was not found");
        return nullptr;
      }
      // 【对齐 tinyLMS】MTU 按 IP 缓存（见 measured_mtu_for）
      mtu = measured_mtu_for(target, *connection->find);
      mtu_done = std::chrono::steady_clock::now();
      cache_target_endpoint(target_id, target, mtu);
    }
    connection->mtu = mtu;

    connection->sync = std::make_unique<DirectSync>(
      source_context,
      next_block,
      release_block);
    // 【对齐 tinyLMS】THRED_MODE(289) = NOFASTFEEDBACK | FEEDBACKOFFSET(1) |
    // CRITICAL：反馈滑动平均 + 关闭快速反馈，真机验证组合；
    // open 第二参 = Information Packet Cycle（SDK 头文件语义），用动态周期
    // 替代官方样例的固定 100ms（哒声定位：新会话建立段，见
    // compute_cycle_time_us 注释）
    const std::uint32_t cycle_time_us = compute_cycle_time_us(mtu, bytes_per_second);
    const auto thread_mode = static_cast<DIRETTA::Sync::THRED_MODE>(289);
    const auto ifno = static_cast<std::uint16_t>(target.get_ifno());
    if (!connection->sync->open(
          thread_mode,
          ACQUA::Clock::MicroSeconds(cycle_time_us),
          ifno,
          "SPlayer-Next",
          0,
          0,
          0,
          0,
          DIRETTA::Sync::MSMODE_AUTO)) {
      set_error("failed to open Diretta Source Direct sync");
      return nullptr;
    }

    // v7 诊断开关 A：Sink 缓冲时长（SPLAYER_DIRECT_SINK_BUFFER_MS，默认 100，
    // tinyLMS 用 30）。请求 100ms Sink 缓冲（Phase0 诊断：上层排空垫时长需覆盖该值）
    const int sink_buffer_ms = env_int("SPLAYER_DIRECT_SINK_BUFFER_MS", 100, 10, 500);
    connection->requested_sink_buffer_us = static_cast<std::uint64_t>(
      ACQUA::Clock::MilliSeconds(sink_buffer_ms).getMicroSeconds());
    // 第三参 = Disable Sink's playback rejection：false 启用 target 播放拒绝
    // 门控（对齐 tinyLMS：确保 connectPrepare(true) 后由显式 play() 控制起播）。
    // 【真机 A/B 依据】跨格式切歌低频咚声定位实验：暂停（sync->stop）与切歌
    // 均有咚、停止（纯 disconnect）无咚——新会话以 true（无门控）建立时
    // target 侧输出重配置产生咚；false 门控下起播时序由 host 显式 play 主导
    if (!connection->sync->setSink(target, ACQUA::Clock::MilliSeconds(sink_buffer_ms), false, mtu)) {
      set_error("failed to configure Diretta sink");
      return nullptr;
    }
    std::fprintf(stderr,
                 "[diretta-v7] knobs: sink_buffer_ms=%d connect_prepare=%d connect_delay_ms=%d\n",
                 sink_buffer_ms,
                 env_int("SPLAYER_DIRECT_CONNECTPREPARE", 1, 0, 1),
                 env_int("SPLAYER_DIRECT_CONNECT_DELAY_MS", 0, 0, 10000));
    std::fflush(stderr);

    DIRETTA::FormatConfigure format;
    if (!format.setSpeed(sample_rate) || !format.setChannel(channels)) {
      set_error(std::string("Diretta SDK rejected exact Source Direct ") + format_name + " rate/channels");
      return nullptr;
    }
    if (used_candidate_index != nullptr) {
      *used_candidate_index = 0;
    }
    // R8 位图协商：按调用方给定的偏好顺序逐个探测（checkSinkSupport 为本地
    // 校验，无网络往返），取第一个 Target 支持者——替代旧的"主/备两条硬编码"
    std::size_t chosen = candidate_count;
    for (std::size_t index = 0; index < candidate_count; ++index) {
      if (format.setFormat(candidate_formats[index]) &&
          connection->sync->checkSinkSupport(format)) {
        chosen = index;
        break;
      }
    }
    if (chosen == candidate_count) {
      set_error(std::string("Diretta target does not support Source Direct ") + format_name + " format");
      return nullptr;
    }
    if (used_candidate_index != nullptr) {
      *used_candidate_index = chosen;
    }
    if (!connection->sync->setSinkConfigure(format)) {
      set_error("failed to apply exact Diretta Source Direct format");
      return nullptr;
    }

    // 【对齐 tinyLMS】(cycle, 0, 30ms)：三参依次为 Minimum Sync System Time /
    // Target Cycle Time(0=默认) / Maximum Cycle Time(busy 恢复)。
    // min = 动态信息包周期（DSD64≈2ms / PCM44.1≈8ms），max = 30ms——与
    // tinyLMS 完全一致；历史值 (100ms,0,30ms) min>max 自相矛盾、
    // (200µs,0,100ms) 范围极端，均与 target 端 DAC 重配置哒声相关；
    // 如需回退：MicroSeconds(200) / Clock() / MicroSeconds(100000)
    connection->sync->configTransferAuto(
      ACQUA::Clock::MicroSeconds(cycle_time_us),
      ACQUA::Clock(),
      ACQUA::Clock::MicroSeconds(30000));

    const auto configure_done = std::chrono::steady_clock::now();

    // true = 强制 Target 状态机重置：全量重连（跨格式/重连）需要干净的协商起点。
    // 连接建立带重试（含残留状态清理）：Target 旧会话释放慢/状态残留时
    // connectWait 可能瞬时失败（.dbg 现场 connectWait-timeout, is_connect=0），
    // 退避重试通常可自愈；RT 权限缺失等确定性失败会快速连败后按原错误返回
    bool connected = false;
    const auto connect_started = std::chrono::steady_clock::now();
    std::string connect_error;
    for (int attempt = 1; attempt <= 3 && !connected; ++attempt) {
      if (attempt > 1) {
        std::this_thread::sleep_for(std::chrono::milliseconds(300 * (attempt - 1)));
        try {
          if (connection->sync->is_connect()) {
            connection->sync->stop();
            connection->sync->disconnect_flgset();
            connection->sync->disconnect(true);
            connection->sync->disconnectWait();
          }
        } catch (...) {
          // 残留状态清理失败不阻断重试
        }
      }
      // v7 诊断开关 B：connectPrepare 参数（SPLAYER_DIRECT_CONNECTPREPARE，
      // 默认 1=自动调整 Target delay；0=不自动调整，观察咚声是否来自
      // Target 端 delay 重配置瞬间）
      const bool auto_adjust_target_delay =
        env_int("SPLAYER_DIRECT_CONNECTPREPARE", 1, 0, 1) != 0;
      if (!connection->sync->connectPrepare(auto_adjust_target_delay)) {
        connect_error = "failed to prepare Diretta Source Direct connection";
        continue;
      }
      if (!connection->sync->connect(0)) {
        connect_error = "failed to start Diretta Source Direct connection";
        continue;
      }
      if (connection->sync->connectWait()) {
        connected = true;
      } else {
        connect_error = "failed to complete Diretta Source Direct connection";
      }
    }
    if (!connected) {
      set_error(connect_error.empty() ? "Diretta Source Direct connection failed"
                                      : connect_error);
      return nullptr;
    }

    const auto connected_done = std::chrono::steady_clock::now();
    const auto elapsed_ms = [](const auto& start, const auto& end) {
      return std::chrono::duration_cast<std::chrono::milliseconds>(end - start).count();
    };
    std::fprintf(stderr,
                 "[diretta-v13] open timing target=%s target_cache_hit=%d discovery_ms=%lld mtu_ms=%lld configure_ms=%lld connect_ms=%lld total_ms=%lld\n",
                 target_id,
                 target_cache_hit ? 1 : 0,
                 static_cast<long long>(elapsed_ms(open_started, discovery_done)),
                 static_cast<long long>(elapsed_ms(discovery_done, mtu_done)),
                 static_cast<long long>(elapsed_ms(mtu_done, configure_done)),
                 static_cast<long long>(elapsed_ms(connect_started, connected_done)),
                 static_cast<long long>(elapsed_ms(open_started, connected_done)));
    std::fflush(stderr);

    // v7 诊断开关 C：connectWait 完成后、返回上层（上层才会 play 起播）前
    // 的静默等待（SPLAYER_DIRECT_CONNECT_DELAY_MS，默认 0）。设为 3000 可把
    // "咚"精确定位在新会话建立瞬间 vs 首帧起播瞬间。
    const int connect_delay_ms = env_int("SPLAYER_DIRECT_CONNECT_DELAY_MS", 0, 0, 10000);
    if (connect_delay_ms > 0) {
      std::fprintf(stderr, "[diretta-v7] connect done, holding silence %d ms before play\n",
                   connect_delay_ms);
      std::fflush(stderr);
      std::this_thread::sleep_for(std::chrono::milliseconds(connect_delay_ms));
    }

    // v11：post-play pre-roll 静音武装（SPLAYER_DIRECT_PREROLL_MS，默认 0=关闭）。
    // play() 后前 N ms 交付静音块给 DAC 锁相留时间，之后 SDK 才拉到真实音频
    {
      const int preroll_ms = env_int("SPLAYER_DIRECT_PREROLL_MS", 0, 0, 2000);
      if (preroll_ms > 0) {
        connection->sync->set_preroll(static_cast<std::uint32_t>(preroll_ms),
                                      bytes_per_second);
        std::fprintf(stderr, "[diretta-v11] preroll %d ms armed\n", preroll_ms);
        std::fflush(stderr);
      }
    }

    connection->sync->prepareFallbackSilence();
    connection->format = connection->sync->getSinkConfigure();
    return connection.release();
  } catch (const std::exception& error) {
    set_error(error.what());
  } catch (...) {
    set_error("unknown exception while opening Diretta Source Direct target");
  }
  return nullptr;
}

} // namespace

extern "C" {

const char* splayer_diretta_last_error() {
  return g_last_error.c_str();
}

std::size_t splayer_diretta_scan(SPlayerDirettaDevice* devices, std::size_t capacity) {
  clear_error();
  try {
    DIRETTA::Find::Setting setting;
    setting.Loopback = false;
    setting.ProductID = 0;
    DIRETTA::Find find(setting);
    DIRETTA::Find::PortResalts results;
    if (!discover(find, results)) return 0;

    const std::size_t count = std::min(capacity, results.size());
    std::size_t index = 0;
    for (const auto& [address, info] : results) {
      if (index >= count) break;
      const std::string id = address.get_full_str();
      const std::string label = !info.outputName.empty()
        ? info.outputName
        : (!info.targetName.empty() ? info.targetName : "Diretta Target");
      if (devices != nullptr) {
        copy_text(devices[index].id, id);
        copy_text(devices[index].name, label);
        copy_text(devices[index].ipv6_addr, address.get_str());
        copy_text(devices[index].full_addr, id);
        devices[index].if_idx = static_cast<int32_t>(address.get_ifno());
        copy_text(devices[index].target_name, info.targetName);
        copy_text(devices[index].output_name, info.outputName);
        copy_text(devices[index].model_name, info.targetName);
        devices[index].mtu = 1500;
      }
      ++index;
    }
    return results.size();
  } catch (const std::exception& error) {
    set_error(error.what());
  } catch (...) {
    set_error("unknown exception during Diretta discovery");
  }
  return 0;
}

void* splayer_diretta_open_direct(
  const char* target_id,
  std::uint32_t sample_rate,
  std::uint16_t channels,
  std::uint8_t storage_bits,
  void* source_context,
  SPlayerDirettaNextBlock next_block,
  SPlayerDirettaReleaseBlock release_block) {
  clear_error();
  if (target_id == nullptr || *target_id == '\0') {
    set_error("Diretta target id is required");
    return nullptr;
  }
  if (sample_rate == 0 || channels == 0 || source_context == nullptr ||
      next_block == nullptr || release_block == nullptr) {
    set_error("invalid Diretta Source Direct configuration");
    return nullptr;
  }
  const auto format_id = pcm_format(storage_bits);
  if (format_id == DIRETTA::FormatID::NONE) {
    set_error("Diretta Source Direct supports only packed PCM16/PCM32 storage");
    return nullptr;
  }

  const DIRETTA::FormatID candidates[] = { format_id };
  // 传输周期计算用字节率：采样率 × 声道 × 存储位深（PCM 槽位宽 = 存储位深）
  const double bytes_per_second =
    static_cast<double>(sample_rate) * channels * (storage_bits / 8.0);
  // v7 排空诊断：把连接指针交给上层前先经过 wire 位深回退判定
  auto* connection = static_cast<DirettaConnection*>(open_direct_with_format(
    target_id,
    sample_rate,
    channels,
    candidates,
    1,
    source_context,
    next_block,
    release_block,
    "PCM",
    nullptr,
    bytes_per_second));
  // v11-1: S32 优先（tinyLMS 同款偏好，统一容器后 wire 恒请求 S32LE）。
  // Target 不支持 S32LE 时整链回退 S16 wire：重开连接（周期/预卷按 S16
  // 字节率正确重算），并在 DirectSync 内把 i32 容器降位（>>16）。
  // 仅对"格式不支持"类失败回退；target 未找到等错误原样返回
  if (connection == nullptr && storage_bits == 32 &&
      g_last_error.find("does not support Source Direct PCM") != std::string::npos) {
    std::fprintf(stderr,
                 "[diretta-v11] target rejected PCM32, retrying with PCM16 wire\n");
    std::fflush(stderr);
    const DIRETTA::FormatID s16_candidates[] = { DIRETTA::FormatID::FMT_PCM_SIGNED_16 };
    const double s16_bytes_per_second =
      static_cast<double>(sample_rate) * channels * 2.0;
    connection = static_cast<DirettaConnection*>(open_direct_with_format(
      target_id,
      sample_rate,
      channels,
      s16_candidates,
      1,
      source_context,
      next_block,
      release_block,
      "PCM",
      nullptr,
      s16_bytes_per_second));
    if (connection != nullptr) {
      connection->sync->wire_storage_bits = 16;
      // 复审加固：预留降位缓冲（64KB 输入 ≈ 16K 样本，覆盖常规解码帧上限），
      // 避免回调内首次/偶发大块触发堆分配
      connection->sync->reserve_wire_convert(64 * 1024);
      std::fprintf(stderr, "[diretta-v11] PCM16 wire fallback engaged\n");
      std::fflush(stderr);
    }
  }
  return connection;
}

void* splayer_diretta_open_dsd_direct(
  const char* target_id,
  std::uint32_t bit_rate,
  std::uint16_t channels,
  bool source_lsb_first,
  bool* wire_lsb_first,
  void* source_context,
  SPlayerDirettaNextBlock next_block,
  SPlayerDirettaReleaseBlock release_block) {
  clear_error();
  if (target_id == nullptr || *target_id == '\0') {
    set_error("Diretta target id is required");
    return nullptr;
  }
  if (bit_rate == 0 || channels == 0 || wire_lsb_first == nullptr ||
      source_context == nullptr || next_block == nullptr || release_block == nullptr) {
    set_error("invalid Diretta Native DSD configuration");
    return nullptr;
  }

  // R8：DSD 位图 4 组合探测，顺序偏好 = 源位序优先（避免 Rust 侧
  // set_wire_bit_order_while_paused 的位重排）、字节序其次（BIG 先，
  // 沿用原硬编码偏好）。SDK Format.hpp：LSB↔DSF、MSB↔DFF
  const DIRETTA::FormatID bit_orders[] = {
    source_lsb_first ? DIRETTA::FormatID::FMT_DSD_LSB : DIRETTA::FormatID::FMT_DSD_MSB,
    source_lsb_first ? DIRETTA::FormatID::FMT_DSD_MSB : DIRETTA::FormatID::FMT_DSD_LSB,
  };
  const DIRETTA::FormatID byte_orders[] = {
    DIRETTA::FormatID::FMT_DSD_BIG,
    DIRETTA::FormatID::FMT_DSD_LITTLE,
  };
  DIRETTA::FormatID candidates[4];
  std::size_t candidate_count = 0;
  for (const auto bit_order : bit_orders) {
    for (const auto byte_order : byte_orders) {
      candidates[candidate_count++] = DIRETTA::FormatID::FMT_DSD1 |
                                      DIRETTA::FormatID::FMT_DSD_SIZ_32 |
                                      bit_order | byte_order;
    }
  }
  std::size_t used_candidate_index = 0;
  // 传输周期计算用字节率：DSD 位率（每声道 bit/s）× 声道 / 8
  const double dsd_bytes_per_second =
    static_cast<double>(bit_rate) * channels / 8.0;
  auto* connection = open_direct_with_format(
    target_id,
    bit_rate,
    channels,
    candidates,
    candidate_count,
    source_context,
    next_block,
    release_block,
    "Native DSD",
    &used_candidate_index,
    dsd_bytes_per_second);
  if (connection != nullptr) {
    // 候选表 [0,1] 为源位序、[2,3] 为翻转位序
    *wire_lsb_first = used_candidate_index < 2
      ? source_lsb_first
      : !source_lsb_first;
  }
  return connection;
}

bool splayer_diretta_play(void* opaque) {
  clear_error();
  auto* connection = static_cast<DirettaConnection*>(opaque);
  if (connection == nullptr || !connection->sync) {
    set_error("invalid Diretta connection");
    return false;
  }
  try {
    connection->sync->play();
    return true;
  } catch (const std::exception& error) {
    set_error(error.what());
  } catch (...) {
    set_error("unknown exception while starting Diretta playback");
  }
  return false;
}

bool splayer_diretta_pause(void* opaque) {
  clear_error();
  auto* connection = static_cast<DirettaConnection*>(opaque);
  if (connection == nullptr || !connection->sync) {
    set_error("invalid Diretta connection");
    return false;
  }
  try {
    // 保持 Sync::play，解除当前真实块并转为循环静音，避免 DAC stop 点击。
    // 不可在控制线程提前归还当前源块：SDK148 规定回调返回的内存在
    // 下一次 getNewStream 调用前必须保持有效。pause_silence 会在下一次
    // 回调接管输出；恢复后的 next_block 再自然归还旧块，避免 Target
    // 发送线程仍读取被 Rust 环形缓冲复用的块而产生短促点击。
    if (connection->sync->is_connect()) connection->sync->stop();
    return true;
  } catch (const std::exception& error) {
    set_error(error.what());
  } catch (...) {
    set_error("unknown exception while pausing Diretta playback");
  }
  return false;
}

void splayer_diretta_close(void* opaque) {
  clear_error();
  auto connection = std::unique_ptr<DirettaConnection>(static_cast<DirettaConnection*>(opaque));
}

// ============================================================================
// Phase0 诊断导出：Sink 实测参数（排空垫时长动态化 + DSD 静音字节定案）
// 全部 try/catch 包裹，连接无效或 SDK 调用失败时返回 0（上层按"未注入"处理）
// ============================================================================
std::uint64_t splayer_diretta_sink_latency_us(void* opaque) {
  auto* connection = static_cast<DirettaConnection*>(opaque);
  if (connection == nullptr || !connection->sync) return 0;
  try {
    return static_cast<std::uint64_t>(connection->sync->getLatency().getMicroSeconds());
  } catch (...) {
    return 0;
  }
}

std::uint64_t splayer_diretta_sink_buffer_us(void* opaque) {
  auto* connection = static_cast<DirettaConnection*>(opaque);
  if (connection == nullptr || !connection->sync) return 0;
  try {
    // Sink 自报值（100µs 单位）与 setSink 请求值取大者
    // 注意：Linux LP64 下 uint64_t(unsigned long) 与 ULL 字面量类型不同，显式统一为 uint64_t
    const std::uint64_t reported = static_cast<std::uint64_t>(connection->sync->getSinkInfo().latencyBuffer) * 100ULL;
    return std::max(reported, connection->requested_sink_buffer_us);
  } catch (...) {
    return connection->requested_sink_buffer_us;
  }
}

std::size_t splayer_diretta_cycle_size(void* opaque) {
  auto* connection = static_cast<DirettaConnection*>(opaque);
  if (connection == nullptr || !connection->sync) return 0;
  try {
    return connection->sync->getCycleSize();
  } catch (...) {
    return 0;
  }
}

std::uint8_t splayer_diretta_mute_byte(void* opaque) {
  auto* connection = static_cast<DirettaConnection*>(opaque);
  if (connection == nullptr || !connection->sync) return 0;
  try {
    return connection->sync->getSinkConfigure().getMuteByte();
  } catch (...) {
    return 0;
  }
}

// ============================================================================
// v11 实验：不拆会话热重配（同族 PCM 采样率变化）
// 序列：stop（停流但会话保持）→ setSinkConfigure（会话内下发新格式）→
// configTransferAuto（周期随新字节率重算）→ play。SDK148 无文档保证连接中
// setSinkConfigure 会向外发送（tinyLMS 反证），生效与否以听感为准；
// 失败由上层回退 full reconnect。preroll 重新武装（若启用）
// ============================================================================
bool splayer_diretta_pcm_reconfigure(
  void* opaque,
  std::uint32_t sample_rate,
  std::uint16_t channels,
  std::uint8_t storage_bits,
  double bytes_per_second) {
  clear_error();
  auto* connection = static_cast<DirettaConnection*>(opaque);
  if (connection == nullptr || connection->sync == nullptr) {
    set_error("Diretta connection is not open");
    return false;
  }
  if (sample_rate == 0 || channels == 0 || bytes_per_second <= 0.0) {
    set_error("invalid Diretta hot reconfigure parameters");
    return false;
  }
  try {
    if (!connection->sync->is_connect()) {
      set_error("Diretta connection is not online for hot reconfigure");
      return false;
    }
    // v11-1: 热重配保持既有 wire 位深——S32 被拒回退 S16 的连接不得被
    // 重配回 S32（Rust 源虽然恒发 32 位容器，wire 降位由 DirectSync 负责）。
    // 字节率按 wire 位深在桥内重算，不信任调用方传值（回退场景会失配）
    const std::uint8_t wire_bits = connection->sync->wire_storage_bits;
    if (storage_bits != 0 && storage_bits != wire_bits) {
      set_error("hot reconfigure storage bits must match negotiated wire format");
      return false;
    }
    const auto format_id = pcm_format(wire_bits);
    if (format_id == DIRETTA::FormatID::NONE) {
      set_error("Diretta hot reconfigure supports only packed PCM16/PCM32 storage");
      return false;
    }
    const double wire_bytes_per_second =
      static_cast<double>(sample_rate) * channels * (wire_bits / 8.0);
    DIRETTA::FormatConfigure format;
    if (!format.setSpeed(sample_rate) || !format.setChannel(channels) ||
        !format.setFormat(format_id)) {
      set_error("Diretta SDK rejected hot reconfigure format");
      return false;
    }
    connection->sync->stop();
    // 复审加固：stop 可能留下未归还的 in-flight 块租约（与 shutdown 同模式
    // 显式归还）。Rust 侧 release_in_flight 以 swap(NO_SLOT) 实现幂等安全；
    // 随后 armed 换源的 reset_for_transition 亦会兜底清理
    connection->sync->releaseSourceBlock();
    if (!connection->sync->setSinkConfigure(format)) {
      set_error("failed to setSinkConfigure for hot reconfigure");
      return false;
    }
    // 周期随新字节率重算（drain/块时钟数学依赖该值），min/max 对齐 open 流程
    const std::uint32_t cycle_time_us =
      compute_cycle_time_us(connection->mtu, wire_bytes_per_second);
    connection->sync->configTransferAuto(
      ACQUA::Clock::MicroSeconds(cycle_time_us),
      ACQUA::Clock(),
      ACQUA::Clock::MicroSeconds(30000));
    {
      const int preroll_ms = env_int("SPLAYER_DIRECT_PREROLL_MS", 0, 0, 2000);
      if (preroll_ms > 0) {
        connection->sync->set_preroll(static_cast<std::uint32_t>(preroll_ms),
                                      wire_bytes_per_second);
      }
    }
    connection->sync->play();
    std::fprintf(stderr,
                 "[diretta-v11] hot reconfigure ok: rate=%u ch=%u cycle=%uus\n",
                 sample_rate, channels, cycle_time_us);
    std::fflush(stderr);
    return true;
  } catch (const std::exception& error) {
    set_error(error.what());
  } catch (...) {
    set_error("unknown exception during Diretta hot reconfigure");
  }
  return false;
}

} // extern "C"

// ============================================================================
// 临时 Sync（仅用于能力查询，不接收音频数据）
// ============================================================================
constexpr std::size_t kQuerySilenceSize = 65536;
std::uint8_t s_query_silence_block[kQuerySilenceSize] = {0};

class QuerySync final : public DIRETTA::Sync {
 protected:
  bool getNewStream(diretta_stream& stream) override {
    // 连接握手期间 SDK 工作线程会持续索要数据流；返回 false 会终止工作线程，
    // 导致 connectWait 无法完成。参照 tinyLMS 临时连接回送静音块。
    const std::size_t cycle = getCycleSize();
    if (cycle == 0) {
      return true;
    }
    stream.Data.P = s_query_silence_block;
    stream.Size = cycle;
    return true;
  }
};

// 把字符串写入固定宽度 C 字段（保证 NUL 终止）
template <std::size_t N>
void fill_cstr(char (&dst)[N], const std::string& value) {
  static_assert(N > 0);
  const std::size_t count = std::min(value.size(), N - 1);
  std::memcpy(dst, value.data(), count);
  dst[count] = '\0';
}

extern "C" bool splayer_diretta_query_target_caps(const char* target_id,
                                                SPlayerDirettaTargetCaps* out_caps) {
  // 整个函数被 try/catch 包裹；C ABI 不抛异常
  try {
    if (out_caps == nullptr) {
      set_error("out_caps pointer is null");
      return false;
    }
    if (target_id == nullptr || *target_id == '\0') {
      set_error("target_id is required");
      return false;
    }
    clear_error();
    std::memset(out_caps, 0, sizeof(SPlayerDirettaTargetCaps));

    // 1. 发现目标
    DIRETTA::Find::Setting setting;
    setting.Loopback = false;
    setting.ProductID = 0;
    DIRETTA::Find find(setting);
    DIRETTA::Find::PortResalts results;
    if (!discover(find, results)) {
      return false;
    }

    ACQUA::IPAddress target;
    DIRETTA::Find::TargetConnectInfo target_info;
    bool found = false;
    for (const auto& [address, info] : results) {
      // 优先按 IPv6,PORT 全地址匹配
      if (address.get_full_str() == target_id) {
        target = address;
        target_info = info;
        found = true;
        break;
      }
    }
    if (!found) {
      // 退化：按 IPv6 字符串（不含 port）匹配
      for (const auto& [address, info] : results) {
        if (address.get_str() == target_id) {
          target = address;
          target_info = info;
          found = true;
          break;
        }
      }
    }
    if (!found) {
      set_error("requested Diretta target was not found");
      return false;
    }

    // 2. 预填基本字段
    fill_cstr(out_caps->target_name, target_info.targetName);
    fill_cstr(out_caps->output_name, target_info.outputName);
    fill_cstr(out_caps->ipv6_addr, target.get_str());
    fill_cstr(out_caps->full_addr, target.get_full_str());
    out_caps->if_idx = static_cast<int32_t>(target.get_ifno());

    // 3. 测量 MTU（Find 需预热；尽量复用扫描结果，按 IP 缓存）
    std::uint32_t measured_mtu = measured_mtu_for(target, find);
    out_caps->mtu_measured = measured_mtu;

    // 4. 创建临时 QuerySync 并打开
    QuerySync sync;
    const std::uint16_t ifno = static_cast<std::uint16_t>(target.get_ifno());
    if (!sync.open(
          static_cast<DIRETTA::Sync::THRED_MODE>(0),
          ACQUA::Clock::MilliSeconds(100),
          ifno,
          "SPlayer-Query",
          0,
          0,
          0,
          0,
          DIRETTA::Sync::MSMODE_AUTO)) {
      set_error("failed to open temporary Diretta query sync");
      return false;
    }

    // 后续清理 lambda（任何异常路径都会执行）
    auto cleanup = [&sync]() noexcept {
      try {
        if (sync.is_connect()) {
          sync.stop();
          sync.disconnect_flgset();
          sync.disconnect(true);
          sync.disconnectWait();
        }
        sync.close();
      } catch (...) {
        // 清理失败不能穿过 C ABI
      }
    };

    // 5. setSink（tinyLMS QueryDeviceCapabilitiesEarly 传 false, 0）
    if (!sync.setSink(target, ACQUA::Clock::MilliSeconds(100), false, 0)) {
      set_error("failed to setSink for Diretta query");
      cleanup();
      return false;
    }

    // 6. 尝试 PCM 配置（按优先级：32bit@48k → 32bit@44.1k → 16bit@48k → 16bit@44.1k）
    DIRETTA::FormatConfigure fcfg;
    fcfg.setSpeed(48000);
    fcfg.setChannel(2);
    bool format_ok = false;
    const std::array<std::pair<DIRETTA::FormatID, DIRETTA::FormatID>, 4> try_formats = {{
      {DIRETTA::FormatID::CHA_2 | DIRETTA::FormatID::FMT_PCM_SIGNED_32 | DIRETTA::FormatID::RAT_48000,
       DIRETTA::FormatID::RAT_48000},
      {DIRETTA::FormatID::CHA_2 | DIRETTA::FormatID::FMT_PCM_SIGNED_32 | DIRETTA::FormatID::RAT_44100,
       DIRETTA::FormatID::RAT_44100},
      {DIRETTA::FormatID::CHA_2 | DIRETTA::FormatID::FMT_PCM_SIGNED_16 | DIRETTA::FormatID::RAT_48000,
       DIRETTA::FormatID::RAT_48000},
      {DIRETTA::FormatID::CHA_2 | DIRETTA::FormatID::FMT_PCM_SIGNED_16 | DIRETTA::FormatID::RAT_44100,
       DIRETTA::FormatID::RAT_44100},
    }};
    for (const auto& [fid, /*rat*/ _ignore] : try_formats) {
      fcfg.setFormat(fid);
      if (sync.checkSinkSupport(fcfg)) {
        if (sync.setSinkConfigure(fcfg)) {
          format_ok = true;
          break;
        }
      }
    }
    if (!format_ok) {
      // 回退到 32bit@48k（即使设备不支持也尝试建立连接以读取 Info）
      fcfg.setFormat(DIRETTA::FormatID::CHA_2 |
                     DIRETTA::FormatID::FMT_PCM_SIGNED_32 |
                     DIRETTA::FormatID::RAT_48000);
      sync.setSinkConfigure(fcfg);
    }

    // 7. configTransferAuto + connectPrepare(true) + connect + connectWait
    sync.configTransferAuto(
      ACQUA::Clock::MicroSeconds(2620),
      ACQUA::Clock(),
      ACQUA::Clock::MicroSeconds(100000));

    if (!sync.connectPrepare(true)) {
      set_error("failed to prepare Diretta query connection");
      cleanup();
      return false;
    }
    if (!sync.connect(0)) {
      set_error("failed to start Diretta query connection");
      cleanup();
      return false;
    }
    if (!sync.connectWait()) {
      set_error("failed to complete Diretta query connection");
      cleanup();
      return false;
    }

    // 8. 读取 Sync::Info 并填充 PCM/DSD/MTU/MS
    const DIRETTA::Sync::Info& info = sync.getSinkInfo();
    out_caps->supports_pcm     = info.checkSinkSupportPCM()  ? 1u : 0u;
    out_caps->supports_dsd     = info.checkSinkSupportDSD()  ? 1u : 0u;
    out_caps->support_pcm_raw  = static_cast<std::uint64_t>(info.supportPCM);
    out_caps->support_dsd_lsb_raw = static_cast<std::uint64_t>(info.supportDSDlsb);
    out_caps->support_dsd_msb_raw = static_cast<std::uint64_t>(info.supportDSDmsb);
    out_caps->supports_dsd_lsb = info.checkSinkSupportDSDlsb() ? 1u : 0u;
    out_caps->supports_dsd_msb = info.checkSinkSupportDSDmsb() ? 1u : 0u;

    // PCM FormatSupport 范围
    if (info.checkSinkSupportPCM()) {
      DIRETTA::FormatSupport pcm(info.supportPCM);
      out_caps->pcm_min_sample_rate = pcm.getSpeedMin();
      out_caps->pcm_max_sample_rate = pcm.getSpeedMax();
      out_caps->pcm_min_bits        = pcm.getBitsMin();
      out_caps->pcm_max_bits        = pcm.getBitsMax();
      out_caps->pcm_min_channels    = pcm.getChMin();
      out_caps->pcm_max_channels    = pcm.getChMax();
    }

    // DSD FormatSupport 范围（LSB | MSB 合并）
    if (info.checkSinkSupportDSD()) {
      const DIRETTA::FormatID dsd_combined =
        DIRETTA::FormatID(static_cast<std::uint64_t>(info.supportDSDlsb) |
                          static_cast<std::uint64_t>(info.supportDSDmsb));
      DIRETTA::FormatSupport dsd(dsd_combined);
      out_caps->dsd_min_sample_rate = dsd.getSpeedMin();
      out_caps->dsd_max_sample_rate = dsd.getSpeedMax();
      out_caps->dsd_min_bits        = dsd.getBitsMin();
      out_caps->dsd_max_bits        = dsd.getBitsMax();
      out_caps->dsd_min_channels    = dsd.getChMin();
      out_caps->dsd_max_channels    = dsd.getChMax();
    }

    // MTU 范围
    out_caps->mtu_min   = info.minMTU;
    out_caps->mtu_req   = info.reqMTU;
    out_caps->mtu_max   = static_cast<std::uint32_t>(info.maxMTU);
    out_caps->max_size  = info.maxSize;

    // MS mode 位图
    out_caps->support_ms_mode = info.supportMSmode;

    // Sink 延迟（B7.2，单位 100 微秒）
    out_caps->latency_buffer_x100us = info.latencyBuffer;
    out_caps->latency_max_x100us = info.latencyMax;
    out_caps->latency_hw_x100us = info.latencyHw;

    // 9. 固件版本（Find::FwVersion），失败不致命
    std::string fw_version;
    if (find.FwVersion(target, fw_version)) {
      fill_cstr(out_caps->firmware_version, fw_version);
    }

    // 10. 清理
    cleanup();
    return true;
  } catch (const std::exception& error) {
    set_error(error.what());
  } catch (...) {
    set_error("unknown exception while querying Diretta target capabilities");
  }
  return false;
}
