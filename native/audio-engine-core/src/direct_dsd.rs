use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::priority::{bind_current_thread_to_performance_cores, boost_current_audio_thread};
use anyhow::{bail, ensure, Context, Result};

/// DSD 专用静音字节（PDM 零电平）。
///
/// DSD（Direct Stream Digital）采用脉冲密度调制（PDM）编码，逻辑"0"代表负向偏转，
/// 逻辑"1"代表正向偏转。当缓冲区欠载（Underrun）或切歌过渡时，若填充 `0x00`（全 0 位流），
/// 等效于连续负向满幅直流偏置（DC Offset），会导致 DAC 模拟输出端产生极强的爆音和高频噪音。
///
/// `0x69`（二进制 `01101001`）是 1-bit PDM 流的直流均衡基准（01 交替平衡），
/// 等效于零电平静音，是 Diretta、HQPlayer 等专业 DSD 播放器使用的标准 DSD 静音值。
const DSD_SILENCE_BYTE: u8 = 0x69;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectDsdBitOrder {
    LsbFirst,
    MsbFirst,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectDsdContainer {
    Dsf,
    Dff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectDsdFormat {
    pub bit_rate: u32,
    pub channels: u16,
    pub bit_order: DirectDsdBitOrder,
    pub container: DirectDsdContainer,
}

enum DirectDsdLayout {
    Dsf {
        block_size: usize,
        data_offset: u64,
        data_size: usize,
        total_per_channel: usize,
        remaining_per_channel: usize,
        data_remaining: usize,
        skip_per_channel: usize,
    },
    Dff {
        data_offset: u64,
        data_size: usize,
        remaining: usize,
    },
    Sacd {
        source: crate::sacd::SacdNativeSource,
    },
}

pub struct DirectDsdReader {
    file: File,
    format: DirectDsdFormat,
    layout: DirectDsdLayout,
    input: Vec<u8>,
    max_output_len: usize,
}

impl DirectDsdReader {
    pub fn open_local(path: &Path) -> Result<Self> {
        let path_str = path.to_string_lossy();
        if let Some(sacd_info) = crate::sacd::parse_sacd_virtual_path(&path_str) {
            return Self::open_sacd(&sacd_info.iso_path, &path_str);
        }
        // 裸 ISO 无法定位轨道（零 LSN 虚拟轨必失败），fail loud 拒绝（H2.4）
        if path_str.to_lowercase().ends_with(".iso") {
            bail!("SACD ISO 需经曲库虚拟轨播放，不支持裸路径: {path_str}");
        }

        let mut file = File::open(path).context("打开 Source Direct DSD 文件失败")?;
        let file_len = usize::try_from(file.metadata()?.len()).context("DSD 文件过大")?;
        let mut magic = [0_u8; 4];
        file.read_exact(&mut magic).context("读取 DSD 文件头失败")?;
        file.seek(SeekFrom::Start(0))?;

        match &magic {
            b"DSD " => Self::open_dsf(file, file_len),
            b"FRM8" => Self::open_dff(file, file_len),
            _ => bail!("Source Direct Native DSD 仅支持 DSF/DFF/SACD ISO"),
        }
    }

    fn open_sacd(iso_path: &str, virtual_path: &str) -> Result<Self> {
        let dummy_file = File::open(iso_path).context("打开 SACD ISO 文件失败")?;
        let source = crate::sacd::SacdNativeSource::open(iso_path, virtual_path)?;
        let format = DirectDsdFormat {
            bit_rate: source.sample_rate,
            channels: source.channels,
            bit_order: DirectDsdBitOrder::MsbFirst,
            container: DirectDsdContainer::Dsf,
        };
        Ok(Self {
            file: dummy_file,
            format,
            layout: DirectDsdLayout::Sacd { source },
            input: Vec::new(),
            max_output_len: 65536,
        })
    }

    pub fn format(&self) -> DirectDsdFormat {
        self.format
    }

    pub fn max_output_len(&self) -> usize {
        self.max_output_len
    }

    /// 返回文件内声明的 Native DSD 时长，单位秒。
    pub fn duration_secs(&self) -> f64 {
        match &self.layout {
            DirectDsdLayout::Dsf {
                total_per_channel, ..
            } => {
                let bits_per_channel = (*total_per_channel as u64).saturating_mul(8);
                bits_per_channel as f64 / f64::from(self.format.bit_rate)
            }
            DirectDsdLayout::Dff { data_size, .. } => {
                let channels = u64::from(self.format.channels).max(1);
                let bits_per_channel = (u64::try_from(*data_size)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(8))
                    / channels;
                bits_per_channel as f64 / f64::from(self.format.bit_rate)
            }
            DirectDsdLayout::Sacd { source } => source.duration_secs,
        }
    }

    pub fn seek_seconds(&mut self, position_secs: f64) -> Result<f64> {
        ensure!(
            position_secs.is_finite() && position_secs >= 0.0,
            "Native DSD seek 位置无效"
        );
        let bit_rate = u64::from(self.format.bit_rate);
        let channels = usize::from(self.format.channels);
        ensure!(bit_rate > 0 && channels > 0, "Native DSD format 无效");
        let requested_bits = (position_secs * bit_rate as f64)
            .floor()
            .min(u64::MAX as f64) as u64;
        let aligned_bits = requested_bits / 32 * 32;

        let actual_bits = match &mut self.layout {
            DirectDsdLayout::Dsf {
                block_size,
                data_offset,
                data_size,
                total_per_channel,
                remaining_per_channel,
                data_remaining,
                skip_per_channel,
            } => {
                let requested_bytes = usize::try_from(aligned_bits / 8)?;
                let target_bytes = requested_bytes.min(*total_per_channel);
                let block_index = target_bytes / *block_size;
                let block_skip = target_bytes % *block_size;
                ensure!(block_skip % 4 == 0, "DSF seek 未按 DSD_SIZ_32 对齐");
                let stored_offset = block_index
                    .checked_mul(*block_size)
                    .and_then(|bytes| bytes.checked_mul(channels))
                    .context("DSF seek offset 溢出")?;
                ensure!(stored_offset <= *data_size, "DSF seek 超出 data chunk");
                self.file.seek(SeekFrom::Start(
                    data_offset
                        .checked_add(u64::try_from(stored_offset)?)
                        .context("DSF seek file offset 溢出")?,
                ))?;
                *remaining_per_channel = *total_per_channel - target_bytes;
                *data_remaining = *data_size - stored_offset;
                *skip_per_channel = if *remaining_per_channel == 0 {
                    0
                } else {
                    block_skip
                };
                u64::try_from(target_bytes)?.saturating_mul(8)
            }
            DirectDsdLayout::Dff {
                data_offset,
                data_size,
                remaining,
            } => {
                let unit = channels.checked_mul(4).context("DFF seek unit 溢出")?;
                let requested_groups = usize::try_from(aligned_bits / 32)?;
                let max_groups = *data_size / unit;
                let groups = requested_groups.min(max_groups);
                let byte_offset = groups.checked_mul(unit).context("DFF seek offset 溢出")?;
                self.file.seek(SeekFrom::Start(
                    data_offset
                        .checked_add(u64::try_from(byte_offset)?)
                        .context("DFF seek file offset 溢出")?,
                ))?;
                *remaining = *data_size - byte_offset;
                u64::try_from(groups)?.saturating_mul(32)
            }
            DirectDsdLayout::Sacd { source } => {
                source.seek_secs(position_secs)?;
                return Ok(position_secs);
            }
        };

        Ok(actual_bits as f64 / bit_rate as f64)
    }

    pub fn read_block(&mut self, output: &mut [u8]) -> Result<Option<usize>> {
        match &mut self.layout {
            DirectDsdLayout::Dsf {
                block_size,
                remaining_per_channel,
                data_remaining,
                skip_per_channel,
                ..
            } => {
                if *remaining_per_channel == 0 {
                    return Ok(None);
                }
                let channels = usize::from(self.format.channels);
                ensure!(
                    *skip_per_channel < *block_size,
                    "DSF seek block offset 无效"
                );
                let valid_per_channel =
                    (*remaining_per_channel).min(*block_size - *skip_per_channel);
                ensure!(
                    valid_per_channel % 4 == 0,
                    "DSF 尾部无法无损对齐 Diretta DSD_SIZ_32"
                );
                let stored_len = block_size
                    .checked_mul(channels)
                    .context("DSF block 长度溢出")?;
                ensure!(
                    *data_remaining >= stored_len,
                    "DSF data chunk 少于声明的 channel block"
                );
                ensure!(self.input.len() >= stored_len, "DSF input buffer 太小");
                let output_len = valid_per_channel
                    .checked_mul(channels)
                    .context("DSF output 长度溢出")?;
                ensure!(output.len() >= output_len, "DSF output buffer 太小");

                self.file
                    .read_exact(&mut self.input[..stored_len])
                    .context("读取 DSF native payload 失败")?;
                repack_dsf_l4r4(
                    &self.input[..stored_len],
                    output,
                    channels,
                    *block_size,
                    *skip_per_channel,
                    valid_per_channel,
                )?;
                *remaining_per_channel -= valid_per_channel;
                *data_remaining -= stored_len;
                *skip_per_channel = 0;
                Ok(Some(output_len))
            }
            DirectDsdLayout::Dff { remaining, .. } => {
                if *remaining == 0 {
                    return Ok(None);
                }
                let channels = usize::from(self.format.channels);
                let unit = channels.checked_mul(4).context("DFF frame unit 溢出")?;
                let mut read_len = (*remaining).min(self.input.len());
                if read_len < *remaining {
                    read_len -= read_len % unit;
                }
                ensure!(
                    read_len > 0 && read_len % unit == 0,
                    "DFF payload 无法对齐 DSD_SIZ_32"
                );
                ensure!(output.len() >= read_len, "DFF output buffer 太小");

                self.file
                    .read_exact(&mut self.input[..read_len])
                    .context("读取 DFF native payload 失败")?;
                repack_dff_l4r4(&self.input[..read_len], &mut output[..read_len], channels)?;
                *remaining -= read_len;
                Ok(Some(read_len))
            }
            DirectDsdLayout::Sacd { source } => {
                let block = source.next_block()?;
                if let Some(buf) = block {
                    let len = buf.len();
                    ensure!(output.len() >= len, "SACD output buffer 太小");
                    output[..len].copy_from_slice(&buf);
                    Ok(Some(len))
                } else {
                    Ok(None)
                }
            }
        }
    }

    fn open_dsf(mut file: File, file_len: usize) -> Result<Self> {
        let mut header = [0_u8; 28];
        file.read_exact(&mut header)
            .context("读取 DSF DSD chunk 失败")?;
        ensure!(&header[..4] == b"DSD ", "DSF DSD chunk magic 无效");
        ensure!(read_le_u64(&header[4..12]) >= 28, "DSF DSD chunk size 无效");

        let mut fmt_header = [0_u8; 12];
        file.read_exact(&mut fmt_header)
            .context("读取 DSF fmt chunk 失败")?;
        ensure!(&fmt_header[..4] == b"fmt ", "DSF fmt chunk 缺失");
        let fmt_size =
            usize::try_from(read_le_u64(&fmt_header[4..12])).context("DSF fmt size 越界")?;
        ensure!(fmt_size >= 52, "DSF fmt chunk 太短");
        let mut fmt_payload = vec![0_u8; fmt_size - 12];
        file.read_exact(&mut fmt_payload)
            .context("读取 DSF fmt payload 失败")?;
        ensure!(fmt_payload.len() >= 40, "DSF fmt payload 太短");

        let format_id = read_le_u32(&fmt_payload[4..8]);
        ensure!(format_id == 0, "DSF 非 raw DSD format_id 不支持");
        let channels = read_le_u32(&fmt_payload[12..16]);
        ensure!(
            channels > 0 && channels <= 16,
            "DSF 声道数不支持: {channels}"
        );
        let bit_rate = read_le_u32(&fmt_payload[16..20]);
        ensure!(bit_rate > 0, "DSF DSD bit rate 无效");
        let bits_per_sample = read_le_u32(&fmt_payload[20..24]);
        let bit_order = match bits_per_sample {
            1 => DirectDsdBitOrder::LsbFirst,
            8 => DirectDsdBitOrder::MsbFirst,
            value => bail!("DSF bits_per_sample 不支持: {value}"),
        };
        let sample_count =
            usize::try_from(read_le_u64(&fmt_payload[24..32])).context("DSF sample count 越界")?;
        let block_size =
            usize::try_from(read_le_u32(&fmt_payload[32..36])).context("DSF block size 越界")?;
        ensure!(
            block_size > 0 && block_size % 4 == 0,
            "DSF block size 必须为 4 字节倍数"
        );
        ensure!(
            sample_count % 32 == 0,
            "DSF sample count 无法无损对齐 Diretta DSD_SIZ_32"
        );
        let bytes_per_channel = sample_count / 8;

        let (data_offset, data_size) = find_dsf_data_chunk(&mut file, file_len)?;
        let blocks = bytes_per_channel
            .checked_add(block_size - 1)
            .context("DSF block count 溢出")?
            / block_size;
        let stored_required = blocks
            .checked_mul(block_size)
            .and_then(|bytes| bytes.checked_mul(usize::try_from(channels).ok()?))
            .context("DSF stored payload 长度溢出")?;
        ensure!(
            data_size >= stored_required,
            "DSF data chunk 不足以容纳声明的 samples"
        );
        file.seek(SeekFrom::Start(u64::try_from(data_offset)?))?;

        let channels_usize = usize::try_from(channels)?;
        let input_len = block_size
            .checked_mul(channels_usize)
            .context("DSF input block 长度溢出")?;
        Ok(Self {
            file,
            format: DirectDsdFormat {
                bit_rate,
                channels: u16::try_from(channels)?,
                bit_order,
                container: DirectDsdContainer::Dsf,
            },
            layout: DirectDsdLayout::Dsf {
                block_size,
                data_offset: u64::try_from(data_offset)?,
                data_size,
                total_per_channel: bytes_per_channel,
                remaining_per_channel: bytes_per_channel,
                data_remaining: data_size,
                skip_per_channel: 0,
            },
            input: vec![0; input_len],
            max_output_len: input_len,
        })
    }

    fn open_dff(mut file: File, file_len: usize) -> Result<Self> {
        let mut form = [0_u8; 16];
        file.read_exact(&mut form).context("读取 DFF FRM8 失败")?;
        ensure!(
            &form[..4] == b"FRM8" && &form[12..16] == b"DSD ",
            "DFF FORM 类型无效"
        );

        let mut bit_rate = 0_u32;
        let mut channels = 0_u16;
        let mut compression_is_raw = false;
        let mut data_offset = None;
        let mut data_size = 0_usize;

        while usize::try_from(file.stream_position()?)? + 12 <= file_len {
            let mut chunk_header = [0_u8; 12];
            file.read_exact(&mut chunk_header)?;
            let chunk_size = usize::try_from(read_be_u64(&chunk_header[4..12]))
                .context("DFF chunk size 越界")?;
            let payload_offset = usize::try_from(file.stream_position()?)?;
            ensure!(payload_offset + chunk_size <= file_len, "DFF chunk 越界");

            match &chunk_header[..4] {
                b"PROP" => {
                    let mut payload = vec![0_u8; chunk_size];
                    file.read_exact(&mut payload)?;
                    let props = parse_dff_properties(&payload)?;
                    if let Some(value) = props.bit_rate {
                        bit_rate = value;
                    }
                    if let Some(value) = props.channels {
                        channels = value;
                    }
                    if let Some(raw) = props.raw_dsd {
                        compression_is_raw = raw;
                    }
                }
                b"DSD " => {
                    data_offset = Some(payload_offset);
                    data_size = chunk_size;
                    break;
                }
                _ => {
                    file.seek(SeekFrom::Current(i64::try_from(chunk_size)?))?;
                }
            }
            if chunk_size % 2 != 0 {
                file.seek(SeekFrom::Current(1))?;
            }
        }

        ensure!(bit_rate > 0, "DFF 缺少 FS sample rate");
        ensure!(
            channels > 0 && channels <= 16,
            "DFF 声道数不支持: {channels}"
        );
        ensure!(
            compression_is_raw,
            "DFF 仅支持未压缩 native DSD，DST/其他压缩显式拒绝"
        );
        let data_offset = data_offset.context("DFF 缺少 DSD data chunk")?;
        let unit = usize::from(channels)
            .checked_mul(4)
            .context("DFF DSD_SIZ_32 unit 溢出")?;
        ensure!(
            data_size > 0 && data_size % unit == 0,
            "DFF data 无法无损对齐 Diretta DSD_SIZ_32"
        );
        file.seek(SeekFrom::Start(u64::try_from(data_offset)?))?;

        let input_len = 32 * 1024 / unit * unit;
        ensure!(input_len > 0, "DFF input block size 无效");
        Ok(Self {
            file,
            format: DirectDsdFormat {
                bit_rate,
                channels,
                bit_order: DirectDsdBitOrder::MsbFirst,
                container: DirectDsdContainer::Dff,
            },
            layout: DirectDsdLayout::Dff {
                data_offset: u64::try_from(data_offset)?,
                data_size,
                remaining: data_size,
            },
            input: vec![0; input_len],
            max_output_len: input_len,
        })
    }
}

struct DffProperties {
    bit_rate: Option<u32>,
    channels: Option<u16>,
    raw_dsd: Option<bool>,
}

fn parse_dff_properties(payload: &[u8]) -> Result<DffProperties> {
    ensure!(
        payload.len() >= 4 && &payload[..4] == b"SND ",
        "DFF PROP 不是 SND"
    );
    let mut offset = 4_usize;
    let mut result = DffProperties {
        bit_rate: None,
        channels: None,
        raw_dsd: None,
    };
    while offset + 12 <= payload.len() {
        let id = &payload[offset..offset + 4];
        let size = usize::try_from(read_be_u64(&payload[offset + 4..offset + 12]))
            .context("DFF PROP subchunk size 越界")?;
        let start = offset + 12;
        let end = start.checked_add(size).context("DFF PROP subchunk 溢出")?;
        ensure!(end <= payload.len(), "DFF PROP subchunk 越界");
        match id {
            b"FS  " if size >= 4 => result.bit_rate = Some(read_be_u32(&payload[start..start + 4])),
            b"CHNL" if size >= 2 => {
                result.channels = Some(u16::from_be_bytes([payload[start], payload[start + 1]]));
            }
            b"CMPR" if size >= 4 => result.raw_dsd = Some(&payload[start..start + 4] == b"DSD "),
            _ => {}
        }
        offset = end + (size % 2);
    }
    Ok(result)
}

fn find_dsf_data_chunk(file: &mut File, file_len: usize) -> Result<(usize, usize)> {
    loop {
        let offset = usize::try_from(file.stream_position()?)?;
        ensure!(offset + 12 <= file_len, "DSF 未找到 data chunk");
        let mut header = [0_u8; 12];
        file.read_exact(&mut header)?;
        let chunk_size =
            usize::try_from(read_le_u64(&header[4..12])).context("DSF chunk size 越界")?;
        ensure!(chunk_size >= 12, "DSF chunk size 无效");
        let payload_len = chunk_size - 12;
        let payload_offset = usize::try_from(file.stream_position()?)?;
        ensure!(payload_offset + payload_len <= file_len, "DSF chunk 越界");
        if &header[..4] == b"data" {
            return Ok((payload_offset, payload_len));
        }
        file.seek(SeekFrom::Current(i64::try_from(payload_len)?))?;
    }
}

fn repack_dsf_l4r4(
    input: &[u8],
    output: &mut [u8],
    channels: usize,
    block_size: usize,
    start_per_channel: usize,
    valid_per_channel: usize,
) -> Result<()> {
    ensure!(
        channels > 0
            && start_per_channel < block_size
            && start_per_channel % 4 == 0
            && valid_per_channel % 4 == 0,
        "DSF repack 参数无效"
    );
    ensure!(
        input.len() >= block_size * channels,
        "DSF repack input 太小"
    );
    ensure!(
        output.len() >= valid_per_channel * channels,
        "DSF repack output 太小"
    );
    let mut dst = 0_usize;
    for relative_offset in (0..valid_per_channel).step_by(4) {
        let byte_offset = start_per_channel + relative_offset;
        for channel in 0..channels {
            let src = channel * block_size + byte_offset;
            output[dst..dst + 4].copy_from_slice(&input[src..src + 4]);
            dst += 4;
        }
    }
    Ok(())
}

fn repack_dff_l4r4(input: &[u8], output: &mut [u8], channels: usize) -> Result<()> {
    ensure!(channels > 0, "DFF repack 声道数无效");
    let unit = channels.checked_mul(4).context("DFF repack unit 溢出")?;
    ensure!(
        input.len() % unit == 0,
        "DFF repack input 未按 DSD_SIZ_32 对齐"
    );
    ensure!(output.len() >= input.len(), "DFF repack output 太小");
    let groups = input.len() / unit;
    let mut dst = 0_usize;
    for group in 0..groups {
        let base = group * unit;
        for channel in 0..channels {
            for byte in 0..4 {
                output[dst] = input[base + byte * channels + channel];
                dst += 1;
            }
        }
    }
    Ok(())
}

fn adapt_dsd_bit_order(
    payload: &mut [u8],
    source_bit_order: DirectDsdBitOrder,
    wire_bit_order: DirectDsdBitOrder,
) {
    if source_bit_order == wire_bit_order {
        return;
    }
    for byte in payload {
        *byte = byte.reverse_bits();
    }
}

/// ring 预缓冲目标时长（毫秒）：整环深度按此换算。CIFS 等网络文件系统单次
/// 读延迟毛刺实测可达 ~70ms，固定 8 槽（DSD64 DSF 仅 ~93ms）在毛刺集中时被
/// 打穿，SDK 拉到静音块与真实数据交替——即 Diretta DSD 播放的哒哒杂音
const DIRECT_DSD_TARGET_BUFFERED_MS: f64 = 400.0;
const DIRECT_DSD_RING_DEPTH_MIN: usize = 8;
/// 深度上限约束内存：DSD512 DSF 单槽 8KB，256 槽 = 2MB
const DIRECT_DSD_RING_DEPTH_MAX: usize = 256;

/// 槽位容量是各声道合计字节数：换算回单声道比特数后除以 DSD 比特率得到单槽
/// 时长，整环深度取「目标时长 / 单槽时长」向上取整并夹在上下限内。换源
/// handoff 要求 same_dsd_transport（同比特率），深度在连接生命周期内保持有效
fn ring_depth_for(format: DirectDsdFormat, slot_capacity: usize) -> usize {
    let channels = usize::from(format.channels.max(1));
    let bits_per_channel = (slot_capacity / channels).saturating_mul(8);
    if format.bit_rate == 0 || bits_per_channel == 0 {
        return DIRECT_DSD_RING_DEPTH_MIN;
    }
    let slot_ms = bits_per_channel as f64 * 1000.0 / f64::from(format.bit_rate);
    ((DIRECT_DSD_TARGET_BUFFERED_MS / slot_ms).ceil() as usize)
        .clamp(DIRECT_DSD_RING_DEPTH_MIN, DIRECT_DSD_RING_DEPTH_MAX)
}
/// pre-mute 静音窗口时长：覆盖换源/seek 复位的供数空窗（对齐 tinyLMS DSD 20 cycles）
const PRE_MUTE_WINDOW_MS: u64 = 80;

/// 进程内单调毫秒时钟（pre-mute 窗口用，不受系统墙钟跳变影响）
fn mono_millis() -> u64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}
const SLOT_FREE: u8 = 0;
const SLOT_FILLING: u8 = 1;
const SLOT_READY: u8 = 2;
const SLOT_IN_FLIGHT: u8 = 3;
const NO_SLOT: usize = usize::MAX;
/// producer 条件等待上限：所有关键事件（slot 释放/命令/状态翻转）都有 notify，
/// 超时仅作为漏报兜底；到期后回到循环顶保持命令响应性
const PRODUCER_WAIT_CEILING: Duration = Duration::from_millis(100);

/// 无缝边界空窗的 consumer 等待上限：换真数据而非立即注入静音垫。
/// 10ms 远小于 Target 端缓冲（实测 buffer_us=100000），不会造成设备欠载
const DSD_BOUNDARY_GAP_WAIT: Duration = Duration::from_millis(10);

struct DirectDsdSlot {
    state: AtomicU8,
    payload_ptr: AtomicPtr<u8>,
    payload_len: AtomicUsize,
    bits_per_channel: AtomicU64,
    boundary: AtomicBool,
    boundary_duration_micros: AtomicU64,
    boundary_generation: AtomicU64,
    buffer: UnsafeCell<Box<[u8]>>,
}

impl DirectDsdSlot {
    fn new(capacity: usize) -> Self {
        Self {
            state: AtomicU8::new(SLOT_FREE),
            payload_ptr: AtomicPtr::new(ptr::null_mut()),
            payload_len: AtomicUsize::new(0),
            bits_per_channel: AtomicU64::new(0),
            boundary: AtomicBool::new(false),
            boundary_duration_micros: AtomicU64::new(0),
            boundary_generation: AtomicU64::new(0),
            // 注意：DSD 缓冲区初始值必须为 0x69（DSD 专用静音字节），
            // 而非 0x00（0x00 会使 DSD DAC 输出满幅直流偏置，造成爆音）。
            buffer: UnsafeCell::new(vec![DSD_SILENCE_BYTE; capacity].into_boxed_slice()),
        }
    }
}

// buffer 仅在 FILLING 时由 producer 写入，在 READY/IN_FLIGHT 时只读。
unsafe impl Sync for DirectDsdSlot {}

enum DirectDsdCommand {
    SetWireBitOrder {
        bit_order: DirectDsdBitOrder,
        position_secs: f64,
        response: mpsc::SyncSender<Result<f64>>,
    },
    Seek {
        position_secs: f64,
        response: mpsc::SyncSender<Result<f64>>,
    },
    ReplaceLocal {
        source: String,
        start_secs: f64,
        response: mpsc::SyncSender<Result<DirectDsdFormat>>,
    },
    StageLocal {
        path: PathBuf,
        start_secs: f64,
        duration_micros: u64,
        generation: u64,
        response: mpsc::SyncSender<Result<()>>,
    },
    CancelStaged,
}

struct DirectDsdRing {
    slots: Box<[DirectDsdSlot]>,
    consumer_index: AtomicUsize,
    in_flight: AtomicUsize,
    consumed_bits_per_channel: AtomicU64,
    duration_micros: AtomicU64,
    transition_count: AtomicU64,
    boundary_generation: AtomicU64,
    finished: AtomicBool,
    failed: AtomicBool,
    stopped: AtomicBool,
    /// 单一状态信号：producer 与控制线程共享的条件等待通道（避免任何忙等/轮询）
    signal: Mutex<()>,
    signal_cv: Condvar,
    /// 控制通道存在待处理命令的提示位：命令发送方置位并唤醒，producer 消费命令前清零
    command_pending: AtomicBool,
    /// staged 候选已接受且尚未装填进 ring：曲终判定必须避开此窗口
    /// （stage 迟到时 producer 在 EOF 处等待，装填后 finished 复位）
    stage_pending: AtomicBool,
    /// stage 准备代数：CancelStaged/ReplaceLocal 推进，使在途后台 prepare
    /// 的结果过期（prepare 在独立线程执行，完成时代数可能已变）
    stage_epoch: AtomicU64,
    /// 后台 prepare 已产出 Ready 结果待 producer 收取（finished 分支等待的
    /// 唤醒提示位；producer 收空通道后复位）
    staged_ready: AtomicBool,
    /// pre-mute 静音窗口截止（monotonic 毫秒，0 = 未触发）：换源/seek 复位前触发，
    /// 窗口内 SDK 拉取遇供数空窗时交付静音块，消除 Target 欠载杂音
    pre_mute_until_ms: AtomicU64,
    /// pre-mute 静音缓冲（0x69）：仅 SDK 回调线程（consumer）读写，producer 不触碰
    pre_mute_buf: Mutex<Vec<u8>>,
    /// 最近一次交付块的长度（pre-mute 静音块的几何依据）
    last_delivered_len: AtomicUsize,
    /// 换源/拆线前排空请求：置位后回调侧把每个交付块替换为 0x69 静音并累计
    /// 静音垫（DSD 位流不可乘增益，无淡出通道；排空 = 静音顶掉设备端缓冲）。
    /// replace/seek 复位时清除
    drain_requested: AtomicBool,
    /// 排空期间已交付的静音块数
    silence_blocks: AtomicU32,
    /// 排空期间已交付静音的累计时长（微秒）：块尺寸波动大，时长时间为主谓词
    silence_micros: AtomicU64,
    /// 排空目标时长（µs）：建连后由上层注入（Sink 实测延迟/自报缓冲 + 余量）；
    /// 0 = 未注入，谓词退回旧常量 DIRECT_FADE_DRAIN_MIN_MICROS（默认行为可回退）
    drain_target_micros: AtomicU64,
    /// 静音字节：以 SDK getMuteByte() 返回为准；0 = 未注入（退回 0x69 硬编码）
    mute_byte: AtomicU8,
    /// DSD 比特率（Hz）：静音垫时长换算用
    bit_rate: u32,
}

#[derive(Clone, Copy)]
pub struct DirectDsdBlock {
    pub data: *const u8,
    pub len: usize,
}

impl DirectDsdRing {
    fn new(capacity: usize, format: DirectDsdFormat) -> Self {
        let slots = (0..ring_depth_for(format, capacity))
            .map(|_| DirectDsdSlot::new(capacity))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            slots,
            consumer_index: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(NO_SLOT),
            consumed_bits_per_channel: AtomicU64::new(0),
            duration_micros: AtomicU64::new(0),
            transition_count: AtomicU64::new(0),
            boundary_generation: AtomicU64::new(0),
            finished: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            signal: Mutex::new(()),
            signal_cv: Condvar::new(),
            command_pending: AtomicBool::new(false),
            stage_pending: AtomicBool::new(false),
            stage_epoch: AtomicU64::new(0),
            staged_ready: AtomicBool::new(false),
            pre_mute_until_ms: AtomicU64::new(0),
            pre_mute_buf: Mutex::new(Vec::new()),
            last_delivered_len: AtomicUsize::new(0),
            drain_requested: AtomicBool::new(false),
            silence_blocks: AtomicU32::new(0),
            silence_micros: AtomicU64::new(0),
            // 0 = 未注入：drain 谓词/mute 字节退回旧常量行为（与改动前等价，可回退）
            drain_target_micros: AtomicU64::new(0),
            mute_byte: AtomicU8::new(0),
            bit_rate: format.bit_rate,
        }
    }

    /// 状态变化通知：取一次锁再释放后 notify，保证不会丢失在等待方进入之前。
    /// 供 SDK 回调线程调用：仅一次短暂锁 + futex wake，无分配，实时安全。
    fn notify_state(&self) {
        let _guard = self
            .signal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.signal_cv.notify_all();
    }

    /// 命令发送侧：置提示位并唤醒 producer（其可能正处于条件等待中）
    fn signal_command(&self) {
        self.command_pending.store(true, Ordering::Release);
        self.notify_state();
    }

    /// 条件等待：谓词为真立即返回 true；deadline 内未满足返回 false（超时兜底）。
    /// 谓词只依赖 ring 自身状态；等待方返回后应回到命令循环保持命令响应性。
    fn wait_for(&self, predicate: impl Fn(&Self) -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut guard = self
            .signal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if predicate(self) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (next, _) = self
                .signal_cv
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard = next;
        }
    }

    fn next_block(&self) -> Option<DirectDsdBlock> {
        self.release_in_flight();
        if self.failed.load(Ordering::Acquire) {
            return None;
        }
        // pre-mute 窗口（换源/seek 复位触发）：SDK 拉取遇供数空窗时交付 0x69
        // 静音块而非"无块"，消除 Target 欠载杂音；不推进 consumed/boundary 会计。
        // 真实数据优先：候选 slot 已就绪时不让静音块抢占——换源排空垫已在
        // replace 前顶掉 Target 缓冲，新曲就绪即应立即开始，无需额外静音窗口
        let index = self.consumer_index.load(Ordering::Relaxed);
        let slot = &self.slots[index];
        let mut slot_ready = slot.state.load(Ordering::Acquire) == SLOT_READY;
        // 无缝边界空窗：finished/stage_pending 置位说明 producer 正在
        // EOF→装填 staged 候选的临界区（stage 已 prepare 完成时为 µs 级）。
        // 短暂事件等待换真数据，避免 0x69 静音垫被夹进无缝边界（staged
        // 边界无排空语义，静音=可闻断裂；此即边界单测偶发失败的残留窗口）。
        // pre-mute 窗口内（换源排空垫）保持既有静音语义；失败/超时回退静音垫。
        // 等待上限 10ms 远小于 Target 缓冲（实测 buffer_us=100000），不致欠载
        if !slot_ready
            && !self.pre_mute_active()
            && (self.finished.load(Ordering::Acquire) || self.stage_pending.load(Ordering::Acquire))
        {
            self.wait_for(
                |ring| {
                    let idx = ring.consumer_index.load(Ordering::Relaxed);
                    ring.slots[idx].state.load(Ordering::Acquire) == SLOT_READY
                        || ring.failed.load(Ordering::Acquire)
                        || (!ring.finished.load(Ordering::Acquire)
                            && !ring.stage_pending.load(Ordering::Acquire))
                },
                DSD_BOUNDARY_GAP_WAIT,
            );
            slot_ready = slot.state.load(Ordering::Acquire) == SLOT_READY;
        }
        if self.pre_mute_active() && !slot_ready {
            if let Some(block) = self.pre_mute_block() {
                return Some(block);
            }
        }
        if slot
            .state
            .compare_exchange(
                SLOT_READY,
                SLOT_IN_FLIGHT,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            // SDK 契约:getNewStream 返回 false 会终止发送线程(之后永不拉流,
            // 表现为消费冻结)。供数空窗时交付 0x69 静音块保持块时钟,
            // producer 填好槽后自动恢复真数据
            if let Some(block) = self.pre_mute_block() {
                return Some(block);
            }
            return None;
        }
        let data = slot.payload_ptr.load(Ordering::Relaxed).cast_const();
        let len = slot.payload_len.load(Ordering::Relaxed);
        if data.is_null() || len == 0 {
            slot.state.store(SLOT_FREE, Ordering::Release);
            self.failed.store(true, Ordering::Release);
            return None;
        }
        if slot.boundary.swap(false, Ordering::AcqRel) {
            self.consumed_bits_per_channel.store(0, Ordering::Release);
            self.duration_micros.store(
                slot.boundary_duration_micros.load(Ordering::Relaxed),
                Ordering::Release,
            );
            self.boundary_generation.store(
                slot.boundary_generation.load(Ordering::Relaxed),
                Ordering::Release,
            );
            self.transition_count.fetch_add(1, Ordering::AcqRel);
        }
        self.in_flight.store(index, Ordering::Release);
        self.consumer_index
            .store((index + 1) % self.slots.len(), Ordering::Relaxed);
        self.last_delivered_len.store(len, Ordering::Relaxed);
        Some(DirectDsdBlock { data, len })
    }

    /// 触发 pre-mute 静音窗口（换源/seek 复位前调用）：窗口内 SDK 拉取遇
    /// 供数空窗时交付 0x69 静音块而非"无块"，消除 Target 侧欠载杂音。
    /// 缓冲预分配在本函数（producer/控制线程上下文）完成，
    /// SDK 回调线程内只读不分配（R1：回调路径禁止堆分配）
    fn trigger_pre_mute(&self) {
        let len = self.last_delivered_len.load(Ordering::Acquire);
        if len > 0 {
            let mut buf = self.pre_mute_buf.lock().unwrap_or_else(|p| p.into_inner());
            if buf.len() < len {
                buf.resize(len, self.silence_byte());
            }
        }
        self.pre_mute_until_ms
            .store(mono_millis() + PRE_MUTE_WINDOW_MS, Ordering::Release);
    }

    /// 静音字节：SDK 注入值优先，未注入（0）时退回 0x69 硬编码（PDM 直流均衡）
    fn silence_byte(&self) -> u8 {
        match self.mute_byte.load(Ordering::Acquire) {
            0 => DSD_SILENCE_BYTE,
            byte => byte,
        }
    }

    fn pre_mute_active(&self) -> bool {
        self.pre_mute_until_ms.load(Ordering::Acquire) > mono_millis()
    }

    /// 供数空窗静音块：静音字节（PDM 直流均衡），几何沿用最近一次交付块
    fn pre_mute_block(&self) -> Option<DirectDsdBlock> {
        let len = self.last_delivered_len.load(Ordering::Acquire);
        if len == 0 {
            return None;
        }
        let mut buf = self.pre_mute_buf.lock().unwrap_or_else(|p| p.into_inner());
        if buf.len() < len {
            // DSD 静音必须为直流均衡字节（0x69 或 SDK mute_byte），防止满幅直流偏置爆音；
            // 正常路径已在 trigger_pre_mute 预分配，此分支仅作防御
            buf.resize(len, self.silence_byte());
        }
        Some(DirectDsdBlock {
            data: buf.as_ptr(),
            len,
        })
    }

    /// 请求换源/拆线前排空：此后每个交付块在回调侧替换为 0x69 静音
    /// （DSD 位流不可乘增益，无淡出通道——排空以静音垫置换设备端缓冲）。
    /// 由控制线程调用，回调线程经原子位观察
    fn begin_drain(&self) {
        self.drain_requested.store(true, Ordering::Release);
    }

    /// 清除排空请求（Phase3 软暂停配套）：恢复播放前必须调用——
    /// 静音垫请求不清除则恢复后所有交付块持续为 0x69（永久静音，正确性关键）
    fn clear_drain(&self) {
        self.drain_requested.store(false, Ordering::Release);
        self.silence_blocks.store(0, Ordering::Relaxed);
        self.silence_micros.store(0, Ordering::Relaxed);
    }

    /// 软暂停就绪谓词：排空请求已置位且至少交付一个 0x69 静音块
    ///（停发时 Target 端新到块为零电平）
    fn soft_pause_ready(&self) -> bool {
        self.drain_requested.load(Ordering::Acquire)
            && self.silence_blocks.load(Ordering::Acquire) >= 1
    }

    /// SDK 回调内对即将交付的块原位应用排空静音（与 PCM apply_fade 对称：
    /// 块 IN_FLIGHT 期仅回调可写，写入安全）。真实旧数据块同样被替换——
    /// 排空期间旧曲立即静音，静音垫按交付节奏持续顶掉 Target 端缓冲
    fn apply_drain(&self, data: *mut u8, len: usize) {
        if !self.drain_requested.load(Ordering::Acquire) {
            return;
        }
        // R1：SDK 实时回调线程内严禁 lock/notify/堆分配 —— 只做原子 store，
        // 等待方改为短周期轮询（见 DirectDsdMonitor::wait_fade_drained）
        unsafe { ptr::write_bytes(data, self.silence_byte(), len) };
        self.silence_blocks.fetch_add(1, Ordering::Release);
        if self.bit_rate > 0 {
            // bytes × 8 bit/byte × 1e6 µs/s ÷ bit/s = µs
            self.silence_micros.fetch_add(
                len as u64 * 8_000_000 / u64::from(self.bit_rate),
                Ordering::Release,
            );
        }
    }

    /// 排空目标时长（µs）：上层未注入（0）时退回旧常量，保证默认行为可回退
    fn drain_target_micros(&self) -> u64 {
        match self.drain_target_micros.load(Ordering::Acquire) {
            0 => crate::direct_runtime::DIRECT_FADE_DRAIN_MIN_MICROS,
            micros => micros,
        }
    }

    /// 排空完成谓词：未请求排空视为已完成；已请求则要求静音垫同时达到
    /// 时长时间下限（主谓词）与块数下限（兜底）——原 OR 逻辑导致 200ms
    /// 时长下限永不生效（块数先到），静音垫不足 Sink 缓冲即切歌产生爆音
    fn drain_complete(&self, min_blocks: u32) -> bool {
        if !self.drain_requested.load(Ordering::Acquire) {
            return true;
        }
        // AND：块数为下限兜底，时长时间为主谓词
        if self.silence_blocks.load(Ordering::Acquire) < min_blocks {
            return false;
        }
        self.bit_rate == 0 || self.silence_micros.load(Ordering::Acquire) >= self.drain_target_micros()
    }

    fn release_in_flight(&self) {
        let index = self.in_flight.swap(NO_SLOT, Ordering::AcqRel);
        if index != NO_SLOT {
            let slot = &self.slots[index];
            let bits = slot.bits_per_channel.swap(0, Ordering::Relaxed);
            self.consumed_bits_per_channel
                .fetch_add(bits, Ordering::Relaxed);
            slot.state.store(SLOT_FREE, Ordering::Release);
            // slot 释放：唤醒等待空闲 slot 的 producer
            self.notify_state();
        }
    }

    fn reset_for_transition(&self) {
        self.release_in_flight();
        self.consumer_index.store(0, Ordering::Relaxed);
        self.in_flight.store(NO_SLOT, Ordering::Relaxed);
        self.consumed_bits_per_channel.store(0, Ordering::Relaxed);
        self.finished.store(false, Ordering::Relaxed);
        self.failed.store(false, Ordering::Relaxed);
        // 换源后新曲从正常供数开始：清除排空请求与静音垫计数
        self.drain_requested.store(false, Ordering::Relaxed);
        self.silence_blocks.store(0, Ordering::Relaxed);
        self.silence_micros.store(0, Ordering::Relaxed);
        for slot in &self.slots {
            slot.state.store(SLOT_FREE, Ordering::Relaxed);
            slot.payload_ptr.store(ptr::null_mut(), Ordering::Relaxed);
            slot.payload_len.store(0, Ordering::Relaxed);
            slot.bits_per_channel.store(0, Ordering::Relaxed);
            slot.boundary.store(false, Ordering::Relaxed);
            slot.boundary_duration_micros.store(0, Ordering::Relaxed);
            slot.boundary_generation.store(0, Ordering::Relaxed);
        }
        // finished/failed 已清除：唤醒 producer
        self.notify_state();
    }

    fn ensure_capacity(&self, capacity: usize) {
        for slot in &self.slots {
            let buffer = unsafe { &mut *slot.buffer.get() };
            if buffer.len() < capacity {
                // DSD 缓冲区必须用 0x69（DSD 静音）而非 0x00 初始化，防止扩容时送出爆音帧。
                *buffer = vec![DSD_SILENCE_BYTE; capacity].into_boxed_slice();
            }
        }
    }
}

fn seek_dsd_ring(
    reader: &mut DirectDsdReader,
    ring: &DirectDsdRing,
    expected_format: DirectDsdFormat,
    wire_bit_order: DirectDsdBitOrder,
    position_secs: f64,
) -> Result<f64> {
    // seek 复位同样存在供数空窗：pre-mute 窗口内 SDK 拉取拿到 0x69 静音块而非欠载
    ring.trigger_pre_mute();
    ring.reset_for_transition();
    let actual_position = reader.seek_seconds(position_secs)?;
    ensure!(
        reader.format() == expected_format,
        "Native DSD seek 后音频格式发生变化"
    );
    fill_slot(reader, &ring.slots[0], wire_bit_order)?
        .context("Native DSD seek 后没有可播放 payload")?;
    Ok(actual_position)
}

fn same_dsd_transport(left: DirectDsdFormat, right: DirectDsdFormat) -> bool {
    left.bit_rate == right.bit_rate && left.channels == right.channels
}

struct StagedDsdSource {
    reader: DirectDsdReader,
    format: DirectDsdFormat,
    duration_micros: u64,
    generation: u64,
}

/// 后台 stage prepare 线程的结果。epoch 为 ring 的 stage 代数：
/// CancelStaged/ReplaceLocal 会推进代数，过期结果直接丢弃
enum StagePrepareOutcome {
    Ready(u64, StagedDsdSource),
    Failed(u64),
}

fn prepare_staged_dsd_source(
    path: &Path,
    start_secs: f64,
    current_format: DirectDsdFormat,
    duration_micros: u64,
    generation: u64,
) -> Result<StagedDsdSource> {
    let mut reader = DirectDsdReader::open_local(path)?;
    if start_secs > 0.0 {
        reader.seek_seconds(start_secs)?;
    }
    let format = reader.format();
    let duration_micros = if duration_micros > 0 {
        duration_micros
    } else {
        (reader.duration_secs() * 1_000_000.0)
            .round()
            .clamp(0.0, u64::MAX as f64) as u64
    };
    ensure!(
        same_dsd_transport(current_format, format),
        "[Direct] staged Native DSD wire format 与当前 Diretta connection 不一致"
    );
    Ok(StagedDsdSource {
        reader,
        format,
        duration_micros,
        generation,
    })
}

fn install_staged_dsd_slot(
    mut staged: StagedDsdSource,
    slot: &DirectDsdSlot,
    wire_bit_order: DirectDsdBitOrder,
) -> Result<(DirectDsdReader, DirectDsdFormat)> {
    let required = staged.reader.max_output_len();
    let buffer = unsafe { &mut *slot.buffer.get() };
    if buffer.len() < required {
        // DSD staged 缓冲区扩容：填充 0x69 DSD 静音，防止空闲区间送出爆音帧。
        *buffer = vec![DSD_SILENCE_BYTE; required].into_boxed_slice();
    }
    fill_claimed_slot(&mut staged.reader, slot, wire_bit_order)?
        .context("Native DSD staged 音源没有可播放 payload")?;
    slot.boundary_duration_micros
        .store(staged.duration_micros, Ordering::Relaxed);
    slot.boundary_generation
        .store(staged.generation, Ordering::Relaxed);
    slot.boundary.store(true, Ordering::Relaxed);
    Ok((staged.reader, staged.format))
}

fn replace_dsd_ring(
    source: &str,
    start_secs: f64,
    ring: &DirectDsdRing,
    current_format: DirectDsdFormat,
    wire_bit_order: DirectDsdBitOrder,
) -> Result<(DirectDsdReader, DirectDsdFormat)> {
    let mut reader = DirectDsdReader::open_local(Path::new(source))?;
    if start_secs > 0.0 {
        reader.seek_seconds(start_secs)?;
    }
    let new_format = reader.format();
    ensure!(
        same_dsd_transport(current_format, new_format),
        "[Direct] 新音源 Native DSD wire format 与当前 Diretta connection 不一致"
    );
    let duration_micros = (reader.duration_secs() * 1_000_000.0)
        .round()
        .clamp(0.0, u64::MAX as f64) as u64;
    // 换源复位有供数空窗：pre-mute 窗口内 SDK 拉取拿到 0x69 静音块而非欠载（消除切杂音）
    ring.trigger_pre_mute();
    ring.reset_for_transition();
    ring.ensure_capacity(reader.max_output_len());
    // 对齐 staged 安装语义：boundary 元数据在 slot 发布（READY）前写好，
    // duration/generation 随新源首块被消费时才原子切换——ReplaceLocal 瞬间
    // 不再直接覆盖 duration_micros，时间显示与位置同步翻转不跳变
    let first_slot = &ring.slots[0];
    first_slot
        .boundary_duration_micros
        .store(duration_micros, Ordering::Relaxed);
    first_slot.boundary_generation.store(
        ring.boundary_generation.load(Ordering::Relaxed) + 1,
        Ordering::Relaxed,
    );
    first_slot.boundary.store(true, Ordering::Relaxed);
    fill_slot(&mut reader, first_slot, wire_bit_order)?
        .context("Native DSD handoff 后没有可播放 payload")?;
    // R3：新曲首块几何生效——pre-mute 窗口内的静音块随之切换，
    // 保证 静音块 → 新曲首块 的交付块长度连续，避免 SDK 侧 Size 突变
    let first_len = first_slot.payload_len.load(Ordering::Acquire);
    if first_len > 0 {
        ring.last_delivered_len.store(first_len, Ordering::Release);
    }
    Ok((reader, new_format))
}

#[derive(Clone)]
pub struct DirectDsdMonitor {
    ring: Arc<DirectDsdRing>,
    bit_rate: u32,
}

impl DirectDsdMonitor {
    pub fn consumed_position(&self) -> f64 {
        self.ring.consumed_bits_per_channel.load(Ordering::Acquire) as f64
            / f64::from(self.bit_rate)
    }

    pub fn failed(&self) -> bool {
        self.ring.failed.load(Ordering::Acquire)
    }

    /// 等待设备消费的数据块数（READY + IN_FLIGHT）。
    /// 停滞检测用：消费冻结且仍有待消费数据 = 链路不取数；
    /// 待消费为零的冻结 = SDK 等解码补数据，不算链路故障
    pub fn pending_blocks(&self) -> usize {
        self.ring
            .slots
            .iter()
            .filter(|slot| {
                matches!(
                    slot.state.load(Ordering::Acquire),
                    SLOT_READY | SLOT_IN_FLIGHT
                )
            })
            .count()
    }

    pub fn finished(&self) -> bool {
        self.ring.finished.load(Ordering::Acquire)
            && self.ring.in_flight.load(Ordering::Acquire) == NO_SLOT
            && self
                .ring
                .slots
                .iter()
                .all(|slot| slot.state.load(Ordering::Acquire) == SLOT_FREE)
    }

    /// staged 候选已接受但尚未装填：此窗口内 finished 不可作为曲终依据
    /// （stage 迟到时 producer 在 EOF 处等待，装填后 finished 复位）
    pub fn staging(&self) -> bool {
        self.ring.stage_pending.load(Ordering::Acquire)
    }

    /// 排空等待（短周期轮询）：换源/拆线前请求排空后调用，静音垫同时达到
    /// 时长时间与块数下限（或排空被复位）即返回。句柄持有 ring 的 Arc 引用，
    /// 供调用方在 player 锁外等待。
    /// R1 取舍：不再依赖回调线程 notify（SDK 实时线程禁止 lock），
    /// 改为控制线程 10ms 轮询——判定最多延迟一个轮询周期，发生在瞬时
    /// 排空等待内，不在常态播放路径；回调线程零锁零负担
    pub fn wait_fade_drained(&self, min_blocks: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.ring.drain_complete(min_blocks) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// 注入排空目标时长（µs）：建连后由上层根据 Sink 实测参数调用；
    /// 0 = 恢复旧常量行为
    pub fn set_drain_target_micros(&self, micros: u64) {
        self.ring.drain_target_micros.store(micros, Ordering::Release);
    }

    /// 排空目标时长（µs）：供调用方联动计算超时
    pub fn drain_target_micros(&self) -> u64 {
        self.ring.drain_target_micros()
    }

    /// 注入静音字节（SDK getMuteByte() 返回值）；0 = 退回 0x69 硬编码
    pub fn set_mute_byte(&self, byte: u8) {
        self.ring.mute_byte.store(byte, Ordering::Release);
    }

    /// 事件驱动首块消费等待：任一位被设备消费、失败或源提前结束即唤醒；
    /// 无事件时阻塞至超时（返回 false）。用于启动校验，取代轮询 sleep
    pub fn wait_first_consumed(&self, timeout: Duration) -> bool {
        self.ring.wait_for(
            |ring| {
                ring.consumed_bits_per_channel.load(Ordering::Acquire) > 0
                    || ring.failed.load(Ordering::Acquire)
                    || ring.finished.load(Ordering::Acquire)
            },
            timeout,
        )
    }

    pub fn transition_count(&self) -> u64 {
        self.ring.transition_count.load(Ordering::Acquire)
    }

    pub fn duration(&self) -> f64 {
        self.ring.duration_micros.load(Ordering::Acquire) as f64 / 1_000_000.0
    }

    pub fn boundary_generation(&self) -> u64 {
        self.ring.boundary_generation.load(Ordering::Acquire)
    }
}

#[derive(Clone)]
pub struct DirectDsdStageHandle {
    control_tx: mpsc::Sender<DirectDsdCommand>,
    ring: Arc<DirectDsdRing>,
}

impl DirectDsdStageHandle {
    pub fn stage_local(
        &self,
        path: &Path,
        start_secs: f64,
        duration_secs: f64,
        generation: u64,
    ) -> Result<()> {
        let duration_micros = if duration_secs > 0.0 {
            (duration_secs * 1_000_000.0)
                .round()
                .clamp(0.0, u64::MAX as f64) as u64
        } else {
            0
        };
        let (response_tx, response_rx) = mpsc::sync_channel(0);
        self.control_tx
            .send(DirectDsdCommand::StageLocal {
                path: path.to_owned(),
                start_secs,
                duration_micros,
                generation,
                response: response_tx,
            })
            .context("提交 Native DSD staged source 失败")?;
        self.ring.signal_command();
        response_rx
            .recv()
            .context("等待 Native DSD staged source 结果失败")?
    }

    pub fn cancel(&self) {
        let _ = self.control_tx.send(DirectDsdCommand::CancelStaged);
        self.ring.signal_command();
    }
}

pub struct DirectDsdSource {
    ring: Arc<DirectDsdRing>,
    format: DirectDsdFormat,
    control_tx: mpsc::Sender<DirectDsdCommand>,
    producer: Option<JoinHandle<()>>,
}

impl DirectDsdSource {
    pub fn open_local(path: &Path) -> Result<Self> {
        let (source, _) = Self::open_local_at(path, 0.0)?;
        Ok(source)
    }

    /// 换源/拆线前的源级排空请求：此后交付块在回调侧替换为 0x69 静音，
    /// 直到 replace/seek 复位或连接关闭。DSD 无淡出通道，排空即静音垫
    pub fn begin_drain(&self) {
        self.ring.begin_drain();
    }

    /// 软暂停等待（Phase3）：首个交付块已被替换为 0x69 零电平（或超时，
    /// 超时后调用方仍会停发，退化为改动前行为）。控制线程 5ms 轮询
    pub fn wait_soft_pause(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.ring.soft_pause_ready() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// 清除排空请求（恢复播放配套）：不清除则恢复后永久静音（正确性关键）
    pub fn clear_drain(&self) {
        self.ring.clear_drain();
    }

    /// 注入排空目标时长（µs）：建连后由上层根据 Sink 实测参数调用
    pub fn set_drain_target_micros(&self, micros: u64) {
        self.ring.drain_target_micros.store(micros, Ordering::Release);
    }

    /// 注入静音字节（SDK getMuteByte() 返回值）；0 = 退回 0x69 硬编码
    pub fn set_mute_byte(&self, byte: u8) {
        self.ring.mute_byte.store(byte, Ordering::Release);
    }

    pub fn open_local_at(path: &Path, position_secs: f64) -> Result<(Self, f64)> {
        let mut reader = DirectDsdReader::open_local(path)?;
        let duration_micros = (reader.duration_secs() * 1_000_000.0)
            .round()
            .clamp(0.0, u64::MAX as f64) as u64;
        let actual_position = if position_secs > 0.0 {
            reader.seek_seconds(position_secs)?
        } else {
            0.0
        };
        let format = reader.format();
        let ring = Arc::new(DirectDsdRing::new(reader.max_output_len(), format));
        tracing::info!(
            target: "diretta_dsd",
            depth = ring.slots.len(),
            slot_bytes = reader.max_output_len(),
            bit_rate = format.bit_rate,
            "Native DSD ring 初始化（目标预缓冲 {DIRECT_DSD_TARGET_BUFFERED_MS:.0}ms）"
        );
        ring.duration_micros
            .store(duration_micros, Ordering::Release);
        fill_slot(&mut reader, &ring.slots[0], format.bit_order)?
            .context("Native DSD 音源没有可播放 payload")?;

        let (control_tx, control_rx) = mpsc::channel();
        let producer_ring = Arc::clone(&ring);
        let producer = thread::Builder::new()
            .name("diretta-direct-dsd".into())
            .spawn(move || {
                // 绑定到 CPU 性能核心（ARM 大核 / x86 独立物理核）并设置 SCHED_FIFO 实时调度，
                // 防止 DSD 推流时钟抖动（Jitter）和 CPU 大小核漂移造成 DAC 爆音。
                bind_current_thread_to_performance_cores("diretta-direct-dsd");
                boost_current_audio_thread("diretta-direct-dsd");
                let mut active_format = format;
                let mut wire_bit_order = format.bit_order;
                let mut staged: Option<StagedDsdSource> = None;
                // 后台 stage prepare 的结果通道：prepare 在独立线程执行（打开文件/
                // CUE seek 可达数百 ms，绝不能阻塞 producer 数据通路）
                let (stage_result_tx, stage_result_rx) =
                    mpsc::channel::<StagePrepareOutcome>();
                let mut next_slot = 1 % producer_ring.slots.len();
                while !producer_ring.stopped.load(Ordering::Acquire) {
                    // 消费命令前清提示位：此后发送方的新命令会重新置位并唤醒
                    producer_ring
                        .command_pending
                        .store(false, Ordering::Release);
                    match control_rx.try_recv() {
                        Ok(DirectDsdCommand::SetWireBitOrder {
                            bit_order,
                            position_secs,
                            response,
                        }) => {
                            wire_bit_order = bit_order;
                            let result = seek_dsd_ring(
                                &mut reader,
                                &producer_ring,
                                active_format,
                                wire_bit_order,
                                position_secs,
                            );
                            if result.is_err() {
                                producer_ring.failed.store(true, Ordering::Release);
                            }
                            let _ = response.send(result);
                            next_slot = 1 % producer_ring.slots.len();
                            continue;
                        }
                        Ok(DirectDsdCommand::Seek {
                            position_secs,
                            response,
                        }) => {
                            let result = seek_dsd_ring(
                                &mut reader,
                                &producer_ring,
                                active_format,
                                wire_bit_order,
                                position_secs,
                            );
                            if result.is_err() {
                                producer_ring.failed.store(true, Ordering::Release);
                            }
                            let _ = response.send(result);
                            next_slot = 1 % producer_ring.slots.len();
                            continue;
                        }
                        Ok(DirectDsdCommand::ReplaceLocal {
                            source,
                            start_secs,
                            response,
                        }) => {
                            let result = replace_dsd_ring(
                                &source,
                                start_secs,
                                &producer_ring,
                                active_format,
                                wire_bit_order,
                            );
                            match result {
                                Ok((new_reader, new_format)) => {
                                    reader = new_reader;
                                    active_format = new_format;
                                    staged = None;
                                    // 推进代数：旧候选的在途 prepare 结果作废
                                    producer_ring.stage_epoch.fetch_add(1, Ordering::AcqRel);
                                    producer_ring
                                        .stage_pending
                                        .store(false, Ordering::Release);
                                    let _ = response.send(Ok(new_format));
                                    next_slot = 1 % producer_ring.slots.len();
                                }
                                Err(error) => {
                                    let _ = response.send(Err(error));
                                }
                            }
                            continue;
                        }
                        Ok(DirectDsdCommand::StageLocal {
                            path,
                            start_secs,
                            duration_micros,
                            generation,
                            response,
                        }) => {
                            // stage_pending 置位后直到装填进 ring 或取消/失败才清除。
                            // prepare 在独立线程执行，完成后经 stage_result 通道回交
                            // producer（epoch 校验防过期）
                            producer_ring.stage_pending.store(true, Ordering::Release);
                            let epoch = producer_ring.stage_epoch.load(Ordering::Acquire);
                            let fmt = active_format;
                            let result_tx = stage_result_tx.clone();
                            let flag = std::sync::Arc::clone(&producer_ring);
                            let spawned = std::thread::Builder::new()
                                .name("direct-dsd-stage-prepare".into())
                                .spawn(move || {
                                    let result = prepare_staged_dsd_source(
                                        &path,
                                        start_secs,
                                        fmt,
                                        duration_micros,
                                        generation,
                                    );
                                    match result {
                                        Ok(candidate) => {
                                            let _ = result_tx.send(StagePrepareOutcome::Ready(
                                                epoch, candidate,
                                            ));
                                            flag.staged_ready.store(true, Ordering::Release);
                                            flag.notify_state();
                                            let _ = response.send(Ok(()));
                                        }
                                        Err(error) => {
                                            flag.stage_pending.store(false, Ordering::Release);
                                            let _ = result_tx
                                                .send(StagePrepareOutcome::Failed(epoch));
                                            let _ = response.send(Err(error));
                                        }
                                    }
                                });
                            if spawned.is_err() {
                                // 线程启动失败：response 随闭包丢弃，调用方 recv()
                                // 直接得到错误；此处仅复位提示位
                                producer_ring.stage_pending.store(false, Ordering::Release);
                            }
                            continue;
                        }
                        Ok(DirectDsdCommand::CancelStaged) => {
                            staged = None;
                            producer_ring.stage_epoch.fetch_add(1, Ordering::AcqRel);
                            producer_ring.stage_pending.store(false, Ordering::Release);
                            continue;
                        }
                        Err(mpsc::TryRecvError::Disconnected) => return,
                        Err(mpsc::TryRecvError::Empty) => {}
                    }
                    // 收取后台 prepare 的结果：epoch 匹配才装填（过期结果丢弃）；
                    // 失败仅清除 stage_pending
                    while let Ok(outcome) = stage_result_rx.try_recv() {
                        match outcome {
                            StagePrepareOutcome::Ready(epoch, candidate) => {
                                if epoch == producer_ring.stage_epoch.load(Ordering::Acquire) {
                                    staged = Some(candidate);
                                }
                            }
                            StagePrepareOutcome::Failed(epoch) => {
                                if epoch == producer_ring.stage_epoch.load(Ordering::Acquire) {
                                    producer_ring.stage_pending.store(false, Ordering::Release);
                                }
                            }
                        }
                    }
                    producer_ring.staged_ready.store(false, Ordering::Release);
                    if producer_ring.failed.load(Ordering::Acquire) {
                        // 事件等待：等 failed 被清（reset_for_transition）或有新命令
                        producer_ring.wait_for(
                            |ring| {
                                !ring.failed.load(Ordering::Acquire)
                                    || ring.command_pending.load(Ordering::Acquire)
                            },
                            PRODUCER_WAIT_CEILING,
                        );
                        continue;
                    }
                    if producer_ring.finished.load(Ordering::Acquire) {
                        let Some(candidate) = staged.take() else {
                            // 事件等待：等新源 stage 完成、状态翻转或命令到达
                            producer_ring.wait_for(
                                |ring| {
                                    !ring.finished.load(Ordering::Acquire)
                                        || ring.staged_ready.load(Ordering::Acquire)
                                        || ring.command_pending.load(Ordering::Acquire)
                                },
                                PRODUCER_WAIT_CEILING,
                            );
                            continue;
                        };
                        let slot = &producer_ring.slots[next_slot];
                        if slot
                            .state
                            .compare_exchange(
                                SLOT_FREE,
                                SLOT_FILLING,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_err()
                        {
                            staged = Some(candidate);
                            // 事件等待：等 consumer 释放 slot 或新命令
                            producer_ring.wait_for(
                                |ring| {
                                    ring.slots[next_slot].state.load(Ordering::Acquire) == SLOT_FREE
                                        || ring.command_pending.load(Ordering::Acquire)
                                },
                                PRODUCER_WAIT_CEILING,
                            );
                            continue;
                        }
                        match install_staged_dsd_slot(candidate, slot, wire_bit_order) {
                            Ok((new_reader, new_format)) => {
                                reader = new_reader;
                                active_format = new_format;
                                producer_ring.finished.store(false, Ordering::Release);
                                producer_ring.stage_pending.store(false, Ordering::Release);
                                next_slot = (next_slot + 1) % producer_ring.slots.len();
                            }
                            Err(_) => {
                                slot.state.store(SLOT_FREE, Ordering::Release);
                                producer_ring.failed.store(true, Ordering::Release);
                                producer_ring.stage_pending.store(false, Ordering::Release);
                            }
                        }
                        continue;
                    }
                    let slot = &producer_ring.slots[next_slot];
                    if slot
                        .state
                        .compare_exchange(
                            SLOT_FREE,
                            SLOT_FILLING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                    {
                        // 事件等待：等 consumer 释放 slot 或新命令
                        producer_ring.wait_for(
                            |ring| {
                                ring.slots[next_slot].state.load(Ordering::Acquire) == SLOT_FREE
                                    || ring.command_pending.load(Ordering::Acquire)
                            },
                            PRODUCER_WAIT_CEILING,
                        );
                        continue;
                    }
                    match fill_claimed_slot(&mut reader, slot, wire_bit_order) {
                        Ok(Some(())) => next_slot = (next_slot + 1) % producer_ring.slots.len(),
                        Ok(None) => {
                            // EOF：staged 候选可能已 prepare 完成但尚未被本循环
                            // 收取——stage_local 返回即保证结果已在结果通道内，
                            // 此处先排空通道再判定。否则 finished 置位与下一轮
                            // 收取之间留有一拍空窗，consumer 命中供数空窗会交付
                            // 0x69 静音垫，无缝边界被夹进静音（单测
                            // staged_dsd_handoff_is_bit_contiguous 偶发失败根因）
                            if staged.is_none() {
                                while let Ok(outcome) = stage_result_rx.try_recv() {
                                    match outcome {
                                        StagePrepareOutcome::Ready(epoch, candidate) => {
                                            if epoch
                                                == producer_ring
                                                    .stage_epoch
                                                    .load(Ordering::Acquire)
                                            {
                                                staged = Some(candidate);
                                            }
                                        }
                                        StagePrepareOutcome::Failed(epoch) => {
                                            if epoch
                                                == producer_ring
                                                    .stage_epoch
                                                    .load(Ordering::Acquire)
                                            {
                                                producer_ring
                                                    .stage_pending
                                                    .store(false, Ordering::Release);
                                            }
                                        }
                                    }
                                }
                            }
                            match staged.take() {
                                Some(candidate) => {
                                    match install_staged_dsd_slot(candidate, slot, wire_bit_order) {
                                        Ok((new_reader, new_format)) => {
                                            reader = new_reader;
                                            active_format = new_format;
                                            producer_ring.finished.store(false, Ordering::Release);
                                            producer_ring
                                                .stage_pending
                                                .store(false, Ordering::Release);
                                            next_slot = (next_slot + 1) % producer_ring.slots.len();
                                        }
                                        Err(_) => {
                                            slot.state.store(SLOT_FREE, Ordering::Release);
                                            producer_ring.failed.store(true, Ordering::Release);
                                            producer_ring
                                                .stage_pending
                                                .store(false, Ordering::Release);
                                        }
                                    }
                                }
                                None => {
                                    // fill 返回 None = reader 报告 EOF：若远早于曲目时长，
                                    // 即文件读取提前终止（CIFS/介质读失败常伪装成 EOF）
                                    tracing::warn!(
                                        target: "diretta_dsd",
                                        "Native DSD 源提前 EOF，production 结束"
                                    );
                                    slot.state.store(SLOT_FREE, Ordering::Release);
                                    producer_ring.finished.store(true, Ordering::Release);
                                }
                            }
                        }
                        Err(ref error) => {
                            tracing::warn!(
                                target: "diretta_dsd",
                                error = format!("{error:#}"),
                                "Native DSD 填槽失败，production 置 failed"
                            );
                            slot.state.store(SLOT_FREE, Ordering::Release);
                            producer_ring.failed.store(true, Ordering::Release);
                        }
                    }
                }
            })
            .context("启动 Native DSD producer 失败")?;

        Ok((
            Self {
                ring,
                format,
                control_tx,
                producer: Some(producer),
            },
            actual_position,
        ))
    }

    pub fn format(&self) -> DirectDsdFormat {
        self.format
    }

    pub fn callback_context(&self) -> *mut c_void {
        Arc::as_ptr(&self.ring).cast_mut().cast()
    }

    pub fn monitor(&self) -> DirectDsdMonitor {
        DirectDsdMonitor {
            ring: Arc::clone(&self.ring),
            bit_rate: self.format.bit_rate,
        }
    }

    pub fn stage_handle(&self) -> DirectDsdStageHandle {
        DirectDsdStageHandle {
            control_tx: self.control_tx.clone(),
            ring: Arc::clone(&self.ring),
        }
    }

    pub fn failed(&self) -> bool {
        self.monitor().failed()
    }

    pub fn finished(&self) -> bool {
        self.monitor().finished()
    }

    pub fn consumed_position(&self) -> f64 {
        self.monitor().consumed_position()
    }

    pub fn set_wire_bit_order_while_paused(
        &mut self,
        bit_order: DirectDsdBitOrder,
        position_secs: f64,
    ) -> Result<f64> {
        let (response_tx, response_rx) = mpsc::sync_channel(0);
        self.control_tx
            .send(DirectDsdCommand::SetWireBitOrder {
                bit_order,
                position_secs,
                response: response_tx,
            })
            .context("提交 Native DSD wire bit-order 适配失败")?;
        self.ring.signal_command();
        response_rx
            .recv()
            .context("等待 Native DSD wire bit-order 适配结果失败")?
    }

    pub fn seek_while_paused(&mut self, position_secs: f64) -> Result<f64> {
        let (response_tx, response_rx) = mpsc::sync_channel(0);
        self.control_tx
            .send(DirectDsdCommand::Seek {
                position_secs,
                response: response_tx,
            })
            .context("提交 Native DSD seek 失败")?;
        self.ring.signal_command();
        response_rx
            .recv()
            .context("等待 Native DSD seek 结果失败")?
    }

    pub fn replace_drained_local(
        &mut self,
        source: &str,
        start_secs: f64,
    ) -> Result<DirectDsdFormat> {
        let (response_tx, response_rx) = mpsc::sync_channel(0);
        self.control_tx
            .send(DirectDsdCommand::ReplaceLocal {
                source: source.to_owned(),
                start_secs,
                response: response_tx,
            })
            .context("提交 Native DSD handoff 失败")?;
        self.ring.signal_command();
        let format = response_rx
            .recv()
            .context("等待 Native DSD handoff 结果失败")??;
        self.format = format;
        Ok(format)
    }
}

fn fill_slot(
    reader: &mut DirectDsdReader,
    slot: &DirectDsdSlot,
    wire_bit_order: DirectDsdBitOrder,
) -> Result<Option<()>> {
    ensure!(
        slot.state
            .compare_exchange(SLOT_FREE, SLOT_FILLING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok(),
        "Native DSD slot 状态无效"
    );
    match fill_claimed_slot(reader, slot, wire_bit_order) {
        Ok(value) => Ok(value),
        Err(error) => {
            slot.state.store(SLOT_FREE, Ordering::Release);
            Err(error)
        }
    }
}

fn fill_claimed_slot(
    reader: &mut DirectDsdReader,
    slot: &DirectDsdSlot,
    wire_bit_order: DirectDsdBitOrder,
) -> Result<Option<()>> {
    let buffer = unsafe { &mut *slot.buffer.get() };
    let source_bit_order = reader.format().bit_order;
    let Some(len) = reader.read_block(buffer)? else {
        return Ok(None);
    };
    adapt_dsd_bit_order(&mut buffer[..len], source_bit_order, wire_bit_order);
    slot.payload_ptr
        .store(buffer.as_mut_ptr(), Ordering::Relaxed);
    slot.payload_len.store(len, Ordering::Relaxed);
    let channels = u64::from(reader.format().channels);
    ensure!(channels > 0, "Native DSD 声道数无效");
    let bits_per_channel = u64::try_from(len)?
        .checked_mul(8)
        .and_then(|bits| bits.checked_div(channels))
        .context("Native DSD block 位数溢出")?;
    slot.bits_per_channel
        .store(bits_per_channel, Ordering::Relaxed);
    slot.state.store(SLOT_READY, Ordering::Release);
    Ok(Some(()))
}

pub unsafe extern "C" fn direct_dsd_next_block(
    context: *mut c_void,
    data: *mut *const u8,
    len: *mut usize,
) -> bool {
    if context.is_null() || data.is_null() || len.is_null() {
        return false;
    }
    let ring = unsafe { &*context.cast::<DirectDsdRing>() };
    let Some(block) = ring.next_block() else {
        return false;
    };
    // 换源/拆线前排空：交付前把块原位替换为 0x69 静音（块 IN_FLIGHT 期仅本回调可写）
    ring.apply_drain(block.data.cast_mut(), block.len);
    unsafe {
        *data = block.data;
        *len = block.len;
    }
    true
}

pub unsafe extern "C" fn direct_dsd_release_block(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    let ring = unsafe { &*context.cast::<DirectDsdRing>() };
    ring.release_in_flight();
}

impl Drop for DirectDsdSource {
    fn drop(&mut self) {
        self.ring.stopped.store(true, Ordering::Release);
        self.ring.release_in_flight();
        if let Some(producer) = self.producer.take() {
            let _ = producer.join();
        }
    }
}

fn read_le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().expect("u32 slice"))
}

fn read_le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().expect("u64 slice"))
}

fn read_be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().expect("u32 slice"))
}

fn read_be_u64(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(bytes[..8].try_into().expect("u64 slice"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TempDsdFile {
        path: std::path::PathBuf,
    }

    impl TempDsdFile {
        fn new(extension: &str, bytes: &[u8]) -> Self {
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "splayer-direct-dsd-{}-{id}.{extension}",
                std::process::id()
            ));
            fs::write(&path, bytes).expect("写入 Native DSD fixture 失败");
            Self { path }
        }
    }

    impl Drop for TempDsdFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn dsf_fixture(sample_count: u64) -> (Vec<u8>, Vec<u8>) {
        let left = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let right = [0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10];
        let mut raw = Vec::new();
        raw.extend_from_slice(&left);
        raw.extend_from_slice(&right);

        let mut file = Vec::new();
        file.extend_from_slice(b"DSD ");
        file.extend_from_slice(&28_u64.to_le_bytes());
        file.extend_from_slice(&0_u64.to_le_bytes());
        file.extend_from_slice(&0_u64.to_le_bytes());
        file.extend_from_slice(b"fmt ");
        file.extend_from_slice(&52_u64.to_le_bytes());
        file.extend_from_slice(&1_u32.to_le_bytes());
        file.extend_from_slice(&0_u32.to_le_bytes());
        file.extend_from_slice(&2_u32.to_le_bytes());
        file.extend_from_slice(&2_u32.to_le_bytes());
        file.extend_from_slice(&2_822_400_u32.to_le_bytes());
        file.extend_from_slice(&1_u32.to_le_bytes());
        file.extend_from_slice(&sample_count.to_le_bytes());
        file.extend_from_slice(&8_u32.to_le_bytes());
        file.extend_from_slice(&0_u32.to_le_bytes());
        file.extend_from_slice(b"data");
        file.extend_from_slice(&(12_u64 + raw.len() as u64).to_le_bytes());
        file.extend_from_slice(&raw);
        let file_len = file.len() as u64;
        file[12..20].copy_from_slice(&file_len.to_le_bytes());

        let expected = vec![
            0x01, 0x23, 0x45, 0x67, 0xfe, 0xdc, 0xba, 0x98, 0x89, 0xab, 0xcd, 0xef, 0x76, 0x54,
            0x32, 0x10,
        ];
        (file, expected)
    }

    fn append_dff_chunk(dst: &mut Vec<u8>, id: &[u8; 4], payload: &[u8]) {
        dst.extend_from_slice(id);
        dst.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        dst.extend_from_slice(payload);
        if payload.len() % 2 != 0 {
            dst.push(0);
        }
    }

    fn logical_dsd_bits(bytes: &[u8], bit_order: DirectDsdBitOrder) -> Vec<u8> {
        let mut bits = Vec::with_capacity(bytes.len() * 8);
        for byte in bytes {
            for index in 0..8 {
                let shift = match bit_order {
                    DirectDsdBitOrder::LsbFirst => index,
                    DirectDsdBitOrder::MsbFirst => 7 - index,
                };
                bits.push((byte >> shift) & 1);
            }
        }
        bits
    }

    fn dff_fixture() -> (Vec<u8>, Vec<u8>) {
        let left = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let right = [0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10];
        let mut raw = Vec::new();
        for index in 0..left.len() {
            raw.push(left[index]);
            raw.push(right[index]);
        }

        let mut prop = Vec::new();
        prop.extend_from_slice(b"SND ");
        append_dff_chunk(&mut prop, b"FS  ", &2_822_400_u32.to_be_bytes());
        let mut chnl = Vec::new();
        chnl.extend_from_slice(&2_u16.to_be_bytes());
        chnl.extend_from_slice(b"SLFTSRGT");
        append_dff_chunk(&mut prop, b"CHNL", &chnl);
        append_dff_chunk(&mut prop, b"CMPR", b"DSD ");

        let mut body = Vec::new();
        append_dff_chunk(&mut body, b"PROP", &prop);
        append_dff_chunk(&mut body, b"DSD ", &raw);

        let mut file = Vec::new();
        file.extend_from_slice(b"FRM8");
        file.extend_from_slice(&(4_u64 + body.len() as u64).to_be_bytes());
        file.extend_from_slice(b"DSD ");
        file.extend_from_slice(&body);

        let expected = vec![
            0x01, 0x23, 0x45, 0x67, 0xfe, 0xdc, 0xba, 0x98, 0x89, 0xab, 0xcd, 0xef, 0x76, 0x54,
            0x32, 0x10,
        ];
        (file, expected)
    }

    #[test]
    fn native_dsd_core_never_enters_pcm_or_float_pipeline() {
        let source = include_str!("direct_dsd.rs");
        let implementation = source
            .split("#[cfg(test)]")
            .next()
            .expect("Native DSD implementation section should exist");
        for forbidden in [
            "ffmpeg_audio",
            "Resampler",
            "Equalizer",
            "f32",
            "DSD2PCM",
            "DoP",
        ] {
            assert!(
                !implementation.contains(forbidden),
                "Native DSD core must not depend on {forbidden}"
            );
        }
    }

    #[test]
    fn native_dsd_duration_uses_one_bit_rate_semantics() {
        let (dsf, _) = dsf_fixture(64);
        let dsf_fixture = TempDsdFile::new("dsf", &dsf);
        let dsf_reader = DirectDsdReader::open_local(&dsf_fixture.path).unwrap();
        assert_eq!(dsf_reader.format().bit_rate, 2_822_400);
        assert!((dsf_reader.duration_secs() - 64.0 / 2_822_400.0).abs() < f64::EPSILON);

        let (dff, _) = dff_fixture();
        let dff_fixture = TempDsdFile::new("dff", &dff);
        let dff_reader = DirectDsdReader::open_local(&dff_fixture.path).unwrap();
        assert_eq!(dff_reader.format().bit_rate, 2_822_400);
        assert!((dff_reader.duration_secs() - 64.0 / 2_822_400.0).abs() < f64::EPSILON);
    }

    #[test]
    fn dsf_lsb_payload_keeps_every_native_bit_and_only_repacks_l4r4() {
        let (bytes, expected) = dsf_fixture(64);
        let fixture = TempDsdFile::new("dsf", &bytes);
        let mut reader = DirectDsdReader::open_local(&fixture.path).unwrap();
        assert_eq!(
            reader.format(),
            DirectDsdFormat {
                bit_rate: 2_822_400,
                channels: 2,
                bit_order: DirectDsdBitOrder::LsbFirst,
                container: DirectDsdContainer::Dsf,
            }
        );
        let mut output = vec![0_u8; reader.max_output_len()];
        let len = reader.read_block(&mut output).unwrap().unwrap();
        assert_eq!(&output[..len], expected);
        assert!(reader.read_block(&mut output).unwrap().is_none());
    }

    #[test]
    fn dsf_lsb_adapts_to_msb_wire_without_changing_the_logical_dsd_sequence() {
        let (bytes, source_expected) = dsf_fixture(64);
        let fixture = TempDsdFile::new("dsf", &bytes);
        let mut source = DirectDsdSource::open_local(&fixture.path).unwrap();
        source
            .set_wire_bit_order_while_paused(DirectDsdBitOrder::MsbFirst, 0.0)
            .unwrap();

        let mut data = ptr::null();
        let mut len = 0_usize;
        assert!(unsafe { direct_dsd_next_block(source.callback_context(), &mut data, &mut len) });
        let actual = unsafe { std::slice::from_raw_parts(data, len) };
        let wire_expected: Vec<u8> = source_expected
            .iter()
            .map(|byte| byte.reverse_bits())
            .collect();
        assert_eq!(actual, wire_expected);
        assert_eq!(
            logical_dsd_bits(&source_expected, DirectDsdBitOrder::LsbFirst),
            logical_dsd_bits(actual, DirectDsdBitOrder::MsbFirst)
        );
        unsafe { direct_dsd_release_block(source.callback_context()) };
    }

    #[test]
    fn dff_msb_payload_keeps_every_native_bit_and_only_repacks_l4r4() {
        let (bytes, expected) = dff_fixture();
        let fixture = TempDsdFile::new("dff", &bytes);
        let mut reader = DirectDsdReader::open_local(&fixture.path).unwrap();
        assert_eq!(
            reader.format(),
            DirectDsdFormat {
                bit_rate: 2_822_400,
                channels: 2,
                bit_order: DirectDsdBitOrder::MsbFirst,
                container: DirectDsdContainer::Dff,
            }
        );
        let mut output = vec![0_u8; reader.max_output_len()];
        let len = reader.read_block(&mut output).unwrap().unwrap();
        assert_eq!(&output[..len], expected);
        assert!(reader.read_block(&mut output).unwrap().is_none());
    }

    #[test]
    fn dsd_callback_borrows_the_same_preallocated_repack_buffer() {
        let (bytes, expected) = dsf_fixture(64);
        let fixture = TempDsdFile::new("dsf", &bytes);
        let source = DirectDsdSource::open_local(&fixture.path).unwrap();
        assert_eq!(source.format().bit_order, DirectDsdBitOrder::LsbFirst);
        assert!(!source.failed());
        assert!(!source.finished());
        let first_buffer = unsafe { &*source.ring.slots[0].buffer.get() };
        let expected_ptr = first_buffer.as_ptr();
        let mut data = ptr::null();
        let mut len = 0_usize;

        assert!(unsafe { direct_dsd_next_block(source.callback_context(), &mut data, &mut len) });
        assert_eq!(data, expected_ptr);
        assert_eq!(unsafe { std::slice::from_raw_parts(data, len) }, expected);
        unsafe { direct_dsd_release_block(source.callback_context()) };
    }

    #[test]
    fn native_dsd_seek_keeps_exact_bits_for_dsf_and_dff() {
        let target_secs = 32.0 / 2_822_400.0;
        let expected_tail = [0x89, 0xab, 0xcd, 0xef, 0x76, 0x54, 0x32, 0x10];

        for (extension, bytes) in {
            let (dsf, _) = dsf_fixture(64);
            let (dff, _) = dff_fixture();
            [("dsf", dsf), ("dff", dff)]
        } {
            let fixture = TempDsdFile::new(extension, &bytes);
            let mut reader = DirectDsdReader::open_local(&fixture.path).unwrap();
            let actual = reader.seek_seconds(target_secs).unwrap();
            assert_eq!(actual, target_secs);
            let mut output = vec![0_u8; reader.max_output_len()];
            let len = reader.read_block(&mut output).unwrap().unwrap();
            assert_eq!(&output[..len], expected_tail);
            assert!(reader.read_block(&mut output).unwrap().is_none());
        }
    }

    #[test]
    fn dsd_source_position_advances_only_when_sdk_releases_in_flight_block() {
        let (bytes, _) = dsf_fixture(64);
        let fixture = TempDsdFile::new("dsf", &bytes);
        let (source, actual) = DirectDsdSource::open_local_at(&fixture.path, 0.0).unwrap();
        assert_eq!(actual, 0.0);
        let mut data = ptr::null();
        let mut len = 0_usize;
        assert!(unsafe { direct_dsd_next_block(source.callback_context(), &mut data, &mut len) });
        assert_eq!(source.consumed_position(), 0.0);
        unsafe { direct_dsd_release_block(source.callback_context()) };
        let expected_position = 64.0 / 2_822_400.0;
        assert_eq!(source.consumed_position(), expected_position);
    }

    #[test]
    fn dsd_source_seek_reuses_the_same_callback_context_and_ring() {
        let (bytes, _) = dsf_fixture(64);
        let fixture = TempDsdFile::new("dsf", &bytes);
        let mut source = DirectDsdSource::open_local(&fixture.path).unwrap();
        let context_before = source.callback_context();
        let target_secs = 32.0 / 2_822_400.0;
        let expected_tail = [0x89, 0xab, 0xcd, 0xef, 0x76, 0x54, 0x32, 0x10];

        let actual = source.seek_while_paused(target_secs).unwrap();
        assert_eq!(actual, target_secs);
        assert_eq!(source.callback_context(), context_before);
        let mut data = ptr::null();
        let mut len = 0_usize;
        assert!(unsafe { direct_dsd_next_block(source.callback_context(), &mut data, &mut len) });
        assert_eq!(
            unsafe { std::slice::from_raw_parts(data, len) },
            expected_tail
        );
        unsafe { direct_dsd_release_block(source.callback_context()) };
    }

    #[test]
    fn same_wire_format_dsd_handoff_keeps_callback_context_and_native_bits() {
        let (first_bytes, _) = dsf_fixture(64);
        let (mut second_bytes, _) = dsf_fixture(64);
        let second_raw = [
            0xaa, 0xbb, 0xcc, 0xdd, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xab,
            0xcd, 0xef,
        ];
        let raw_start = second_bytes.len() - second_raw.len();
        second_bytes[raw_start..].copy_from_slice(&second_raw);
        let expected = [
            0xaa, 0xbb, 0xcc, 0xdd, 0x55, 0x66, 0x77, 0x88, 0x11, 0x22, 0x33, 0x44, 0x99, 0xab,
            0xcd, 0xef,
        ];
        let first = TempDsdFile::new("dsf", &first_bytes);
        let second = TempDsdFile::new("dsf", &second_bytes);
        let mut source = DirectDsdSource::open_local(&first.path).unwrap();
        let context = source.callback_context();

        let format = source
            .replace_drained_local(&second.path.to_string_lossy(), 0.0)
            .unwrap();
        assert_eq!(source.callback_context(), context);
        assert_eq!(format.bit_rate, 2_822_400);
        assert_eq!(format.channels, 2);
        assert_eq!(format.bit_order, DirectDsdBitOrder::LsbFirst);

        let mut data = ptr::null();
        let mut len = 0_usize;
        assert!(unsafe { direct_dsd_next_block(source.callback_context(), &mut data, &mut len) });
        assert_eq!(unsafe { std::slice::from_raw_parts(data, len) }, expected);
        unsafe { direct_dsd_release_block(source.callback_context()) };

        // ReplaceLocal 与 staged 同语义：boundary 随新源首块消费生效，
        // duration/generation 原子切换而非 replace 瞬间覆盖
        let monitor = source.monitor();
        assert_eq!(monitor.transition_count(), 1);
        assert_eq!(monitor.boundary_generation(), 1);
        assert!((monitor.duration() - 64.0 / 2_822_400.0).abs() < 0.000_001);
    }

    #[test]
    fn same_rate_dsf_to_dff_handoff_adapts_to_the_existing_wire_bit_order() {
        let (dsf_bytes, _) = dsf_fixture(64);
        let (dff_bytes, dff_expected) = dff_fixture();
        let dsf = TempDsdFile::new("dsf", &dsf_bytes);
        let dff = TempDsdFile::new("dff", &dff_bytes);
        let mut source = DirectDsdSource::open_local(&dsf.path).unwrap();
        let context = source.callback_context();

        let format = source
            .replace_drained_local(&dff.path.to_string_lossy(), 0.0)
            .unwrap();
        assert_eq!(source.callback_context(), context);
        assert_eq!(format.bit_order, DirectDsdBitOrder::MsbFirst);
        assert!(!source.failed());

        let mut data = ptr::null();
        let mut len = 0_usize;
        assert!(unsafe { direct_dsd_next_block(source.callback_context(), &mut data, &mut len) });
        let actual = unsafe { std::slice::from_raw_parts(data, len) };
        let wire_expected: Vec<u8> = dff_expected
            .iter()
            .map(|byte| byte.reverse_bits())
            .collect();
        assert_eq!(actual, wire_expected);
        assert_eq!(
            logical_dsd_bits(&dff_expected, DirectDsdBitOrder::MsbFirst),
            logical_dsd_bits(actual, DirectDsdBitOrder::LsbFirst)
        );
        unsafe { direct_dsd_release_block(source.callback_context()) };

        // 跨容器（DSF→DFF）同速率换源同样带 boundary 标记
        let monitor = source.monitor();
        assert_eq!(monitor.transition_count(), 1);
        assert_eq!(monitor.boundary_generation(), 1);
    }

    #[test]
    fn dsf_unaligned_tail_fails_instead_of_padding_or_dropping_bits() {
        let (bytes, _) = dsf_fixture(31);
        let fixture = TempDsdFile::new("dsf", &bytes);
        let error = match DirectDsdReader::open_local(&fixture.path) {
            Ok(_) => panic!("未对齐 DSF 不应通过 Source Direct parser"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("DSD_SIZ_32"));
    }

    #[test]
    fn staged_dsd_handoff_is_bit_contiguous_and_marks_the_exact_boundary() {
        let (first_bytes, first_expected) = dsf_fixture(64);
        let (mut second_bytes, first_pattern) = dsf_fixture(64);
        let payload_start = second_bytes.len() - 16;
        for byte in &mut second_bytes[payload_start..] {
            *byte ^= 0xff;
        }
        let second_expected: Vec<u8> = first_pattern.into_iter().map(|byte| byte ^ 0xff).collect();
        let first = TempDsdFile::new("dsf", &first_bytes);
        let second = TempDsdFile::new("dsf", &second_bytes);
        let source = DirectDsdSource::open_local(&first.path).unwrap();
        let monitor = source.monitor();
        source
            .stage_handle()
            .stage_local(&second.path, 0.0, 0.0, 9)
            .unwrap();

        let mut collected = Vec::new();
        let target_len = first_expected.len() + second_expected.len();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while collected.len() < target_len && std::time::Instant::now() < deadline {
            let mut data = ptr::null();
            let mut len = 0_usize;
            if unsafe { direct_dsd_next_block(source.callback_context(), &mut data, &mut len) } {
                collected.extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
            } else {
                thread::sleep(Duration::from_millis(1));
            }
        }
        unsafe { direct_dsd_release_block(source.callback_context()) };

        let mut expected = first_expected;
        expected.extend_from_slice(&second_expected);
        assert_eq!(collected, expected);
        assert_eq!(monitor.transition_count(), 1);
        assert_eq!(monitor.boundary_generation(), 9);
        assert!((monitor.duration() - 64.0 / 2_822_400.0).abs() < 0.000_001);
    }

    /// CUE 虚拟轨 staging 语义：带 start_secs 的 staged 源从轨内偏移出样，
    /// 而非物理文件 0:00（H1.1 回归；DSD reader 自带字节精确 seek）
    #[test]
    fn staged_dsd_source_locates_to_the_requested_start_offset() {
        let (first_bytes, first_expected) = dsf_fixture(64);
        let (second_bytes, second_expected) = dsf_fixture(64);
        let first = TempDsdFile::new("dsf", &first_bytes);
        let second = TempDsdFile::new("dsf", &second_bytes);
        let source = DirectDsdSource::open_local(&first.path).unwrap();
        let monitor = source.monitor();
        // 32 bit：seek_seconds 的 32bit 组对齐落点，交付 second 的后半
        let start_secs = 32.0 / 2_822_400.0;
        source
            .stage_handle()
            .stage_local(&second.path, start_secs, 0.0, 9)
            .unwrap();

        let mut collected = Vec::new();
        let target_len = first_expected.len() + second_expected[8..].len();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while collected.len() < target_len && std::time::Instant::now() < deadline {
            let mut data = ptr::null();
            let mut len = 0_usize;
            if unsafe { direct_dsd_next_block(source.callback_context(), &mut data, &mut len) } {
                collected.extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
                unsafe { direct_dsd_release_block(source.callback_context()) };
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        let mut expected = first_expected;
        expected.extend_from_slice(&second_expected[8..]);
        assert_eq!(collected, expected);
        assert_eq!(monitor.transition_count(), 1);
        assert_eq!(monitor.boundary_generation(), 9);
    }

    fn test_dsd64_format() -> DirectDsdFormat {
        DirectDsdFormat {
            bit_rate: 2_822_400,
            channels: 2,
            bit_order: DirectDsdBitOrder::LsbFirst,
            container: DirectDsdContainer::Dsf,
        }
    }

    /// 整环预缓冲时长必须覆盖目标毫秒数（CIFS 读毛刺吸收能力）
    #[test]
    fn ring_depth_covers_target_buffered_duration() {
        let format = test_dsd64_format();
        // DSF 4096B/声道：单槽 11.61ms
        let depth = ring_depth_for(format, 8192);
        let buffered_ms = depth as f64 * 4096.0 * 8.0 * 1000.0 / 2_822_400.0;
        assert!(buffered_ms >= DIRECT_DSD_TARGET_BUFFERED_MS);

        // DFF 槽位更大（16KB/声道）：槽数变少但整环时长仍达标
        let dff_depth = ring_depth_for(format, 32768);
        let dff_buffered_ms = dff_depth as f64 * 16384.0 * 8.0 * 1000.0 / 2_822_400.0;
        assert!(dff_buffered_ms >= DIRECT_DSD_TARGET_BUFFERED_MS);

        // DSD512 单槽仅 ~1.45ms：深度夹在上限 256（内存 2MB 封顶）
        let dsd512 = DirectDsdFormat {
            bit_rate: 22_579_200,
            ..format
        };
        assert_eq!(ring_depth_for(dsd512, 8192), DIRECT_DSD_RING_DEPTH_MAX);
    }

    #[test]
    fn strict_dsd_ring_underrun_returns_no_block_instead_of_inserting_data() {
        let ring = DirectDsdRing::new(64, test_dsd64_format());
        assert!(ring.next_block().is_none());
    }

    /// pre-mute 窗口（换源/seek 复位触发）：空环拉取交付 0x69 静音块而非"无块"，
    /// 消除 Target 欠载杂音；窗口关闭后恢复严格欠载语义
    #[test]
    fn dsd_pre_mute_window_delivers_0x69_silence_over_the_gap() {
        let ring = DirectDsdRing::new(64, test_dsd64_format());
        // 未触发窗口：严格欠载语义（None）保持不变
        assert!(ring.next_block().is_none());

        // 模拟播放中记录的块几何 + 换源/seek 触发的 pre-mute 窗口
        ring.last_delivered_len.store(1024, Ordering::Relaxed);
        ring.trigger_pre_mute();
        let block = ring
            .next_block()
            .expect("pre-mute 窗口内应交付 0x69 静音块");
        assert_eq!(block.len, 1024);
        assert!(unsafe { std::slice::from_raw_parts(block.data, block.len) }
            .iter()
            .all(|&b| b == DSD_SILENCE_BYTE));

        // 窗口关闭后欠载仍交付 0x69 静音：SDK 契约禁止 getNewStream 返回
        // false（会终止发送线程，此后永不消费——真机复现的消费冻结）
        ring.pre_mute_until_ms.store(0, Ordering::Relaxed);
        let block = ring.next_block().expect("欠载空窗应交付静音块而非 None");
        assert_eq!(block.len, 1024);
        assert!(unsafe { std::slice::from_raw_parts(block.data, block.len) }
            .iter()
            .all(|&b| b == DSD_SILENCE_BYTE));
    }
}
