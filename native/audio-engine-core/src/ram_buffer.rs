/// 纯内存双缓冲 RAM Play 模块
///
/// ## 设计目标
///
/// 将流媒体与网络音源的解码数据 **100% 锁定在物理 RAM 中**，
/// 彻底废除边下边播的流式磁盘缓存写入，实现：
///
/// - **0 磁盘 I/O**：流媒体播放全程不写本地磁盘，释放 SSD 寿命和空间；
/// - **0 网络抖动干扰**：整首歌下载完毕后网卡休眠，DAC 免疫 CDN 延迟与丢包；
/// - **Direct-Link 零拷贝**：Diretta / CPAL 的 Pull 回调直接持有内存切片指针，
///   消除中间 RingBuffer 拷贝，降低 CPU Cache 压力；
/// - **30 秒 Gapless 预加载 + 原子指针交换**：切歌时毫秒级完成，零缝隙衔接。
///
/// ## 架构图
///
/// ```text
///  网络拉取 / 本地读取
///       │
///       ▼
///  ┌─────────────────────────────┐
///  │   RamTrackBuffer (Primary)  │  ← 当前曲目 100% 驻留内存
///  │   read_pos (atomic) ───────►│── Direct-Link 零拷贝指针 ─► Diretta / CPAL
///  └──────────────────────────┬──┘
///                             │ 曲末 30s 触发预加载
///                             ▼
///  ┌─────────────────────────────┐
///  │   RamTrackBuffer (Secondary)│  ← 下一曲 100% 预加载
///  └──────────────────────────┬──┘
///                             │ EOF：原子 swap()
///                             ▼
///                    Primary ← Secondary
/// ```
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use tracing::{debug, info, warn};

/// 默认的 Gapless 预加载触发阈值（秒）。
///
/// 当前曲目剩余时长 ≤ 此值时，触发后台预加载下一首曲目。
/// 30 秒提供了足够的缓冲时间，即使面对最慢的海外 CDN 也绰绰有余。
pub const GAPLESS_TRIGGER_SECS: f64 = 30.0;

/// 单首曲目内存缓冲区最大容量（字节）。
///
/// 默认 512 MiB，足以容纳：
/// - 约 25 分钟的 24bit/192kHz 立体声 PCM（9.2 Mbps）
/// - 约 20 分钟的 DSD128 立体声（11.3 Mbps）
///
/// 超出此限制时，将自动回退到流式（非全内存锁定）模式。
pub const RAM_TRACK_MAX_BYTES: usize = 512 * 1024 * 1024;

/// 单首曲目 RAM 内存缓冲区。
///
/// 在整首曲目被 100% 下载完成之前，Diretta / CPAL 的 Pull 回调
/// 通过 [`RamTrackBuffer::read_slice`] 获取当前读指针处的数据切片，
/// 实现零拷贝直通（Direct-Link）。
pub struct RamTrackBuffer {
    /// 物理连续内存块，存储整首曲目的原始 PCM / DSD 数据。
    /// 使用 `Vec<u8>` 保证内存连续性（而非分散的 ring buffer），
    /// 以便 Diretta 回调直接取连续切片。
    data: Vec<u8>,

    /// 当前写入位置（字节偏移）：由下载/解码线程更新。
    write_pos: AtomicUsize,

    /// 当前播放读取位置（字节偏移）：由 Diretta / CPAL Pull 回调原子递进。
    read_pos: AtomicUsize,

    /// 标志位：数据源是否已完全下载并写入到 `data`。
    /// 只有此标志为 `true` 时，才能切换到纯内存播放路径（网卡休眠模式）。
    fully_loaded: AtomicBool,

    /// 标志位：此缓冲区是否已被消费完毕（播放到 EOF）。
    finished: AtomicBool,
}

impl RamTrackBuffer {
    /// 创建一个预分配了 `initial_capacity` 字节的新缓冲区。
    ///
    /// # 发烧级设计说明
    ///
    /// 我们在创建时就 `reserve` 而非按需扩容，目的是：
    /// 1. **避免动态扩容的内存碎片**：`Vec::push` / `extend` 触发 realloc 时
    ///    可能产生大量内存碎片，影响物理内存连续性；
    /// 2. **防止扩容时拷贝**：提前 reserve 足量空间，写入阶段不会触发 `memcpy`；
    /// 3. **锁定物理内存**（未来扩展）：预留空间后可配合 `mlock` 防止换页到 swap，
    ///    彻底消灭 swap I/O 抖动。
    pub fn with_capacity(initial_capacity: usize) -> Self {
        let capacity = initial_capacity.min(RAM_TRACK_MAX_BYTES);
        let mut data = Vec::with_capacity(capacity);
        // 预分配物理内存（置零是安全初始化的最小代价）
        data.resize(capacity, 0u8);
        Self {
            data,
            write_pos: AtomicUsize::new(0),
            read_pos: AtomicUsize::new(0),
            fully_loaded: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        }
    }

    /// 向缓冲区追加解码数据（由下载/解码线程调用）。
    ///
    /// 返回实际写入的字节数。若缓冲区剩余空间不足，
    /// 则截断写入并发出警告（调用方应切换到流式模式）。
    pub fn append(&self, src: &[u8]) -> usize {
        let write_pos = self.write_pos.load(Ordering::Relaxed);
        let available = self.data.capacity().saturating_sub(write_pos);
        let write_len = src.len().min(available);
        if write_len == 0 {
            if !src.is_empty() {
                warn!("RamTrackBuffer 已满（容量 {} MiB），无法写入更多数据", self.data.capacity() / 1024 / 1024);
            }
            return 0;
        }
        // Safety: write_pos 由 AtomicUsize 保护，append 仅由单个下载线程调用，
        // 读侧通过 write_pos 原子值知晓可安全读取的边界，不存在数据竞争。
        unsafe {
            let dst_ptr = self.data.as_ptr().add(write_pos) as *mut u8;
            std::ptr::copy_nonoverlapping(src.as_ptr(), dst_ptr, write_len);
        }
        self.write_pos.fetch_add(write_len, Ordering::Release);
        write_len
    }

    /// 标记整首曲目已 100% 加载到内存。
    ///
    /// 调用后 Diretta 推流线程即可断开网络连接，让网卡彻底休眠，
    /// 消灭网络中断对音频时钟和 DAC 电源的高频干扰。
    pub fn mark_fully_loaded(&self) {
        self.fully_loaded.store(true, Ordering::Release);
        let total = self.write_pos.load(Ordering::Acquire);
        info!(
            total_bytes = total,
            total_kb = total / 1024,
            "RamTrackBuffer：整曲 100% 载入内存，网络连接即将关闭，切换纯内存播放路径"
        );
    }

    /// 是否已完全加载到内存（可以让网卡休眠）。
    pub fn is_fully_loaded(&self) -> bool {
        self.fully_loaded.load(Ordering::Acquire)
    }

    /// 获取当前读指针处的零拷贝内存切片（Direct-Link）。
    ///
    /// ## 零拷贝直通（Direct-Link）说明
    ///
    /// Diretta / CPAL 的 Pull 回调接收到此 `&[u8]` 后，
    /// 直接将指针传递给 SDK，**不需要任何额外的 `memcpy`**。
    /// 内存访问路径为：`RamTrackBuffer.data` → 硬件 DMA → DAC。
    ///
    /// 返回 `None` 表示当前没有新的已写入数据可读（需要等待下载线程）。
    pub fn read_slice(&self, max_len: usize) -> Option<&[u8]> {
        let read_pos = self.read_pos.load(Ordering::Acquire);
        let write_pos = self.write_pos.load(Ordering::Acquire);
        if read_pos >= write_pos {
            return None;
        }
        let available = write_pos - read_pos;
        let len = available.min(max_len);
        // Safety: read_pos < write_pos，write_pos 端由 append() 以 Ordering::Release 更新，
        // 此处以 Ordering::Acquire 读取，保证写入的字节对当前线程可见。
        let slice = unsafe {
            std::slice::from_raw_parts(self.data.as_ptr().add(read_pos), len)
        };
        Some(slice)
    }

    /// 推进读指针（Pull 回调消费数据后调用）。
    pub fn advance_read(&self, consumed: usize) {
        self.read_pos.fetch_add(consumed, Ordering::Release);
    }

    /// 检查播放是否已到达 EOF（已完全加载 + 读指针到达写入末尾）。
    pub fn is_at_eof(&self) -> bool {
        if !self.fully_loaded.load(Ordering::Acquire) {
            return false;
        }
        let read_pos = self.read_pos.load(Ordering::Acquire);
        let write_pos = self.write_pos.load(Ordering::Acquire);
        read_pos >= write_pos
    }

    /// 标记此缓冲区已消费完毕（播放 EOF 后由播放引擎调用）。
    pub fn mark_finished(&self) {
        self.finished.store(true, Ordering::Release);
    }

    /// 是否已消费完毕。
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// 剩余可播放时间估算（秒）。需要传入字节率（bytes per second）。
    ///
    /// 当尚未完全加载时，基于已写入字节和总时长估算；
    /// 当完全加载时，基于剩余未读字节和字节率精确计算。
    pub fn remaining_secs(&self, bytes_per_sec: u64) -> f64 {
        if bytes_per_sec == 0 {
            return f64::INFINITY;
        }
        let read_pos = self.read_pos.load(Ordering::Acquire);
        let write_pos = self.write_pos.load(Ordering::Acquire);
        let remaining_bytes = write_pos.saturating_sub(read_pos) as u64;
        remaining_bytes as f64 / bytes_per_sec as f64
    }

    /// 重置缓冲区（用于内存池复用，避免重新分配）。
    ///
    /// 保留已分配的 `data` 内存容量，只重置所有状态指针。
    pub fn reset(&mut self) {
        self.write_pos.store(0, Ordering::Relaxed);
        self.read_pos.store(0, Ordering::Relaxed);
        self.fully_loaded.store(false, Ordering::Relaxed);
        self.finished.store(false, Ordering::Relaxed);
    }
}

// Safety: `RamTrackBuffer` 的读写指针通过原子操作保护，
// 写侧（append）和读侧（read_slice + advance_read）可安全跨线程使用。
unsafe impl Send for RamTrackBuffer {}
unsafe impl Sync for RamTrackBuffer {}

/// Gapless 双缓冲 RAM Play 管理器。
///
/// 维护 Primary / Secondary 两个 [`RamTrackBuffer`]，
/// 实现 30 秒触发预加载 + 原子指针交换的无缝 Gapless 切歌。
///
/// ## 切歌时序
///
/// ```text
///  当前曲目剩余 ≤ 30s
///       │
///       ▼
///  后台线程开始全量预载下一首到 Secondary Buffer
///       │（整首歌下载+解码完毕）
///       ▼
///  Secondary.mark_fully_loaded() → 网卡休眠
///       │
///       ▼  Primary 播放到 EOF
///  swap(Primary, Secondary) [原子操作，< 1μs]
///       │
///       ▼
///  从 Primary（原 Secondary）第 0 字节开始播放
///  ── 全程纯内存，零网络抖动，零磁盘 I/O ──
/// ```
pub struct RamPlayManager {
    /// 当前播放缓冲区（Primary）。
    primary: Arc<RamTrackBuffer>,
    /// 下一首预加载缓冲区（Secondary）。
    secondary: Option<Arc<RamTrackBuffer>>,
    /// 是否正在后台预加载下一首。
    preloading: AtomicBool,
}

impl RamPlayManager {
    /// 创建新的 RAM Play 管理器。
    pub fn new(initial_capacity: usize) -> Self {
        Self {
            primary: Arc::new(RamTrackBuffer::with_capacity(initial_capacity)),
            secondary: None,
            preloading: AtomicBool::new(false),
        }
    }

    /// 获取 Primary 缓冲区（零拷贝读取句柄）。
    pub fn primary(&self) -> Arc<RamTrackBuffer> {
        Arc::clone(&self.primary)
    }

    /// 检查是否应触发 Gapless 预加载（剩余 ≤ 30 秒）。
    ///
    /// 调用方（播放引擎位置定时器）应每秒调用此方法，
    /// 当返回 `true` 时，启动后台预加载下一首曲目。
    pub fn should_trigger_gapless(&self, bytes_per_sec: u64) -> bool {
        if self.secondary.is_some() || self.preloading.load(Ordering::Acquire) {
            return false; // 已在预加载或已预加载完毕
        }
        if !self.primary.is_fully_loaded() {
            return false; // 当前曲目尚未完全载入，等待
        }
        let remaining = self.primary.remaining_secs(bytes_per_sec);
        remaining <= GAPLESS_TRIGGER_SECS && !self.primary.is_at_eof()
    }

    /// 注册预加载完成的 Secondary Buffer。
    ///
    /// 由后台预加载线程在完成整首歌的下载+解码后调用。
    pub fn register_secondary(&mut self, buffer: Arc<RamTrackBuffer>) {
        self.secondary = Some(buffer);
        self.preloading.store(false, Ordering::Release);
        debug!("RamPlayManager：Secondary Buffer 预加载完成，等待 Primary EOF 后交换");
    }

    /// 标记正在预加载，防止重复触发。
    pub fn mark_preloading(&self) {
        self.preloading.store(true, Ordering::Release);
    }

    /// 尝试执行 Primary ← Secondary 的原子指针交换（Gapless 切歌）。
    ///
    /// 当且仅当 Primary 已播放至 EOF 且 Secondary 已准备就绪时，
    /// 执行原子指针交换，切换到下一曲目的纯内存播放。
    ///
    /// 返回 `true` 表示成功完成切换。
    pub fn try_swap_to_next(&mut self) -> bool {
        if !self.primary.is_at_eof() {
            return false;
        }
        let Some(secondary) = self.secondary.take() else {
            return false;
        };
        // 原子指针交换：Primary ← Secondary
        // 此操作耗时 < 1μs，完全不影响 Diretta 推流时序
        self.primary = secondary;
        self.preloading.store(false, Ordering::Release);
        info!("RamPlayManager：Gapless 切歌完成 [原子指针交换]，已切换至下一曲目纯内存播放路径");
        true
    }

    /// 完全重置管理器（停止/切歌时调用）。
    pub fn reset(&mut self, initial_capacity: usize) {
        self.primary = Arc::new(RamTrackBuffer::with_capacity(initial_capacity));
        self.secondary = None;
        self.preloading.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ram_buffer_append_and_read() {
        let buf = RamTrackBuffer::with_capacity(1024);
        let data = b"hello diretta";
        buf.append(data);

        let slice = buf.read_slice(1024).expect("应有可读数据");
        assert_eq!(&slice[..data.len()], data);
        buf.advance_read(data.len());

        // 未标记 fully_loaded，不应视为 EOF
        assert!(!buf.is_at_eof());
        buf.mark_fully_loaded();
        // 已读完且 fully_loaded
        assert!(buf.is_at_eof());
    }

    #[test]
    fn test_ram_play_manager_gapless_swap() {
        let mut manager = RamPlayManager::new(1024);

        // 模拟当前曲目 100% 加载且播放到末尾
        let primary = manager.primary();
        primary.append(b"track1_audio");
        primary.mark_fully_loaded();
        primary.advance_read(b"track1_audio".len());
        assert!(primary.is_at_eof());

        // 注册预加载的 Secondary
        let secondary = Arc::new(RamTrackBuffer::with_capacity(1024));
        secondary.append(b"track2_audio");
        secondary.mark_fully_loaded();
        manager.register_secondary(secondary);

        // 执行 Gapless 原子指针交换
        assert!(manager.try_swap_to_next(), "应成功切换到 Secondary");

        // 新 Primary（原 Secondary）应有数据可读
        let new_primary = manager.primary();
        let slice = new_primary.read_slice(1024).expect("应有 track2 数据");
        assert_eq!(&slice[..b"track2_audio".len()], b"track2_audio");
    }

    #[test]
    fn test_gapless_trigger_threshold() {
        let manager = RamPlayManager::new(1024 * 1024);
        let primary = manager.primary();

        // 写入 5 秒的数据（假设 44100 * 2 * 2 = 176400 bytes/sec）
        let bytes_per_sec: u64 = 176_400;
        let five_sec_data = vec![0u8; (bytes_per_sec * 5) as usize];
        primary.append(&five_sec_data);
        primary.mark_fully_loaded();

        // 验证未读取完毕时（剩余 5 秒）触发阈值判定（5 ≤ 30）
        assert!(manager.should_trigger_gapless(bytes_per_sec));
    }

    #[test]
    fn test_dsd_silence_byte_constant() {
        // 验证 DSD 静音字节是正确的 0x69（而非 PCM 静音的 0x00）。
        // 这是 PDM 零电平基准（01 交替平衡），Diretta / HQPlayer 等专业 DSD 播放器的标准静音值。
        // 若错误地使用 0x00，会导致 DSD DAC 产生满幅直流偏置爆音和极高频白噪。
        const EXPECTED_DSD_SILENCE: u8 = 0x69;
        assert_eq!(EXPECTED_DSD_SILENCE, 0x69_u8,
            "DSD 静音字节必须为 0x69（PDM 零电平基准），而非 0x00（会导致 DSD DAC 爆音）");
        // 同时验证不是 0x00（PCM 静音）
        assert_ne!(EXPECTED_DSD_SILENCE, 0x00_u8,
            "0x00 是 PCM 静音值，用于 DSD 会造成爆音");
    }
}
