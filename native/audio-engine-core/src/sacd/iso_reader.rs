//! SACD ISO 镜像扇区读取层。
//!
//! SACD ISO 镜像本质上是 2048 字节扇区的扁平序列（无 ISO 9660 文件系统层），
//! ScarletBook 规范直接按 LSN（Logical Sector Number）寻址。
//!
//! 本模块在 `std::fs::File` 之上提供按 LSN 读取扇区的能力，
//! 对应 `tinyLMS-old` 中 `sacd_reader.c` + `sacd_input.c` 的职责。
//!
//! 仅用于 P4 SACD 元数据探测 / 解码读取，不参与通用音频文件路径。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{anyhow, Context, Result};

/// SACD 扇区大小（字节）。ScarletBook 规范固定为 2048。
pub const SACD_LSN_SIZE: usize = 2048;

/// SACD 读取器：持有打开的 ISO 文件句柄，按 LSN 读取扇区。
pub struct IsoReader {
    file: File,
    /// 文件总扇区数（缓存避免重复 stat）
    total_lsn: u64,
}

impl IsoReader {
    /// 打开 SACD ISO 镜像文件。
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_ref = path.as_ref();
        let file = File::open(path_ref)
            .with_context(|| format!("打开 SACD ISO 文件失败: {}", path_ref.display()))?;
        let meta = file
            .metadata()
            .with_context(|| format!("读取 ISO 元数据失败: {}", path_ref.display()))?;
        let size = meta.len();
        if size < (510 + 10) * SACD_LSN_SIZE as u64 {
            return Err(anyhow!(
                "ISO 文件过小 ({} 字节)，不像是 SACD 镜像（至少需要 {} 字节以容纳 Master TOC）",
                size,
                (510 + 10) * SACD_LSN_SIZE
            ));
        }
        let total_lsn = size / SACD_LSN_SIZE as u64;
        Ok(Self { file, total_lsn })
    }

    /// 返回 ISO 镜像的总扇区数。
    #[allow(dead_code)] // 供 #[cfg(has_libdstdec)] 通路使用，当前无消费者
    pub fn total_lsn(&self) -> u64 {
        self.total_lsn
    }

    /// 读取从 `lsn` 起的 `count` 个连续扇区，返回 `count * SACD_LSN_SIZE` 字节的缓冲。
    ///
    /// 越界时返回 `Err`。读取过程中遇到 EOF 提前结束时返回实际读取的扇区数对应的字节。
    pub fn read_sectors(&mut self, lsn: u64, count: u64) -> Result<Vec<u8>> {
        if lsn >= self.total_lsn {
            return Err(anyhow!("LSN {} 越界（总扇区数 {}）", lsn, self.total_lsn));
        }
        let max_count = self.total_lsn - lsn;
        let actual_count = count.min(max_count);
        let byte_offset = lsn * SACD_LSN_SIZE as u64;
        let byte_len = actual_count as usize * SACD_LSN_SIZE;

        self.file
            .seek(SeekFrom::Start(byte_offset))
            .with_context(|| format!("seek 到 LSN {} 失败", lsn))?;

        let mut buf = vec![0u8; byte_len];
        let mut read_total = 0usize;
        while read_total < byte_len {
            let n = self
                .file
                .read(&mut buf[read_total..])
                .with_context(|| format!("读取 LSN {}+{} 失败", lsn, actual_count))?;
            if n == 0 {
                // EOF 提前到达：截断到实际读取长度
                buf.truncate(read_total);
                break;
            }
            read_total += n;
        }
        Ok(buf)
    }

    /// 读取单个扇区（便利方法）。
    #[allow(dead_code)] // 供 #[cfg(has_libdstdec)] 通路（native_source.rs）使用，当前无消费者
    pub fn read_sector(&mut self, lsn: u64) -> Result<Vec<u8>> {
        self.read_sectors(lsn, 1)
    }
}

// SAFETY: IsoReader 仅持有 std::fs::File（本身 Send）。
// SACD 解码线程独占使用，跨线程 move 安全。
unsafe impl Send for IsoReader {}
