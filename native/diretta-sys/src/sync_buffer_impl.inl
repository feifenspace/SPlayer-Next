// DirettaSyncImpl: 基于官方 DIRETTA::SyncBuffer 组合实现的标准 Push Mode 传输类
#pragma once

class DirettaSyncImpl final {
public:
    DIRETTA::SyncBuffer syncbuffer_;
    diretta_event_cb cb_{nullptr};
    void* user_data_{nullptr};
    std::atomic<bool> opened_{false};
    std::atomic<bool> connected_{false};
    std::atomic<bool> playing_{false};
    std::mutex ctrl_mtx_;
    std::atomic<bool> is_dsd_{false};
    bool dsd_bit_reverse_{false};
    bool dsd_byte_swap_{false};
    std::atomic<int> pre_mute_frames_{0};

    void emit_event(int event_type, int error_code) {
        if (cb_ != nullptr) {
            cb_(event_type, error_code, user_data_);
        }
    }

    DirettaSyncImpl(diretta_event_cb cb, void* user_data)
        : cb_(cb)
        , user_data_(user_data)
        , opened_(false)
        , connected_(false)
        , playing_(false)
    {
        ensure_syslog_initialized();
    }

    ~DirettaSyncImpl() {
        try {
            if (playing_.load(std::memory_order_acquire)) {
                syncbuffer_.stop();
                playing_.store(false, std::memory_order_release);
            }
            if (connected_.load(std::memory_order_acquire)) {
                try {
                    syncbuffer_.pre_disconnect(true);
                    syncbuffer_.disconnect(false);
                    syncbuffer_.disconnectWait();
                } catch (...) {}
                connected_.store(false, std::memory_order_release);
            }
            if (opened_.load(std::memory_order_acquire)) {
                try {
                    syncbuffer_.close();
                } catch (...) {}
                opened_.store(false, std::memory_order_release);
            }
        } catch (...) {}
    }

    DirettaSyncImpl(const DirettaSyncImpl&) = delete;
    DirettaSyncImpl& operator=(const DirettaSyncImpl&) = delete;
    DirettaSyncImpl(DirettaSyncImpl&&) = delete;
    DirettaSyncImpl& operator=(DirettaSyncImpl&&) = delete;

    void reset_ring() {}
    void clear_ring_buffer() {}

    bool push(const void* data, size_t size) {
        if (data == nullptr || size == 0) return true;
        if (!connected_.load(std::memory_order_acquire)) {
            return false;
        }
        try {
            DIRETTA::Stream str;
            str.resize(size);
            std::memcpy(str.get(), data, size);
            return syncbuffer_.setStream(str);
        } catch (const std::exception& e) {
            DS_DBG("push exception: %s", e.what());
            return false;
        } catch (...) {
            return false;
        }
    }

    bool set_sink(const std::string& ip_str, std::uint16_t port,
                  std::uint32_t ifno, std::uint32_t mtu,
                  std::uint32_t buffer_ms,
                  std::uint32_t sample_rate, std::uint32_t channels,
                  std::uint32_t bits_per_sample) {
        (void)bits_per_sample;
        DS_DBG("enter set_sink ip=%s port=%u ifno=%u mtu=%u buffer_ms=%u sr=%u ch=%u",
               ip_str.c_str(), port, ifno, mtu, buffer_ms,
               sample_rate, channels);
        std::lock_guard<std::mutex> lk(ctrl_mtx_);

        if (!opened_.load(std::memory_order_acquire)) {
            ensure_syslog_initialized();
            DS_DBG("calling SyncBuffer::open THRED_MODE(5) ifno=0");
            bool ok = syncbuffer_.open(
                static_cast<DIRETTA::Sync::THRED_MODE>(5),
                ACQUA::Clock::MilliSeconds(100),
                0,
                std::string("SPlayer-Next"),
                0,
                0, 0, 0,
                DIRETTA::Sync::MSMODE_AUTO);
            DS_DBG("SyncBuffer::open returned %d", ok ? 1 : 0);
            if (!ok) return false;
            opened_.store(true, std::memory_order_release);
        }

        try {
            ACQUA::IPAddress sink_addr;
            if (!sink_addr.set_V6_str(ip_str)) {
                DS_DBG("set_V6_str failed");
                return false;
            }
            sink_addr.set_port_host(port);
            sink_addr.set_ifno(ifno);

            DS_DBG("calling SyncBuffer::setSink buffer_ms=%u mtu=%u", buffer_ms, mtu);
            ACQUA::Clock buf_clock = (buffer_ms == 0)
                ? ACQUA::Clock::MilliSeconds(100)
                : ACQUA::Clock::MilliSeconds(buffer_ms);
            bool ok = syncbuffer_.setSink(sink_addr, buf_clock, false, mtu);
            DS_DBG("SyncBuffer::setSink returned %d", ok ? 1 : 0);
            if (!ok) return false;

            syncbuffer_.inquirySupportFormat(sink_addr);

            DIRETTA::FormatConfigure fcfg;
            fcfg.setSpeed(sample_rate);
            fcfg.setChannel(channels);

            DIRETTA::FormatID fid = DIRETTA::FormatID::NONE;
            auto try_pcm = [&](DIRETTA::FormatID pcm_id) -> bool {
                fcfg.setFormat(pcm_id);
                if (syncbuffer_.checkSinkSupport(fcfg)) {
                    fid = static_cast<DIRETTA::FormatID>(fcfg);
                    return true;
                }
                return false;
            };

            if (!try_pcm(DIRETTA::FormatID::FMT_PCM_SIGNED_32)) {
                if (!try_pcm(DIRETTA::FormatID::FMT_PCM_SIGNED_24)) {
                    try_pcm(DIRETTA::FormatID::FMT_PCM_SIGNED_16);
                }
            }
            if (fid == DIRETTA::FormatID::NONE) {
                fcfg.setFormat(DIRETTA::FormatID::FMT_PCM_SIGNED_32);
                fid = static_cast<DIRETTA::FormatID>(fcfg);
            }

            DS_DBG("calling SyncBuffer::setSinkConfigure fid=0x%llx (speed=%u, ch=%u)",
                   (unsigned long long)static_cast<std::uint64_t>(fid),
                   fcfg.getSpeed(), fcfg.getChannel());
            if (!syncbuffer_.setSinkConfigure(fid)) {
                DS_DBG("setSinkConfigure failed");
                return false;
            }

            DS_DBG("calling SyncBuffer::configTransferAuto");
            syncbuffer_.configTransferAuto(
                ACQUA::Clock::MicroSeconds(200),
                ACQUA::Clock(),
                ACQUA::Clock::MicroSeconds(100000));

            const int chunk_fs = std::max(1, (int)(sample_rate / 100)); // 10ms 帧块匹配 Rust 推流粒度
            DS_DBG("calling SyncBuffer::setupBuffer chunk_fs=%d depth=100", chunk_fs);
            syncbuffer_.setupBuffer(chunk_fs, 100, false);

            is_dsd_.store(false, std::memory_order_release);
            return true;
        } catch (const std::exception& e) {
            DS_DBG("set_sink exception: %s", e.what());
            return false;
        } catch (...) {
            DS_DBG("set_sink unknown exception");
            return false;
        }
    }

    bool set_sink_dsd(const std::string& ip_str, std::uint16_t port,
                      std::uint32_t ifno, std::uint32_t mtu,
                      std::uint32_t buffer_ms,
                      std::uint32_t dsd_rate_multiplier,
                      std::uint32_t dsd_byte_order,
                      std::uint32_t channels) {
        DS_DBG("enter set_sink_dsd mult=%u order=%u ch=%u", dsd_rate_multiplier, dsd_byte_order, channels);
        std::lock_guard<std::mutex> lk(ctrl_mtx_);

        if (!opened_.load(std::memory_order_acquire)) {
            ensure_syslog_initialized();
            bool ok = syncbuffer_.open(
                static_cast<DIRETTA::Sync::THRED_MODE>(5),
                ACQUA::Clock::MilliSeconds(100),
                ifno,
                std::string("SPlayer-Next"),
                0,
                0, 0, 0,
                DIRETTA::Sync::MSMODE_AUTO);
            if (!ok) return false;
            opened_.store(true, std::memory_order_release);
        }

        try {
            ACQUA::IPAddress sink_addr;
            if (!sink_addr.set_V6_str(ip_str)) return false;
            sink_addr.set_port_host(port);
            sink_addr.set_ifno(ifno);

            ACQUA::Clock buf_clock = (buffer_ms == 0)
                ? ACQUA::Clock::MilliSeconds(100)
                : ACQUA::Clock::MilliSeconds(buffer_ms);
            if (!syncbuffer_.setSink(sink_addr, buf_clock, false, mtu)) {
                return false;
            }

            syncbuffer_.inquirySupportFormat(sink_addr);

            uint32_t dsd_sample_rate = 44100u * dsd_rate_multiplier;
            DIRETTA::FormatConfigure fcfg;
            fcfg.setSpeed(dsd_sample_rate);
            fcfg.setChannel(channels);

            DIRETTA::FormatID fid = DIRETTA::FormatID::FMT_DSD1 | DIRETTA::FormatID::FMT_DSD_SIZ_32;
            if (dsd_byte_order == 0) {
                fid = fid | DIRETTA::FormatID::FMT_DSD_LSB;
            } else {
                fid = fid | DIRETTA::FormatID::FMT_DSD_MSB;
            }

            if (!syncbuffer_.setSinkConfigure(fid)) {
                return false;
            }

            syncbuffer_.configTransferAuto(
                ACQUA::Clock::MicroSeconds(200),
                ACQUA::Clock(),
                ACQUA::Clock::MicroSeconds(100000));

            const int chunk_fs = std::max(1, (int)(dsd_sample_rate / 8 / 100));
            syncbuffer_.setupBuffer(chunk_fs, 100, false);

            is_dsd_.store(true, std::memory_order_release);
            return true;
        } catch (...) {
            return false;
        }
    }

    int connect_sink(int timeout_ms = 5000) {
        (void)timeout_ms;
        DS_DBG("enter connect_sink timeout_ms=%d", timeout_ms);
        std::lock_guard<std::mutex> lk(ctrl_mtx_);
        if (!opened_.load(std::memory_order_acquire)) {
            return DIRETTA_ERR_GENERIC;
        }
        try {
            syncbuffer_.connectPrepare();
            DS_DBG("calling SyncBuffer::connect(false, 0) [push mode]");
            if (!syncbuffer_.connect(false, 0)) {
                DS_DBG("connect failed");
                return DIRETTA_ERR_REFUSED;
            }
            DS_DBG("calling SyncBuffer::connectWait()");
            syncbuffer_.connectWait();
            DS_DBG("connect_sink ok: is_connect=%d is_online=%d",
                   syncbuffer_.is_connect() ? 1 : 0,
                   syncbuffer_.is_online() ? 1 : 0);
            connected_.store(true, std::memory_order_release);
            emit_event(DIRETTA_EVENT_CONNECTED, DIRETTA_OK);
            return DIRETTA_OK;
        } catch (const std::exception& e) {
            DS_DBG("connect_sink exception: %s", e.what());
            return DIRETTA_ERR_GENERIC;
        } catch (...) {
            DS_DBG("connect_sink unknown exception");
            return DIRETTA_ERR_GENERIC;
        }
    }

    bool disconnect_sink() {
        DS_DBG("enter disconnect_sink");
        std::lock_guard<std::mutex> lk(ctrl_mtx_);
        try {
            if (playing_.load(std::memory_order_acquire)) {
                syncbuffer_.stop();
                playing_.store(false, std::memory_order_release);
            }
            if (connected_.load(std::memory_order_acquire)) {
                try {
                    syncbuffer_.pre_disconnect(true);
                    syncbuffer_.disconnect(false);
                    syncbuffer_.disconnectWait();
                } catch (...) {}
                connected_.store(false, std::memory_order_release);
            }
            if (opened_.load(std::memory_order_acquire)) {
                try {
                    syncbuffer_.close();
                } catch (...) {}
                opened_.store(false, std::memory_order_release);
            }
            DS_DBG("disconnect_sink ok");
            emit_event(DIRETTA_EVENT_DISCONNECTED, DIRETTA_OK);
            return true;
        } catch (...) {
            return false;
        }
    }

    bool play_sink() {
        DS_DBG("enter play_sink");
        std::lock_guard<std::mutex> lk(ctrl_mtx_);
        try {
            syncbuffer_.play();
            playing_.store(true, std::memory_order_release);
            DS_DBG("SyncBuffer::play ok, isPlay=%d is_online=%d",
                   syncbuffer_.isPlay() ? 1 : 0,
                   syncbuffer_.is_online() ? 1 : 0);
            return true;
        } catch (const std::exception& e) {
            DS_DBG("play_sink exception: %s", e.what());
            return false;
        } catch (...) {
            return false;
        }
    }

    bool stop_sink() {
        DS_DBG("enter stop_sink");
        std::lock_guard<std::mutex> lk(ctrl_mtx_);
        try {
            syncbuffer_.stop();
            playing_.store(false, std::memory_order_release);
            DS_DBG("SyncBuffer::stop ok");
            return true;
        } catch (...) {
            return false;
        }
    }

    bool isOnline() { return syncbuffer_.is_online(); }
    bool isPlaying() { return syncbuffer_.isPlay(); }

    void trigger_pre_mute(int count) {
        if (count < 0) count = 0;
        pre_mute_frames_.store(count, std::memory_order_release);
    }
    bool wait_pre_mute_done(int timeout_ms) {
        (void)timeout_ms;
        return true;
    }
    bool dsd_transform(bool* bit_reverse, bool* byte_swap) const {
        if (bit_reverse) *bit_reverse = dsd_bit_reverse_;
        if (byte_swap)   *byte_swap   = dsd_byte_swap_;
        return true;
    }
    DIRETTA::FormatConfigure get_sink_configure() const {
        return syncbuffer_.getSinkConfigure();
    }
    DIRETTA::Sync::Info get_sink_info() const {
        return syncbuffer_.getSinkInfo();
    }
};
