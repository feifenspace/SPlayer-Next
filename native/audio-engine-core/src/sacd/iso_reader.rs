//! SACD 光盘镜像 2048 字节扇区流读取器 (IsoReader)

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// SACD 逻辑扇区大小 (2048 字节)
pub const SACD_SECTOR_SIZE: usize = 2048;

/// 2048 字节扇区 Seekable 读取器
pub struct IsoReader {
    pub path: PathBuf,
    file: File,
    pub total_sectors: u32,
}

impl IsoReader {
    /// 打开 SACD ISO 文件并初始化扇区读取器
    pub fn open<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
        let mut file = File::open(&path_buf)?;
        let file_size = file.seek(SeekFrom::End(0))?;
        file.seek(SeekFrom::Start(0))?;

        let total_sectors = (file_size / SACD_SECTOR_SIZE as u64) as u32;

        Ok(Self {
            path: path_buf,
            file,
            total_sectors,
        })
    }

    /// 从指定扇区号 (LSN) 开始读取连续的扇区
    pub fn read_sectors(&mut self, lsn: u32, count: u32, buf: &mut [u8]) -> std::io::Result<usize> {
        let target_bytes = count as usize * SACD_SECTOR_SIZE;
        if buf.len() < target_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Buffer too small for requested sectors",
            ));
        }

        let offset = lsn as u64 * SACD_SECTOR_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(&mut buf[..target_bytes])?;
        Ok(target_bytes)
    }

    /// 在指定字节偏移处读取数据
    pub fn read_bytes_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(buf)?;
        Ok(buf.len())
    }
}
