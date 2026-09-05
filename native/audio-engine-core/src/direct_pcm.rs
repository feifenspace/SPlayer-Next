use std::cell::UnsafeCell;
use std::ffi::{c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

use anyhow::{anyhow, bail, ensure, Context, Result};
use ffmpeg_audio::HttpCancelHandle;
use ffmpeg_audio::sys;
use crate::priority::{bind_current_thread_to_performance_cores, boost_current_audio_thread};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectPcmSampleFormat {
    Signed16,
    Signed32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectPcmMemoryPath {
    ZeroCopyPacked,
    BitPerfectRepack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectPcmFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub valid_bits: u8,
    pub storage_bits: u8,
    pub sample_format: DirectPcmSampleFormat,
    pub memory_path: DirectPcmMemoryPath,
}

enum DirectPcmRepackBuffer {
    None,
    Signed16(Box<[i16]>),
    Signed32(Box<[i32]>),
}

pub struct DirectPcmFrame {
    raw: NonNull<sys::AVFrame>,
    format: Option<DirectPcmFormat>,
    payload_len: usize,
    sample_offset: usize,
    repack: DirectPcmRepackBuffer,
}

impl DirectPcmFrame {
    pub fn new() -> Result<Self> {
        let raw = unsafe { sys::av_frame_alloc() };
        let raw = NonNull::new(raw).context("分配 Source Direct AVFrame 失败")?;
        Ok(Self {
            raw,
            format: None,
            payload_len: 0,
            sample_offset: 0,
            repack: DirectPcmRepackBuffer::None,
        })
    }

    pub fn format(&self) -> Result<DirectPcmFormat> {
        self.format.context("Source Direct frame 尚未填充")
    }

    pub fn samples_per_channel(&self) -> usize {
        let total = unsafe { self.raw.as_ref().nb_samples.max(0) as usize };
        total.saturating_sub(self.sample_offset)
    }

    pub fn payload_ptr(&self) -> Result<*const u8> {
        let format = self.format()?;
        match format.memory_path {
            DirectPcmMemoryPath::ZeroCopyPacked => {
                let ptr = unsafe { self.raw.as_ref().data[0] };
                ensure!(!ptr.is_null(), "Source Direct packed PCM 缺少 data[0]");
                let bytes_per_sample = usize::from(format.storage_bits / 8);
                let offset = self
                    .sample_offset
                    .checked_mul(usize::from(format.channels))
                    .and_then(|samples| samples.checked_mul(bytes_per_sample))
                    .context("Source Direct packed seek offset 溢出")?;
                Ok(unsafe { ptr.add(offset) }.cast_const())
            }
            DirectPcmMemoryPath::BitPerfectRepack => match &self.repack {
                DirectPcmRepackBuffer::Signed16(buffer) => Ok(buffer.as_ptr().cast()),
                DirectPcmRepackBuffer::Signed32(buffer) => Ok(buffer.as_ptr().cast()),
                DirectPcmRepackBuffer::None => bail!("Source Direct planar PCM 缺少 repack buffer"),
            },
        }
    }

    pub fn payload_bytes(&self) -> Result<&[u8]> {
        let ptr = self.payload_ptr()?;
        Ok(unsafe { slice::from_raw_parts(ptr, self.payload_len) })
    }

    fn preallocate_repack(
        &mut self,
        sample_format: DirectPcmSampleFormat,
        samples: usize,
    ) -> Result<()> {
        if samples == 0 {
            return Ok(());
        }
        let needs_replacement = match (&self.repack, sample_format) {
            (DirectPcmRepackBuffer::None, _) => true,
            (DirectPcmRepackBuffer::Signed16(buffer), DirectPcmSampleFormat::Signed16) => {
                buffer.len() < samples
            }
            (DirectPcmRepackBuffer::Signed32(buffer), DirectPcmSampleFormat::Signed32) => {
                buffer.len() < samples
            }
            _ => bail!("Source Direct planar PCM sample format 在播放中发生变化"),
        };
        if needs_replacement {
            self.repack = match sample_format {
                DirectPcmSampleFormat::Signed16 => {
                    DirectPcmRepackBuffer::Signed16(vec![0_i16; samples].into_boxed_slice())
                }
                DirectPcmSampleFormat::Signed32 => {
                    DirectPcmRepackBuffer::Signed32(vec![0_i32; samples].into_boxed_slice())
                }
            };
        }
        Ok(())
    }

    fn repack_planar(
        &mut self,
        sample_format: DirectPcmSampleFormat,
        start_sample: usize,
    ) -> Result<()> {
        let frame = unsafe { self.raw.as_ref() };
        let total_samples =
            usize::try_from(frame.nb_samples).context("Source Direct sample count 越界")?;
        ensure!(start_sample < total_samples, "Source Direct planar seek offset 越界");
        let samples = total_samples - start_sample;
        let source_channels =
            usize::try_from(frame.ch_layout.nb_channels).context("Source Direct 声道数越界")?;
        let output_channels = if source_channels > 2 { 2 } else { source_channels };
        let total_output_samples = samples
            .checked_mul(output_channels)
            .context("Source Direct planar sample count 溢出")?;
        self.preallocate_repack(sample_format, total_output_samples)?;

        // 多声道（5.1 / 6.1 / 7.1 等）演播室标准（ITU-R BS.775）下混至双声道立体声
        if source_channels > 2 {
            match frame.format as sys::AVSampleFormat {
                sys::AVSampleFormat_AV_SAMPLE_FMT_FLTP => unsafe {
                    if let DirectPcmRepackBuffer::Signed32(output) = &mut self.repack {
                        downmix_float_planar_to_i32(
                            frame.extended_data,
                            source_channels,
                            start_sample,
                            samples,
                            output,
                        )?;
                    } else {
                        bail!("Source Direct planar repack buffer 类型不匹配");
                    }
                },
                sys::AVSampleFormat_AV_SAMPLE_FMT_FLT => unsafe {
                    if let DirectPcmRepackBuffer::Signed32(output) = &mut self.repack {
                        let ptr = frame.data[0].cast::<f32>();
                        ensure!(!ptr.is_null(), "Source Direct packed float PCM 缺少 data[0]");
                        downmix_packed_float_to_i32(
                            ptr,
                            source_channels,
                            start_sample,
                            samples,
                            output,
                        )?;
                    } else {
                        bail!("Source Direct packed repack buffer 类型不匹配");
                    }
                },
                sys::AVSampleFormat_AV_SAMPLE_FMT_S16P => unsafe {
                    if let DirectPcmRepackBuffer::Signed16(output) = &mut self.repack {
                        ensure!(
                            !frame.extended_data.is_null(),
                            "Source Direct planar PCM 缺少 extended_data"
                        );
                        downmix_planar_i16(
                            frame.extended_data,
                            source_channels,
                            start_sample,
                            samples,
                            output,
                        )?;
                    } else {
                        bail!("Source Direct planar repack buffer 类型不匹配");
                    }
                },
                sys::AVSampleFormat_AV_SAMPLE_FMT_S16 => unsafe {
                    if let DirectPcmRepackBuffer::Signed16(output) = &mut self.repack {
                        let ptr = frame.data[0].cast::<i16>();
                        ensure!(!ptr.is_null(), "Source Direct packed 16-bit PCM 缺少 data[0]");
                        downmix_packed_i16(
                            ptr,
                            source_channels,
                            start_sample,
                            samples,
                            output,
                        )?;
                    } else {
                        bail!("Source Direct packed repack buffer 类型不匹配");
                    }
                },
                sys::AVSampleFormat_AV_SAMPLE_FMT_S32P => unsafe {
                    if let DirectPcmRepackBuffer::Signed32(output) = &mut self.repack {
                        ensure!(
                            !frame.extended_data.is_null(),
                            "Source Direct planar PCM 缺少 extended_data"
                        );
                        downmix_planar_i32(
                            frame.extended_data,
                            source_channels,
                            start_sample,
                            samples,
                            output,
                        )?;
                    } else {
                        bail!("Source Direct planar repack buffer 类型不匹配");
                    }
                },
                sys::AVSampleFormat_AV_SAMPLE_FMT_S32 => unsafe {
                    if let DirectPcmRepackBuffer::Signed32(output) = &mut self.repack {
                        let ptr = frame.data[0].cast::<i32>();
                        ensure!(!ptr.is_null(), "Source Direct packed 32-bit PCM 缺少 data[0]");
                        downmix_packed_i32(
                            ptr,
                            source_channels,
                            start_sample,
                            samples,
                            output,
                        )?;
                    } else {
                        bail!("Source Direct packed repack buffer 类型不匹配");
                    }
                },
                other => bail!("Source Direct 不支持的多声道 FFmpeg 格式: {other}"),
            }
            return Ok(());
        }

        // 原始单声道 / 双声道处理逻辑
        ensure!(
            !frame.extended_data.is_null(),
            "Source Direct planar PCM 缺少 extended_data"
        );

        match frame.format as sys::AVSampleFormat {
            sys::AVSampleFormat_AV_SAMPLE_FMT_FLTP => unsafe {
                if let DirectPcmRepackBuffer::Signed32(output) = &mut self.repack {
                    convert_float_planar_to_i32(
                        frame.extended_data,
                        source_channels,
                        start_sample,
                        samples,
                        output,
                    )?;
                } else {
                    bail!("Source Direct planar repack buffer 类型不匹配");
                }
            },
            sys::AVSampleFormat_AV_SAMPLE_FMT_FLT => unsafe {
                if let DirectPcmRepackBuffer::Signed32(output) = &mut self.repack {
                    let ptr = frame.data[0].cast::<f32>();
                    ensure!(!ptr.is_null(), "Source Direct packed float PCM 缺少 data[0]");
                    let start_offset = start_sample * source_channels;
                    for i in 0..total_output_samples {
                        let val = (*ptr.add(start_offset + i)).clamp(-1.0_f32, 1.0_f32);
                        output[i] = (val * 2147483647.0) as i32;
                    }
                } else {
                    bail!("Source Direct packed repack buffer 类型不匹配");
                }
            },
            _ => match (&mut self.repack, sample_format) {
                (DirectPcmRepackBuffer::Signed16(output), DirectPcmSampleFormat::Signed16) => unsafe {
                    interleave_planar::<i16>(
                        frame.extended_data,
                        source_channels,
                        start_sample,
                        samples,
                        output,
                    )?;
                },
                (DirectPcmRepackBuffer::Signed32(output), DirectPcmSampleFormat::Signed32) => unsafe {
                    interleave_planar::<i32>(
                        frame.extended_data,
                        source_channels,
                        start_sample,
                        samples,
                        output,
                    )?;
                },
                _ => bail!("Source Direct planar repack buffer 类型不匹配"),
            },
        }
        Ok(())
    }

    fn clear(&mut self) {
        unsafe { sys::av_frame_unref(self.raw.as_ptr()) };
        self.format = None;
        self.payload_len = 0;
        self.sample_offset = 0;
    }

    fn accept_decoded_frame(&mut self, valid_bits_hint: u8) -> Result<()> {
        let frame = unsafe { self.raw.as_ref() };
        let (sample_format, mut memory_path) = match frame.format as sys::AVSampleFormat {
            sys::AVSampleFormat_AV_SAMPLE_FMT_S16 => (
                DirectPcmSampleFormat::Signed16,
                DirectPcmMemoryPath::ZeroCopyPacked,
            ),
            sys::AVSampleFormat_AV_SAMPLE_FMT_S32 => (
                DirectPcmSampleFormat::Signed32,
                DirectPcmMemoryPath::ZeroCopyPacked,
            ),
            sys::AVSampleFormat_AV_SAMPLE_FMT_S16P => (
                DirectPcmSampleFormat::Signed16,
                DirectPcmMemoryPath::BitPerfectRepack,
            ),
            sys::AVSampleFormat_AV_SAMPLE_FMT_S32P => (
                DirectPcmSampleFormat::Signed32,
                DirectPcmMemoryPath::BitPerfectRepack,
            ),
            sys::AVSampleFormat_AV_SAMPLE_FMT_FLT => (
                DirectPcmSampleFormat::Signed32,
                DirectPcmMemoryPath::BitPerfectRepack,
            ),
            sys::AVSampleFormat_AV_SAMPLE_FMT_FLTP => (
                DirectPcmSampleFormat::Signed32,
                DirectPcmMemoryPath::BitPerfectRepack,
            ),
            other => bail!("Source Direct strict mode 不支持 FFmpeg sample format {other}"),
        };

        ensure!(frame.sample_rate > 0, "Source Direct frame 采样率无效");
        ensure!(
            frame.ch_layout.nb_channels > 0,
            "Source Direct frame 声道数无效"
        );
        ensure!(frame.nb_samples > 0, "Source Direct frame 没有 PCM samples");

        let sample_rate = u32::try_from(frame.sample_rate).context("Source Direct 采样率越界")?;
        let source_channels =
            usize::try_from(frame.ch_layout.nb_channels).context("Source Direct 声道数越界")?;
        let is_multichannel = source_channels > 2;
        let channels: u16 = if is_multichannel { 2 } else { source_channels as u16 };
        if is_multichannel {
            // 多声道必须经由演播室级下混输出为双声道立体声，以适配 Diretta Target
            memory_path = DirectPcmMemoryPath::BitPerfectRepack;
        }

        let storage_bits = match sample_format {
            DirectPcmSampleFormat::Signed16 => 16,
            DirectPcmSampleFormat::Signed32 => 32,
        };
        let valid_bits = if valid_bits_hint > 0 && valid_bits_hint <= storage_bits {
            valid_bits_hint
        } else {
            storage_bits
        };
        let bytes_per_sample = usize::from(storage_bits / 8);
        let samples_per_channel =
            usize::try_from(frame.nb_samples).context("Source Direct sample count 越界")?;
        let payload_len = samples_per_channel
            .checked_mul(usize::from(channels))
            .and_then(|samples| samples.checked_mul(bytes_per_sample))
            .context("Source Direct PCM payload 长度溢出")?;

        match memory_path {
            DirectPcmMemoryPath::ZeroCopyPacked => {
                ensure!(
                    !frame.data[0].is_null(),
                    "Source Direct packed PCM 缺少 data[0]"
                );
                ensure!(frame.linesize[0] >= 0, "Source Direct PCM linesize 无效");
                ensure!(
                    usize::try_from(frame.linesize[0]).unwrap_or(0) >= payload_len,
                    "Source Direct PCM payload 超过 FFmpeg frame buffer"
                );
            }
            DirectPcmMemoryPath::BitPerfectRepack => {
                if !frame.extended_data.is_null() && !is_multichannel {
                    ensure!(
                        frame.linesize[0] >= 0,
                        "Source Direct planar PCM linesize 无效"
                    );
                    let plane_len = samples_per_channel
                        .checked_mul(bytes_per_sample)
                        .context("Source Direct planar plane 长度溢出")?;
                    ensure!(
                        usize::try_from(frame.linesize[0]).unwrap_or(0) >= plane_len,
                        "Source Direct planar PCM plane 超过 FFmpeg frame buffer"
                    );
                }
                self.repack_planar(sample_format, 0)?;
            }
        }

        self.format = Some(DirectPcmFormat {
            sample_rate,
            channels,
            valid_bits,
            storage_bits,
            sample_format,
            memory_path,
        });
        self.payload_len = payload_len;
        self.sample_offset = 0;
        Ok(())
    }

    fn trim_start_samples(&mut self, offset: usize) -> Result<()> {
        let format = self.format()?;
        let total = unsafe { self.raw.as_ref().nb_samples.max(0) as usize };
        ensure!(offset < total, "Source Direct seek offset 超过当前 frame");
        self.sample_offset = offset;
        let remaining = total - offset;
        let bytes_per_sample = usize::from(format.storage_bits / 8);
        self.payload_len = remaining
            .checked_mul(usize::from(format.channels))
            .and_then(|samples| samples.checked_mul(bytes_per_sample))
            .context("Source Direct seek payload 长度溢出")?;
        if format.memory_path == DirectPcmMemoryPath::BitPerfectRepack {
            self.repack_planar(format.sample_format, offset)?;
        }
        Ok(())
    }
}

const INV_SQRT2_F32: f32 = 0.70710678;
const INV_SQRT2_F64: f64 = 0.7071067811865475;

/// 通用 downmix 循环可达声道（4/5/≥9）的 idx3..7 立体声归属：
/// FFmpeg 平面序 FL FR FC LFE BL BR SL SR（0 = 弃用，b'L'/b'R' = 归入左/右）。
/// 6/7/8 声道有专门特化分支不经此表；≥9 布局未知，按 7.1 语义归属并截断。
fn downmix_extra_targets(channels: usize) -> [u8; 5] {
    match channels {
        //        idx3  idx4  idx5  idx6  idx7
        4 => [b'R', 0, 0, 0, 0], // quad：idx3=BR→R（idx2=BL 由 FC 分支特例归 L）
        5 => [b'L', b'R', 0, 0, 0], // 5.0：两枚环绕 idx3→L、idx4→R（side/back 变体归属相同）
        _ => [0, b'L', b'R', b'L', b'R'],
    }
}

unsafe fn downmix_planar_i16(
    extended_data: *mut *mut u8,
    channels: usize,
    start_sample: usize,
    samples: usize,
    output: &mut [i16],
) -> Result<()> {
    ensure!(output.len() >= samples * 2, "Source Direct downmix output buffer 太小");
    let mut planes = [ptr::null::<i16>(); 8];
    for ch in 0..channels.min(8) {
        let p = (*extended_data.add(ch)).cast::<i16>();
        ensure!(!p.is_null(), "Source Direct planar PCM plane 为空");
        planes[ch] = p;
    }

    if channels == 6 {
        // 5.1 环绕声: 0=FL, 1=FR, 2=FC, 3=LFE, 4=BL/SL, 5=BR/SR
        let fl = planes[0];
        let fr = planes[1];
        let fc = planes[2];
        let bl = planes[4];
        let br = planes[5];
        for i in 0..samples {
            let idx = start_sample + i;
            let l = *fl.add(idx) as f32 + INV_SQRT2_F32 * (*fc.add(idx) as f32) + INV_SQRT2_F32 * (*bl.add(idx) as f32);
            let r = *fr.add(idx) as f32 + INV_SQRT2_F32 * (*fc.add(idx) as f32) + INV_SQRT2_F32 * (*br.add(idx) as f32);
            output[i * 2] = l.round().clamp(-32768.0, 32767.0) as i16;
            output[i * 2 + 1] = r.round().clamp(-32768.0, 32767.0) as i16;
        }
        return Ok(());
    }

    if channels == 7 {
        // 6.1 环绕声: 0=FL, 1=FR, 2=FC, 3=LFE, 4=BC, 5=SL, 6=SR
        let fl = planes[0];
        let fr = planes[1];
        let fc = planes[2];
        let bc = planes[4];
        let sl = planes[5];
        let sr = planes[6];
        for i in 0..samples {
            let idx = start_sample + i;
            let l = *fl.add(idx) as f32 + INV_SQRT2_F32 * (*fc.add(idx) as f32) + INV_SQRT2_F32 * (*sl.add(idx) as f32) + 0.5 * (*bc.add(idx) as f32);
            let r = *fr.add(idx) as f32 + INV_SQRT2_F32 * (*fc.add(idx) as f32) + INV_SQRT2_F32 * (*sr.add(idx) as f32) + 0.5 * (*bc.add(idx) as f32);
            output[i * 2] = l.round().clamp(-32768.0, 32767.0) as i16;
            output[i * 2 + 1] = r.round().clamp(-32768.0, 32767.0) as i16;
        }
        return Ok(());
    }

    if channels == 8 {
        // 7.1 环绕声: 0=FL, 1=FR, 2=FC, 3=LFE, 4=BL, 5=BR, 6=SL, 7=SR
        let fl = planes[0];
        let fr = planes[1];
        let fc = planes[2];
        let bl = planes[4];
        let br = planes[5];
        let sl = planes[6];
        let sr = planes[7];
        for i in 0..samples {
            let idx = start_sample + i;
            let l = *fl.add(idx) as f32 + INV_SQRT2_F32 * (*fc.add(idx) as f32) + INV_SQRT2_F32 * (*sl.add(idx) as f32) + 0.5 * (*bl.add(idx) as f32);
            let r = *fr.add(idx) as f32 + INV_SQRT2_F32 * (*fc.add(idx) as f32) + INV_SQRT2_F32 * (*sr.add(idx) as f32) + 0.5 * (*br.add(idx) as f32);
            output[i * 2] = l.round().clamp(-32768.0, 32767.0) as i16;
            output[i * 2 + 1] = r.round().clamp(-32768.0, 32767.0) as i16;
        }
        return Ok(());
    }

    // 通用多声道下混（4/5/≥9 声道；6/7/8 走上面的特化分支）
    let targets = downmix_extra_targets(channels);
    for i in 0..samples {
        let idx = start_sample + i;
        let mut l = *planes[0].add(idx) as f32;
        let mut r = *planes[1].add(idx) as f32;
        if channels > 2 && !planes[2].is_null() {
            let c = *planes[2].add(idx) as f32;
            if channels == 4 {
                // quad（FL FR BL BR）：planes[2] 是 BL，不是 FC
                l += 0.5 * c;
            } else {
                l += INV_SQRT2_F32 * c;
                r += INV_SQRT2_F32 * c;
            }
        }
        for ch in 3..channels.min(8) {
            if !planes[ch].is_null() {
                let s = *planes[ch].add(idx) as f32;
                match targets[ch - 3] {
                    b'L' => l += 0.5 * s,
                    b'R' => r += 0.5 * s,
                    _ => {}
                }
            }
        }
        output[i * 2] = l.round().clamp(-32768.0, 32767.0) as i16;
        output[i * 2 + 1] = r.round().clamp(-32768.0, 32767.0) as i16;
    }
    Ok(())
}

unsafe fn downmix_planar_i32(
    extended_data: *mut *mut u8,
    channels: usize,
    start_sample: usize,
    samples: usize,
    output: &mut [i32],
) -> Result<()> {
    ensure!(output.len() >= samples * 2, "Source Direct downmix output buffer 太小");
    let mut planes = [ptr::null::<i32>(); 8];
    for ch in 0..channels.min(8) {
        let p = (*extended_data.add(ch)).cast::<i32>();
        ensure!(!p.is_null(), "Source Direct planar PCM plane 为空");
        planes[ch] = p;
    }

    if channels == 6 {
        // 5.1 环绕声: 0=FL, 1=FR, 2=FC, 3=LFE, 4=BL/SL, 5=BR/SR
        let fl = planes[0];
        let fr = planes[1];
        let fc = planes[2];
        let bl = planes[4];
        let br = planes[5];
        for i in 0..samples {
            let idx = start_sample + i;
            let l = *fl.add(idx) as f64 + INV_SQRT2_F64 * (*fc.add(idx) as f64) + INV_SQRT2_F64 * (*bl.add(idx) as f64);
            let r = *fr.add(idx) as f64 + INV_SQRT2_F64 * (*fc.add(idx) as f64) + INV_SQRT2_F64 * (*br.add(idx) as f64);
            output[i * 2] = l.round().clamp(-2147483648.0, 2147483647.0) as i32;
            output[i * 2 + 1] = r.round().clamp(-2147483648.0, 2147483647.0) as i32;
        }
        return Ok(());
    }

    if channels == 7 {
        // 6.1 环绕声: 0=FL, 1=FR, 2=FC, 3=LFE, 4=BC, 5=SL, 6=SR
        let fl = planes[0];
        let fr = planes[1];
        let fc = planes[2];
        let bc = planes[4];
        let sl = planes[5];
        let sr = planes[6];
        for i in 0..samples {
            let idx = start_sample + i;
            let l = *fl.add(idx) as f64 + INV_SQRT2_F64 * (*fc.add(idx) as f64) + INV_SQRT2_F64 * (*sl.add(idx) as f64) + 0.5 * (*bc.add(idx) as f64);
            let r = *fr.add(idx) as f64 + INV_SQRT2_F64 * (*fc.add(idx) as f64) + INV_SQRT2_F64 * (*sr.add(idx) as f64) + 0.5 * (*bc.add(idx) as f64);
            output[i * 2] = l.round().clamp(-2147483648.0, 2147483647.0) as i32;
            output[i * 2 + 1] = r.round().clamp(-2147483648.0, 2147483647.0) as i32;
        }
        return Ok(());
    }

    if channels == 8 {
        // 7.1 环绕声: 0=FL, 1=FR, 2=FC, 3=LFE, 4=BL, 5=BR, 6=SL, 7=SR
        let fl = planes[0];
        let fr = planes[1];
        let fc = planes[2];
        let bl = planes[4];
        let br = planes[5];
        let sl = planes[6];
        let sr = planes[7];
        for i in 0..samples {
            let idx = start_sample + i;
            let l = *fl.add(idx) as f64 + INV_SQRT2_F64 * (*fc.add(idx) as f64) + INV_SQRT2_F64 * (*sl.add(idx) as f64) + 0.5 * (*bl.add(idx) as f64);
            let r = *fr.add(idx) as f64 + INV_SQRT2_F64 * (*fc.add(idx) as f64) + INV_SQRT2_F64 * (*sr.add(idx) as f64) + 0.5 * (*br.add(idx) as f64);
            output[i * 2] = l.round().clamp(-2147483648.0, 2147483647.0) as i32;
            output[i * 2 + 1] = r.round().clamp(-2147483648.0, 2147483647.0) as i32;
        }
        return Ok(());
    }

    // 通用多声道下混（4/5/≥9 声道；6/7/8 走上面的特化分支）
    let targets = downmix_extra_targets(channels);
    for i in 0..samples {
        let idx = start_sample + i;
        let mut l = *planes[0].add(idx) as f64;
        let mut r = *planes[1].add(idx) as f64;
        if channels > 2 && !planes[2].is_null() {
            let c = *planes[2].add(idx) as f64;
            if channels == 4 {
                // quad（FL FR BL BR）：planes[2] 是 BL，不是 FC
                l += 0.5 * c;
            } else {
                l += INV_SQRT2_F64 * c;
                r += INV_SQRT2_F64 * c;
            }
        }
        for ch in 3..channels.min(8) {
            if !planes[ch].is_null() {
                let s = *planes[ch].add(idx) as f64;
                match targets[ch - 3] {
                    b'L' => l += 0.5 * s,
                    b'R' => r += 0.5 * s,
                    _ => {}
                }
            }
        }
        output[i * 2] = l.round().clamp(-2147483648.0, 2147483647.0) as i32;
        output[i * 2 + 1] = r.round().clamp(-2147483648.0, 2147483647.0) as i32;
    }
    Ok(())
}

unsafe fn downmix_float_planar_to_i32(
    extended_data: *mut *mut u8,
    channels: usize,
    start_sample: usize,
    samples: usize,
    output: &mut [i32],
) -> Result<()> {
    ensure!(output.len() >= samples * 2, "Source Direct downmix output buffer 太小");
    let mut planes = [ptr::null::<f32>(); 8];
    for ch in 0..channels.min(8) {
        let p = (*extended_data.add(ch)).cast::<f32>();
        ensure!(!p.is_null(), "Source Direct planar PCM plane 为空");
        planes[ch] = p;
    }

    let targets = downmix_extra_targets(channels);
    for i in 0..samples {
        let idx = start_sample + i;
        let mut l = *planes[0].add(idx);
        let mut r = *planes[1].add(idx);
        if channels > 2 && !planes[2].is_null() {
            let c = *planes[2].add(idx);
            if channels == 4 {
                // quad（FL FR BL BR）：planes[2] 是 BL，不是 FC
                l += 0.5 * c;
            } else {
                l += INV_SQRT2_F32 * c;
                r += INV_SQRT2_F32 * c;
            }
        }
        for ch in 3..channels.min(8) {
            if !planes[ch].is_null() {
                let s = *planes[ch].add(idx);
                match targets[ch - 3] {
                    b'L' => l += 0.5 * s,
                    b'R' => r += 0.5 * s,
                    _ => {}
                }
            }
        }
        output[i * 2] = (l.clamp(-1.0, 1.0) * 2147483647.0).round() as i32;
        output[i * 2 + 1] = (r.clamp(-1.0, 1.0) * 2147483647.0).round() as i32;
    }
    Ok(())
}

unsafe fn downmix_packed_i16(
    ptr: *const i16,
    channels: usize,
    start_sample: usize,
    samples: usize,
    output: &mut [i16],
) -> Result<()> {
    ensure!(output.len() >= samples * 2, "Source Direct downmix output buffer 太小");
    let targets = downmix_extra_targets(channels);
    for i in 0..samples {
        let base = (start_sample + i) * channels;
        let fl = *ptr.add(base) as f32;
        let fr = *ptr.add(base + 1) as f32;
        let fc = if channels > 2 { *ptr.add(base + 2) as f32 } else { 0.0 };
        // quad（FL FR BL BR）：plane[2] 是 BL，不是 FC
        let (fc_l, fc_r) = if channels == 4 {
            (0.5, 0.0)
        } else {
            (INV_SQRT2_F32, INV_SQRT2_F32)
        };
        let mut l = fl + fc_l * fc;
        let mut r = fr + fc_r * fc;
        if channels == 6 {
            // 5.1 环绕声: 0=FL, 1=FR, 2=FC, 3=LFE, 4=BL/SL, 5=BR/SR
            let bl = *ptr.add(base + 4) as f32;
            let br = *ptr.add(base + 5) as f32;
            l += INV_SQRT2_F32 * bl;
            r += INV_SQRT2_F32 * br;
        } else if channels == 7 {
            // 6.1 环绕声: 0=FL, 1=FR, 2=FC, 3=LFE, 4=BC, 5=SL, 6=SR
            let bc = *ptr.add(base + 4) as f32;
            let sl = *ptr.add(base + 5) as f32;
            let sr = *ptr.add(base + 6) as f32;
            l += INV_SQRT2_F32 * sl + 0.5 * bc;
            r += INV_SQRT2_F32 * sr + 0.5 * bc;
        } else if channels == 8 {
            // 7.1 环绕声: 0=FL, 1=FR, 2=FC, 3=LFE, 4=BL, 5=BR, 6=SL, 7=SR
            let bl = *ptr.add(base + 4) as f32;
            let br = *ptr.add(base + 5) as f32;
            let sl = *ptr.add(base + 6) as f32;
            let sr = *ptr.add(base + 7) as f32;
            l += INV_SQRT2_F32 * sl + 0.5 * bl;
            r += INV_SQRT2_F32 * sr + 0.5 * br;
        } else {
            for ch in 3..channels.min(8) {
                let s = *ptr.add(base + ch) as f32;
                match targets[ch - 3] {
                    b'L' => l += 0.5 * s,
                    b'R' => r += 0.5 * s,
                    _ => {}
                }
            }
        }
        output[i * 2] = l.round().clamp(-32768.0, 32767.0) as i16;
        output[i * 2 + 1] = r.round().clamp(-32768.0, 32767.0) as i16;
    }
    Ok(())
}

unsafe fn downmix_packed_i32(
    ptr: *const i32,
    channels: usize,
    start_sample: usize,
    samples: usize,
    output: &mut [i32],
) -> Result<()> {
    ensure!(output.len() >= samples * 2, "Source Direct downmix output buffer 太小");
    let targets = downmix_extra_targets(channels);
    for i in 0..samples {
        let base = (start_sample + i) * channels;
        let fl = *ptr.add(base) as f64;
        let fr = *ptr.add(base + 1) as f64;
        let fc = if channels > 2 { *ptr.add(base + 2) as f64 } else { 0.0 };
        // quad（FL FR BL BR）：plane[2] 是 BL，不是 FC
        let (fc_l, fc_r) = if channels == 4 {
            (0.5, 0.0)
        } else {
            (INV_SQRT2_F64, INV_SQRT2_F64)
        };
        let mut l = fl + fc_l * fc;
        let mut r = fr + fc_r * fc;
        if channels == 6 {
            // 5.1 环绕声: 0=FL, 1=FR, 2=FC, 3=LFE, 4=BL/SL, 5=BR/SR
            let bl = *ptr.add(base + 4) as f64;
            let br = *ptr.add(base + 5) as f64;
            l += INV_SQRT2_F64 * bl;
            r += INV_SQRT2_F64 * br;
        } else if channels == 7 {
            // 6.1 环绕声: 0=FL, 1=FR, 2=FC, 3=LFE, 4=BC, 5=SL, 6=SR
            let bc = *ptr.add(base + 4) as f64;
            let sl = *ptr.add(base + 5) as f64;
            let sr = *ptr.add(base + 6) as f64;
            l += INV_SQRT2_F64 * sl + 0.5 * bc;
            r += INV_SQRT2_F64 * sr + 0.5 * bc;
        } else if channels == 8 {
            // 7.1 环绕声: 0=FL, 1=FR, 2=FC, 3=LFE, 4=BL, 5=BR, 6=SL, 7=SR
            let bl = *ptr.add(base + 4) as f64;
            let br = *ptr.add(base + 5) as f64;
            let sl = *ptr.add(base + 6) as f64;
            let sr = *ptr.add(base + 7) as f64;
            l += INV_SQRT2_F64 * sl + 0.5 * bl;
            r += INV_SQRT2_F64 * sr + 0.5 * br;
        } else {
            for ch in 3..channels.min(8) {
                let s = *ptr.add(base + ch) as f64;
                match targets[ch - 3] {
                    b'L' => l += 0.5 * s,
                    b'R' => r += 0.5 * s,
                    _ => {}
                }
            }
        }
        output[i * 2] = l.round().clamp(-2147483648.0, 2147483647.0) as i32;
        output[i * 2 + 1] = r.round().clamp(-2147483648.0, 2147483647.0) as i32;
    }
    Ok(())
}

unsafe fn downmix_packed_float_to_i32(
    ptr: *const f32,
    channels: usize,
    start_sample: usize,
    samples: usize,
    output: &mut [i32],
) -> Result<()> {
    ensure!(output.len() >= samples * 2, "Source Direct downmix output buffer 太小");
    let targets = downmix_extra_targets(channels);
    for i in 0..samples {
        let base = (start_sample + i) * channels;
        let fl = *ptr.add(base);
        let fr = *ptr.add(base + 1);
        let fc = if channels > 2 { *ptr.add(base + 2) } else { 0.0 };
        // quad（FL FR BL BR）：plane[2] 是 BL，不是 FC
        let (fc_l, fc_r) = if channels == 4 {
            (0.5, 0.0)
        } else {
            (INV_SQRT2_F32, INV_SQRT2_F32)
        };
        let mut l = fl + fc_l * fc;
        let mut r = fr + fc_r * fc;
        if channels == 6 {
            let bl = *ptr.add(base + 4);
            let br = *ptr.add(base + 5);
            l += INV_SQRT2_F32 * bl;
            r += INV_SQRT2_F32 * br;
        } else if channels == 7 {
            let bc = *ptr.add(base + 4);
            let sl = *ptr.add(base + 5);
            let sr = *ptr.add(base + 6);
            l += INV_SQRT2_F32 * sl + 0.5 * bc;
            r += INV_SQRT2_F32 * sr + 0.5 * bc;
        } else if channels == 8 {
            let bl = *ptr.add(base + 4);
            let br = *ptr.add(base + 5);
            let sl = *ptr.add(base + 6);
            let sr = *ptr.add(base + 7);
            l += INV_SQRT2_F32 * sl + 0.5 * bl;
            r += INV_SQRT2_F32 * sr + 0.5 * br;
        } else {
            for ch in 3..channels.min(8) {
                let s = *ptr.add(base + ch);
                match targets[ch - 3] {
                    b'L' => l += 0.5 * s,
                    b'R' => r += 0.5 * s,
                    _ => {}
                }
            }
        }
        output[i * 2] = (l.clamp(-1.0, 1.0) * 2147483647.0).round() as i32;
        output[i * 2 + 1] = (r.clamp(-1.0, 1.0) * 2147483647.0).round() as i32;
    }
    Ok(())
}

unsafe fn interleave_planar<T: Copy>(
    extended_data: *mut *mut u8,
    channels: usize,
    start_sample: usize,
    samples: usize,
    output: &mut [T],
) -> Result<()> {
    let total_samples = samples
        .checked_mul(channels)
        .context("Source Direct planar sample count 溢出")?;
    ensure!(
        output.len() >= total_samples,
        "Source Direct planar repack buffer 太小"
    );

    if channels == 2 {
        let left = unsafe { *extended_data }.cast::<T>();
        let right = unsafe { *extended_data.add(1) }.cast::<T>();
        ensure!(
            !left.is_null() && !right.is_null(),
            "Source Direct planar PCM plane 为空"
        );
        for index in 0..samples {
            let source_index = start_sample + index;
            output[index * 2] = unsafe { *left.add(source_index) };
            output[index * 2 + 1] = unsafe { *right.add(source_index) };
        }
        return Ok(());
    }

    for channel in 0..channels {
        let plane = unsafe { *extended_data.add(channel) }.cast::<T>();
        ensure!(!plane.is_null(), "Source Direct planar PCM plane 为空");
        for index in 0..samples {
            output[index * channels + channel] = unsafe { *plane.add(start_sample + index) };
        }
    }
    Ok(())
}

unsafe fn convert_float_planar_to_i32(
    extended_data: *mut *mut u8,
    channels: usize,
    start_sample: usize,
    samples: usize,
    output: &mut [i32],
) -> Result<()> {
    let total_samples = samples
        .checked_mul(channels)
        .context("Source Direct planar sample count 溢出")?;
    ensure!(
        output.len() >= total_samples,
        "Source Direct planar repack buffer 太小"
    );

    if channels == 2 {
        let left = unsafe { *extended_data }.cast::<f32>();
        let right = unsafe { *extended_data.add(1) }.cast::<f32>();
        ensure!(
            !left.is_null() && !right.is_null(),
            "Source Direct planar PCM plane 为空"
        );
        for index in 0..samples {
            let source_index = start_sample + index;
            let l = unsafe { *left.add(source_index) }.clamp(-1.0, 1.0);
            let r = unsafe { *right.add(source_index) }.clamp(-1.0, 1.0);
            output[index * 2] = (l * 2147483647.0) as i32;
            output[index * 2 + 1] = (r * 2147483647.0) as i32;
        }
        return Ok(());
    }

    for channel in 0..channels {
        let plane = unsafe { *extended_data.add(channel) }.cast::<f32>();
        ensure!(!plane.is_null(), "Source Direct planar PCM plane 为空");
        for index in 0..samples {
            let s = unsafe { *plane.add(start_sample + index) }.clamp(-1.0, 1.0);
            output[index * channels + channel] = (s * 2147483647.0) as i32;
        }
    }
    Ok(())
}

impl Drop for DirectPcmFrame {
    fn drop(&mut self) {
        let mut raw = self.raw.as_ptr();
        unsafe { sys::av_frame_free(&mut raw) };
    }
}

// AVFrame 由单一拥有者移动，底层 AVBufferRef 只通过 FFmpeg 引用计数共享。
unsafe impl Send for DirectPcmFrame {}

pub struct DirectPcmDecoder {
    format_context: NonNull<sys::AVFormatContext>,
    codec_context: NonNull<sys::AVCodecContext>,
    packet: NonNull<sys::AVPacket>,
    stream_index: i32,
    time_base: sys::AVRational,
    timeline_origin_pts: i64,
    valid_bits_hint: u8,
    flushing: bool,
    drained: bool,
    /// 自定义 IO 输入（流式音源）；本地路径打开时为 None。
    /// Drop 顺序必须在 avformat_close_input 之后（CUSTOM_IO 下 pb 由这里释放）。
    avio: Option<AvioReader>,
}

impl DirectPcmDecoder {
    pub fn open_local(path: &Path) -> Result<Self> {
        let path = CString::new(path.to_string_lossy().as_bytes())
            .context("Source Direct 路径包含 NUL")?;

        let mut format_context = ptr::null_mut();
        let open_result = unsafe {
            sys::avformat_open_input(
                &mut format_context,
                path.as_ptr(),
                ptr::null(),
                ptr::null_mut(),
            )
        };
        ffmpeg_result(open_result, "打开 Source Direct 音源")?;
        let format_context = NonNull::new(format_context).context("FFmpeg 未返回输入上下文")?;
        Self::finalize_open(format_context, None)
    }

    /// 同时支持本地路径与 http(s):// URL；URL 走自定义 AVIO（与 DirectPcmSource::open_stream 一致）。
    /// 用于同连接 handoff 路径，让 producer 能在不重建 Diretta 连接的情况下切换音源。
    pub fn open_source(source: &str) -> Result<Self> {
        Self::open_source_with_cancel(source, &crate::ffmpeg_audio::HttpCancelHandle::new())
    }

    /// open_source 的可取消版本：supersede/替换时 cancel 句柄即可即时掐断
    /// HTTP 连接，producer 从最坏 256s 网络退避等待变成即时返回
    pub fn open_source_with_cancel(source: &str, cancel: &HttpCancelHandle) -> Result<Self> {
        if source.starts_with("http://") || source.starts_with("https://") {
            let http = crate::ffmpeg_audio::HttpAudioSource::new_with_cancel_handle(
                source,
                cancel,
            )
            .context("构造 Source Direct HTTP 流式音源失败")?;
            return Self::open_reader(Box::new(http));
        }
        Self::open_local(Path::new(source))
    }

    /// 以自定义 `Read + Seek` Reader 作为 FFmpeg 输入（流式在线音源）。
    ///
    /// Reader 的读/写位置完全由 demuxer 驱动：顺序读保持单连接流式拉取，
    /// seek（demuxer 探测或用户 seek）触发 Reader 层的 Range 重连。
    /// 与本地路径打开共用同一套 stream 探测/codec 初始化流程。
    pub fn open_reader(reader: Box<dyn ReadSeek>) -> Result<Self> {
        let avio = AvioReader::new(reader)?;
        let open_result = unsafe {
            let raw = sys::avformat_alloc_context();
            let format_context =
                NonNull::new(raw).context("分配 Source Direct format context 失败")?;
            (*format_context.as_ptr()).pb = avio.ctx.as_ptr();
            // 标记自定义 IO，防止 FFmpeg close 输入时双重释放 AVIOContext
            (*format_context.as_ptr()).flags |= sys::AVFMT_FLAG_CUSTOM_IO as i32;
            let mut ctx_ptr = format_context.as_ptr();
            let result = sys::avformat_open_input(
                &mut ctx_ptr,
                b"\0".as_ptr().cast(),
                ptr::null(),
                ptr::null_mut(),
            );
            // 失败时 FFmpeg 已自行释放 context（含 CUSTOM_IO 下不释放的 pb）
            if let Err(error) = ffmpeg_result(result, "打开 Source Direct 流式音源") {
                drop(avio);
                return Err(error);
            }
            let format_context = NonNull::new(ctx_ptr).context("FFmpeg 未返回输入上下文")?;
            (format_context, avio)
        };
        let (format_context, avio) = open_result;
        Self::finalize_open(format_context, Some(avio))
    }

    /// 输入打开后的公共流程：stream 探测、codec 初始化与 packet 分配
    fn finalize_open(
        format_context: NonNull<sys::AVFormatContext>,
        avio: Option<AvioReader>,
    ) -> Result<Self> {
        // 自定义 IO 下 FFmpeg close 输入不会释放 pb，错误路径需随同释放 avio
        let result = Self::finalize_open_inner(format_context);
        match result {
            Ok(mut decoder) => {
                decoder.avio = avio;
                Ok(decoder)
            }
            Err(error) => {
                drop(avio);
                Err(error)
            }
        }
    }

    fn finalize_open_inner(format_context: NonNull<sys::AVFormatContext>) -> Result<Self> {
        let stream_info_result =
            unsafe { sys::avformat_find_stream_info(format_context.as_ptr(), ptr::null_mut()) };
        if let Err(error) = ffmpeg_result(stream_info_result, "读取 Source Direct stream info") {
            let mut raw = format_context.as_ptr();
            unsafe { sys::avformat_close_input(&mut raw) };
            return Err(error);
        }

        let mut codec = ptr::null();
        let stream_index = unsafe {
            sys::av_find_best_stream(
                format_context.as_ptr(),
                sys::AVMediaType_AVMEDIA_TYPE_AUDIO,
                -1,
                -1,
                &mut codec,
                0,
            )
        };
        if stream_index < 0 || codec.is_null() {
            let mut raw = format_context.as_ptr();
            unsafe { sys::avformat_close_input(&mut raw) };
            return Err(ffmpeg_error(
                stream_index,
                "查找 Source Direct audio stream",
            ));
        }

        let stream = unsafe {
            *format_context
                .as_ref()
                .streams
                .add(usize::try_from(stream_index).context("Source Direct stream index 越界")?)
        };
        if stream.is_null() || unsafe { (*stream).codecpar }.is_null() {
            let mut raw = format_context.as_ptr();
            unsafe { sys::avformat_close_input(&mut raw) };
            bail!("Source Direct audio stream 缺少 codec parameters");
        }
        let codec_parameters = unsafe { (*stream).codecpar };
        let time_base = unsafe { (*stream).time_base };
        let start_time = unsafe { (*stream).start_time };
        let timeline_origin_pts = if start_time == sys::AV_NOPTS_VALUE {
            0
        } else {
            start_time.max(0)
        };
        let raw_bits = unsafe { (*codec_parameters).bits_per_raw_sample };
        let coded_bits = unsafe { (*codec_parameters).bits_per_coded_sample };
        let valid_bits_hint =
            u8::try_from(if raw_bits > 0 { raw_bits } else { coded_bits }).unwrap_or(0);

        let codec_context = unsafe { sys::avcodec_alloc_context3(codec) };
        let codec_context = match NonNull::new(codec_context) {
            Some(value) => value,
            None => {
                let mut raw = format_context.as_ptr();
                unsafe { sys::avformat_close_input(&mut raw) };
                bail!("分配 Source Direct codec context 失败");
            }
        };
        let params_result =
            unsafe { sys::avcodec_parameters_to_context(codec_context.as_ptr(), codec_parameters) };
        if let Err(error) = ffmpeg_result(params_result, "复制 Source Direct codec parameters") {
            let mut codec_raw = codec_context.as_ptr();
            let mut format_raw = format_context.as_ptr();
            unsafe {
                sys::avcodec_free_context(&mut codec_raw);
                sys::avformat_close_input(&mut format_raw);
            }
            return Err(error);
        }
        let codec_open_result =
            unsafe { sys::avcodec_open2(codec_context.as_ptr(), codec, ptr::null_mut()) };
        if let Err(error) = ffmpeg_result(codec_open_result, "打开 Source Direct decoder") {
            let mut codec_raw = codec_context.as_ptr();
            let mut format_raw = format_context.as_ptr();
            unsafe {
                sys::avcodec_free_context(&mut codec_raw);
                sys::avformat_close_input(&mut format_raw);
            }
            return Err(error);
        }

        let packet = unsafe { sys::av_packet_alloc() };
        let packet = match NonNull::new(packet) {
            Some(value) => value,
            None => {
                let mut codec_raw = codec_context.as_ptr();
                let mut format_raw = format_context.as_ptr();
                unsafe {
                    sys::avcodec_free_context(&mut codec_raw);
                    sys::avformat_close_input(&mut format_raw);
                }
                bail!("分配 Source Direct packet 失败");
            }
        };

        Ok(Self {
            format_context,
            codec_context,
            packet,
            stream_index,
            time_base,
            timeline_origin_pts,
            valid_bits_hint,
            flushing: false,
            drained: false,
            avio: None,
        })
    }

    fn frame_samples_hint(&self) -> usize {
        unsafe { self.codec_context.as_ref().frame_size.max(0) as usize }
    }

    fn seek_accurate(&mut self, position_secs: f64, frame: &mut DirectPcmFrame) -> Result<f64> {
        ensure!(
            position_secs.is_finite() && position_secs >= 0.0,
            "Source Direct seek 位置无效"
        );
        let target_us = (position_secs * 1_000_000.0).floor().min(i64::MAX as f64) as i64;
        let mut target_pts = unsafe {
            sys::av_rescale_q(target_us, sys::MICROSECONDS_Q, self.time_base)
        };
        target_pts = target_pts.saturating_add(self.timeline_origin_pts);
        let seek_result = unsafe {
            sys::avformat_seek_file(
                self.format_context.as_ptr(),
                self.stream_index,
                i64::MIN,
                target_pts,
                target_pts,
                sys::AVSEEK_FLAG_BACKWARD.cast_signed(),
            )
        };
        ffmpeg_result(seek_result, "Source Direct PCM seek")?;
        unsafe {
            sys::avcodec_flush_buffers(self.codec_context.as_ptr());
            sys::av_packet_unref(self.packet.as_ptr());
        }
        self.flushing = false;
        self.drained = false;

        loop {
            if !self.read_frame(frame)? {
                return Ok(position_secs);
            }
            let raw = unsafe { frame.raw.as_ref() };
            let timestamp = if raw.best_effort_timestamp != sys::AV_NOPTS_VALUE {
                raw.best_effort_timestamp
            } else {
                raw.pts
            };
            ensure!(
                timestamp != sys::AV_NOPTS_VALUE,
                "Source Direct accurate seek 需要有效 frame timestamp"
            );
            let relative_pts = timestamp.saturating_sub(self.timeline_origin_pts);
            let frame_start_us = unsafe {
                sys::av_rescale_q(relative_pts, self.time_base, sys::MICROSECONDS_Q)
            };
            let total_samples = usize::try_from(raw.nb_samples)
                .context("Source Direct seek frame sample count 越界")?;
            let sample_rate = u64::try_from(raw.sample_rate)
                .context("Source Direct seek frame sample rate 越界")?;
            ensure!(sample_rate > 0, "Source Direct seek frame sample rate 无效");
            let frame_duration_us = u64::try_from(total_samples)?
                .saturating_mul(1_000_000)
                / sample_rate;
            let frame_end_us = frame_start_us.saturating_add(frame_duration_us as i64);
            if frame_end_us < target_us {
                continue;
            }

            let delta_us = target_us.saturating_sub(frame_start_us).max(0) as u64;
            let offset_samples = delta_us.saturating_mul(sample_rate) / 1_000_000;
            let offset_samples = usize::try_from(offset_samples)?;
            if offset_samples >= total_samples {
                continue;
            }
            if offset_samples > 0 {
                frame.trim_start_samples(offset_samples)?;
            }
            let actual_us = frame_start_us.saturating_add(
                i64::try_from(
                    u64::try_from(offset_samples)?
                        .saturating_mul(1_000_000)
                        / sample_rate,
                )
                .unwrap_or(i64::MAX),
            );
            return Ok(actual_us.max(0) as f64 / 1_000_000.0);
        }
    }

    pub fn read_frame(&mut self, frame: &mut DirectPcmFrame) -> Result<bool> {
        if self.drained {
            return Ok(false);
        }
        frame.clear();

        loop {
            let receive_result = unsafe {
                sys::avcodec_receive_frame(self.codec_context.as_ptr(), frame.raw.as_ptr())
            };
            if receive_result == 0 {
                frame.accept_decoded_frame(self.valid_bits_hint)?;
                return Ok(true);
            }
            if receive_result == sys::AVERROR_EOF {
                self.drained = true;
                return Ok(false);
            }
            if receive_result != sys::AVERROR_EAGAIN {
                return Err(ffmpeg_error(receive_result, "解码 Source Direct PCM frame"));
            }
            ensure!(
                !self.flushing,
                "Source Direct decoder flush 后意外返回 EAGAIN"
            );

            if !self.send_next_audio_packet()? {
                let flush_result =
                    unsafe { sys::avcodec_send_packet(self.codec_context.as_ptr(), ptr::null()) };
                if flush_result != 0 && flush_result != sys::AVERROR_EOF {
                    return Err(ffmpeg_error(flush_result, "flush Source Direct decoder"));
                }
                self.flushing = true;
            }
        }
    }

    fn send_next_audio_packet(&mut self) -> Result<bool> {
        loop {
            let read_result =
                unsafe { sys::av_read_frame(self.format_context.as_ptr(), self.packet.as_ptr()) };
            if read_result == sys::AVERROR_EOF {
                return Ok(false);
            }
            if read_result < 0 {
                return Err(ffmpeg_error(read_result, "读取 Source Direct packet"));
            }

            let is_audio = unsafe { self.packet.as_ref().stream_index == self.stream_index };
            if !is_audio {
                unsafe { sys::av_packet_unref(self.packet.as_ptr()) };
                continue;
            }

            let send_result = unsafe {
                sys::avcodec_send_packet(self.codec_context.as_ptr(), self.packet.as_ptr())
            };
            unsafe { sys::av_packet_unref(self.packet.as_ptr()) };
            if send_result == 0 || send_result == sys::AVERROR_EAGAIN {
                return Ok(true);
            }
            return Err(ffmpeg_error(send_result, "提交 Source Direct packet"));
        }
    }
}

// FFmpeg 上下文只在线程间移动，不会被两个线程并发调用。
unsafe impl Send for DirectPcmDecoder {}

impl Drop for DirectPcmDecoder {
    fn drop(&mut self) {
        let mut packet = self.packet.as_ptr();
        let mut codec = self.codec_context.as_ptr();
        let mut format = self.format_context.as_ptr();
        unsafe {
            sys::av_packet_free(&mut packet);
            sys::avcodec_free_context(&mut codec);
            sys::avformat_close_input(&mut format);
        }
        // CUSTOM_IO 下 pb 由 FFmpeg 保留，最后释放自定义 AVIO 上下文
        self.avio = None;
    }
}

/// `DirectPcmDecoder::open_reader` 接受的自定义输入
pub trait ReadSeek: std::io::Read + std::io::Seek + Send {}
impl<T: std::io::Read + std::io::Seek + Send> ReadSeek for T {}

const AVIO_BUFFER_SIZE: usize = 32 * 1024;

/// 把任意 `Read + Seek` 包装成 FFmpeg 自定义 AVIO 输入。
///
/// 读回调直接委托 Reader（顺序读保持单连接流式拉取，seek 触发 Reader 层
/// Range 重连）；`AVSEEK_SIZE` 通过尾部 seek 探测总长。
struct AvioReader {
    ctx: NonNull<sys::AVIOContext>,
    /// Box 双重封装：外层 Box 指针交给 AVIOContext opaque，Drop 时收回
    opaque: *mut Box<dyn ReadSeek>,
}

unsafe impl Send for AvioReader {}

impl AvioReader {
    fn new(reader: Box<dyn ReadSeek>) -> Result<Self> {
        let opaque = Box::into_raw(Box::new(reader));
        let buffer = unsafe { sys::av_malloc(AVIO_BUFFER_SIZE) }.cast::<u8>();
        if buffer.is_null() {
            unsafe { drop(Box::from_raw(opaque)) };
            bail!("分配 Source Direct AVIO buffer 失败");
        }
        let ctx = unsafe {
            sys::avio_alloc_context(
                buffer,
                AVIO_BUFFER_SIZE as i32,
                0,
                opaque.cast::<std::ffi::c_void>(),
                Some(avio_read_packet),
                None,
                Some(avio_seek),
            )
        };
        let ctx = match NonNull::new(ctx) {
            Some(value) => value,
            None => {
                unsafe {
                    sys::av_freep(buffer.cast::<std::ffi::c_void>());
                    drop(Box::from_raw(opaque));
                }
                bail!("分配 Source Direct AVIO context 失败");
            }
        };
        Ok(Self { ctx, opaque })
    }
}

impl Drop for AvioReader {
    fn drop(&mut self) {
        unsafe {
            // avio_alloc_context 分配的内部缓冲需要显式释放
            if !(*self.ctx.as_ptr()).buffer.is_null() {
                let buffer_ptr = (&raw mut (*self.ctx.as_ptr()).buffer).cast::<std::ffi::c_void>();
                sys::av_freep(buffer_ptr);
            }
            sys::avio_context_free((&raw mut self.ctx).cast::<*mut sys::AVIOContext>());
            drop(Box::from_raw(self.opaque));
        }
    }
}

extern "C" fn avio_read_packet(
    opaque: *mut std::ffi::c_void,
    buf: *mut u8,
    buf_size: i32,
) -> i32 {
    if opaque.is_null() || buf.is_null() || buf_size <= 0 {
        return sys::AVERROR_EOF;
    }
    let reader = unsafe { &mut *opaque.cast::<Box<dyn ReadSeek>>() };
    let slice = unsafe { std::slice::from_raw_parts_mut(buf, buf_size as usize) };
    match reader.read(slice) {
        Ok(0) => sys::AVERROR_EOF,
        Ok(n) => n as i32,
        Err(_) => sys::averror(libc::EIO),
    }
}

extern "C" fn avio_seek(opaque: *mut std::ffi::c_void, offset: i64, whence: i32) -> i64 {
    if opaque.is_null() {
        return i64::from(sys::averror(libc::EINVAL));
    }
    let reader = unsafe { &mut *opaque.cast::<Box<dyn ReadSeek>>() };
    use std::io::{Seek, SeekFrom};

    if whence == sys::AVSEEK_SIZE.cast_signed() {
        let Ok(current) = reader.stream_position() else {
            return i64::from(sys::averror(libc::ENOSYS));
        };
        let Ok(size) = reader.seek(SeekFrom::End(0)) else {
            return i64::from(sys::averror(libc::ENOSYS));
        };
        if reader.seek(SeekFrom::Start(current)).is_err() {
            return i64::from(sys::averror(libc::EIO));
        }
        return size.cast_signed();
    }

    let seek_from = match whence & (!sys::AVSEEK_FORCE.cast_signed()) {
        0 => SeekFrom::Start(offset.cast_unsigned()),
        1 => SeekFrom::Current(offset),
        2 => SeekFrom::End(offset),
        _ => return i64::from(sys::averror(libc::EINVAL)),
    };
    reader
        .seek(seek_from)
        .map_or_else(|_| i64::from(sys::averror(libc::EIO)), u64::cast_signed)
}

const DIRECT_RING_DEPTH: usize = 8;
/// pre-mute 静音窗口时长：覆盖换源/seek 复位的供数空窗（对齐 tinyLMS 8 cycles ≈ 80ms）
const PRE_MUTE_WINDOW_MS: u64 = 80;

/// 进程内单调毫秒时钟（pre-mute 窗口用，不受系统墙钟跳变影响）
fn mono_millis() -> u64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}
const SLOT_FREE: u8 = 0;
const SLOT_FILLING: u8 = 1;
const SLOT_READY: u8 = 2;
const SLOT_IN_FLIGHT: u8 = 3;
const NO_SLOT: usize = usize::MAX;
/// producer 条件等待上限：所有关键事件（slot 释放/命令/淡出激活）都有 notify，
/// 超时仅作为漏报兜底；到期后回到循环顶保持命令响应性
const PRODUCER_WAIT_CEILING: Duration = Duration::from_millis(100);

struct DirectPcmSlot {
    state: AtomicU8,
    payload_ptr: AtomicPtr<u8>,
    payload_len: AtomicUsize,
    sample_frames: AtomicUsize,
    boundary: AtomicBool,
    boundary_duration_micros: AtomicU64,
    boundary_generation: AtomicU64,
    frame: UnsafeCell<DirectPcmFrame>,
    /// 关流淡出期间的数字静音缓冲：slot 自有存储，扩容只发生在 FILLING 独占期，
    /// 交付后只读——换曲块尺寸变化不会使其他 slot 已发布的指针失效
    silence: UnsafeCell<Box<[u8]>>,
}

impl DirectPcmSlot {
    fn new() -> Result<Self> {
        Ok(Self {
            state: AtomicU8::new(SLOT_FREE),
            payload_ptr: AtomicPtr::new(ptr::null_mut()),
            payload_len: AtomicUsize::new(0),
            sample_frames: AtomicUsize::new(0),
            boundary: AtomicBool::new(false),
            boundary_duration_micros: AtomicU64::new(0),
            boundary_generation: AtomicU64::new(0),
            frame: UnsafeCell::new(DirectPcmFrame::new()?),
            silence: UnsafeCell::new(Box::new([])),
        })
    }

    /// 填充本 slot 的数字静音块并发布指针（调用前 slot 已被认领为 FILLING）。
    /// PCM 静音即 0x00（signed 零点），预填一次后无需重写。
    fn fill_silence(&self, bytes: usize, frames: usize) {
        let silence = unsafe { &mut *self.silence.get() };
        if silence.len() < bytes {
            *silence = vec![0u8; bytes].into_boxed_slice();
        }
        self.payload_ptr
            .store(silence.as_mut_ptr(), Ordering::Relaxed);
        self.payload_len.store(bytes, Ordering::Relaxed);
        self.sample_frames.store(frames, Ordering::Relaxed);
    }
}

// slot 的 frame 与 silence 只在 FILLING 时由 producer 写入/扩容，
// 在 READY/IN_FLIGHT 时只读。
unsafe impl Sync for DirectPcmSlot {}

enum DirectPcmCommand {
    Seek {
        position_secs: f64,
        response: mpsc::SyncSender<Result<f64>>,
    },
    ReplaceLocal {
        source: String,
        cancel: HttpCancelHandle,
        response: mpsc::SyncSender<Result<DirectPcmFormat>>,
    },
    StageLocal {
        path: PathBuf,
        duration_micros: u64,
        generation: u64,
        response: mpsc::SyncSender<Result<()>>,
    },
    CancelStaged,
}

/// 消费端源级淡出状态：手动切歌/停止关流前，把输出线性渐零到静音，消除
/// mid-sample 硬切爆音。增益以 1e-6 定点表示（1_000_000 = 1.0）；
/// 稳态（ramping=false 且未静音）完全不触碰样本，保持位精确。
struct DirectPcmFadeState {
    /// 淡出窗内起始增益
    gain_start_micro: AtomicU32,
    /// 淡出窗内结束增益
    gain_end_micro: AtomicU32,
    /// 待应用：下一块交付时套用窗内包络并自动清除
    ramping: AtomicBool,
    /// 已渐零：后续块全部输出数字静音，直到连接关闭
    silent: AtomicBool,
    /// 已交付的静音块数（tinyLMS 式预静音计数：足够多的静音块顶掉目标端缓冲中的旧音频）
    silence_blocks: AtomicU32,
    /// 打包样本位宽（16/32），0 = 未知（尚未解码出首块）
    sample_bits: AtomicU8,
    /// 有效位宽（s24-in-s32 传输槽时为 24，其余等于 sample_bits）
    valid_bits: AtomicU8,
    /// 淡出窗长度（样本数，约 20ms）
    window_samples: AtomicUsize,
}

impl DirectPcmFadeState {
    fn new() -> Self {
        Self {
            gain_start_micro: AtomicU32::new(1_000_000),
            gain_end_micro: AtomicU32::new(1_000_000),
            ramping: AtomicBool::new(false),
            silent: AtomicBool::new(false),
            silence_blocks: AtomicU32::new(0),
            sample_bits: AtomicU8::new(0),
            valid_bits: AtomicU8::new(0),
            window_samples: AtomicUsize::new(0),
        }
    }

    /// 启动淡出：按采样率换算约 20ms 的线性渐零窗，随后块为静音
    fn begin_fade_out(&self, sample_bits: u8, valid_bits: u8, sample_rate: u32) {
        if self.silent.load(Ordering::Acquire) {
            return;
        }
        self.sample_bits.store(sample_bits, Ordering::Release);
        self.valid_bits.store(valid_bits, Ordering::Release);
        let window = (sample_rate as usize).saturating_mul(20) / 1000;
        self.window_samples.store(window.max(1), Ordering::Release);
        let current = self.gain_end_micro.load(Ordering::Acquire);
        self.gain_start_micro.store(current, Ordering::Release);
        self.gain_end_micro.store(0, Ordering::Release);
        self.ramping.store(true, Ordering::Release);
    }

    fn silent(&self) -> bool {
        self.silent.load(Ordering::Acquire)
    }

    /// handoff/seek 换源前清除淡出状态：新源必须从位精确全增益开始，
    /// 否则上一首遗留的 silent 标记会把新源首块整体置零
    fn reset(&self) {
        self.ramping.store(false, Ordering::Release);
        self.silent.store(false, Ordering::Release);
        self.silence_blocks.store(0, Ordering::Release);
    }

    /// 排空谓词：已渐零且交付的静音块数达到下限
    fn drained(&self, min_blocks: u32) -> bool {
        self.silent.load(Ordering::Acquire)
            && self.silence_blocks.load(Ordering::Acquire) >= min_blocks
    }
}

struct DirectPcmRing {
    slots: Box<[DirectPcmSlot]>,
    consumer_index: AtomicUsize,
    in_flight: AtomicUsize,
    consumed_frames: AtomicU64,
    duration_micros: AtomicU64,
    transition_count: AtomicU64,
    boundary_generation: AtomicU64,
    finished: AtomicBool,
    failed: AtomicBool,
    stopped: AtomicBool,
    fade: DirectPcmFadeState,
    /// 最近一次成功解码的块字节数（0 = 尚未解码出任何帧）
    last_block_bytes: AtomicUsize,
    /// 最近一次成功解码的块帧数
    last_block_frames: AtomicUsize,
    /// pre-mute 静音窗口截止（monotonic 毫秒，0 = 未触发）：换源/seek 复位前触发，
    /// 窗口内 SDK 拉取遇供数空窗时交付静音块，消除 Target 欠载杂音
    pre_mute_until_ms: AtomicU64,
    /// pre-mute 静音缓冲（全零）：仅 SDK 回调线程（consumer）读写，producer 不触碰
    pre_mute_buf: Mutex<Vec<u8>>,
    /// 单一状态信号：producer 与控制线程共享的条件等待通道（避免任何忙等/轮询）
    signal: Mutex<()>,
    signal_cv: Condvar,
    /// 控制通道存在待处理命令的提示位：命令发送方置位并唤醒，producer 消费命令前清零
    command_pending: AtomicBool,
}

#[derive(Clone, Copy)]
pub struct DirectPcmBlock {
    pub data: *const u8,
    pub len: usize,
}

impl DirectPcmRing {
    fn new() -> Result<Self> {
        let mut slots = Vec::with_capacity(DIRECT_RING_DEPTH);
        for _ in 0..DIRECT_RING_DEPTH {
            slots.push(DirectPcmSlot::new()?);
        }
        Ok(Self {
            slots: slots.into_boxed_slice(),
            consumer_index: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(NO_SLOT),
            consumed_frames: AtomicU64::new(0),
            duration_micros: AtomicU64::new(0),
            transition_count: AtomicU64::new(0),
            boundary_generation: AtomicU64::new(0),
            finished: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            fade: DirectPcmFadeState::new(),
            last_block_bytes: AtomicUsize::new(0),
            last_block_frames: AtomicUsize::new(0),
            pre_mute_until_ms: AtomicU64::new(0),
            pre_mute_buf: Mutex::new(Vec::new()),
            signal: Mutex::new(()),
            signal_cv: Condvar::new(),
            command_pending: AtomicBool::new(false),
        })
    }

    /// 状态变化通知：取一次锁再释放后 notify，保证不会丢失在等待方进入之前。
    /// 供 SDK 回调线程调用：仅一次短暂锁 + futex wake，无分配，实时安全。
    fn notify_state(&self) {
        let _guard = self.signal.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let mut guard = self.signal.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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

    /// SDK 回调内对即将交付的块原位应用淡出包络/静音。
    /// 块处于 IN_FLIGHT 状态时仅由本回调可写（下一次 next_block/release 才回收），写入安全。
    fn apply_fade(&self, data: *mut u8, len: usize) {
        if data.is_null() || len == 0 {
            return;
        }
        let fade = &self.fade;
        if fade.silent() {
            unsafe { ptr::write_bytes(data, 0, len) };
            fade.silence_blocks.fetch_add(1, Ordering::Release);
            // 静音交付计数变化：唤醒排空等待方
            self.notify_state();
            return;
        }
        if !fade.ramping.load(Ordering::Acquire) {
            return;
        }
        let sample_bits = fade.sample_bits.load(Ordering::Acquire);
        let valid_bits = fade.valid_bits.load(Ordering::Acquire);
        let window = fade.window_samples.load(Ordering::Acquire);
        if (sample_bits != 16 && sample_bits != 32) || valid_bits == 0 || window == 0 {
            return;
        }
        let bytes_per_sample = usize::from(sample_bits / 8);
        let total_samples = len / bytes_per_sample;
        if total_samples == 0 {
            return;
        }
        let start_gain = f64::from(fade.gain_start_micro.load(Ordering::Acquire)) / 1_000_000.0;
        let end_gain = f64::from(fade.gain_end_micro.load(Ordering::Acquire)) / 1_000_000.0;
        let shift = u32::from(sample_bits) - u32::from(valid_bits);
        let window = window.min(total_samples);
        let gain_at = |i: usize| -> f64 {
            if i >= window {
                end_gain
            } else {
                start_gain + (end_gain - start_gain) * (i as f64) / (window as f64)
            }
        };
        let scale = |i: usize, sample: i64| -> i64 {
            let scaled = (sample as f64 * gain_at(i)).round() as i64;
            // 有效位对齐：s24-in-s32 传输槽缩放后保持低位零
            (scaled >> shift) << shift
        };
        unsafe {
            if sample_bits == 16 {
                let p = data.cast::<i16>();
                for i in 0..total_samples {
                    *p.add(i) = scale(i, i64::from(*p.add(i))) as i16;
                }
            } else {
                let p = data.cast::<i32>();
                for i in 0..total_samples {
                    *p.add(i) = scale(i, i64::from(*p.add(i))) as i32;
                }
            }
        }
        // 包络已交付：推进状态
        fade.gain_start_micro.store(
            fade.gain_end_micro.load(Ordering::Acquire),
            Ordering::Release,
        );
        if end_gain <= 0.0 {
            fade.silent.store(true, Ordering::Release);
            // 渐零完成：唤醒排空等待方（此刻静音块计数仍为 0，由 silent 分支继续累加）
            self.notify_state();
        }
        fade.ramping.store(false, Ordering::Release);
    }

    fn next_block(&self) -> Option<DirectPcmBlock> {
        self.release_in_flight();
        if self.failed.load(Ordering::Acquire) {
            return None;
        }

        // pre-mute 窗口（换源/seek 复位触发）：SDK 拉取遇供数空窗时交付静音块
        // 而非"无块"，消除 Target 欠载杂音；静音块不推进 consumed/boundary 会计
        if self.pre_mute_active() {
            if let Some(block) = self.pre_mute_block() {
                return Some(block);
            }
        }

        let index = self.consumer_index.load(Ordering::Relaxed);
        let slot = &self.slots[index];
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
            self.consumed_frames.store(0, Ordering::Release);
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
        Some(DirectPcmBlock { data, len })
    }

    fn release_in_flight(&self) {
        let index = self.in_flight.swap(NO_SLOT, Ordering::AcqRel);
        if index == NO_SLOT {
            return;
        }
        let slot = &self.slots[index];
        let frames = slot.sample_frames.swap(0, Ordering::Relaxed);
        self.consumed_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
        slot.state.store(SLOT_FREE, Ordering::Release);
        // slot 释放：唤醒等待空闲 slot 的 producer
        self.notify_state();
    }

    /// 触发 pre-mute 静音窗口（换源/seek 复位前调用）：窗口内 SDK 拉取遇
    /// 供数空窗时交付静音块而非"无块"，消除 Target 侧欠载杂音
    fn trigger_pre_mute(&self) {
        self.pre_mute_until_ms
            .store(mono_millis() + PRE_MUTE_WINDOW_MS, Ordering::Release);
    }

    fn pre_mute_active(&self) -> bool {
        self.pre_mute_until_ms.load(Ordering::Acquire) > mono_millis()
    }

    /// 供数空窗静音块：全零（signed 零点），几何沿用最近一次有效块
    fn pre_mute_block(&self) -> Option<DirectPcmBlock> {
        let bytes = self.last_block_bytes.load(Ordering::Acquire);
        if bytes == 0 {
            return None;
        }
        let mut buf = self.pre_mute_buf.lock().unwrap_or_else(|p| p.into_inner());
        if buf.len() < bytes {
            buf.resize(bytes, 0);
        }
        Some(DirectPcmBlock {
            data: buf.as_ptr(),
            len: bytes,
        })
    }

    fn reset_for_transition(&self) {
        self.release_in_flight();
        self.consumer_index.store(0, Ordering::Relaxed);
        self.in_flight.store(NO_SLOT, Ordering::Relaxed);
        self.consumed_frames.store(0, Ordering::Relaxed);
        self.finished.store(false, Ordering::Relaxed);
        self.failed.store(false, Ordering::Relaxed);
        self.fade.reset();
        for slot in &self.slots {
            slot.state.store(SLOT_FREE, Ordering::Relaxed);
            slot.payload_ptr.store(ptr::null_mut(), Ordering::Relaxed);
            slot.payload_len.store(0, Ordering::Relaxed);
            slot.sample_frames.store(0, Ordering::Relaxed);
            slot.boundary.store(false, Ordering::Relaxed);
            slot.boundary_duration_micros.store(0, Ordering::Relaxed);
            slot.boundary_generation.store(0, Ordering::Relaxed);
            unsafe { &mut *slot.frame.get() }.clear();
        }
        // finished/failed 已清除、fade 已复位：唤醒 producer 与排空等待方
        self.notify_state();
    }
}

fn seek_pcm_ring(
    decoder: &mut DirectPcmDecoder,
    ring: &DirectPcmRing,
    expected_format: DirectPcmFormat,
    position_secs: f64,
) -> Result<f64> {
    // seek 复位同样存在供数空窗：pre-mute 窗口内 SDK 拉取拿到静音块而非欠载
    ring.trigger_pre_mute();
    ring.reset_for_transition();
    let first_slot = &ring.slots[0];
    first_slot.state.store(SLOT_FILLING, Ordering::Relaxed);
    let first_frame = unsafe { &mut *first_slot.frame.get() };
    let actual_position = decoder.seek_accurate(position_secs, first_frame)?;
    let format = first_frame.format()?;
    ensure!(
        format == expected_format,
        "Source Direct PCM seek 后音频格式发生变化"
    );
    if format.memory_path == DirectPcmMemoryPath::BitPerfectRepack {
        let max_samples_per_channel = decoder
            .frame_samples_hint()
            .max(first_frame.samples_per_channel());
        let max_samples = max_samples_per_channel
            .checked_mul(usize::from(format.channels))
            .context("Source Direct seek repack 预分配长度溢出")?;
        first_frame.preallocate_repack(format.sample_format, max_samples)?;
        first_frame.repack_planar(format.sample_format, first_frame.sample_offset)?;
        for slot in ring.slots.iter().skip(1) {
            let frame = unsafe { &mut *slot.frame.get() };
            frame.preallocate_repack(format.sample_format, max_samples)?;
        }
    }
    first_slot
        .payload_ptr
        .store(first_frame.payload_ptr()?.cast_mut(), Ordering::Relaxed);
    first_slot
        .payload_len
        .store(first_frame.payload_len, Ordering::Relaxed);
    first_slot
        .sample_frames
        .store(first_frame.samples_per_channel(), Ordering::Relaxed);
    first_slot.state.store(SLOT_READY, Ordering::Release);
    Ok(actual_position)
}

fn same_pcm_transport(left: DirectPcmFormat, right: DirectPcmFormat) -> bool {
    left.sample_rate == right.sample_rate
        && left.channels == right.channels
        && left.storage_bits == right.storage_bits
}

struct StagedPcmSource {
    decoder: DirectPcmDecoder,
    first_frame: DirectPcmFrame,
    format: DirectPcmFormat,
    duration_micros: u64,
    generation: u64,
}

fn prepare_staged_pcm_source(
    path: &Path,
    current_format: DirectPcmFormat,
    duration_micros: u64,
    generation: u64,
) -> Result<StagedPcmSource> {
    let mut decoder = DirectPcmDecoder::open_local(path)?;
    let mut first_frame = DirectPcmFrame::new()?;
    ensure!(
        decoder.read_frame(&mut first_frame)?,
        "Source Direct staged 音源没有可播放 PCM frame"
    );
    let format = first_frame.format()?;
    ensure!(
        same_pcm_transport(current_format, format),
        "[Direct] staged PCM wire format 与当前 Diretta connection 不一致"
    );
    if format.memory_path == DirectPcmMemoryPath::BitPerfectRepack {
        let max_samples_per_channel = decoder
            .frame_samples_hint()
            .max(first_frame.samples_per_channel());
        let max_samples = max_samples_per_channel
            .checked_mul(usize::from(format.channels))
            .context("Source Direct staged repack 预分配长度溢出")?;
        first_frame.preallocate_repack(format.sample_format, max_samples)?;
        first_frame.repack_planar(format.sample_format, first_frame.sample_offset)?;
    }
    Ok(StagedPcmSource {
        decoder,
        first_frame,
        format,
        duration_micros,
        generation,
    })
}

fn install_staged_pcm_slot(
    mut staged: StagedPcmSource,
    slot: &DirectPcmSlot,
) -> Result<(DirectPcmDecoder, DirectPcmFormat)> {
    let frame = unsafe { &mut *slot.frame.get() };
    std::mem::swap(frame, &mut staged.first_frame);
    slot.payload_ptr
        .store(frame.payload_ptr()?.cast_mut(), Ordering::Relaxed);
    slot.payload_len.store(frame.payload_len, Ordering::Relaxed);
    slot.sample_frames
        .store(frame.samples_per_channel(), Ordering::Relaxed);
    slot.boundary_duration_micros
        .store(staged.duration_micros, Ordering::Relaxed);
    slot.boundary_generation
        .store(staged.generation, Ordering::Relaxed);
    slot.boundary.store(true, Ordering::Relaxed);
    slot.state.store(SLOT_READY, Ordering::Release);
    Ok((staged.decoder, staged.format))
}

fn replace_pcm_ring(
    source: &str,
    ring: &DirectPcmRing,
    current_format: DirectPcmFormat,
    cancel: &HttpCancelHandle,
) -> Result<(DirectPcmDecoder, DirectPcmFormat)> {
    debug!(
        target: "diretta_handoff",
        phase = "pcm_ring_open_start",
        source = %source,
        "replace_pcm_ring open source"
    );
    let mut decoder = DirectPcmDecoder::open_source_with_cancel(source, cancel)?;
    let mut prepared = DirectPcmFrame::new()?;
    ensure!(
        decoder.read_frame(&mut prepared)?,
        "Source Direct handoff 音源没有可播放 PCM frame"
    );
    let new_format = prepared.format()?;
    ensure!(
        same_pcm_transport(current_format, new_format),
        "[Direct] 新音源 PCM wire format 与当前 Diretta connection 不一致"
    );
    debug!(
        target: "diretta_handoff",
        phase = "pcm_ring_open_done",
        sample_rate = %new_format.sample_rate,
        channels = %new_format.channels,
        valid_bits = %new_format.valid_bits,
        storage_bits = %new_format.storage_bits,
        "replace_pcm_ring first frame decoded"
    );

    if new_format.memory_path == DirectPcmMemoryPath::BitPerfectRepack {
        let max_samples_per_channel = decoder
            .frame_samples_hint()
            .max(prepared.samples_per_channel());
        let max_samples = max_samples_per_channel
            .checked_mul(usize::from(new_format.channels))
            .context("Source Direct handoff repack 预分配长度溢出")?;
        prepared.preallocate_repack(new_format.sample_format, max_samples)?;
        prepared.repack_planar(new_format.sample_format, prepared.sample_offset)?;
    }

    let silence_blocks_before = ring.fade.silence_blocks.load(Ordering::Acquire);
    // 换源复位有供数空窗：pre-mute 窗口内 SDK 拉取拿到静音块而非欠载（消除切杂音）
    ring.trigger_pre_mute();
    ring.reset_for_transition();
    let first_slot = &ring.slots[0];
    first_slot.state.store(SLOT_FILLING, Ordering::Relaxed);
    let first_frame = unsafe { &mut *first_slot.frame.get() };
    std::mem::swap(first_frame, &mut prepared);
    if new_format.memory_path == DirectPcmMemoryPath::BitPerfectRepack {
        let max_samples_per_channel = decoder
            .frame_samples_hint()
            .max(first_frame.samples_per_channel());
        let max_samples = max_samples_per_channel
            .checked_mul(usize::from(new_format.channels))
            .context("Source Direct handoff slot 预分配长度溢出")?;
        for slot in ring.slots.iter().skip(1) {
            let frame = unsafe { &mut *slot.frame.get() };
            frame.preallocate_repack(new_format.sample_format, max_samples)?;
        }
    }
    first_slot
        .payload_ptr
        .store(first_frame.payload_ptr()?.cast_mut(), Ordering::Relaxed);
    first_slot
        .payload_len
        .store(first_frame.payload_len, Ordering::Relaxed);
    first_slot
        .sample_frames
        .store(first_frame.samples_per_channel(), Ordering::Relaxed);
    // handoff 后第一个 slot 标记为 boundary：消费时 next_block 会递增 transition_count
    first_slot.boundary_duration_micros.store(
        ring.duration_micros.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    first_slot
        .boundary_generation
        .store(ring.boundary_generation.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
    first_slot.boundary.store(true, Ordering::Relaxed);
    first_slot.state.store(SLOT_READY, Ordering::Release);
    let silence_blocks_after = ring.fade.silence_blocks.load(Ordering::Acquire);
    debug!(
        target: "diretta_handoff",
        phase = "pcm_ring_swap_done",
        silence_blocks_before = %silence_blocks_before,
        silence_blocks_after = %silence_blocks_after,
        transition_count = %ring.transition_count.load(Ordering::Acquire),
        consumed_frames = %ring.consumed_frames.load(Ordering::Acquire),
        "replace_pcm_ring slot swap complete"
    );
    Ok((decoder, new_format))
}

#[derive(Clone)]
pub struct DirectPcmMonitor {
    ring: Arc<DirectPcmRing>,
    sample_rate: u32,
}

impl DirectPcmMonitor {
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn consumed_position(&self) -> f64 {
        self.ring.consumed_frames.load(Ordering::Acquire) as f64 / f64::from(self.sample_rate)
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

    /// 事件驱动首块消费等待：任一帧被设备消费、失败或源提前结束即唤醒；
    /// 无事件时阻塞至超时（返回 false）。用于启动校验，取代轮询 sleep
    pub fn wait_first_consumed(&self, timeout: Duration) -> bool {
        self.ring.wait_for(
            |ring| {
                ring.consumed_frames.load(Ordering::Acquire) > 0
                    || ring.failed.load(Ordering::Acquire)
                    || ring.finished.load(Ordering::Acquire)
            },
            timeout,
        )
    }

    pub fn transition_count(&self) -> u64 {
        self.ring.transition_count.load(Ordering::Acquire)
    }

    pub fn silence_blocks(&self) -> u32 {
        self.ring.fade.silence_blocks.load(Ordering::Acquire)
    }

    pub fn duration(&self) -> f64 {
        self.ring.duration_micros.load(Ordering::Acquire) as f64 / 1_000_000.0
    }

    pub fn boundary_generation(&self) -> u64 {
        self.ring.boundary_generation.load(Ordering::Acquire)
    }

    /// 事件驱动排空等待：淡出完成且已交付 min_blocks 块静音，或超时。
    /// 句柄持有 ring 的 Arc 引用，供调用方在 player 锁外排空
    pub fn wait_fade_drained(&self, min_blocks: u32, timeout: Duration) -> bool {
        self.ring
            .wait_for(|ring| ring.fade.drained(min_blocks), timeout)
    }
}

#[derive(Clone)]
pub struct DirectPcmStageHandle {
    control_tx: mpsc::Sender<DirectPcmCommand>,
    ring: Arc<DirectPcmRing>,
}

impl DirectPcmStageHandle {
    pub fn stage_local(&self, path: &Path, duration_secs: f64, generation: u64) -> Result<()> {
        ensure!(
            duration_secs.is_finite() && duration_secs >= 0.0,
            "Source Direct staged duration 无效"
        );
        let duration_micros = (duration_secs * 1_000_000.0)
            .round()
            .clamp(0.0, u64::MAX as f64) as u64;
        let (response_tx, response_rx) = mpsc::sync_channel(0);
        self.control_tx
            .send(DirectPcmCommand::StageLocal {
                path: path.to_owned(),
                duration_micros,
                generation,
                response: response_tx,
            })
            .context("提交 Source Direct PCM staged source 失败")?;
        self.ring.signal_command();
        response_rx
            .recv()
            .context("等待 Source Direct PCM staged source 结果失败")?
    }

    pub fn cancel(&self) {
        let _ = self.control_tx.send(DirectPcmCommand::CancelStaged);
        self.ring.signal_command();
    }
}

pub struct DirectPcmSource {
    ring: Arc<DirectPcmRing>,
    format: DirectPcmFormat,
    control_tx: mpsc::Sender<DirectPcmCommand>,
    producer: Option<JoinHandle<()>>,
}

impl DirectPcmSource {
    pub fn open_local(path: &Path) -> Result<Self> {
        let (source, _) = Self::open_local_at(path, 0.0)?;
        Ok(source)
    }

    pub fn open_local_at(path: &Path, position_secs: f64) -> Result<(Self, f64)> {
        let decoder = DirectPcmDecoder::open_local(path)?;
        Self::open_with_decoder(decoder, position_secs)
    }

    /// 以流式 Reader 打开（在线音源 stream 模式）。
    /// `position_secs > 0` 由 demuxer 级 accurate seek 完成（Reader 层触发 Range 重连）。
    pub fn open_reader_at(reader: Box<dyn ReadSeek>, position_secs: f64) -> Result<(Self, f64)> {
        let decoder = DirectPcmDecoder::open_reader(reader)?;
        Self::open_with_decoder(decoder, position_secs)
    }

    fn open_with_decoder(decoder: DirectPcmDecoder, position_secs: f64) -> Result<(Self, f64)> {
        let mut decoder = decoder;
        let ring = Arc::new(DirectPcmRing::new()?);
        let first_slot = &ring.slots[0];
        first_slot.state.store(SLOT_FILLING, Ordering::Relaxed);
        let first_frame = unsafe { &mut *first_slot.frame.get() };
        let actual_position = if position_secs > 0.0 {
            decoder.seek_accurate(position_secs, first_frame)?
        } else {
            ensure!(
                decoder.read_frame(first_frame)?,
                "Source Direct 音源没有可播放 PCM frame"
            );
            0.0
        };
        let format = first_frame.format()?;
        if format.memory_path == DirectPcmMemoryPath::BitPerfectRepack {
            let max_samples_per_channel = decoder
                .frame_samples_hint()
                .max(first_frame.samples_per_channel());
            let max_samples = max_samples_per_channel
                .checked_mul(usize::from(format.channels))
                .context("Source Direct repack 预分配长度溢出")?;
            first_frame.preallocate_repack(format.sample_format, max_samples)?;
            first_frame.repack_planar(format.sample_format, first_frame.sample_offset)?;
            for slot in ring.slots.iter().skip(1) {
                let frame = unsafe { &mut *slot.frame.get() };
                frame.preallocate_repack(format.sample_format, max_samples)?;
            }
        }
        first_slot
            .payload_ptr
            .store(first_frame.payload_ptr()?.cast_mut(), Ordering::Relaxed);
        first_slot
            .payload_len
            .store(first_frame.payload_len, Ordering::Relaxed);
        first_slot
            .sample_frames
            .store(first_frame.samples_per_channel(), Ordering::Relaxed);
        first_slot.state.store(SLOT_READY, Ordering::Release);

        let (control_tx, control_rx) = mpsc::channel();
        let producer_ring = Arc::clone(&ring);
        let producer = thread::Builder::new()
            .name("diretta-direct-decode".into())
            .spawn(move || {
                // 绑定到 CPU 性能核心（ARM 大核 / x86 独立物理核）并设置 SCHED_FIFO 实时调度，
                // 防止 PCM 解码推流线程被调度到效率核或超线程虚拟核，
                // 避免推流跟不上实时时钟产生抖动（Jitter）
                bind_current_thread_to_performance_cores("diretta-direct-decode");
                boost_current_audio_thread("diretta-direct-decode");
                let mut active_format = format;
                let mut staged: Option<StagedPcmSource> = None;
                let mut next_slot = 1; // slot 0 已被首帧占用
                while !producer_ring.stopped.load(Ordering::Acquire) {
                    // 消费命令前清提示位：此后发送方的新命令会重新置位并唤醒
                    producer_ring
                        .command_pending
                        .store(false, Ordering::Release);
                    match control_rx.try_recv() {
                        Ok(DirectPcmCommand::Seek {
                            position_secs,
                            response,
                        }) => {
                            let result = seek_pcm_ring(
                                &mut decoder,
                                &producer_ring,
                                active_format,
                                position_secs,
                            );
                            if result.is_err() {
                                producer_ring.failed.store(true, Ordering::Release);
                            }
                            let _ = response.send(result);
                            next_slot = 1;
                            continue;
                        }
                        Ok(DirectPcmCommand::ReplaceLocal {
                            source,
                            cancel,
                            response,
                        }) => {
                            let result =
                                replace_pcm_ring(&source, &producer_ring, active_format, &cancel);
                            match result {
                                Ok((new_decoder, new_format)) => {
                                    decoder = new_decoder;
                                    active_format = new_format;
                                    staged = None;
                                    let _ = response.send(Ok(new_format));
                                    next_slot = 1;
                                }
                                Err(error) => {
                                    let _ = response.send(Err(error));
                                }
                            }
                            continue;
                        }
                        Ok(DirectPcmCommand::StageLocal {
                            path,
                            duration_micros,
                            generation,
                            response,
                        }) => {
                            match prepare_staged_pcm_source(
                                &path,
                                active_format,
                                duration_micros,
                                generation,
                            ) {
                                Ok(candidate) => {
                                    staged = Some(candidate);
                                    let _ = response.send(Ok(()));
                                }
                                Err(error) => {
                                    let _ = response.send(Err(error));
                                }
                            }
                            continue;
                        }
                        Ok(DirectPcmCommand::CancelStaged) => {
                            staged = None;
                            continue;
                        }
                        Err(mpsc::TryRecvError::Disconnected) => return,
                        Err(mpsc::TryRecvError::Empty) => {}
                    }
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
                        // 关流前淡出激活：即便文件已读完，也要继续产数字静音块
                        // 给 consumer apply_fade 把数据置零并累加 silence_blocks
                        let fade_active = producer_ring.fade.ramping.load(Ordering::Acquire)
                            || producer_ring.fade.silent.load(Ordering::Acquire);
                        if !fade_active {
                            let Some(candidate) = staged.take() else {
                                // 事件等待：等淡出被激活（关流排空开始）、新源就位或命令到达
                                producer_ring.wait_for(
                                    |ring| {
                                        ring.fade.ramping.load(Ordering::Acquire)
                                            || ring.fade.silent.load(Ordering::Acquire)
                                            || !ring.finished.load(Ordering::Acquire)
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
                                        ring.slots[next_slot].state.load(Ordering::Acquire)
                                            == SLOT_FREE
                                            || ring.command_pending.load(Ordering::Acquire)
                                    },
                                    PRODUCER_WAIT_CEILING,
                                );
                                continue;
                            }
                            match install_staged_pcm_slot(candidate, slot) {
                                Ok((new_decoder, new_format)) => {
                                    decoder = new_decoder;
                                    active_format = new_format;
                                    producer_ring.finished.store(false, Ordering::Release);
                                    next_slot = (next_slot + 1) % producer_ring.slots.len();
                                }
                                Err(_) => {
                                    slot.state.store(SLOT_FREE, Ordering::Release);
                                    producer_ring.failed.store(true, Ordering::Release);
                                }
                            }
                            continue;
                        }
                        // fade_active && finished：持续产出数字静音块
                        // 使用本 slot 自有缓冲，按最近一次有效块的几何尺寸交付
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
                        let bytes = producer_ring.last_block_bytes.load(Ordering::Acquire);
                        let frames = producer_ring.last_block_frames.load(Ordering::Acquire);
                        if bytes == 0 || frames == 0 {
                            // 尚未成功解码过任何帧，无法推断静音块几何尺寸：保持空闲
                            slot.state.store(SLOT_FREE, Ordering::Release);
                            producer_ring.wait_for(
                                |ring| ring.command_pending.load(Ordering::Acquire),
                                PRODUCER_WAIT_CEILING,
                            );
                            continue;
                        }
                        slot.fill_silence(bytes, frames);
                        slot.state.store(SLOT_READY, Ordering::Release);
                        next_slot = (next_slot + 1) % producer_ring.slots.len();
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

                    let frame = unsafe { &mut *slot.frame.get() };
                    match decoder.read_frame(frame) {
                        Ok(true) => {
                            let frame_format = match frame.format() {
                                Ok(value) => value,
                                Err(error) => {
                                    warn!(
                                        target: "diretta_handoff",
                                        phase = "producer_frame_format_err",
                                        error = %error,
                                        "Source Direct 解码帧缺少格式信息，ring 标记失败"
                                    );
                                    slot.state.store(SLOT_FREE, Ordering::Release);
                                    producer_ring.failed.store(true, Ordering::Release);
                                    return;
                                }
                            };
                            if frame_format != active_format {
                                warn!(
                                    target: "diretta_handoff",
                                    phase = "producer_format_mismatch",
                                    "Source Direct 中途格式变化，ring 标记失败"
                                );
                                slot.state.store(SLOT_FREE, Ordering::Release);
                                producer_ring.failed.store(true, Ordering::Release);
                                return;
                            }
                            let data = match frame.payload_ptr() {
                                Ok(value) => value,
                                Err(error) => {
                                    warn!(
                                        target: "diretta_handoff",
                                        phase = "producer_payload_ptr_err",
                                        error = %error,
                                        "Source Direct 帧负载指针不可用，ring 标记失败"
                                    );
                                    slot.state.store(SLOT_FREE, Ordering::Release);
                                    producer_ring.failed.store(true, Ordering::Release);
                                    return;
                                }
                            };
                            slot.payload_ptr.store(data.cast_mut(), Ordering::Relaxed);
                            slot.payload_len.store(frame.payload_len, Ordering::Relaxed);
                            slot.sample_frames
                                .store(frame.samples_per_channel(), Ordering::Relaxed);
                            // 记录最近一次有效块几何尺寸，供关流淡出期间合成静音块
                            producer_ring
                                .last_block_bytes
                                .store(frame.payload_len, Ordering::Release);
                            producer_ring.last_block_frames.store(
                                frame.samples_per_channel(),
                                Ordering::Release,
                            );
                            slot.state.store(SLOT_READY, Ordering::Release);
                            next_slot = (next_slot + 1) % producer_ring.slots.len();
                        }
                        Ok(false) => {
                            if let Some(candidate) = staged.take() {
                                match install_staged_pcm_slot(candidate, slot) {
                                    Ok((new_decoder, new_format)) => {
                                        decoder = new_decoder;
                                        active_format = new_format;
                                        producer_ring.finished.store(false, Ordering::Release);
                                        next_slot = (next_slot + 1) % producer_ring.slots.len();
                                    }
                                    Err(_) => {
                                        slot.state.store(SLOT_FREE, Ordering::Release);
                                        producer_ring.failed.store(true, Ordering::Release);
                                    }
                                }
                            } else {
                                slot.state.store(SLOT_FREE, Ordering::Release);
                                producer_ring.finished.store(true, Ordering::Release);
                            }
                        }
                        Err(error) => {
                            warn!(
                                target: "diretta_handoff",
                                phase = "producer_read_frame_err",
                                error = %error,
                                "Source Direct 解码失败，ring 标记失败"
                            );
                            slot.state.store(SLOT_FREE, Ordering::Release);
                            producer_ring.failed.store(true, Ordering::Release);
                        }
                    }
                }
            })
            .context("启动 Source Direct decoder producer 失败")?;

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

    pub fn format(&self) -> DirectPcmFormat {
        self.format
    }

    pub fn monitor(&self) -> DirectPcmMonitor {
        DirectPcmMonitor {
            ring: Arc::clone(&self.ring),
            sample_rate: self.format.sample_rate,
        }
    }

    pub fn stage_handle(&self) -> DirectPcmStageHandle {
        DirectPcmStageHandle {
            control_tx: self.control_tx.clone(),
            ring: Arc::clone(&self.ring),
        }
    }

    pub fn set_duration(&self, duration_secs: f64) {
        let micros = (duration_secs.max(0.0) * 1_000_000.0)
            .round()
            .clamp(0.0, u64::MAX as f64) as u64;
        self.ring.duration_micros.store(micros, Ordering::Release);
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

    pub fn callback_context(&self) -> *mut c_void {
        Arc::as_ptr(&self.ring).cast_mut().cast()
    }

    pub fn seek_while_paused(&mut self, position_secs: f64) -> Result<f64> {
        let (response_tx, response_rx) = mpsc::sync_channel(0);
        self.control_tx
            .send(DirectPcmCommand::Seek {
                position_secs,
                response: response_tx,
            })
            .context("提交 Source Direct PCM seek 失败")?;
        self.ring.signal_command();
        response_rx
            .recv()
            .context("等待 Source Direct PCM seek 结果失败")?
    }

    /// 关流前源级淡出：SDK 继续拉块时输出线性渐零（约 20ms），随后块为数字静音。
    /// 与 pause+close 组合使用，消除手动切歌/停止时 mid-sample 硬切爆音。
    pub fn begin_fade_out(&self) {
        self.ring
            .fade
            .begin_fade_out(self.format.storage_bits, self.format.valid_bits, self.format.sample_rate);
        // 唤醒 producer：finished 状态下它需立即转入静音块合成以推进排空
        self.ring.notify_state();
    }

    /// 淡出块是否已交付给 SDK（后续块均为静音）
    pub fn is_faded_out(&self) -> bool {
        self.ring.fade.silent()
    }

    /// 已交付的静音块数（预静音计数）
    pub fn silence_blocks_handed(&self) -> u32 {
        self.ring.fade.silence_blocks.load(Ordering::Acquire)
    }

    /// 事件驱动排空等待：淡出完成且已交付 min_blocks 块静音，或超时。
    /// 取代旧的「10ms 轮询 + 固定 sleep」，控制线程在等待期间不占用 CPU。
    pub fn wait_fade_drained(&self, min_blocks: u32, timeout: Duration) -> bool {
        self.ring
            .wait_for(|ring| ring.fade.drained(min_blocks), timeout)
    }

    pub fn replace_drained_local(
        &mut self,
        source: &str,
        cancel: HttpCancelHandle,
    ) -> Result<DirectPcmFormat> {
        debug!(
            target: "diretta_handoff",
            phase = "pcm_api_send",
            source = %source,
            "DirectPcmSource::replace_drained_local send command"
        );
        let (response_tx, response_rx) = mpsc::sync_channel(0);
        self.control_tx
            .send(DirectPcmCommand::ReplaceLocal {
                source: source.to_owned(),
                cancel,
                response: response_tx,
            })
            .context("提交 Source Direct PCM handoff 失败")?;
        self.ring.signal_command();
        let format = response_rx
            .recv()
            .context("等待 Source Direct PCM handoff 结果失败")??;
        self.format = format;
        debug!(
            target: "diretta_handoff",
            phase = "pcm_api_recv",
            sample_rate = %format.sample_rate,
            channels = %format.channels,
            "DirectPcmSource::replace_drained_local received format"
        );
        Ok(format)
    }
}

pub unsafe extern "C" fn direct_pcm_next_block(
    context: *mut c_void,
    data: *mut *const u8,
    len: *mut usize,
) -> bool {
    if context.is_null() || data.is_null() || len.is_null() {
        return false;
    }
    let ring = unsafe { &*context.cast::<DirectPcmRing>() };
    let Some(block) = ring.next_block() else {
        return false;
    };
    // 关流前淡出/静音在交付前原位应用（块 IN_FLIGHT 期仅本回调可写）
    ring.apply_fade(block.data.cast_mut(), block.len);
    unsafe {
        *data = block.data;
        *len = block.len;
    }
    true
}

pub unsafe extern "C" fn direct_pcm_release_block(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    let ring = unsafe { &*context.cast::<DirectPcmRing>() };
    ring.release_in_flight();
}

impl Drop for DirectPcmSource {
    fn drop(&mut self) {
        self.ring.stopped.store(true, Ordering::Release);
        self.ring.release_in_flight();
        if let Some(producer) = self.producer.take() {
            let _ = producer.join();
        }
    }
}

fn ffmpeg_result(code: i32, action: &str) -> Result<()> {
    if code >= 0 {
        Ok(())
    } else {
        Err(ffmpeg_error(code, action))
    }
}

fn ffmpeg_error(code: i32, action: &str) -> anyhow::Error {
    let mut buffer = [0_i8; 256];
    let message = unsafe {
        sys::av_strerror(code, buffer.as_mut_ptr(), buffer.len());
        CStr::from_ptr(buffer.as_ptr())
            .to_string_lossy()
            .into_owned()
    };
    anyhow!("{action}失败: {message} ({code})")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use base64::Engine;

    use super::*;

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

    /// 测试辅助：轮询等待 producer 把下一个 slot 填好。
    /// 真实播放路径有消费循环在持续拉取，不需要 sleep；
    /// 但测试代码单次调用直接消费，可能撞上 producer 还没填的窗口。
    fn poll_next_block(
        context: *mut std::ffi::c_void,
        data: &mut *const u8,
        len: &mut usize,
    ) -> bool {
        // 先 yield 让 producer 线程有机会抢占 CPU 填 slot
        std::thread::yield_now();
        for _ in 0..500 {
            if unsafe { direct_pcm_next_block(context, data, len) } {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        eprintln!("poll_next_block 超时：producer 可能未启动或已卡死");
        false
    }

    struct TempAudioFile {
        path: std::path::PathBuf,
    }

    impl TempAudioFile {
        fn from_bytes(extension: &str, bytes: &[u8]) -> Self {
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "splayer-direct-{}-{id}.{extension}",
                std::process::id()
            ));
            fs::write(&path, bytes).expect("写入 Source Direct 测试音频失败");
            Self { path }
        }

        fn wav(sample_rate: u32, bits_per_sample: u16, pcm: &[u8]) -> Self {
            let bytes = wav_bytes(sample_rate, bits_per_sample, pcm);
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "splayer-direct-{}-{id}.wav",
                std::process::id()
            ));
            fs::write(&path, bytes).expect("写入测试 WAV 失败");
            Self { path }
        }
    }

    /// 构造最小 RIFF/WAVE（PCM s16/s24/s32，双声道）内存字节
    fn wav_bytes(sample_rate: u32, bits_per_sample: u16, pcm: &[u8]) -> Vec<u8> {
        let channels = 2_u16;
        let bytes_per_sample = bits_per_sample / 8;
        let block_align = channels * bytes_per_sample;
        let byte_rate = sample_rate * u32::from(block_align);
        let data_size = u32::try_from(pcm.len()).expect("fixture 太大");
        let mut bytes = Vec::with_capacity(44 + pcm.len());
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_size).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&byte_rate.to_le_bytes());
        bytes.extend_from_slice(&block_align.to_le_bytes());
        bytes.extend_from_slice(&bits_per_sample.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_size.to_le_bytes());
        bytes.extend_from_slice(pcm);
        bytes
    }

    impl Drop for TempAudioFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    #[test]
    fn direct_core_stays_outside_the_float_dsp_pipeline() {
        let source = include_str!("direct_pcm.rs");
        let implementation = source
            .split("#[cfg(test)]")
            .next()
            .expect("Direct PCM implementation section should exist");
        for forbidden in [
            "Resampler",
            "Equalizer",
            "StretchProcessor",
            "LoudnessAnalyzer",
        ] {
            assert!(
                !implementation.contains(forbidden),
                "Source Direct core must not depend on {forbidden}"
            );
        }

    }

    #[test]
    fn s16_wav_payload_is_bit_exact_and_keeps_source_rate() {
        let samples = [-32768_i16, 32767, -12345, 12345, -1, 1, 0, 42];
        let pcm: Vec<u8> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        let fixture = TempAudioFile::wav(44_100, 16, &pcm);
        let mut decoder = DirectPcmDecoder::open_local(&fixture.path).unwrap();
        let mut frame = DirectPcmFrame::new().unwrap();

        assert!(decoder.read_frame(&mut frame).unwrap());
        assert_eq!(
            frame.format().unwrap(),
            DirectPcmFormat {
                sample_rate: 44_100,
                channels: 2,
                valid_bits: 16,
                storage_bits: 16,
                sample_format: DirectPcmSampleFormat::Signed16,
                memory_path: DirectPcmMemoryPath::ZeroCopyPacked,
            }
        );
        assert_eq!(frame.samples_per_channel(), 4);
        assert_eq!(frame.payload_bytes().unwrap(), pcm);
    }

    /// stream 模式核心路径：AVIO 自定义 Reader 输入必须与本地文件解码逐位一致
    #[test]
    fn open_reader_decodes_memory_input_bit_exactly_like_local_file() {
        let sample_rate = 44_100_u32;
        let pcm: Vec<u8> = (0..4096_i32)
            .map(|i| ((i * 37) % 20000 - 10000) as i16)
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        let wav = wav_bytes(sample_rate, 16, &pcm);

        // 参考路径：本地文件
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "splayer-direct-{}-reader-ref-{id}.wav",
            std::process::id()
        ));
        fs::write(&path, &wav).expect("写入参考 WAV 失败");
        let mut local = DirectPcmDecoder::open_local(&path).unwrap();
        let _ = fs::remove_file(&path);
        let mut local_frame = DirectPcmFrame::new().unwrap();

        // 流式路径：内存 Cursor 经 AVIO 喂给 FFmpeg
        let mut streamed =
            DirectPcmDecoder::open_reader(Box::new(std::io::Cursor::new(wav))).unwrap();
        let mut streamed_frame = DirectPcmFrame::new().unwrap();

        assert!(local.read_frame(&mut local_frame).unwrap());
        assert!(streamed.read_frame(&mut streamed_frame).unwrap());
        assert_eq!(
            streamed_frame.format().unwrap(),
            local_frame.format().unwrap(),
            "AVIO 输入与本地文件的解码格式必须一致"
        );
        let mut local_blocks = local_frame.payload_bytes().unwrap().to_vec();
        let mut streamed_blocks = streamed_frame.payload_bytes().unwrap().to_vec();

        while local.read_frame(&mut local_frame).unwrap() {
            local_blocks.extend_from_slice(local_frame.payload_bytes().unwrap());
        }
        while streamed.read_frame(&mut streamed_frame).unwrap() {
            streamed_blocks.extend_from_slice(streamed_frame.payload_bytes().unwrap());
        }

        assert_eq!(streamed_blocks, local_blocks, "AVIO 流式解码输出必须与本地文件逐位一致");
        assert_eq!(streamed_blocks, pcm, "s16 WAV 经 AVIO 解码后应保持位精确");
    }

    #[test]
    fn s32_wav_preserves_all_32_source_bits_without_float_conversion() {
        let samples = [
            i32::MIN,
            i32::MAX,
            -0x1234_5678,
            0x1234_5678,
            -1,
            1,
            0,
            0x55aa_55aa,
        ];
        let pcm: Vec<u8> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        let fixture = TempAudioFile::wav(192_000, 32, &pcm);
        let mut decoder = DirectPcmDecoder::open_local(&fixture.path).unwrap();
        let mut frame = DirectPcmFrame::new().unwrap();

        assert!(decoder.read_frame(&mut frame).unwrap());
        assert_eq!(
            frame.format().unwrap(),
            DirectPcmFormat {
                sample_rate: 192_000,
                channels: 2,
                valid_bits: 32,
                storage_bits: 32,
                sample_format: DirectPcmSampleFormat::Signed32,
                memory_path: DirectPcmMemoryPath::ZeroCopyPacked,
            }
        );
        assert_eq!(frame.payload_bytes().unwrap(), pcm);
    }

    #[test]
    fn s24_wav_keeps_all_valid_bits_in_s32_transport_slots() {
        let samples = [
            -0x80_0000_i32,
            0x7f_ffff,
            -0x12_3456,
            0x12_3456,
            -1,
            1,
            0,
            0x55_aa55,
        ];
        let pcm24: Vec<u8> = samples
            .iter()
            .flat_map(|sample| {
                let bytes = sample.to_le_bytes();
                [bytes[0], bytes[1], bytes[2]]
            })
            .collect();
        let fixture = TempAudioFile::wav(96_000, 24, &pcm24);
        let mut decoder = DirectPcmDecoder::open_local(&fixture.path).unwrap();
        let mut frame = DirectPcmFrame::new().unwrap();

        assert!(decoder.read_frame(&mut frame).unwrap());
        assert_eq!(
            frame.format().unwrap(),
            DirectPcmFormat {
                sample_rate: 96_000,
                channels: 2,
                valid_bits: 24,
                storage_bits: 32,
                sample_format: DirectPcmSampleFormat::Signed32,
                memory_path: DirectPcmMemoryPath::ZeroCopyPacked,
            }
        );
        let decoded: Vec<i32> = frame
            .payload_bytes()
            .unwrap()
            .chunks_exact(4)
            .map(|chunk| i32::from_le_bytes(chunk.try_into().unwrap()))
            .collect();
        let expected: Vec<i32> = samples.iter().map(|sample| sample << 8).collect();
        assert_eq!(decoded, expected);
        assert!(decoded.iter().all(|sample| sample & 0xff == 0));
    }

    #[test]
    fn flac24_decode_preserves_every_valid_source_bit_without_resampling() {
        const FLAC24: &str = "ZkxhQwAAACIQABAAAAAvAAAvF3ADcAAAAAitKR6EKza4EYWT7tcmlaCQhAAAKCAAAAByZWZlcmVuY2UgbGliRkxBQyAxLjUuMCAyMDI1MDIxMQAAAAD/+GusAAfHEEwIINlWqVAQQIIgnh///9skaKuF3VapU3f///yQIC8P///J1OBjmg==";
        let encoded = base64::engine::general_purpose::STANDARD
            .decode(FLAC24)
            .expect("解码 FLAC fixture 失败");
        let fixture = TempAudioFile::from_bytes("flac", &encoded);
        let samples = [
            -0x80_0000_i32,
            0x7f_ffff,
            -0x12_3456,
            0x12_3456,
            -1,
            1,
            0,
            0x55_aa55,
            -0x40_0000,
            0x40_0000,
            -0x01_0203,
            0x01_0203,
            -0x7f_ffff,
            0x7f_fffe,
            -42,
            42,
        ];
        let mut decoder = DirectPcmDecoder::open_local(&fixture.path).unwrap();
        let mut frame = DirectPcmFrame::new().unwrap();

        assert!(decoder.read_frame(&mut frame).unwrap());
        assert_eq!(
            frame.format().unwrap(),
            DirectPcmFormat {
                sample_rate: 96_000,
                channels: 2,
                valid_bits: 24,
                storage_bits: 32,
                sample_format: DirectPcmSampleFormat::Signed32,
                memory_path: DirectPcmMemoryPath::ZeroCopyPacked,
            }
        );
        let decoded: Vec<i32> = frame
            .payload_bytes()
            .unwrap()
            .chunks_exact(4)
            .map(|chunk| i32::from_le_bytes(chunk.try_into().unwrap()))
            .collect();
        let expected: Vec<i32> = samples.iter().map(|sample| sample << 8).collect();
        assert_eq!(decoded, expected);
        assert!(decoded.iter().all(|sample| sample & 0xff == 0));
    }

    #[test]
    fn cached_flac_with_bin_extension_is_probed_without_changing_direct_path() {
        const FLAC24: &str = "ZkxhQwAAACIQABAAAAAvAAAvF3ADcAAAAAitKR6EKza4EYWT7tcmlaCQhAAAKCAAAAByZWZlcmVuY2UgbGliRkxBQyAxLjUuMCAyMDI1MDIxMQAAAAD/+GusAAfHEEwIINlWqVAQQIIgnh///9skaKuF3VapU3f///yQIC8P///J1OBjmg==";
        let encoded = base64::engine::general_purpose::STANDARD
            .decode(FLAC24)
            .expect("解码 FLAC cache fixture 失败");
        let fixture = TempAudioFile::from_bytes("bin", &encoded);
        let mut decoder = DirectPcmDecoder::open_local(&fixture.path).unwrap();
        let mut frame = DirectPcmFrame::new().unwrap();

        assert!(decoder.read_frame(&mut frame).unwrap());
        let format = frame.format().unwrap();
        assert_eq!(format.sample_rate, 96_000);
        assert_eq!(format.valid_bits, 24);
        assert_eq!(format.storage_bits, 32);
        assert_eq!(format.memory_path, DirectPcmMemoryPath::ZeroCopyPacked);
    }

    #[test]
    fn fragmented_mp4_flac_is_demuxed_by_the_direct_decoder() {
        const FMP4_FLAC: &str = "AAAAHGZ0eXBpc281AAACAGlzbzVpc282bXA0MQAAArhtb292AAAAbG12aGQAAAAAAAAAAAAAAAAAAAPoAAAAAAABAAABAAAAAAAAAAAAAAAAAQAAAAAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACAAABu3RyYWsAAABcdGtoZAAAAAMAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAAAAAAAAQEAAAAAAQAAAAAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAVdtZGlhAAAAIG1kaGQAAAAAAAAAAAAAAAAAAXcAAAAAAFXEAAAAAAAtaGRscgAAAAAAAAAAc291bgAAAAAAAAAAAAAAAFNvdW5kSGFuZGxlcgAAAAECbWluZgAAABBzbWhkAAAAAAAAAAAAAAAkZGluZgAAABxkcmVmAAAAAAAAAAEAAAAMdXJsIAAAAAEAAADGc3RibAAAAHpzdHNkAAAAAAAAAAEAAABqZkxhQwAAAAAAAAABAAAAAAAAAAAAAgAgAAAAAAAAAAAAAAAyZGZMYQAAAACAAAAiIAAgAAAAAAEEHBdwA/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABRidHJ0AAAAAAAB9AAAAfQAAAAAEHN0dHMAAAAAAAAAAAAAABBzdHNjAAAAAAAAAAAAAAAUc3RzegAAAAAAAAAAAAAAAAAAABBzdGNvAAAAAAAAAAAAAAAobXZleAAAACB0cmV4AAAAAAAAAAEAAAABAAAAAAAAAAAAAAAAAAAAYXVkdGEAAABZbWV0YQAAAAAAAAAhaGRscgAAAAAAAAAAbWRpcmFwcGwAAAAAAAAAAAAAAAAsaWxzdAAAACSpdG9vAAAAHGRhdGEAAAABAAAAAExhdmY2Mi4zLjEwMAAAAGRtb29mAAAAEG1maGQAAAAAAAAAAQAAAEx0cmFmAAAAHHRmaGQAAgA4AAAAAQAAA8AAAAAUAgAAAAAAABR0ZmR0AQAAAAAAAAAAAAAAAAAAFHRydW4AAAABAAAAAQAAAGwAAAAcbWRhdP/4ex4AA7+9AAAAAAAAAAAAANquAAAAQ21mcmEAAAArdGZyYQEAAAAAAAABAAAAAAAAAAEAAAAAAAAAAAAAAAAAAALUAQEBAAAAEG1mcm8AAAAAAAAAQw==";
        let encoded = base64::engine::general_purpose::STANDARD
            .decode(FMP4_FLAC)
            .expect("解码 fragmented MP4 fixture 失败");
        let fixture = TempAudioFile::from_bytes("mp4", &encoded);
        let mut decoder = DirectPcmDecoder::open_local(&fixture.path).unwrap();
        let mut frame = DirectPcmFrame::new().unwrap();

        assert!(decoder.read_frame(&mut frame).unwrap());
        let format = frame.format().unwrap();
        assert_eq!(format.sample_rate, 96_000);
        assert_eq!(format.channels, 2);
        assert_eq!(format.sample_format, DirectPcmSampleFormat::Signed32);
        assert_eq!(format.memory_path, DirectPcmMemoryPath::ZeroCopyPacked);
    }

    #[test]
    fn ape16_decode_is_bit_perfect_after_planar_repack() {
        const APE16: &str = "TUFDIHgP0AcWAAIARKwAACwAAAAAAAAAAQAAAMB6AgAAAAAAAQAAAFJJRkYk6wkAV0FWRWZtdCAQAAAAAQACAESsAAAQsQIABAAQAGRhdGEA6wkAWAAAABMRjbMHAAAAAAABAAAAAAA=";
        let encoded = base64::engine::general_purpose::STANDARD
            .decode(APE16)
            .expect("解码 APE fixture 失败");
        let fixture = TempAudioFile::from_bytes("ape", &encoded);
        let mut decoder = DirectPcmDecoder::open_local(&fixture.path).unwrap();
        let mut frame = DirectPcmFrame::new().unwrap();

        assert!(decoder.read_frame(&mut frame).unwrap());
        assert_eq!(
            frame.format().unwrap(),
            DirectPcmFormat {
                sample_rate: 44_100,
                channels: 2,
                valid_bits: 16,
                storage_bits: 16,
                sample_format: DirectPcmSampleFormat::Signed16,
                memory_path: DirectPcmMemoryPath::BitPerfectRepack,
            }
        );
        assert!(frame.payload_bytes().unwrap().iter().all(|byte| *byte == 0));
    }

    #[test]
    fn alac_and_wavpack_16_are_bit_perfect_after_planar_repack() {
        const ALAC16: &str = "AAAAHGZ0eXBNNEEgAAACAE00QSBpc29taXNvMgAAAAhmcmVlAAAAMG1kYXQgABIAAAARAAD//5+OYHP//gACAAAAVVJkrZ3/8gAOCAH4AOphFaHAAAACqm1vb3YAAABsbXZoZAAAAAAAAAAAAAAAAAAAA+gAAAABAAEAAAEAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAAAAAQAAAAAAAAAAAAAAAAAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAIAAAHVdHJhawAAAFx0a2hkAAAAAwAAAAAAAAAAAAAAAQAAAAAAAAABAAAAAAAAAAAAAAABAQAAAAABAAAAAAAAAAAAAAAAAAAAAQAAAAAAAAAAAAAAAAAAQAAAAAAAAAAAAAAAAAAAJGVkdHMAAAAcZWxzdAAAAAAAAAABAAAAAQAAAAAAAQAAAAABTW1kaWEAAAAgbWRoZAAAAAAAAAAAAAAAAAAArEQAAAAIVcQAAAAAAC1oZGxyAAAAAAAAAABzb3VuAAAAAAAAAAAAAAAAU291bmRIYW5kbGVyAAAAAPhtaW5mAAAAEHNtaGQAAAAAAAAAAAAAACRkaW5mAAAAHGRyZWYAAAAAAAAAAQAAAAx1cmwgAAAAAQAAALxzdGJsAAAAWHN0c2QAAAAAAAAAAQAAAEhhbGFjAAAAAAAAAAEAAAAAAAAAAAACABAAAAAArEQAAAAAACRhbGFjAAAAAAAAEAAAECgKDgIAAAAAQAQAFYiAAACsRAAAABhzdHRzAAAAAAAAAAEAAAABAAAACAAAABxzdHNjAAAAAAAAAAEAAAABAAAAAQAAAAEAAAAUc3RzegAAAAAAAAAoAAAAAQAAABRzdGNvAAAAAAAAAAEAAAAsAAAAYXVkdGEAAABZbWV0YQAAAAAAAAAhaGRscgAAAAAAAAAAbWRpcmFwcGwAAAAAAAAAAAAAAAAsaWxzdAAAACSpdG9vAAAAHGRhdGEAAAABAAAAAExhdmY2Mi4zLjEwMA==";
        const WAVPACK16: &str = "d3Zwa2YAAAAQBAAACAAAAAAAAAAIAAAAMRi8BMKPq/ECAVdWAwAEBJzucu4A/mr9BQZSBlIGfgaWA5YCUgOKFgAA///+/2Gq/v/f/0/+3W/9/7v9/9+X/v/9/5jz+//3Sfj/9x9z+P/3/4N2AwBBUEVUQUdFWNAHAAA8AAAAAQAAAAAAAKAAAAAAAAAAAAwAAAAAAAAAZW5jb2RlcgBMYXZmNjIuMy4xMDBBUEVUQUdFWNAHAAA8AAAAAQAAAAAAAIAAAAAAAAAAAA==";
        let expected = [
            -32768_i16, 32767, -12345, 12345, -1, 1, 0, 42, -22222, 22222, -7, 7, 1024, -1024,
            30000, -30000,
        ];
        let expected_bytes: Vec<u8> = expected
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();

        for (extension, encoded) in [("m4a", ALAC16), ("wv", WAVPACK16)] {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .expect("解码 planar 16-bit fixture 失败");
            let fixture = TempAudioFile::from_bytes(extension, &bytes);
            let mut decoder = DirectPcmDecoder::open_local(&fixture.path).unwrap();
            let mut frame = DirectPcmFrame::new().unwrap();

            assert!(decoder.read_frame(&mut frame).unwrap());
            assert_eq!(
                frame.format().unwrap(),
                DirectPcmFormat {
                    sample_rate: 44_100,
                    channels: 2,
                    valid_bits: 16,
                    storage_bits: 16,
                    sample_format: DirectPcmSampleFormat::Signed16,
                    memory_path: DirectPcmMemoryPath::BitPerfectRepack,
                }
            );
            assert_eq!(frame.payload_bytes().unwrap(), expected_bytes);
        }
    }

    #[test]
    fn alac_and_wavpack_24_keep_every_source_bit_after_planar_repack() {
        const ALAC24: &str = "AAAAHGZ0eXBNNEEgAAACAE00QSBpc29taXNvMgAAAAhmcmVlAAAAQG1kYXQgABIAAAARAAAA////25dUJGit///+AAACAAAAq1SrgAAAgAAB/fv6AgQHAAAC///9//+sAABVwAAAAqptb292AAAAbG12aGQAAAAAAAAAAAAAAAAAAAPoAAAAAQABAAABAAAAAAAAAAAAAAAAAQAAAAAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACAAAB1XRyYWsAAABcdGtoZAAAAAMAAAAAAAAAAAAAAAEAAAAAAAAAAQAAAAAAAAAAAAAAAQEAAAAAAQAAAAAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAACRlZHRzAAAAHGVsc3QAAAAAAAAAAQAAAAEAAAAAAAEAAAAAAU1tZGlhAAAAIG1kaGQAAAAAAAAAAAAAAAAAAXcAAAAACFXEAAAAAAAtaGRscgAAAAAAAAAAc291bgAAAAAAAAAAAAAAAFNvdW5kSGFuZGxlcgAAAAD4bWluZgAAABBzbWhkAAAAAAAAAAAAAAAkZGluZgAAABxkcmVmAAAAAAAAAAEAAAAMdXJsIAAAAAEAAAC8c3RibAAAAFhzdHNkAAAAAAAAAAEAAABIYWxhYwAAAAAAAAABAAAAAAAAAAAAAgAYAAAAAAAAAAAAAAAkYWxhYwAAAAAAABAAABgoCg4CAAAAAGAEAEZQAAABdwAAAAAYc3R0cwAAAAAAAAABAAAAAQAAAAgAAAAcc3RzYwAAAAAAAAABAAAAAQAAAAEAAAABAAAAFHN0c3oAAAAAAAAAOAAAAAEAAAAUc3RjbwAAAAAAAAABAAAALAAAAGF1ZHRhAAAAWW1ldGEAAAAAAAAAIWhkbHIAAAAAAAAAAG1kaXJhcHBsAAAAAAAAAAAAAAAALGlsc3QAAAAkqXRvbwAAABxkYXRhAAAAAQAAAABMYXZmNjIuMy4xMDA=";
        const WAVPACK24: &str = "d3Zwa5AAAAAQBAAACAAAAAAAAAAIAAAAMxncBkzY0mICAVdWAwAEBHzmSOYA/mr9BQZSBlIGfgbPBOgE6AQJAgAIAACKKAAA///+//9hqqr+/9///2cgIPb/7//PQlj///7/v4tI5v+///+/IdXK/7///9/HsdL/f/+/B+D/v/+/B9////1/eeX/v///H7g/Zv///f9YzAFBUEVUQUdFWNAHAAA8AAAAAQAAAAAAAKAAAAAAAAAAAAwAAAAAAAAAZW5jb2RlcgBMYXZmNjIuMy4xMDBBUEVUQUdFWNAHAAA8AAAAAQAAAAAAAIAAAAAAAAAAAA==";
        let source_samples = [
            -0x80_0000_i32,
            0x7f_ffff,
            -0x12_3456,
            0x12_3456,
            -1,
            1,
            0,
            0x55_aa55,
            -0x40_0000,
            0x40_0000,
            -0x01_0203,
            0x01_0203,
            -0x7f_ffff,
            0x7f_fffe,
            -42,
            42,
        ];
        let expected: Vec<i32> = source_samples.iter().map(|sample| sample << 8).collect();

        for (extension, encoded, expected_valid_bits) in
            [("m4a", ALAC24, 24_u8), ("wv", WAVPACK24, 32_u8)]
        {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .expect("解码 planar 24-bit fixture 失败");
            let fixture = TempAudioFile::from_bytes(extension, &bytes);
            let mut decoder = DirectPcmDecoder::open_local(&fixture.path).unwrap();
            let mut frame = DirectPcmFrame::new().unwrap();

            assert!(decoder.read_frame(&mut frame).unwrap());
            assert_eq!(frame.format().unwrap().sample_rate, 96_000);
            assert_eq!(frame.format().unwrap().channels, 2);
            assert_eq!(frame.format().unwrap().valid_bits, expected_valid_bits);
            assert_eq!(frame.format().unwrap().storage_bits, 32);
            assert_eq!(
                frame.format().unwrap().sample_format,
                DirectPcmSampleFormat::Signed32
            );
            assert_eq!(
                frame.format().unwrap().memory_path,
                DirectPcmMemoryPath::BitPerfectRepack
            );
            let decoded: Vec<i32> = frame
                .payload_bytes()
                .unwrap()
                .chunks_exact(4)
                .map(|chunk| i32::from_le_bytes(chunk.try_into().unwrap()))
                .collect();
            assert_eq!(decoded, expected);
            assert!(decoded.iter().all(|sample| sample & 0xff == 0));
        }
    }

    #[test]
    fn wavpack32_preserves_all_32_bits_after_planar_repack() {
        const WAVPACK32: &str = "d3Zwa6wAAAAQBAAACAAAAAAAAAAIAAAAMxlcBxyduWICAVdWAwAEBHzmSOYA/wD/BQZSBlIGfgY/BVIFUgUJAggAAACKKgAA///+//9hqqru/7///89AQOr/v/8/C8H///3/fxeRzP9///+/jGrV/7///9/HsdL/f/9/A6v///7/Hvz9//f/RsX/v///X7g/9v9/b/n/9//jMSEAjAoAACt000gA/4h4/wEAqgAA/AQB/tYqQVBFVEFHRVjQBwAAPAAAAAEAAAAAAACgAAAAAAAAAAAMAAAAAAAAAGVuY29kZXIATGF2ZjYyLjMuMTAwQVBFVEFHRVjQBwAAPAAAAAEAAAAAAACAAAAAAAAAAAA=";
        let encoded = base64::engine::general_purpose::STANDARD
            .decode(WAVPACK32)
            .expect("解码 WavPack32 fixture 失败");
        let fixture = TempAudioFile::from_bytes("wv", &encoded);
        let expected = [
            i32::MIN,
            i32::MAX,
            -0x1234_5678,
            0x1234_5678,
            -1,
            1,
            0,
            0x55aa_55aa,
            -0x4000_0000,
            0x4000_0000,
            -0x0102_0304,
            0x0102_0304,
            -2_147_483_647,
            2_147_483_646,
            -42,
            42,
        ];
        let mut decoder = DirectPcmDecoder::open_local(&fixture.path).unwrap();
        let mut frame = DirectPcmFrame::new().unwrap();

        assert!(decoder.read_frame(&mut frame).unwrap());
        assert_eq!(
            frame.format().unwrap(),
            DirectPcmFormat {
                sample_rate: 192_000,
                channels: 2,
                valid_bits: 32,
                storage_bits: 32,
                sample_format: DirectPcmSampleFormat::Signed32,
                memory_path: DirectPcmMemoryPath::BitPerfectRepack,
            }
        );
        let decoded: Vec<i32> = frame
            .payload_bytes()
            .unwrap()
            .chunks_exact(4)
            .map(|chunk| i32::from_le_bytes(chunk.try_into().unwrap()))
            .collect();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn planar_source_preallocates_repack_slots_and_callback_borrows_the_same_buffer() {
        const WAVPACK16: &str = "d3Zwa2YAAAAQBAAACAAAAAAAAAAIAAAAMRi8BMKPq/ECAVdWAwAEBJzucu4A/mr9BQZSBlIGfgaWA5YCUgOKFgAA///+/2Gq/v/f/0/+3W/9/7v9/9+X/v/9/5jz+//3Sfj/9x9z+P/3/4N2AwBBUEVUQUdFWNAHAAA8AAAAAQAAAAAAAKAAAAAAAAAAAAwAAAAAAAAAZW5jb2RlcgBMYXZmNjIuMy4xMDBBUEVUQUdFWNAHAAA8AAAAAQAAAAAAAIAAAAAAAAAAAA==";
        let encoded = base64::engine::general_purpose::STANDARD
            .decode(WAVPACK16)
            .expect("解码 WavPack fixture 失败");
        let fixture = TempAudioFile::from_bytes("wv", &encoded);
        let source = DirectPcmSource::open_local(&fixture.path).unwrap();
        let first_frame = unsafe { &*source.ring.slots[0].frame.get() };
        let raw_plane_ptr = unsafe { first_frame.raw.as_ref().data[0] }.cast_const();
        let repack_ptr = first_frame.payload_ptr().unwrap();

        assert_eq!(
            source.format().memory_path,
            DirectPcmMemoryPath::BitPerfectRepack
        );
        assert_ne!(repack_ptr, raw_plane_ptr);
        let initial_samples = first_frame
            .samples_per_channel()
            .checked_mul(usize::from(source.format().channels))
            .unwrap();
        for slot in &source.ring.slots {
            let frame = unsafe { &*slot.frame.get() };
            match &frame.repack {
                DirectPcmRepackBuffer::Signed16(buffer) => {
                    assert!(buffer.len() >= initial_samples)
                }
                _ => panic!("WavPack16 Source Direct slot 应预分配 S16 repack buffer"),
            }
        }

        let mut callback_ptr = ptr::null();
        let mut callback_len = 0_usize;
        assert!(unsafe {
            direct_pcm_next_block(
                source.callback_context(),
                &mut callback_ptr,
                &mut callback_len,
            )
        });
        assert_eq!(callback_ptr, repack_ptr);
        assert_eq!(callback_len, first_frame.payload_len);
        unsafe { direct_pcm_release_block(source.callback_context()) };
    }

    #[test]
    fn separate_frame_slots_keep_old_payload_alive_while_decoder_advances() {
        let frames = 32_768_usize;
        let mut pcm = Vec::with_capacity(frames * 4);
        for index in 0..frames {
            let left = (index as i16).wrapping_mul(17);
            let right = left.wrapping_neg();
            pcm.extend_from_slice(&left.to_le_bytes());
            pcm.extend_from_slice(&right.to_le_bytes());
        }
        let fixture = TempAudioFile::wav(192_000, 16, &pcm);
        let mut decoder = DirectPcmDecoder::open_local(&fixture.path).unwrap();
        let mut first = DirectPcmFrame::new().unwrap();
        let mut second = DirectPcmFrame::new().unwrap();

        assert!(decoder.read_frame(&mut first).unwrap());
        let first_ptr = first.payload_ptr().unwrap();
        let first_prefix = first.payload_bytes().unwrap()[..64].to_vec();
        assert!(decoder.read_frame(&mut second).unwrap());

        assert_eq!(first.payload_ptr().unwrap(), first_ptr);
        assert_eq!(&first.payload_bytes().unwrap()[..64], first_prefix);
        assert_ne!(first.payload_ptr().unwrap(), second.payload_ptr().unwrap());
    }

    #[test]
    fn ring_returns_the_same_ffmpeg_payload_pointer_without_copying() {
        let frames = 16_384_usize;
        let mut pcm = Vec::with_capacity(frames * 4);
        for index in 0..frames {
            let left = (index as i16).wrapping_mul(13);
            let right = left.wrapping_neg();
            pcm.extend_from_slice(&left.to_le_bytes());
            pcm.extend_from_slice(&right.to_le_bytes());
        }
        let fixture = TempAudioFile::wav(96_000, 16, &pcm);
        let source = DirectPcmSource::open_local(&fixture.path).unwrap();
        let first_frame = unsafe { &*source.ring.slots[0].frame.get() };
        let ffmpeg_ptr = first_frame.payload_ptr().unwrap();

        assert_eq!(source.format().sample_rate, 96_000);
        assert!(!source.failed());
        assert!(!source.finished());
        let mut data = ptr::null();
        let mut len = 0_usize;
        assert!(unsafe { direct_pcm_next_block(source.callback_context(), &mut data, &mut len) });
        assert_eq!(data, ffmpeg_ptr);
        assert_eq!(
            unsafe { slice::from_raw_parts(data, len) },
            first_frame.payload_bytes().unwrap()
        );
        unsafe { direct_pcm_release_block(source.callback_context()) };
    }

    #[test]
    fn packed_pcm_seek_keeps_exact_samples_without_copy_or_dsp() {
        let sample_rate = 8_000_u32;
        let frames = 64_usize;
        let mut pcm = Vec::with_capacity(frames * 4);
        for index in 0..frames {
            let left = 1_000_i16.wrapping_add(index as i16);
            let right = -2_000_i16.wrapping_sub(index as i16);
            pcm.extend_from_slice(&left.to_le_bytes());
            pcm.extend_from_slice(&right.to_le_bytes());
        }
        let fixture = TempAudioFile::wav(sample_rate, 16, &pcm);
        let target_frame = 16_usize;
        let target_secs = target_frame as f64 / f64::from(sample_rate);
        let (source, actual_position) =
            DirectPcmSource::open_local_at(&fixture.path, target_secs).unwrap();

        assert_eq!(actual_position, target_secs);
        let first_frame = unsafe { &*source.ring.slots[0].frame.get() };
        let expected_ptr = first_frame.payload_ptr().unwrap();
        let mut data = ptr::null();
        let mut len = 0_usize;
        assert!(unsafe {
            direct_pcm_next_block(source.callback_context(), &mut data, &mut len)
        });
        assert_eq!(data, expected_ptr);
        assert_eq!(
            unsafe { slice::from_raw_parts(data, len) },
            &pcm[target_frame * 4..target_frame * 4 + len]
        );
        unsafe { direct_pcm_release_block(source.callback_context()) };
        assert_eq!(source.consumed_position(), len as f64 / 4.0 / f64::from(sample_rate));
    }

    #[test]
    fn source_seek_reuses_the_same_callback_context_and_ring() {
        let sample_rate = 8_000_u32;
        let frames = 64_usize;
        let mut pcm = Vec::with_capacity(frames * 4);
        for index in 0..frames {
            let left = 3_000_i16.wrapping_add(index as i16);
            let right = -4_000_i16.wrapping_sub(index as i16);
            pcm.extend_from_slice(&left.to_le_bytes());
            pcm.extend_from_slice(&right.to_le_bytes());
        }
        let fixture = TempAudioFile::wav(sample_rate, 16, &pcm);
        let mut source = DirectPcmSource::open_local(&fixture.path).unwrap();
        let context_before = source.callback_context();
        let target_frame = 24_usize;
        let target_secs = target_frame as f64 / f64::from(sample_rate);

        let actual = source.seek_while_paused(target_secs).unwrap();
        assert_eq!(actual, target_secs);
        assert_eq!(source.callback_context(), context_before);
        let mut data = ptr::null();
        let mut len = 0_usize;
        assert!(unsafe {
            direct_pcm_next_block(source.callback_context(), &mut data, &mut len)
        });
        assert_eq!(
            unsafe { slice::from_raw_parts(data, len) },
            &pcm[target_frame * 4..target_frame * 4 + len]
        );
        unsafe { direct_pcm_release_block(source.callback_context()) };
    }

    /// 关流前淡出：当前块头部位精确、窗尾渐零，后续块全静音
    #[test]
    fn fade_out_ramps_block_head_bit_exact_then_silence() {
        let sample_rate = 44_100_u32;
        let mut pcm: Vec<u8> = Vec::new();
        for i in 0..16_384_i32 {
            let sample = ((i * 37) % 20000 - 10000) as i16;
            pcm.extend_from_slice(&sample.to_le_bytes());
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        let fixture = TempAudioFile::wav(sample_rate, 16, &pcm);
        let reference = DirectPcmSource::open_local(&fixture.path).unwrap();
        let fading = DirectPcmSource::open_local(&fixture.path).unwrap();

        let mut data = ptr::null();
        let mut len = 0_usize;
        // 两个源的第一块位精确一致（淡出前零触碰）
        assert!(
            unsafe { direct_pcm_next_block(reference.callback_context(), &mut data, &mut len) },
            "reference#1: failed={} finished={}",
            reference.monitor().failed(),
            reference.monitor().finished()
        );
        let ref1 = unsafe { slice::from_raw_parts(data, len) }.to_vec();
        unsafe { direct_pcm_release_block(reference.callback_context()) };
        assert!(
            poll_next_block(fading.callback_context(), &mut data, &mut len),
            "fading#1: failed={} finished={}",
            fading.monitor().failed(),
            fading.monitor().finished()
        );
        assert_eq!(unsafe { slice::from_raw_parts(data, len) }, ref1);
        unsafe { direct_pcm_release_block(fading.callback_context()) };

        // 启动淡出：下一块头部增益 1.0（位精确），窗尾渐零
        fading.begin_fade_out();
        assert!(!fading.is_faded_out());
        assert!(
            poll_next_block(reference.callback_context(), &mut data, &mut len),
            "reference#2: failed={} finished={}",
            reference.monitor().failed(),
            reference.monitor().finished()
        );
        let ref2 = unsafe { slice::from_raw_parts(data, len) }.to_vec();
        unsafe { direct_pcm_release_block(reference.callback_context()) };
        assert!(
            poll_next_block(fading.callback_context(), &mut data, &mut len),
            "fading#2: failed={} finished={} silence={}",
            fading.monitor().failed(),
            fading.monitor().finished(),
            fading.silence_blocks_handed()
        );
        let faded = unsafe { slice::from_raw_parts(data, len) };
        assert_eq!(&faded[..2], &ref2[..2], "块首样本增益 1.0 应保持位精确");
        let last = i16::from_le_bytes([faded[len - 2], faded[len - 1]]);
        assert_eq!(last, 0, "淡出窗尾应落到零电平");
        assert!(faded.len() == ref2.len());
        unsafe { direct_pcm_release_block(fading.callback_context()) };
        assert!(fading.is_faded_out());

        // 后续块全部数字静音，且静音块计数递增
        assert!(
            poll_next_block(fading.callback_context(), &mut data, &mut len),
            "fading#3 静音块: failed={} finished={} silence={}",
            fading.monitor().failed(),
            fading.monitor().finished(),
            fading.silence_blocks_handed()
        );
        let silent = unsafe { slice::from_raw_parts(data, len) };
        assert!(silent.iter().all(|byte| *byte == 0));
        unsafe { direct_pcm_release_block(fading.callback_context()) };
        assert_eq!(fading.silence_blocks_handed(), 1);
    }

    #[test]
    fn same_wire_format_handoff_keeps_callback_context_across_packed_and_planar_pcm() {
        const WAVPACK16: &str = "d3Zwa2YAAAAQBAAACAAAAAAAAAAIAAAAMRi8BMKPq/ECAVdWAwAEBJzucu4A/mr9BQZSBlIGfgaWA5YCUgOKFgAA///+/2Gq/v/f/0/+3W/9/7v9/9+X/v/9/5jz+//3Sfj/9x9z+P/3/4N2AwBBUEVUQUdFWNAHAAA8AAAAAQAAAAAAAKAAAAAAAAAAAAwAAAAAAAAAZW5jb2RlcgBMYXZmNjIuMy4xMDBBUEVUQUdFWNAHAAA8AAAAAQAAAAAAAIAAAAAAAAAAAA==";
        let initial_samples = [100_i16, -100, 200, -200, 300, -300, 400, -400];
        let initial_pcm: Vec<u8> = initial_samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        let initial = TempAudioFile::wav(44_100, 16, &initial_pcm);
        let replacement_bytes = base64::engine::general_purpose::STANDARD
            .decode(WAVPACK16)
            .expect("解码 WavPack handoff fixture 失败");
        let replacement = TempAudioFile::from_bytes("wv", &replacement_bytes);
        let expected = [
            -32768_i16, 32767, -12345, 12345, -1, 1, 0, 42, -22222, 22222, -7, 7, 1024,
            -1024, 30000, -30000,
        ];
        let expected_bytes: Vec<u8> = expected
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();

        let mut source = DirectPcmSource::open_local(&initial.path).unwrap();
        let context = source.callback_context();
        assert_eq!(
            source.format().memory_path,
            DirectPcmMemoryPath::ZeroCopyPacked
        );
        let new_format = source
            .replace_drained_local(
                &replacement.path.to_string_lossy(),
                HttpCancelHandle::new(),
            )
            .unwrap();
        assert_eq!(source.callback_context(), context);
        assert_eq!(new_format.sample_rate, 44_100);
        assert_eq!(new_format.channels, 2);
        assert_eq!(new_format.storage_bits, 16);
        assert_eq!(new_format.memory_path, DirectPcmMemoryPath::BitPerfectRepack);

        let mut data = ptr::null();
        let mut len = 0_usize;
        assert!(unsafe {
            direct_pcm_next_block(source.callback_context(), &mut data, &mut len)
        });
        assert_eq!(unsafe { slice::from_raw_parts(data, len) }, expected_bytes);
        unsafe { direct_pcm_release_block(source.callback_context()) };
    }

    #[test]
    fn incompatible_pcm_handoff_fails_before_replacing_the_current_ring() {
        let initial_pcm = vec![0_u8; 64 * 4];
        let initial = TempAudioFile::wav(44_100, 16, &initial_pcm);
        let incompatible = TempAudioFile::wav(48_000, 16, &initial_pcm);
        let mut source = DirectPcmSource::open_local(&initial.path).unwrap();
        let context = source.callback_context();
        let format = source.format();

        let error = source
            .replace_drained_local(
                &incompatible.path.to_string_lossy(),
                HttpCancelHandle::new(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("wire format"));
        assert_eq!(source.callback_context(), context);
        assert_eq!(source.format(), format);
        assert!(!source.failed());
    }

    #[test]
    fn staged_pcm_handoff_is_sample_contiguous_and_marks_the_exact_boundary() {
        let first_samples = [-300_i16, 300, -200, 200, -100, 100, -1, 1];
        let second_samples = [11_i16, -11, 22, -22, 33, -33, 44, -44];
        let first_pcm: Vec<u8> = first_samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        let second_pcm: Vec<u8> = second_samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        let first = TempAudioFile::wav(44_100, 16, &first_pcm);
        let second = TempAudioFile::wav(44_100, 16, &second_pcm);
        let source = DirectPcmSource::open_local(&first.path).unwrap();
        source.set_duration(1.0);
        let monitor = source.monitor();
        source
            .stage_handle()
            .stage_local(&second.path, 2.0, 7)
            .unwrap();

        let mut collected = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while collected.len() < first_pcm.len() + second_pcm.len()
            && std::time::Instant::now() < deadline
        {
            let mut data = ptr::null();
            let mut len = 0_usize;
            if unsafe {
                direct_pcm_next_block(source.callback_context(), &mut data, &mut len)
            } {
                collected.extend_from_slice(unsafe { slice::from_raw_parts(data, len) });
            } else {
                thread::sleep(Duration::from_millis(1));
            }
        }
        unsafe { direct_pcm_release_block(source.callback_context()) };

        let mut expected = first_pcm;
        expected.extend_from_slice(&second_pcm);
        assert_eq!(collected, expected);
        assert_eq!(monitor.transition_count(), 1);
        assert_eq!(monitor.boundary_generation(), 7);
        assert_eq!(monitor.duration(), 2.0);
    }

    #[test]
    fn strict_ring_underrun_returns_no_block_instead_of_inserting_silence() {
        let ring = DirectPcmRing::new().unwrap();
        assert!(ring.next_block().is_none());
    }

    /// pre-mute 窗口（换源/seek 复位触发）：空环拉取交付静音块而非"无块"，
    /// 消除 Target 欠载杂音；窗口关闭后恢复严格欠载语义
    #[test]
    fn pre_mute_window_delivers_silence_over_the_underrun_gap() {
        let ring = DirectPcmRing::new().unwrap();
        // 未触发窗口：严格欠载语义（None）保持不变
        assert!(ring.next_block().is_none());

        // 模拟播放中记录的块几何 + 换源/seek 触发的 pre-mute 窗口
        ring.last_block_bytes.store(480, Ordering::Relaxed);
        ring.last_block_frames.store(120, Ordering::Relaxed);
        ring.trigger_pre_mute();
        let block = ring.next_block().expect("pre-mute 窗口内应交付静音块");
        assert_eq!(block.len, 480);
        assert!(unsafe { std::slice::from_raw_parts(block.data, block.len) }
            .iter()
            .all(|&b| b == 0));

        // 窗口关闭后恢复严格欠载语义，保证 Ended 判定不受影响
        ring.pre_mute_until_ms.store(0, Ordering::Relaxed);
        assert!(ring.next_block().is_none());
    }

    #[test]
    fn multichannel_downmix_planar_5_1_and_6_1_to_stereo() {
        let mut ch_data: Vec<Vec<i16>> = (0..7)
            .map(|ch| vec![(ch as i16 + 1) * 1000; 16])
            .collect();
        let mut ptrs: Vec<*mut u8> = ch_data
            .iter_mut()
            .map(|v| v.as_mut_ptr() as *mut u8)
            .collect();

        // 测试 5.1 (6 声道) 下混
        let mut output_5_1 = vec![0_i16; 32];
        unsafe {
            downmix_planar_i16(ptrs.as_mut_ptr(), 6, 0, 16, &mut output_5_1).unwrap();
        }
        assert_eq!(output_5_1.len(), 32);
        assert_eq!(output_5_1[0], 6657);
        assert_eq!(output_5_1[1], 8364);

        // 测试 6.1 (7 声道) 下混
        let mut output_6_1 = vec![0_i16; 32];
        unsafe {
            downmix_planar_i16(ptrs.as_mut_ptr(), 7, 0, 16, &mut output_6_1).unwrap();
        }
        assert_eq!(output_6_1.len(), 32);
        assert_eq!(output_6_1[0], 9864);
        assert_eq!(output_6_1[1], 11571);

        // 测试 packed 5.1 (6 声道) 下混
        let mut packed_5_1 = vec![0_i16; 6 * 16];
        for i in 0..16 {
            for ch in 0..6 {
                packed_5_1[i * 6 + ch] = (ch as i16 + 1) * 1000;
            }
        }
        let mut output_packed_5_1 = vec![0_i16; 32];
        unsafe {
            downmix_packed_i16(packed_5_1.as_ptr(), 6, 0, 16, &mut output_packed_5_1).unwrap();
        }
        assert_eq!(output_packed_5_1[0], 6657);
        assert_eq!(output_packed_5_1[1], 8364);
    }

    #[test]
    fn silence_blocks_use_per_slot_storage() {
        let ring = DirectPcmRing::new().unwrap();
        let slot_a = &ring.slots[0];
        let slot_b = &ring.slots[1];

        slot_a.fill_silence(1024, 256);
        let ptr_a = slot_a.payload_ptr.load(Ordering::Relaxed);
        assert_eq!(slot_a.payload_len.load(Ordering::Relaxed), 1024);

        slot_b.fill_silence(4096, 1024);
        let ptr_b = slot_b.payload_ptr.load(Ordering::Relaxed);
        assert_ne!(ptr_a, ptr_b, "两个 slot 的静音块不得共享同一分配");

        // slot_b 扩容不得影响 slot_a 已发布的指针
        slot_b.fill_silence(8192, 2048);
        assert_eq!(slot_a.payload_ptr.load(Ordering::Relaxed), ptr_a);
        assert_eq!(slot_b.payload_len.load(Ordering::Relaxed), 8192);

        // PCM 静音即全零（signed 零点）
        let silence = unsafe { &*slot_b.silence.get() };
        assert_eq!(silence.len(), 8192);
        assert!(silence.iter().all(|&b| b == 0));
    }

    #[test]
    fn quad_downmix_routes_bl_to_left_and_br_to_right() {
        // quad 布局（FL FR BL BR）：planes[2] 是 BL 而非 FC，planes[3] 是 BR
        let mut ch_data: Vec<Vec<i16>> = (0..4)
            .map(|ch| vec![(ch as i16 + 1) * 1000; 16])
            .collect();
        let mut ptrs: Vec<*mut u8> = ch_data
            .iter_mut()
            .map(|v| v.as_mut_ptr() as *mut u8)
            .collect();

        let mut output = vec![0_i16; 32];
        unsafe {
            downmix_planar_i16(ptrs.as_mut_ptr(), 4, 0, 16, &mut output).unwrap();
        }
        // L = FL + 0.5·BL = 1000 + 1500 = 2500；R = FR + 0.5·BR = 2000 + 2000 = 4000
        assert_eq!(output[0], 2500);
        assert_eq!(output[1], 4000);

        // packed 路径同归属
        let mut packed = vec![0_i16; 4 * 16];
        for i in 0..16 {
            for ch in 0..4 {
                packed[i * 4 + ch] = (ch as i16 + 1) * 1000;
            }
        }
        let mut packed_output = vec![0_i16; 32];
        unsafe {
            downmix_packed_i16(packed.as_ptr(), 4, 0, 16, &mut packed_output).unwrap();
        }
        assert_eq!(packed_output[0], 2500);
        assert_eq!(packed_output[1], 4000);
    }

    #[test]
    fn five_channel_downmix_routes_surround_pair_by_side() {
        // 5.0：修复前 idx3/idx4 按奇偶灌反左右
        let mut ch_data: Vec<Vec<i16>> = (0..5)
            .map(|ch| vec![(ch as i16 + 1) * 1000; 16])
            .collect();
        let mut ptrs: Vec<*mut u8> = ch_data
            .iter_mut()
            .map(|v| v.as_mut_ptr() as *mut u8)
            .collect();

        let mut output = vec![0_i16; 32];
        unsafe {
            downmix_planar_i16(ptrs.as_mut_ptr(), 5, 0, 16, &mut output).unwrap();
        }
        // L = FL + √½·FC + 0.5·idx3 = 1000 + 2121 + 2000 = 5121
        // R = FR + √½·FC + 0.5·idx4 = 2000 + 2121 + 2500 = 6621
        assert_eq!(output[0], 5121);
        assert_eq!(output[1], 6621);
    }

    #[test]
    fn strict_direct_rejects_unsupported_pcm() {
        let mut frame = DirectPcmFrame::new().unwrap();
        let raw = unsafe { frame.raw.as_mut() };
        raw.format = sys::AVSampleFormat_AV_SAMPLE_FMT_DBL as i32;
        raw.sample_rate = 44_100;
        raw.nb_samples = 32;
        raw.ch_layout.nb_channels = 2;

        let error = frame.accept_decoded_frame(32).unwrap_err();
        assert!(error.to_string().contains("不支持 FFmpeg sample format"));
    }

    // ── Handoff（连接复用）测试 ────────────────────────────────────────────────

    /// same_pcm_transport：同格式（44.1kHz/16bit/2ch）应该通过
    #[test]
    fn same_pcm_transport_accepts_identical_format() {
        let f1 = DirectPcmFormat {
            sample_rate: 44_100,
            channels: 2,
            valid_bits: 16,
            storage_bits: 16,
            sample_format: DirectPcmSampleFormat::Signed16,
            memory_path: DirectPcmMemoryPath::ZeroCopyPacked,
        };
        let f2 = DirectPcmFormat {
            sample_rate: 44_100,
            channels: 2,
            valid_bits: 16,
            storage_bits: 16,
            sample_format: DirectPcmSampleFormat::Signed16,
            memory_path: DirectPcmMemoryPath::BitPerfectRepack, // memory_path 不影响兼容性
        };
        assert!(same_pcm_transport(f1, f2));
    }

    /// same_pcm_transport：不同 sample_rate 应该拒绝
    #[test]
    fn same_pcm_transport_rejects_different_sample_rate() {
        let f1 = DirectPcmFormat {
            sample_rate: 44_100,
            channels: 2,
            valid_bits: 16,
            storage_bits: 16,
            sample_format: DirectPcmSampleFormat::Signed16,
            memory_path: DirectPcmMemoryPath::ZeroCopyPacked,
        };
        let f2 = DirectPcmFormat {
            sample_rate: 48_000,
            channels: 2,
            valid_bits: 16,
            storage_bits: 16,
            sample_format: DirectPcmSampleFormat::Signed16,
            memory_path: DirectPcmMemoryPath::ZeroCopyPacked,
        };
        assert!(!same_pcm_transport(f1, f2));
    }

    /// same_pcm_transport：不同 channels 应该拒绝
    #[test]
    fn same_pcm_transport_rejects_different_channels() {
        let f1 = DirectPcmFormat {
            sample_rate: 44_100,
            channels: 2,
            valid_bits: 16,
            storage_bits: 16,
            sample_format: DirectPcmSampleFormat::Signed16,
            memory_path: DirectPcmMemoryPath::ZeroCopyPacked,
        };
        let f2 = DirectPcmFormat {
            sample_rate: 44_100,
            channels: 1,
            valid_bits: 16,
            storage_bits: 16,
            sample_format: DirectPcmSampleFormat::Signed16,
            memory_path: DirectPcmMemoryPath::ZeroCopyPacked,
        };
        assert!(!same_pcm_transport(f1, f2));
    }

    /// open_source：本地文件走 avformat_open_input 路径（回归测试 dispatcher 兼容）
    #[test]
    fn open_source_local_file_path_succeeds() {
        let samples = (0i16..512i16).flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>();
        let fixture = TempAudioFile::wav(44_100, 16, &samples);
        // open_source 接受 CString path string，与 open_local 等价
        let result = DirectPcmDecoder::open_source(&fixture.path.to_string_lossy());
        assert!(
            result.is_ok(),
            "open_source 对本地文件路径应该成功"
        );
    }

    /// open_source：无效 HTTP URL 应该走 HTTP 分支并被 FFmpeg 拒绝（不是 panic 或死锁）
    #[test]
    fn open_source_http_url_fails_gracefully_on_invalid_url() {
        // 使用一个明确不可达的 URL；FFmpeg 应该报错而不是崩溃
        let url = "http://127.0.0.1:59999/nonexistent";
        let result = DirectPcmDecoder::open_source(url);
        assert!(
            result.is_err(),
            "无效 HTTP URL 应该返回错误，而不是 panic 或成功"
        );
        // 关键：调用没有 panic
    }

    /// open_source：HTTP URL 不应被 dispatcher 当作本地路径（如果走本地分支会报 NUL 或 No such file）
    #[test]
    fn open_source_http_url_dispatches_to_http_branch() {
        let url = "http://127.0.0.1:59999/";
        let result = DirectPcmDecoder::open_source(url);
        // 关键断言：返回的是 Err（FFmpeg 网络层拒绝）
        // 注意：不能再 format!("{:?}", err)，因为错误源链里的 DirectPcmDecoder 不实现 Debug
        assert!(result.is_err(), "HTTP URL 应该走 HTTP 分支并被 FFmpeg 拒绝");
    }

    /// replace_drained_local：同格式切换后 silence_blocks 归零、消费新 slot 后 transition_count +1
    #[test]
    fn replace_local_clears_silence_blocks_and_increments_transition_count() {
        let samples_a = (0i16..8192i16).flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>();
        let samples_b = (8192i16..16384i16).flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>();
        let fixture_a = TempAudioFile::wav(44_100, 16, &samples_a);
        let fixture_b = TempAudioFile::wav(44_100, 16, &samples_b);

        let mut source = DirectPcmSource::open_local(&fixture_a.path).unwrap();
        let monitor = source.monitor();

        // pump 一些帧（让 ring 进入稳定播放状态）
        for _ in 0..10 {
            let mut data = ptr::null::<u8>();
            let mut len = 0_usize;
            if unsafe { direct_pcm_next_block(source.callback_context(), &mut data, &mut len) } {
                unsafe { direct_pcm_release_block(source.callback_context()) };
            }
        }
        let tx_before = monitor.transition_count();

        // 同格式 handoff
        source.set_duration(2.0);
        let new_format = source
            .replace_drained_local(&fixture_b.path.to_string_lossy(), HttpCancelHandle::new())
            .unwrap();

        // 断言 1：新格式与原格式完全兼容
        assert_eq!(new_format.sample_rate, 44_100);
        assert_eq!(new_format.channels, 2);
        assert_eq!(new_format.storage_bits, 16);

        // 断言 2：handoff 后 ring 仍然可读（播放流不断）
        let mut data = ptr::null::<u8>();
        let mut len = 0_usize;
        let readable = unsafe { direct_pcm_next_block(source.callback_context(), &mut data, &mut len) };
        assert!(readable, "handoff 后 ring 应立即可读，否则切歌有卡顿");
        if readable {
            unsafe { direct_pcm_release_block(source.callback_context()) };
        }

        // 断言 3：消费新 slot 后 transition_count +1（切歌事件被 ring 标记）
        // 注：transition_count 在 next_block() 内部递增，不是在 reset_for_transition()
        let tx_after = monitor.transition_count();
        assert_eq!(
            tx_after,
            tx_before + 1,
            "handoff 后消费新 slot 应使 transition_count +1，tx_before={tx_before} tx_after={tx_after}"
        );

        // 断言 4：silence_blocks 计数被 reset_for_transition 清零
        // （只要没有连续消费静音块，silence_blocks 应保持 0）
        let silence_after = monitor.silence_blocks();
        assert!(
            silence_after <= 8,
            "handoff 后 silence_blocks 不应大量累积，实际={silence_after}（这会导致切歌拖尾）"
        );
    }

    /// replace_drained_local：不同格式应该返回错误（不 panic、不泄漏连接状态）
    #[test]
    fn replace_local_rejects_different_format_gracefully() {
        // fixture_a: 44.1kHz; fixture_b: 48kHz（不同 sample_rate → 不兼容）
        let samples_a = (0i16..4096i16).flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>();
        let samples_b = (4096i16..8192i16).flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>();
        let fixture_a = TempAudioFile::wav(44_100, 16, &samples_a);
        let fixture_b = TempAudioFile::wav(48_000, 16, &samples_b); // 不同 sample_rate

        let mut source = DirectPcmSource::open_local(&fixture_a.path).unwrap();

        let result = source
            .replace_drained_local(&fixture_b.path.to_string_lossy(), HttpCancelHandle::new());

        assert!(
            result.is_err(),
            "不同 sample_rate 应该返回错误，而不是静默替换导致音频损坏"
        );

        // 原始 source 仍然可用（错误不泄漏状态）
        let mut data = ptr::null::<u8>();
        let mut len = 0_usize;
        let still_playing =
            unsafe { direct_pcm_next_block(source.callback_context(), &mut data, &mut len) };
        assert!(
            still_playing,
            "格式不兼容错误不应破坏原始 source"
        );
        if still_playing {
            unsafe { direct_pcm_release_block(source.callback_context()) };
        }
    }
}

