#include <Diretta/Find>
#include <Diretta/Format>
#include <Diretta/Sync>
#include <ACQUA/Clock>

#include <algorithm>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <memory>
#include <string>
#include <thread>

namespace {

constexpr std::size_t kTextCapacity = 256;
thread_local std::string g_last_error;

using SPlayerDirettaNextBlock = bool (*)(void*, const std::uint8_t**, std::size_t*);
using SPlayerDirettaReleaseBlock = void (*)(void*);

void set_error(std::string message) {
  g_last_error = std::move(message);
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

  void releaseSourceBlock() {
    if (release_block_ != nullptr && source_context_ != nullptr) {
      release_block_(source_context_);
    }
  }

 protected:
  bool getNewStream(diretta_stream& stream) override {
    const std::uint8_t* data = nullptr;
    std::size_t size = 0;
    if (next_block_ == nullptr || source_context_ == nullptr ||
        !next_block_(source_context_, &data, &size) || data == nullptr || size == 0) {
      return false;
    }
    stream.Data.P = const_cast<std::uint8_t*>(data);
    stream.Size = size;
    return true;
  }

 private:
  void* source_context_;
  SPlayerDirettaNextBlock next_block_;
  SPlayerDirettaReleaseBlock release_block_;
};

struct DirettaConnection {
  std::unique_ptr<DIRETTA::Find> find;
  std::unique_ptr<DirectSync> sync;
  DIRETTA::FormatConfigure format;

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
        sync->disconnect(true);
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
  if (!find.findOutput(results)) {
    set_error("Diretta target discovery failed");
    return false;
  }
  return true;
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

void* open_direct_with_format(
  const char* target_id,
  std::uint32_t sample_rate,
  std::uint16_t channels,
  DIRETTA::FormatID format_id,
  DIRETTA::FormatID alternate_format_id,
  void* source_context,
  SPlayerDirettaNextBlock next_block,
  SPlayerDirettaReleaseBlock release_block,
  const char* format_name,
  bool* used_alternate_format) {
  try {
    auto connection = std::make_unique<DirettaConnection>();

    DIRETTA::Find::Setting setting;
    setting.Loopback = false;
    setting.ProductID = 0;
    connection->find = std::make_unique<DIRETTA::Find>(setting);

    DIRETTA::Find::PortResalts results;
    if (!discover(*connection->find, results) || results.empty()) {
      if (g_last_error.empty()) set_error("no Diretta targets found");
      return nullptr;
    }

    ACQUA::IPAddress target;
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

    std::uint32_t mtu = 0;
    if (!connection->find->measSendMTU(target, mtu) || mtu == 0) {
      mtu = 1500;
    }

    connection->sync = std::make_unique<DirectSync>(
      source_context,
      next_block,
      release_block);
    const auto thread_mode = static_cast<DIRETTA::Sync::THRED_MODE>(5);
    const auto ifno = static_cast<std::uint16_t>(target.get_ifno());
    if (!connection->sync->open(
          thread_mode,
          ACQUA::Clock::MilliSeconds(100),
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

    if (!connection->sync->setSink(target, ACQUA::Clock::MilliSeconds(100), true, mtu)) {
      set_error("failed to configure Diretta sink");
      return nullptr;
    }

    DIRETTA::FormatConfigure format;
    if (!format.setSpeed(sample_rate) || !format.setChannel(channels)) {
      set_error(std::string("Diretta SDK rejected exact Source Direct ") + format_name + " rate/channels");
      return nullptr;
    }
    if (used_alternate_format != nullptr) {
      *used_alternate_format = false;
    }
    bool supported = format.setFormat(format_id) && connection->sync->checkSinkSupport(format);
    if (!supported && alternate_format_id != DIRETTA::FormatID::NONE) {
      supported = format.setFormat(alternate_format_id) && connection->sync->checkSinkSupport(format);
      if (supported && used_alternate_format != nullptr) {
        *used_alternate_format = true;
      }
    }
    if (!supported) {
      set_error(std::string("Diretta target does not support Source Direct ") + format_name + " format");
      return nullptr;
    }
    if (!connection->sync->setSinkConfigure(format)) {
      set_error("failed to apply exact Diretta Source Direct format");
      return nullptr;
    }

    connection->sync->configTransferAuto(
      ACQUA::Clock::MicroSeconds(200),
      ACQUA::Clock(),
      ACQUA::Clock::MicroSeconds(100000));

    if (!connection->sync->connectPrepare()) {
      set_error("failed to prepare Diretta Source Direct connection");
      return nullptr;
    }
    if (!connection->sync->connect(0)) {
      set_error("failed to start Diretta Source Direct connection");
      return nullptr;
    }
    if (!connection->sync->connectWait()) {
      set_error("failed to complete Diretta Source Direct connection");
      return nullptr;
    }

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

struct SPlayerDirettaDevice {
  char id[kTextCapacity];
  char name[kTextCapacity];
  char ipv6_addr[kTextCapacity];
  char full_addr[kTextCapacity];
  int32_t if_idx;
  char target_name[kTextCapacity];
  char output_name[kTextCapacity];
  char model_name[kTextCapacity];
  uint32_t mtu;
};

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

  return open_direct_with_format(
    target_id,
    sample_rate,
    channels,
    format_id,
    DIRETTA::FormatID::NONE,
    source_context,
    next_block,
    release_block,
    "PCM",
    nullptr);
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

  const auto base_format_id = DIRETTA::FormatID::FMT_DSD1 |
                              DIRETTA::FormatID::FMT_DSD_SIZ_32 |
                              DIRETTA::FormatID::FMT_DSD_BIG;
  const auto source_format_id = base_format_id |
    (source_lsb_first ? DIRETTA::FormatID::FMT_DSD_LSB : DIRETTA::FormatID::FMT_DSD_MSB);
  const auto alternate_format_id = base_format_id |
    (source_lsb_first ? DIRETTA::FormatID::FMT_DSD_MSB : DIRETTA::FormatID::FMT_DSD_LSB);
  bool used_alternate_format = false;
  auto* connection = open_direct_with_format(
    target_id,
    bit_rate,
    channels,
    source_format_id,
    alternate_format_id,
    source_context,
    next_block,
    release_block,
    "Native DSD",
    &used_alternate_format);
  if (connection != nullptr) {
    *wire_lsb_first = used_alternate_format ? !source_lsb_first : source_lsb_first;
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
    connection->sync->stop();
    connection->sync->releaseSourceBlock();
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

} // extern "C"
